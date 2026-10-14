# Replay scope and fill fidelity

This document states exactly what decision replay proves, and where the simulated fill model
diverges from real execution. Read it before treating any replay report or simulated result as
evidence about live behaviour.

## What replay actually does

`src/replay.rs` supports nine decision stages. Runtime captures record the inputs and outputs
of stages the selected execution path reaches, then replay re-evaluates those inputs offline.
The static `simulated_buy_fill` capture is test-only; runtime paper buys use `taker_paper_fill`.

| Stage | Production function re-executed |
| --- | --- |
| `fusion` | `fusion::replay::FusionInput::evaluate` |
| `trade_risk` | `bot::risk::evaluate_trade_guardrails_core` |
| `entry_coordination` | `bot::risk::evaluate_entry_coordination_core` |
| `kelly_sizing` | `bot::kelly::evaluate_kelly_size` |
| `entry_sizing` | `bot::execute::scale_entry_amount_core` |
| `simulated_buy_fill` (synthetic capture only) | `bot::execute::simulate_buy_fill_core` |
| `maker_paper_fill` | `bot::simulation::maker_fill_step` |
| `taker_paper_fill` | `bot::simulation::taker_arrival_fill_core` |
| `taker_paper_sell` | `bot::simulation::taker_arrival_sell_core` |

Replay calls the production decision functions. It does not reimplement them or invent expected
results while comparing: expected results are read from the supplied capture or synthetic fixture.
The CLI defaults to requiring all nine stages for the complete synthetic regression capture
produced by `scripts/verify_decision_parity.sh`. A runtime capture cannot satisfy that default;
pass an explicit comma-separated list of stages appropriate to the run as its third argument.
For example, `maker_paper_fill` checks maker fill arithmetic without claiming coverage of
the other stages. A listed stage with no record fails the comparison.

Replay deliberately does **not** call `Config::load` or re-normalise configuration, because that
would consult today's environment variables. It uses the effective, redacted configuration recorded
alongside each decision. Records also carry an `implementation_id`, a hash of the decision-relevant
module sources, their decision helpers and `Cargo.lock`. Every record must match the current
implementation hash; a different recorded hash is a replay error even when its numbers would
otherwise match. This fingerprint identifies the included decision source and dependency lock,
not an entire compiler, operating system or deployment environment.

## What replay does not prove

Replay establishes that **identical inputs and configuration produce identical decisions**. That is
a determinism and regression-evidence property within the recorded decision implementation.
Source or dependency changes require fresh captures; historical captures from another
implementation are rejected. Replay is not evidence of profitability, and says nothing about
whether the captured inputs resembled the market.

Specifically, replay does not cover:

- Feed acquisition, staleness detection, or what the bot would have seen with different timing
- Order placement, cancellation, or anything downstream of the decision
- Portfolio evolution, because each record is evaluated independently
- Whether the recorded market snapshot was itself accurate

## Conservative paper execution assumptions

The actor's paper maker path and the simulated-live taker path have configurable assumptions.
Replay records the fill arithmetic inputs: maker effective admission delay, observed quote age,
queue state, depth haircut and partial-fill cap; taker arrival book, original limit, requested
shares, depth haircut and explicit IOC/FOK policy. Taker wait timing and its configured admission delay are outside
the taker fill record, so matching that stage does not establish that the caller waited correctly.
The basic `PaperBackend` selected when
`sim_live_execution=false` is an accounting demonstration using analysis prices; it does not
use this execution model. Historical candle backtests also do not replay this model because
their data does not contain an order book, queue or trade tape. Neither path establishes
execution realism.
These defaults reduce optimistic fills; they are **not calibrated measurements** of the venue:

| Setting | Default | Effect |
| --- | --- | --- |
| `sim_order_latency_ms` | 250 ms | Discards maker opportunities before admission; taker entry waits and obtains a fresh arrival book while retaining the original decision limit. |
| `sim_available_depth_fraction` | 0.25 | Allows consumption of only one quarter of displayed eligible depth. |
| `maker_sim_queue_ahead_shares` | 5 shares | Consumes opportunity depth against an assumed queue ahead before granting maker fills. |
| `maker_sim_max_fill_fraction` | 0.25 | Caps each maker fill at one quarter of the initial quote; its remainder stays tracked and can be cancelled or replaced. |
| `maker_sim_fill_on_touch` | false | Requires strict trade-through; the legacy optimistic touch rule remains available for comparisons only. |

Maker admission also respects `maker_min_quote_lifetime_ms`: the effective wait is the larger
of that minimum and `sim_order_latency_ms` (800 ms with the supplied configuration).

The maker model uses contra-side BBO depth as an **opportunity proxy**, not as evidence of actual
traded volume. Duplicate or out-of-order observations cannot spend that depth again. At the same
contra price, an unchanged or falling displayed size cannot create another opportunity; only a
positive increase in depth supplies new capacity. Unused capacity is discarded between updates.
Latency and unusable-book observations are discarded, so an early crossing cannot be filled
retroactively by a timer. Actual queue position, hidden depth, cancellations ahead, and aggressive
trade prints are not reconstructed. A changed price level supplies its own displayed capacity;
repeated price oscillations can still differ from actual trading.

An admitted paper maker buy reserves its remaining cash and position slot. Partial fills of
that order update one durable position with cumulative shares, entry cost and weighted price;
small tranches are permitted only after the full quote has met the admission minimum. Order
progress, observation identity and the fill snapshot commit together, so a replayed observation
cannot debit twice. Cancellation releases the remainder. On restart, un-restored paper quotes
are cancelled and their reservations released; existing positions remain recorded.

Runtime simulated taker entries haircut the arrival ask ladder and require every requested
share to fill (FOK), matching the live caller's time in force. An incomplete arrival book
creates no position or cash debit. Successful entry evidence records the FOK policy, delay
and depth fraction. The public legacy IOC helper remains available for explicitly partial
experiments; captured inputs without a policy retain that legacy interpretation, while replay
still requires the current implementation identity. Neither policy models a resting GTC order.
The low-level `simulated_buy_fill` stage remains a static
book-walk primitive, and its result alone omits the admission and liquidity assumptions above.
Paper market sales require enough haircutted arrival bid depth for every requested share (FOK).
Insufficient depth refuses the sale; neither the best bid nor a midpoint is extrapolated over a
missing remainder. Passive maker fills and resolution use their own distinct settlement paths.

## Where simulated fills still diverge from real execution

The arrival model is a paper book walk. Real execution goes through
`bot::execute::execute_live_trade`, which signs an order and posts it to the Polymarket CLOB. The
two are not interchangeable. These remaining differences limit what the simulation establishes.

**The arrival book is still a snapshot.** The simulator walks the haircutted arrival ask ladder
at or below the original limit price. It cannot measure the exact book when a venue would accept
the order or reconstruct other participants' decisions during execution.

**Queue position is assumed, not observed.** Paper maker quotes can fill partially across updates,
retain a remainder and be cancelled/replaced, but their fixed assumed queue ahead is not an
exchange queue. A BBO crossing does not prove a trade consumed that queue.

**An FOK snapshot is not venue execution.** Runtime paper takers now require a complete
one-shot arrival fill, but do not model a venue's acceptance, rejection or acknowledgement.
The legacy IOC experiment remains separate. Maker remainder accounting does not model
venue cancellation acknowledgement timing or fills arriving while a cancellation is in flight.

**Fees are outside the fill primitive.** The book walker returns a gross price and applies no fee.
Paper taker entry costs are estimated separately and stored in `fee_paid` (including the entry
slippage estimate used by that path). Estimated paper exit taker fees reduce the trade's P&L;
they are not accumulated in `fee_paid`. Paper maker entries assume zero maker fees. These are
simulation assumptions, not receipt-verified venue fees. A price-only simulated return is gross;
the stored paper P&L includes exit fees, and net accounting also subtracts entry `fee_paid`.

**Local rules are not exchange acceptance.** Maker quote rules enforce the known tick and minimum
size locally, but the simulator does not reproduce self-trade prevention, rate limits,
authentication failures, venue minimum changes or uncertain submission outcomes.

**Visible liquidity only.** Hidden or iceberg size is not modelled, in either direction. The
simulator cannot fill against liquidity it was not shown, and cannot miss liquidity that was there.

**Latency is configured, not measured.** A fixed admission delay is not a network or matching-engine
latency distribution. Cancellation latency and stochastic outages are not modelled.

## How to read a result

A simulated fill remains an uncalibrated model. Conservative settings reduce optimism, but neither
these assumptions nor the remaining differences form a guaranteed upper bound: an order resting in a
queue can occasionally fill better than a snapshot walk suggests. Treat it as unvalidated,
not as a bound. Treat the gap as unquantified: no measurement in this repository establishes how large it
is, because no live trading has been done at scale. Establishing that gap requires funded execution
compared against simulated counterfactuals over the same markets, which remains outstanding.
