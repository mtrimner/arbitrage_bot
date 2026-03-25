# AGENTS.md

This file is a fast handoff for new AI/code-assistant context windows working on `kalshi_bot`.

## What this project is

`kalshi_bot` is a Rust trading bot for Kalshi Bitcoin binary markets, currently focused on the `KXBTC15M` series. Each market is a short-dated binary contract that settles to **$1 if the event happens** and **$0 otherwise**.

In practice, the bot is not trying to estimate a perfect “true” probability from first principles. Instead, it:

1. uses **Coinbase BTC-USD** as the external signal source,
2. converts that into a **heuristic fair-value probability** for the Kalshi binary,
3. then trades Kalshi mainly through **maker quotes** while enforcing **pair-cost** and **locked-floor** risk constraints.

The core idea is:

- build inventory at acceptable pair cost,
- keep the book reasonably balanced,
- repair bad balanced books when possible,
- avoid opening inventory when the Coinbase-vs-Kalshi relationship looks too dislocated,
- use Kalshi book structure, time remaining, and current inventory to decide quote prices.

---

## High-level strategy goal

The bot’s goal in the Bitcoin binary market is **not** simply “buy when Coinbase says up” or “sell when Coinbase says down.”

The actual objective is:

- accumulate **balanced YES/NO inventory** at favorable combined prices,
- maintain or improve **guaranteed locked-in value** (`locked_floor_cc`),
- keep **average pair cost** low,
- with current defaults, only tolerate transient fill-driven imbalances and otherwise target zero intentional unhedged inventory,
- use Coinbase to decide when it is safe to open pairs, when it should hedge, and when a balanced book is “bad” enough to justify repair attempts.

The most important mental model:

- **Raw Coinbase fair value is a signal, not an executable price.**
- **Kalshi inventory quality is judged by pair cost and locked floor.**
- **Quote prices are derived from both Coinbase signal and live Kalshi top-of-book.**

---

## Main architecture

### Core tasks

The bot runs a few long-lived async tasks:

- `src/main.rs`
  - boots config,
  - creates Kalshi HTTP and WS clients,
  - bootstraps the active market,
  - starts WebSocket, executor, market manager, engine, and Coinbase tasks.

- `src/ws/task.rs`
  - handles Kalshi WS subscriptions,
  - updates order book state,
  - processes trade updates and user fills,
  - rotates subscriptions when markets change,
  - records lead/lag analytics.

- `src/coinbase_ws.rs`
  - connects to Coinbase public WS,
  - consumes ticker + level2 + heartbeat,
  - maintains Coinbase state used for signal generation,
  - optionally records Coinbase→Kalshi response analytics.

- `src/engine/task.rs`
  - wakes on a timer or when state is marked dirty,
  - calls `engine::decision::decide()` for each live Kalshi ticker,
  - emits execution commands.

- `src/exec/task.rs`
  - executes place/cancel commands,
  - either in live mode or paper mode.

- `src/market_manager.rs`
  - tracks the currently active market in each series,
  - rotates to the next market after close,
  - writes end-of-window result summaries.

### Core state objects

- `Shared` (`src/state/mod.rs`)
  - global container for all live tickers,
  - shared Coinbase state,
  - notification primitive.

- `TickerState` / `Market` (`src/state/ticker.rs`)
  - per-market Kalshi state,
  - order book,
  - position,
  - resting hints,
  - timestamps, mode, and imbalance-age tracking.

- `CoinbaseState` (`src/state/coinbase.rs`)
  - live Coinbase trade/top-of-book signal state,
  - EMA/microprice/volatility history,
  - signal generation.

---

## Repo map: where to look first

### Strategy / decision logic

- `src/engine/decision.rs`

This is the most important file. It contains:

- fair-value anchoring into live Kalshi prices,
- mode selection,
- pair opening logic,
- repair logic,
- hedge logic,
- taker fallback logic,
- cancel/replace logic,
- most “why did it do that?” behavior.

### Coinbase fair-value model

- `src/state/coinbase.rs`
- `src/coinbase_ws.rs`

These define:

- how Coinbase book and trade data are stored,
- how `microprice`, `ema_fast`, `ema_slow`, and `vol_ema` are built,
- how `fair_yes` is computed,
- how signal regimes are classified.

### Kalshi book / execution behavior

- `src/state/book.rs`
- `src/ws/task.rs`
- `src/exec/paper.rs`
- `src/exec/task.rs`

### Market rotation and result reporting

- `src/market_manager.rs`
- `src/report.rs`

### Inventory math

- `src/state/position.rs`

---

## Important domain assumptions

### 1. Kalshi book stores only bid ladders

`src/state/book.rs` stores:

- `yes_bids[0..=100]`
- `no_bids[0..=100]`

It does **not** store explicit asks. Asks are derived by complement:

- `yes_ask = 100 - best_no_bid`
- `no_ask = 100 - best_yes_bid`

This is crucial. Many quote and risk calculations depend on these implied asks.

### 2. Prices are in cents, but many risk numbers are in `cc`

`CC_PER_CENT = 100`.

So:

- `49 cents = 4900 cc`
- `99.06 cents pair cost = 9906 cc`

Do not mix up “cents” and “cc” when editing logic.

### 3. A balanced book can still be bad

A position is “balanced” if `yes_qty == no_qty`, but that does **not** mean it is good.

The bot also cares about:

- `pair_cost_cc`
- `locked_floor_cc`

A balanced book may still lock in too little value or have pair cost that is too high.

---

## Position quality metrics

Defined in `src/state/position.rs`.

### `pair_cost_cc()`

Average YES cost plus average NO cost.

If the bot owns both sides, this is the average cost of a complete pair.

Lower is better.

### `locked_floor_cc()`

```text
min(yes_qty, no_qty) * $1 payout - total_cost
```

In code:

```rust
self.yes_qty.min(self.no_qty) * DOLLAR_CC - self.yes_cost_cc - self.no_cost_cc
```

This is the guaranteed value of the matched portion of inventory.

Interpretation:

- positive = matched inventory is locked in above cost,
- zero = break-even on matched inventory,
- negative = matched inventory is underwater.

### Opening fill tracking

`Position` also tracks:

- `opening_yes_price_cents`
- `opening_no_price_cents`

These are recorded only once per side per market, on the first fill for that side. They are used in end-of-window reporting.

---

## Coinbase fair-value model

### Where it is built

`CoinbaseSnapshot::build_signal()` in `src/state/coinbase.rs`

### Inputs

The signal uses Coinbase-derived state:

- last trade price,
- microprice,
- fast EMA,
- slow EMA,
- volatility EMA,
- optional final-window averaging adjustment,
- Kalshi strike price,
- time remaining.

### Distance-to-strike logic

Normally:

```text
distance_usd = ema_fast - strike_price
```

If inside the final averaging window (`final_avg_window_s`) and enough data exists, the model can switch to:

- realized final average so far,
- required remaining average to hit strike,
- distance from current microprice to that required remaining average.

So the model attempts to account for settlement mechanics late in the market.

### Volatility / sigma logic

```text
sigma_usd = fair_sigma_floor_usd
            + vol_ema_usd * sqrt(t_rem) * fair_vol_sqrt_scale
```

clamped to at least `fair_sigma_floor_usd`.

### Probability mapping

The model computes a z-score:

```text
z = distance_usd / sigma_usd
```

then maps it through a logistic curve:

```text
logistic = 1 / (1 + exp(-k * z))
```

then adds a trend term from EMA fast vs EMA slow:

```text
trend_z = (ema_fast - ema_slow) / sigma_usd
fair_yes = logistic + fair_trend_weight * trend_z
```

Finally it clamps to `[0.001, 0.999]` and rounds to cents.

### Important caveat

This is a **heuristic signal model**, not a calibrated options model and not a guaranteed “true probability.”

It often saturates to `99` or `1` when spot is far from strike. The strategy partially corrects for this by blending back toward Kalshi’s live mid before deriving quote prices.

---

## Regimes: what they mean

Regimes are defined by `fair_yes` thresholds in `src/state/coinbase.rs`.

- `TwoSided`
  - fair value is near the middle,
  - bot is comfortable opening fresh paired inventory.

- `DriftUp`
  - bullish leaning, but not extreme.

- `DriftDown`
  - bearish leaning, but not extreme.

- `ExtremeUp`
  - strongly bullish.

- `ExtremeDown`
  - strongly bearish.

- `PinnedUp`
  - model thinks YES is near certain / near max probability.

- `PinnedDown`
  - model thinks YES is near impossible / near min probability.

Late in the market, the thresholds can become stricter or looser via the `*_late` config values.

---

## Kalshi-facing fair value used for quoting

The bot does **not** quote directly off raw Coinbase `fair_yes_cents`.

It first computes a Kalshi-anchored version in `anchored_fair_yes_cents()`:

```text
anchored_fair = kalshi_mid + COINBASE_SIGNAL_WEIGHT * (raw_fair - kalshi_mid)
```

with `COINBASE_SIGNAL_WEIGHT = 0.35`.

So only 35% of the difference between raw Coinbase fair and Kalshi mid is applied.

This is intentional. It keeps quotes closer to what the Kalshi book is actually trading at and avoids chasing Coinbase too aggressively when the venues diverge.

---

## Mode system

Defined in `pick_mode()` in `src/engine/decision.rs`.

Modes:

- `Accumulate`
  - early in the window,
  - okay to build inventory.

- `Hedge`
  - middle of the window,
  - still active, but more inventory-aware.

- `Balance`
  - late in the window,
  - prioritize cleanup and risk control.

Time remaining is based on the actual market open/close timestamps when available.

---

## Decision pipeline in plain English

`decide()` in `src/engine/decision.rs` roughly follows this order:

1. reject trading if market is closed,
2. compute time remaining and mode,
3. freeze if balanced and already good enough near end,
4. require strike price and fresh Coinbase snapshot,
5. build Coinbase signal,
6. cancel stale orders if needed,
7. cancel vulnerable side if the trend says one resting side is dangerous,
8. determine if new pair opening is allowed,
9. if balanced but bad, first try a fresh paired add when opening is allowed and the pair improves the book; otherwise consider a repair quote,
10. if imbalance is too large or opening is blocked, focus on hedge-only logic, including maker hedge, runaway/profitable IOC escalation, and cancel-before-IOC handling,
11. otherwise try to open a fresh pair,
12. if none of the above emits a command, log a “no order placed” reason.

---

## How quote prices are actually set

### Step 1: compute a top maker ceiling/floor per side

`top_maker_price()` chooses the highest acceptable maker bid for a side by looking at:

- current best bid,
- current implied ask,
- `maker_improve_tick` or `maker_improve_tick_balance`,
- `max_buy_price_cents`.

Normally it tries to improve the current best bid by one tick without crossing the implied ask.

If the side is the hedge side and the gap is large enough, it can be forced up toward `ask - 1`.

### Step 2: compute desired center from fair value

`desired_maker_quote()` starts from anchored fair value, then adjusts for:

- current inventory skew (`inventory_shift_cents`),
- volatility (`vol_extra_cents`),
- hedge urgency (`hedge_quote_boost_cents`),
- an edge band that prevents quoting too far behind the live book.

For YES:

```text
target = center - halfspread
```

For NO:

```text
target = 100 - (center + halfspread)
```

Then the target is clamped to be no more aggressive than the side’s `top_maker_price()`.

### Key takeaway

The final quote is **not just fair value**. It is:

- anchored fair,
- inventory adjusted,
- volatility adjusted,
- clipped to the live Kalshi book,
- then passed through risk admission checks.

---

## Pair opening logic

Used when the bot is allowed to open new balanced inventory.

### When pair opening is allowed

`can_open_pairs()` returns true when:

- there is still enough time left (`t_rem > no_new_imbalance_s`), and
- either the regime is mild enough (`TwoSided`, `DriftUp`, `DriftDown`),
- or anchored-fair-vs-Kalshi-mid dislocation is small enough.

### How opening pair prices are chosen

`pair_open_prices()`:

1. tries the best live affordable pair:
   - `top_yes`,
   - `top_no`.
2. if that pair passes `pair_open_ok()`, it uses it.
3. otherwise it falls back to fair-based maker quotes, but only backed off by at most `MAX_PAIR_OPEN_BACKOFF_CENTS = 1` from the top.

### `pair_open_ok()` checks

Opening a pair is allowed if:

- the fully completed pair does not worsen locked floor, and
- if there is no existing paired inventory, the marginal pair is at or below `market_entry_pair_cost_cc`, and
- if there is existing paired inventory:
  - both possible first-leg fills must still leave `locked_floor_cc >= locked_floor_buffer_cc`,
  - the marginal pair must be **strictly** better than the current average pair cost.

### Important nuance

The bot still wants both sides working, but placement order is no longer symmetric:

- `maybe_open_pair_quote()` places the cheaper leg first,
- then tries the other leg in the same pass,
- if a matching resting pair already exists, it returns `Working` and emits no new order.

---

## Hedge logic

If the inventory gap exceeds allowed imbalance, or fresh pair opening is blocked, the bot switches to hedge-only behavior.

### Hedge side

`hedge_side()` is the side the bot needs more of.

If balanced, it breaks ties by the cheaper implied ask.

### Hedge quote path

In hedge-only situations, the bot may:

- cancel the strong side,
- place a maker quote on the weak side,
- or use IOC earlier if the signal is clearly running away from the missing side or the last lot can be flattened profitably.

### Admission rule for hedges

If a fill **reduces imbalance**, the bot mostly judges it by whether it improves `locked_floor_cc` enough.

That is intentionally more permissive than the fresh-risk path.

Most important current nuance:

- if a hedge fill fully flattens the book (`new_gap == 0`), admission is now based on whether the resulting `locked_floor_cc` meets `locked_floor_buffer_cc`,
- it is **not** rejected just because the resulting average pair cost is above the old `safe_pair_cc` / `balance_pair_cc` closeout cap.

### Early IOC triggers

The bot now has two main early-IOC paths for hedge-only situations:

- runaway signal:
  - missing **YES** + `DriftUp` / `ExtremeUp` / `PinnedUp` => IOC can trigger early,
  - missing **NO** + `DriftDown` / `ExtremeDown` / `PinnedDown` => IOC can trigger early.
- profitable last-lot flatten:
  - only for `gap == 1`,
  - only after the imbalance has persisted for at least `maker_first_ms`,
  - only if buying the missing side at the current implied ask would still leave `locked_floor_cc` at least `max(locked_floor_buffer_cc, 500)`.

The maker-first waiting period now follows the age of the imbalance itself (`imbalance_since`), not the age of the latest repriced hedge quote.

Before sending an IOC on the hedge side, the engine first tries to cancel any same-side resting hedge quote so it does not overshoot after flattening.

### Temporary Coinbase outage behavior

If Coinbase snapshot/signal data is temporarily unavailable while the bot already has an imbalance (`gap > 0`), it now keeps useful hedge-side resting orders alive instead of canceling everything immediately.

---

## Repair logic

Repair logic exists for **balanced but bad** books.

### What “balanced but bad” means

`is_balanced_but_bad()` returns true when:

- `yes_qty == no_qty`, and
- there is inventory, and
- either:
  - `locked_floor_cc < locked_floor_buffer_cc`, or
  - `pair_cost_cc > safe_pair_cc + PAIR_REPAIR_HYST_CC`.

With the current hysteresis:

```text
PAIR_REPAIR_HYST_CC = 50
```

That is half a cent in `cc` units.

### Repair side

`repair_side_for(regime)` maps regime to a preferred repair side:

- `DriftDown`, `ExtremeDown`, `PinnedDown` → repair with **YES**
- `DriftUp`, `ExtremeUp`, `PinnedUp` → repair with **NO**
- `TwoSided` → no repair side

### What a repair quote is

A repair quote is a **single-sided maker quote** on the repair side.

It is not a full pair order.

The idea is:

- if a fresh paired add would improve the current book and pair opening is allowed, prefer that over creating a new one-sided repair,
- quote for the side that looks relatively cheap under the current regime,
- only if simulating that fill and later pairing it with a plausible price on the other side would improve the quality of the balanced book.

### Repair admission rule

`repair_quote_improves_book()` simulates:

1. buying the repair side now,
2. then buying the missing/complement side at a plausible future price.

It only allows the quote if the resulting book would improve either:

- `locked_floor_cc`, or
- `pair_cost_cc`.

When the current balanced book is already above the floor buffer, repair is stricter:

- completion pricing uses the current implied ask first, then falls back to maker/bid estimates,
- the projected marginal pair must beat the current average pair by a small amount, so old edge cannot subsidize a bad new repair pair.

### Important nuance about the logs

`balanced_bad_no_repair_quote` does **not always mean there is no repair order resting**.

It only means that on that engine pass, `decide()` did not emit a **new** repair command.

That can happen when:

- a repair quote is already working at the current desired price,
- the current desired repair quote fails the improvement test,
- or the bot cannot legally cancel/reprice yet.

This log reason is therefore best read as:

> “No new repair action was taken on this pass.”

not necessarily:

> “There is no repair quote in the market.”

### Current behavior: repair quotes are sticky value quotes

Once a repair quote is resting on the repair side, the engine does not ratchet it more aggressive as the market moves.

Instead it:

- keeps the quote working if it still passes the repair test,
- cancels it if the existing price is no longer attractive,
- otherwise leaves it alone.

This makes repair behave more like a value bid: sit, fill cheap, or get out.

---

## Cancel / replace behavior

### Stale cancels

`cancel_stale_if_needed()` cancels resting orders older than `cancel_stale_ms`, subject to:

- `min_resting_life_ms`,
- `cancel_retry_ms`.

### Drift cancels

`place_or_manage_resting()` compares existing vs desired price.

It computes:

```text
drift = abs(existing - desired)
```

and cancels if drift exceeds the relevant threshold.

For most quotes this is **absolute movement**, not “only when moving away from the market.”

There is one special case:

- if `only_reprice_if_more_aggressive = true`, then it only cancels when the new desired price is **more aggressive** and drift is large enough.

Currently, hedge-side quotes while unbalanced use this stickier behavior; balanced repair quotes do not.

### Minimum life matters

`cancel_side_force()` and other cancel flows respect `min_resting_life_ms`.

That protection is important to avoid pathological cancel/replace loops.

---

## Order lifecycle

### Engine path

1. `decide()` emits an `ExecCommand`.
2. `stage_place_order()` inserts a pending local order record.
3. executor sends live HTTP or paper action.
4. status is updated on ack.
5. fills come separately through Kalshi WS or paper trade simulation.

### Resting hints

`RestingHint` is the bot’s local model of a live order.

It stores:

- price,
- creation time,
- cancel request time,
- client order id,
- exchange order id (when known),
- queue ahead (paper mode only).

This structure is what the engine uses to avoid duplicates and manage reprice/cancel logic.

---

## Paper mode behavior

Default `.cargo/config.toml` runs the bot in:

```text
EXEC_MODE = "paper"
```

### Important paper-mode semantics

Defined in `src/exec/paper.rs`.

- Post-only orders are rejected if they would cross the implied ask immediately.
- GTC orders become local resting orders with synthetic `paper-*` ids.
- Trade updates can fill maker orders if the tape trades **at or through** the bot’s resting price.
- If the tape trades through a resting price, the simulator assumes the level was swept and sets `queue_ahead = 0`.

This is intentionally somewhat optimistic, but much better than requiring exact-price-only fills.

### Ack handling nuance

Pure acks/rejects/cancels often call `mark_dirty()` rather than `touch()` so the engine sees the new order state on the next pass without immediately re-entering mid-batch and creating churn.

That change is important and should not be casually reverted.

---

## Lead/lag analytics

This is separate from core trading decisions.

### Files

- `src/coinbase_ws.rs`
- `src/leadlag.rs`
- `src/ws/task.rs`

The bot can detect Coinbase moves over a short window and record how Kalshi responds. It logs:

- move direction,
- trigger-time Kalshi book,
- first Kalshi response,
- best post-trigger entry/exit edge,
- minimum pair cost seen after trigger.

This is analytics only; it does not currently drive trading decisions directly.

---

## Result reporting

At market rotation, `market_manager.rs` writes a summary row via `report::append_result_csv()`.

Current CSV includes:

- series ticker,
- market ticker,
- open/close times,
- target strike price,
- Coinbase price at rotation,
- opening YES/NO fill prices,
- final inventory,
- average YES/NO costs,
- pair cost,
- `max_balance_price_cents` for unbalanced end states under the current `locked_floor_buffer_cc`,
- locked floor,
- simple YES-win / NO-win PnL projections.

---

## Important config knobs

These are the most strategy-relevant defaults in `src/config.rs`.

### Time / mode

- `window_s = 900`
- `accumulate_s = 120`
- `balance_s = 300`
- `freeze_if_balanced_s = 300`
- `no_new_imbalance_s = 300`

### Risk / inventory quality

- `safe_pair_cc = 9900`
- `target_pair_cc = 9850`
- `balance_pair_cc = 9900`
- `market_entry_pair_cost_cc = 9850`
- `locked_floor_buffer_cc = 100`
- `max_unhedged_qty_early = 0`
- `max_unhedged_qty_late = 0`

### Quote / hedge behavior

- `quote_base_halfspread_cents = 2`
- `quote_vol_per_extra_cent_usd = 20.0`
- `quote_max_vol_extra_cents = 3`
- `hedge_quote_boost_cents = 0`
- `inventory_skew_per_contract_cents = 1`
- `inventory_skew_max_cents = 3`
- `maker_improve_tick = 1`
- `maker_improve_tick_balance = 1`
- `maker_max_edge_cents = 8`
- `maker_max_edge_cents_balance = 12`
- `hedge_force_ask_minus_one_gap = 1`
- `catchup_aggressiveness = 0.45`
- `catchup_balance_boost = 1.5`
- `catchup_plausibility_buffer_cents = 1`
- `short_side_min_order_qty = 1`
- `max_order_qty = 10`

### Cancel / churn control

- `min_resting_life_ms = 1000`
- `cancel_retry_ms = 600`
- `cancel_stale_ms = 30000`
- `cancel_drift_cents = 2`
- `cancel_drift_cents_hedge = 2`

### Taker fallback

- `maker_first_ms = 1500`
- `taker_cooldown_ms = 1000`
- `taker_desperate_s = 60`
- `taker_force_gap = 1`

### Coinbase signal

- `signal_late_threshold_s = 90`
- `signal_two_sided_low/high = 0.35 / 0.65`
- `signal_extreme_low/high = 0.20 / 0.80`
- `signal_pinned_low/high = 0.10 / 0.90`
- `signal_two_sided_low/high_late = 0.40 / 0.60`
- `signal_extreme_low/high_late = 0.25 / 0.75`
- `signal_pinned_low/high_late = 0.15 / 0.85`
- `fair_sigma_floor_usd = 35.0`
- `fair_vol_sqrt_scale = 1.25`
- `fair_logistic_k = 1.8`
- `fair_trend_weight = 0.07`
- `cancel_trend_z = 0.30`

### Config reality check

- `Config::from_env()` currently overrides only `RESULTS_FILE`, `COINBASE_WS`, `COINBASE_PRODUCT_ID`, `COINBASE_LOG_DELTA_USD`, `COINBASE_STALE_MS`, `QUOTE_BASE_HALFSPREAD_CENTS`, `MARKET_ENTRY_PAIR_CC`, and `LOCKED_FLOOR_BUFFER_CC`.
- Several fields still exist in `Config` but are currently unused anywhere in `src/`: `aggressive_tick`, `bootstrap_pair_cc`, `final_balance_pair_cc`, `bootstrap_max_one_side_qty`, `bootstrap_rescue_min_improve_cc`, `early_imbalance_cap`, `late_imbalance_cap`, `imbalance_min_total`, `imbalance_cap_small_total`, `maker_qty_price_tol_cents`, `maker_qty_price_tol_cents_balance`, `min_taker_improve_cc`, and `taker_big_improve_cc`.

---

## Common log reasons and what they usually mean

### `pair_open_quote_unavailable`

The bot wanted to open a fresh pair but could not produce a valid pair-opening command.

Common causes:

- pair prices fail `pair_open_ok()`,
- quotes already working,
- no admissible price after constraints.

### `rebalancing_quote_ineligible`

The bot has an imbalance and wants to hedge, but the candidate hedge quote fails admission/risk logic.

Current logs for this reason also include:

- `hedge_side`
- `short_side_ask`
- `max_balance_price_cents`
- `floor_if_filled_at_short_ask_cc`

### `balanced_bad_no_repair_quote`

Balanced inventory exists, but the book is still considered low quality, and no **new** repair command was emitted this pass.

Do not assume this means no repair quote is resting.

### `regime_blocks_opening`

Coinbase/Kalshi relationship is too directional or too dislocated to justify opening fresh two-sided inventory.

### `freeze_balanced`

Near end of market, already balanced and good enough, so bot stops trading and may cancel leftovers.

### `balanced_endgame_hold`

Balanced late in the market with no desire to open new imbalance.

---

## What “quality of the balanced book” means here

It does **not** mean the external Kalshi order book is broken.

It means the bot’s **own held inventory** is balanced but unattractive.

A balanced book is poor quality when, despite being matched:

- the guaranteed locked-in value is too low, or
- the average pair cost is too high.

So “repair” is about improving the bot’s inventory economics, not fixing the exchange’s market price.

---

## Known current behavior / limitations

1. **Fair value is heuristic, not calibrated to true probability**
   - It is based on Coinbase price-vs-strike with volatility and trend overlays.
   - It is only partially anchored back to Kalshi mid.

2. **Existing-inventory pair scaling is intentionally conservative**
   - Non-flat books only reopen if the full pair is better, both first-leg paths preserve the floor buffer, and the marginal pair is strictly better than the current average pair.

3. **Gap-1 profitable flatten can escalate earlier than old maker babysitting behavior**
   - After `maker_first_ms`, the bot may take the live ask to flatten if the remaining locked floor would still be at least `max(locked_floor_buffer_cc, 500)`.

4. **Raw fair frequently saturates near 99/1**
   - The anchored fair and quote clamps keep execution logic from blindly following that, but the saturation is expected.

5. **Logs can be semantically broader than they sound**
   - Example: `balanced_bad_no_repair_quote` can coexist with `resting_yes=true`.

6. **Paper simulator uses through-price fill assumptions**
   - Useful for testing, but not a perfect live fill model.

---

## If you need to change strategy logic, touch these carefully

### If changing fair value behavior

Look at:

- `src/state/coinbase.rs`
- `anchored_fair_yes_cents()` in `src/engine/decision.rs`
- `can_open_pairs()`
- `desired_maker_quote()`

### If changing repair logic

Look at:

- `is_balanced_but_bad()`
- `repair_side_for()`
- `repair_quote_improves_book()`
- `maybe_repair_quote()`

### If changing pair opening

Look at:

- `marginal_pair_cc()`
- `pair_open_survives_first_leg()`
- `pair_open_ok()`
- `pair_open_prices()`
- `maybe_open_pair_quote()`

### If changing hedge timing / last-lot flatten

Look at:

- `update_imbalance_since()`
- `imbalance_age_ms()`
- `profitable_flatten_floor_cc()`
- `should_force_profitable_flatten()`
- `maybe_balance_ioc()`

### If changing cancel/reprice stickiness

Look at:

- `place_or_manage_resting()`
- `cancel_side_force()`
- `min_resting_life_ms`
- `cancel_drift_cents*`

### If changing paper fill realism

Look at:

- `paper_on_trade_fill()`
- `paper_on_delta_queue()`
- `paper_place()`

---

## Guidance for future AI/code agents

1. **Do not confuse raw Coinbase fair with executable quote price.**
   The trading logic intentionally anchors back toward Kalshi and then applies inventory/volatility/book constraints.

2. **Always track both pair cost and locked floor.**
   Many seemingly “good” fills are actually bad once those are recomputed.

3. **When analyzing logs, check resting state.**
   A no-order reason often means “no new command emitted,” not “nothing is working.”

4. **Respect `min_resting_life_ms` unless there is a very strong reason not to.**
   Removing that usually reintroduces churn.

5. **Remember that asks are implied from the opposite side’s best bid.**
   This drives a lot of quote math and can produce `None` asks if the opposite side book is empty.

6. **For hedge escalation, track imbalance age, not hedge-quote age.**
   The maker-first IOC delay is keyed off `imbalance_since`, so repricing a maker hedge should not restart the clock.

7. **Existing-inventory pair scaling must account for first-leg damage, not just full-pair economics.**
   If you loosen pair reopening, check both first-leg floor survivability and marginal pair quality.

8. **If you change reporting or rotation, keep market-manager + report schema in sync.**

9. **Defaults are paper-mode defaults.**
   Be careful when reasoning about fills, cancels, and queue behavior; paper mode is an approximation.

---

## One-sentence summary

This bot is a Coinbase-informed, Kalshi-maker strategy that tries to accumulate and maintain low-cost balanced YES/NO inventory, repair bad balanced books when possible, and strictly gate any quote that worsens the bot’s guaranteed inventory economics.
