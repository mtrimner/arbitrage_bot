
# AGENTS.md

This file is a fast handoff for new AI/code-assistant context windows working on `kalshi_bot`.

## What this project is

`kalshi_bot` is a Rust trading bot for Kalshi Bitcoin binary markets, currently focused on the `KXBTC15M` series. Each market is a short-dated binary contract that settles to **$1 if the event happens** and **$0 otherwise**.

In practice, the bot is not trying to estimate a perfect “true” probability from first principles. Instead, it:

1. uses **Coinbase BTC-USD** as the external signal source,
2. converts that into a **heuristic fair-value probability** for the Kalshi binary,
3. then trades Kalshi mainly through **maker quotes** while enforcing **pair-cost**, **size-scaled locked-floor**, and **planned-pair completion** constraints.

The core idea is now:

- build inventory at acceptable pair cost,
- keep the book reasonably balanced,
- keep **locked floor high relative to matched size**, not just positive in total,
- preserve the economics of an opened pair after the first leg fills,
- repair bad balanced books when possible,
- avoid opening inventory when the Coinbase-vs-Kalshi relationship looks too dislocated,
- use Kalshi book structure, time remaining, current inventory, and any active pair plan to decide quote prices.

---

## High-level strategy goal

The bot’s goal in the Bitcoin binary market is **not** simply “buy when Coinbase says up” or “sell when Coinbase says down.”

The actual objective is:

- accumulate **balanced YES/NO inventory** at favorable combined prices,
- maintain or improve **guaranteed locked-in value** (`locked_floor_cc`),
- maintain a useful **locked floor per matched pair** through a size-scaled floor requirement,
- keep **average pair cost** low,
- preserve the original economics of a planned pair after the first leg fills,
- with current defaults, only tolerate transient fill-driven imbalances and otherwise target zero intentional unhedged inventory,
- use Coinbase to decide when it is safe to open pairs, when it should hedge, and when a balanced book is “bad” enough to justify repair attempts.

The most important mental model:

- **Raw Coinbase fair value is a signal, not an executable price.**
- **Kalshi inventory quality is judged by pair cost plus a size-scaled locked-floor target.**
- **Quote prices are derived from both Coinbase signal and live Kalshi top-of-book.**
- **Once a pair leg fills, the remaining completion should respect the original pair budget whenever possible.**

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
  - optional active **pair plan** for first-leg / completion tracking,
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
- size-scaled floor math,
- pair-plan-aware completion logic,
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
- **required locked floor** based on matched size

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

### `matched_qty()`

This is the number of fully matched YES/NO pairs currently held.

It is now used in multiple quality checks.

### Size-scaled required floor

The bot no longer uses only a flat total floor buffer.

It now computes:

```text
required_locked_floor_cc
  = locked_floor_buffer_cc
  + matched_qty * locked_floor_per_pair_cc
```

Interpretation:

- `locked_floor_buffer_cc` is the base floor target,
- `locked_floor_per_pair_cc` is the additional required floor per matched pair.

This prevents large books from being considered acceptable merely because they still lock a few cents in total.

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

## Pair-plan tracking

The engine now keeps an optional **pair plan** whenever it opens a fresh pair quote.

The pair plan stores:

- target YES quote
- target NO quote
- total pair budget
- preferred first side
- first filled side
- first fill price

This matters because after the first leg fills, the completion leg is no longer treated as a generic hedge only. The engine now tries to preserve the original pair economics by capping the completion price relative to the pair budget.

---

## Decision pipeline in plain English

`decide()` in `src/engine/decision.rs` roughly follows this order:

1. reject trading if market is closed,
2. compute time remaining and mode,
3. clear stale inactive pair-plan state,
4. freeze if balanced **and** already good enough under the size-scaled floor and pair-cost constraints,
5. require strike price and fresh Coinbase snapshot,
6. build Coinbase signal,
7. cancel stale orders if needed,
8. cancel vulnerable side if the trend says one resting side is dangerous,
9. determine if new pair opening is allowed,
10. if balanced but bad:
    - before `no_new_imbalance_s`, first try a fresh paired add when opening is allowed and the pair improves the book,
    - otherwise consider a repair quote,
    - and continue allowing repair until `repair_cutoff_s`,
11. if imbalance is too large or opening is blocked, focus on hedge-only logic, including:
    - maker hedge,
    - pair-plan-aware completion quoting,
    - runaway / profitable IOC escalation,
    - emergency flatten for bad books,
    - cancel-before-IOC only when an IOC candidate is actually admissible,
12. otherwise try to open a fresh pair,
13. if none of the above emits a command, log a “no order placed” reason.

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

### Pair-plan completion cap

If a pair plan exists and one leg has already filled, the hedge-side quote may be capped by the remaining pair budget.

This is what prevents a good first-leg fill from automatically turning into a near-$1 pair just because the generic hedge logic still had spare total floor.

### Key takeaway

The final quote is **not just fair value**. It is:

- anchored fair,
- inventory adjusted,
- volatility adjusted,
- clipped to the live Kalshi book,
- sometimes capped by a pair-plan completion budget,
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
   - `top_no`
2. if that pair passes `pair_open_ok()`, it uses it
3. otherwise it falls back to fair-based maker quotes, but only backed off by at most `MAX_PAIR_OPEN_BACKOFF_CENTS = 1` from the top

### `pair_open_ok()` checks

Opening a pair is allowed if:

- the completed simulated book satisfies the size-scaled required floor,
- if there is no existing paired inventory, the marginal pair is at or below `market_entry_pair_cost_cc`,
- if there is existing paired inventory:
  - both possible first-leg paths must still allow a completion that satisfies the size-scaled floor,
  - the marginal pair must stay below `safe_pair_cc`,
  - if current pair cost is already above target, the marginal pair must improve it by at least `pair_scale_min_improve_cc`,
  - otherwise it may be equal or better, but not worse.

### First-leg ordering

The bot no longer always places the cheaper leg first.

It now uses:

- `DriftDown` / `ExtremeDown` / `PinnedDown` → **NO first**
- `DriftUp` / `ExtremeUp` / `PinnedUp` → **YES first**
- `TwoSided` → cheaper first

This reduces the chance that the leg most likely to run away is left for later completion.

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
- use a pair-plan-aware completion cap when appropriate,
- or use IOC if the signal is clearly running away from the missing side, the last lot can still complete at good quality, or the book is bad enough that an emergency flatten materially improves floor.

### Admission rule for ordinary hedges

If a fill **reduces imbalance**, ordinary maker / quality-completion admission now uses:

- size-scaled floor requirements,
- `safe_pair_cc` protection on final balance,
- pair-plan completion caps when relevant.

This is stricter than the old “final balance only needs to leave 1 cent total floor” rule.

### Emergency flatten

The engine now distinguishes **quality completion** from **emergency flatten**.

Emergency flatten exists for cases like:

- runaway signal on the missing side,
- gap-1 last lot late in the window,
- already bad book where the live ask cannot satisfy the full quality gate but would still materially improve worst-case PnL.

In those cases, the engine may use IOC even when the resulting final book still does not satisfy the normal quality bar, provided the fill improves floor by at least a minimum amount.

### IOC candidate gating

Before canceling the same-side hedge quote, the engine now first checks whether there is a real IOC candidate that passes either:

- ordinary admission, or
- emergency flatten admission.

This prevents the old cancel / repost churn where the bot repeatedly canceled a useful maker hedge and then failed the IOC admission anyway.

### Profitable last-lot flatten

The “profitable flatten” path is now judged against the **size-scaled final floor requirement**, not just a flat `max(locked_floor_buffer_cc, 500)` threshold.

---

## Repair logic

Repair logic exists for **balanced but bad** books.

### What “balanced but bad” means

`is_balanced_but_bad()` returns true when:

- `yes_qty == no_qty`,
- inventory exists,
- and either:
  - `locked_floor_cc < required_locked_floor_cc`, or
  - `pair_cost_cc > safe_pair_cc + PAIR_REPAIR_HYST_CC`

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

### Repair window

Balanced-book repair is now allowed later than fresh pair opening.

Typical behavior is:

- fresh pair opening shuts off at `no_new_imbalance_s`,
- single-sided repair can continue until `repair_cutoff_s`,
- after that, no new repair risk is opened.

### What a repair quote is

A repair quote is a **single-sided maker quote** on the repair side.

It is not a full pair order.

The idea is:

- if a fresh paired add would improve the current book and pair opening is still allowed, prefer that over creating a new one-sided repair,
- otherwise quote for the side that looks relatively cheap under the current regime,
- only if simulating that fill and later pairing it with a plausible price on the other side would improve the quality of the balanced book.

### Repair admission rule

`repair_quote_improves_book()` simulates:

1. buying the repair side now,
2. then buying the missing/complement side at a plausible future price.

It only allows the quote if the resulting book would improve either:

- locked floor, or
- pair cost.

When the current balanced book already satisfies the size-scaled floor requirement, repair stays strict and requires a real projected pair improvement.

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

Currently, hedge-side quotes while unbalanced still use this stickier behavior, but pair-plan completion caps can now override it by forcing a cancel if the existing quote is simply too expensive relative to the remaining budget.

### Minimum life matters

`cancel_side_force()` and other cancel flows respect `min_resting_life_ms`.

That protection is still important to avoid pathological cancel/replace loops.

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

### Pair-plan bookkeeping in paper mode

Paper fills must now update the same pair-plan state used by live fills.

That means all paper fill paths should use the market’s tracked-fill helper, not call `pos.apply_fill()` directly.

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

Current CSV should now include:

- series ticker,
- market ticker,
- open/close times,
- target strike price,
- Coinbase price at rotation,
- opening YES/NO fill prices,
- final inventory,
- average YES/NO costs,
- pair cost,
- `max_balance_price_cents` under the **size-scaled** floor requirement,
- `required_locked_floor_cents`,
- `required_locked_floor_dollars`,
- actual locked floor,
- simple YES-win / NO-win PnL projections.

If you change the size-scaled floor formula, update both `decision.rs` and `report.rs` together.

---

## Important config knobs

These are the most strategy-relevant defaults in `src/config.rs`.

### Time / mode

- `window_s = 900`
- `accumulate_s = 120`
- `balance_s = 300`
- `freeze_if_balanced_s = 300`
- `no_new_imbalance_s = 300`
- `repair_cutoff_s = 90`

### Risk / inventory quality

- `safe_pair_cc = 9900`
- `target_pair_cc = 9850`
- `balance_pair_cc = 9900`
- `market_entry_pair_cost_cc = 9850`
- `locked_floor_buffer_cc = 100`
- `locked_floor_per_pair_cc = 25`
- `pair_scale_min_improve_cc = 10`
- `pair_completion_slippage_cents = 1`
- `emergency_flatten_min_improve_cc = 500`
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

`Config::from_env()` should now expose at least:

- `RESULTS_FILE`
- `COINBASE_WS`
- `COINBASE_PRODUCT_ID`
- `COINBASE_LOG_DELTA_USD`
- `COINBASE_STALE_MS`
- `QUOTE_BASE_HALFSPREAD_CENTS`
- `MARKET_ENTRY_PAIR_CC`
- `LOCKED_FLOOR_BUFFER_CC`
- `LOCKED_FLOOR_PER_PAIR_CC`
- `PAIR_SCALE_MIN_IMPROVE_CC`
- `PAIR_COMPLETION_SLIPPAGE_CENTS`
- `REPAIR_CUTOFF_S`
- `EMERGENCY_FLATTEN_MIN_IMPROVE_CC`

---

## Common log reasons and what they usually mean

### `pair_open_quote_unavailable`

The bot wanted to open a fresh pair but could not produce a valid pair-opening command.

Common causes:

- pair prices fail `pair_open_ok()`,
- quotes already working,
- no admissible price after constraints.

### `rebalancing_quote_ineligible`

The bot has an imbalance and wants to hedge, but the candidate maker hedge quote fails the current quality checks.

Logs for this reason should now be interpreted alongside:

- `required_locked_floor_cc`
- `hedge_side`
- `short_side_ask`
- dynamic `max_balance_price_cents`
- optional `pair_plan_completion_cap_cents`
- `floor_if_filled_at_short_ask_cc`

### `balanced_bad_no_repair_quote`

Balanced inventory exists, but the book is still considered low quality, and no **new** repair command was emitted this pass.

Do not assume this means no repair quote is resting.

### `balanced_bad_repair_window_closed`

Balanced inventory is still poor quality, but the late repair window has closed, so the engine is no longer allowed to open new repair risk.

### `regime_blocks_opening`

Coinbase/Kalshi relationship is too directional or too dislocated to justify opening fresh two-sided inventory.

### `freeze_balanced`

Near end of market, already balanced **and** good enough under the stricter quality rules, so the bot stops trading and may cancel leftovers.

### `balanced_endgame_hold`

Balanced late in the market with no desire to open new imbalance.

---

## What “quality of the balanced book” means here

It does **not** mean the external Kalshi order book is broken.

It means the bot’s **own held inventory** is balanced but unattractive.

A balanced book is poor quality when, despite being matched:

- the guaranteed locked-in value is too low **relative to matched size**, or
- the average pair cost is too high.

So “repair” is about improving the bot’s inventory economics, not fixing the exchange’s market price.

---

## Known current behavior / limitations

1. **Fair value is heuristic, not calibrated to true probability**
   - It is based on Coinbase price-vs-strike with volatility and trend overlays.
   - It is only partially anchored back to Kalshi mid.

2. **Pair completion is now path-aware, but only with one active plan**
   - The bot tracks one active planned pair at a time.
   - That is intentional because the strategy still targets zero intentional imbalance.

3. **Final flatten is split into quality completion vs emergency flatten**
   - Good books should complete only when the final state still satisfies pair-cost and size-scaled floor constraints.
   - Bad books can still be flattened late or on runaway tape if the fill materially improves floor.

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

### If changing size-scaled floor behavior

Look at:

- `required_locked_floor_cc()`
- `required_locked_floor_after_balance_cc()`
- `is_balanced_but_bad()`
- `should_freeze_trading()`
- `report::append_result_csv()`

### If changing pair-plan behavior

Look at:

- `PairPlan` in `src/state/ticker.rs`
- `apply_tracked_fill()`
- `pair_plan_completion_cap_cents()`
- `maybe_open_pair_quote()`
- `maybe_signal_maker_quote()`

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
- `should_force_profitable_flatten()`
- `emergency_flatten_ok()`
- `balance_ioc_candidate()`

### If changing cancel/reprice stickiness

Look at:

- `place_or_manage_resting()`
- `cancel_if_resting_above_limit()`
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
   The trading logic intentionally anchors back toward Kalshi and then applies inventory, volatility, book, and pair-plan constraints.

2. **Always track both pair cost and the size-scaled floor requirement.**
   A large balanced book with a few cents total floor is no longer automatically acceptable.

3. **When analyzing logs, check pair-plan state.**
   A missing-side hedge may be budget-capped by an earlier first-leg fill.

4. **Respect `min_resting_life_ms` unless there is a very strong reason not to.**
   The hedge churn fix should come from IOC candidate gating, not from disabling resting-life protections.

5. **Remember that asks are implied from the opposite side’s best bid.**
   This drives a lot of quote math and can produce `None` asks if the opposite side book is empty.

6. **For hedge escalation, track imbalance age, not hedge-quote age.**
   The maker-first IOC delay is keyed off `imbalance_since`, so repricing a maker hedge should not restart the clock.

7. **If you change pair completion, keep pair-plan tracking and fill bookkeeping aligned.**
   Websocket fills and paper fills must both update the same state.

8. **If you change reporting or rotation, keep market-manager + report schema in sync.**

9. **Defaults are paper-mode defaults.**
   Be careful when reasoning about fills, cancels, and queue behavior; paper mode is an approximation.

---

## One-sentence summary

This bot is a Coinbase-informed, Kalshi-maker strategy that now tries to accumulate and maintain low-cost balanced YES/NO inventory using a size-scaled floor requirement and pair-plan-aware completion logic, repair bad balanced books when possible, and separate high-quality completion from emergency flatten when the book gets into trouble.
