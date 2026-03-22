use tracing::info;

use crate::state::position::Position;
use crate::types::CC_PER_CENT;

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use std::io::ErrorKind;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

const RESULTS_HEADER: &str = "run_ts_utc,series_ticker,market_ticker,open_time_utc,close_time_utc,target_price_usd,coinbase_price_usd,opening_yes_cents,opening_no_cents,yes_qty,no_qty,yes_avg_cents,no_avg_cents,pair_cost_cents,pair_cost_dollars,max_balance_price_cents,locked_floor_cents,locked_floor_dollars,pnl_yes_win,pnl_no_win\n";

fn cc_to_cents(cc: i64) -> f64 {
    cc as f64 / CC_PER_CENT as f64
}
fn cc_to_dollars(cc: i64) -> f64 {
    cc as f64 / (CC_PER_CENT as f64 * 100.0)
}

pub fn log_position(ticker: &str, pos: &Position) {
    let opening_yes_cents = pos.opening_yes_price_cents.map(|v| v as f64);
    let opening_no_cents = pos.opening_no_price_cents.map(|v| v as f64);
    let yes_avg_cents = pos.avg_yes_cc().map(cc_to_cents);
    let no_avg_cents = pos.avg_no_cc().map(cc_to_cents);
    let pair_cost_cents = pos.pair_cost_cc().map(cc_to_cents);
    let pair_cost_dollars = pos.pair_cost_cc().map(cc_to_dollars);
    let locked_floor_cents = cc_to_cents(pos.locked_floor_cc());
    let locked_floor_dollars = cc_to_dollars(pos.locked_floor_cc());

    info!(
        ticker = %ticker,
        yes_qty = pos.yes_qty,
        no_qty = pos.no_qty,
        opening_yes_cents = ?opening_yes_cents,
        opening_no_cents = ?opening_no_cents,
        yes_avg_cents = ?yes_avg_cents,
        no_avg_cents = ?no_avg_cents,
        pair_cost_cents = ?pair_cost_cents,
        pair_cost_dollars = ?pair_cost_dollars,
        locked_floor_cents,
        locked_floor_dollars,
        "position snapshot"
    );
}

fn fmt_ts_rfc3339(ts: i64) -> String {
    Utc.timestamp_opt(ts, 0)
        .single()
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| ts.to_string())
}

fn fmt_opt_2(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.2}"))
        .unwrap_or_else(|| "".to_string())
}

fn fmt_opt_4(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.4}"))
        .unwrap_or_else(|| "".to_string())
}

pub async fn append_result_csv(
    path: &str,
    series_ticker: &str,
    market_ticker: &str,
    open_ts: i64,
    close_ts: i64,
    target_price: Option<f64>,
    coinbase_price: Option<f64>,
    locked_floor_buffer_cc: i64,
    pos: &Position,
) -> Result<()> {
    let p = std::path::Path::new(path);

    let needs_header = match tokio::fs::metadata(p).await {
        Ok(m) => m.len() == 0,
        Err(e) if e.kind() == ErrorKind::NotFound => true,
        Err(e) => return Err(e).context("metadata(results_file)"),
    };

    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(p)
        .await
        .with_context(|| format!("open results file {}", p.display()))?;

    if needs_header {
        f.write_all(RESULTS_HEADER.as_bytes()).await?;
    }

    let run_ts = Utc::now().to_rfc3339();
    let open_time = fmt_ts_rfc3339(open_ts);
    let close_time = fmt_ts_rfc3339(close_ts);

    let opening_yes_cents = pos.opening_yes_price_cents.map(|v| v as f64);
    let opening_no_cents = pos.opening_no_price_cents.map(|v| v as f64);
    let yes_avg_cents = pos.avg_yes_cc().map(cc_to_cents);
    let no_avg_cents = pos.avg_no_cc().map(cc_to_cents);
    let pair_cost_cents = pos.pair_cost_cc().map(cc_to_cents);
    let pair_cost_dollars = pos.pair_cost_cc().map(cc_to_dollars);
    let max_balance_price_cents = if pos.yes_qty < pos.no_qty {
        pos.max_avg_price_to_balance_cc(crate::types::Side::Yes, locked_floor_buffer_cc)
            .map(cc_to_cents)
    } else if pos.no_qty < pos.yes_qty {
        pos.max_avg_price_to_balance_cc(crate::types::Side::No, locked_floor_buffer_cc)
            .map(cc_to_cents)
    } else {
        None
    };
    let locked_floor_cents = cc_to_cents(pos.locked_floor_cc());
    let locked_floor_dollars = cc_to_dollars(pos.locked_floor_cc());

    let total_cost_cc = pos.yes_cost_cc.saturating_add(pos.no_cost_cc);
    let total_cost_dollars = cc_to_dollars(total_cost_cc);
    let yes_qty = pos.yes_qty.max(0) as f64;
    let no_qty = pos.no_qty.max(0) as f64;
    let pnl_yes_win_dollars = yes_qty - total_cost_dollars;
    let pnl_no_win_dollars = no_qty - total_cost_dollars;

    let line = format!(
        "{run_ts},{series_ticker},{market_ticker},{open_time},{close_time},{},{},{},{},{},{},{},{},{},{},{},{:.2},{:.4},{},{}\n",
        fmt_opt_2(target_price),
        fmt_opt_2(coinbase_price),
        fmt_opt_2(opening_yes_cents),
        fmt_opt_2(opening_no_cents),
        pos.yes_qty,
        pos.no_qty,
        fmt_opt_2(yes_avg_cents),
        fmt_opt_2(no_avg_cents),
        fmt_opt_2(pair_cost_cents),
        fmt_opt_4(pair_cost_dollars),
        fmt_opt_2(max_balance_price_cents),
        locked_floor_cents,
        locked_floor_dollars,
        pnl_yes_win_dollars,
        pnl_no_win_dollars,
    );

    f.write_all(line.as_bytes()).await?;
    f.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn append_result_csv_writes_window_summary_fields() {
        let path =
            std::env::temp_dir().join(format!("kalshi-results-{}.csv", uuid::Uuid::new_v4()));
        let path_str = path.to_string_lossy().to_string();
        let mut pos = Position::default();

        pos.apply_fill(crate::types::Side::Yes, 49, 1);
        pos.apply_fill(crate::types::Side::No, 50, 1);

        append_result_csv(
            &path_str,
            "KXBTC15M",
            "KXBTC15M-TEST",
            1_700_000_000,
            1_700_000_900,
            Some(95_000.0),
            Some(95_123.45),
            100,
            &pos,
        )
        .await
        .unwrap();

        let contents = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(contents.contains("series_ticker,market_ticker"));
        assert!(contents.contains("target_price_usd,coinbase_price_usd"));
        assert!(contents.contains("opening_yes_cents,opening_no_cents"));
        assert!(contents.contains("max_balance_price_cents"));
        assert!(contents.contains("KXBTC15M,KXBTC15M-TEST"));
        assert!(contents.contains("95000.00,95123.45"));
        assert!(contents.contains("49.00,50.00,1,1"));

        let _ = tokio::fs::remove_file(path).await;
    }
}
