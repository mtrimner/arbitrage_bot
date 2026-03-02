// src/report.rs
use tracing::info;

use crate::state::position::Position;
use crate::types::CC_PER_CENT;

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use std::io::ErrorKind;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

fn cc_to_cents(cc: i64) -> f64 {
    cc as f64 / CC_PER_CENT as f64
}
fn cc_to_dollars(cc: i64) -> f64 {
    cc as f64 / (CC_PER_CENT as f64 * 100.0)
}

pub fn log_position(ticker: &str, pos: &Position) {
    let yes_avg_cents = pos.avg_yes_cc().map(cc_to_cents);
    let no_avg_cents  = pos.avg_no_cc().map(cc_to_cents);

    // “total avg price” in your world is really “pair cost” (avg_yes + avg_no)
    let pair_cost_cents  = pos.pair_cost_cc().map(cc_to_cents);
    let pair_cost_dollars = pos.pair_cost_cc().map(cc_to_dollars);

    info!(
        ticker = %ticker,
        yes_qty = pos.yes_qty,
        no_qty = pos.no_qty,
        yes_avg_cents = ?yes_avg_cents,
        no_avg_cents = ?no_avg_cents,
        pair_cost_cents = ?pair_cost_cents,
        pair_cost_dollars = ?pair_cost_dollars,
        "position snapshot"
    );
}

// NEW: append one CSV row per closed window
fn fmt_ts_rfc3339(ts: i64) -> String {
    Utc.timestamp_opt(ts, 0)
        .single()
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| ts.to_string())
}

fn fmt_opt_2(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.2}")).unwrap_or_else(|| "".to_string())
}

fn fmt_opt_4(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.4}")).unwrap_or_else(|| "".to_string())
}

pub async fn append_result_csv(
    path: &str,
    open_ts: i64,
    close_ts: i64,
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
        let header = "run_ts_utc,open_time_utc,close_time_utc,yes_qty,no_qty,yes_avg_cents,no_avg_cents,pair_cost_cents,pair_cost_dollars,pnl_yes_win,pnl_no_win\n";
        f.write_all(header.as_bytes()).await?;
    }

    let run_ts = Utc::now().to_rfc3339();
    let open_time = fmt_ts_rfc3339(open_ts);
    let close_time = fmt_ts_rfc3339(close_ts);

    let yes_avg_cents = pos.avg_yes_cc().map(cc_to_cents);
    let no_avg_cents  = pos.avg_no_cc().map(cc_to_cents);
    let pair_cost_cents = pos.pair_cost_cc().map(cc_to_cents);
    let pair_cost_dollars = pos.pair_cost_cc().map(cc_to_dollars);

    let total_cost_cc = pos.yes_cost_cc.saturating_add(pos.no_cost_cc);
    
    let total_cost_dollars = cc_to_dollars(total_cost_cc);

    let yes_qty = pos.yes_qty.max(0) as f64;
    let no_qty  = pos.no_qty.max(0) as f64;

    let pnl_yes_win_dollars = yes_qty - total_cost_dollars;
    let pnl_no_win_dollars  = no_qty  - total_cost_dollars;
    
    let line = format!(
        "{run_ts},{open_time},{close_time},{},{},{},{},{},{},{},{}\n",
        pos.yes_qty,
        pos.no_qty,
        fmt_opt_2(yes_avg_cents),
        fmt_opt_2(no_avg_cents),
        fmt_opt_2(pair_cost_cents),
        fmt_opt_4(pair_cost_dollars),
        pnl_yes_win_dollars,
        pnl_no_win_dollars,
    );

    f.write_all(line.as_bytes()).await?;
    f.flush().await?;
    Ok(())
}
