# Truth-First Program

`macbot-rs` is currently in a plumbing-first phase.

The project goal for this phase is not alpha discovery or rollout promotion. The goal is to make the system truthful enough that future alpha evidence means something.

## Objective

Focus the next batch of work on execution, lifecycle, feed, and reporting truth.

This means:
- proving simulated execution prices are plausible
- proving shutdown leaves no stranded trades
- proving fills, closes, and rejections are persisted consistently
- proving control/validation soaks explain why the bot did or did not trade

## Allowed Work

- validation control soaks
- shutdown-validation soaks
- execution and reconciliation hardening
- trade lifecycle integrity checks
- feed/session truth and stale-state diagnostics
- reporting, blocker summaries, and observability
- storage, backup, and artifact hygiene

## Deferred Work

- alpha tuning
- new maker or taker candidate development
- signal-weight optimization
- new rollout candidate promotion
- canary or live rollout prep

## Why This Does Not Use `MACBOT_FREEZE_ALPHA`

The runtime already supports `MACBOT_FREEZE_ALPHA=1`, but that blocks new entries entirely in the analysis cycle.

That switch is too blunt for this phase because validation work still needs controlled entries and open inventory in order to prove:
- shutdown cleanup
- fill persistence
- lifecycle consistency
- control-run reporting

So the truth-first program is an operator and rollout-state decision, not a global runtime freeze.

## Exit Criteria

The project can leave this phase only after all of the following are true:

1. Repeated control runs produce only plausible execution fills.
2. Shutdown validation is proven with open inventory at exit.
3. Validation runs end with no stranded `OPEN` or `CLOSING` trades.
4. Reporting explains suppression, market quality, trade rejection, and shutdown outcomes without manual forensic digging.
5. The validation rail is repeatable enough that a bad run can be classified as logic, market quality, or environment, rather than mystery.

## Current Shutdown Status

Shutdown proofing is now considered **automated-acceptance-complete but market-evidence-incomplete**.

What is done:
- the shutdown-validation harness is in place
- lifecycle integrity checks are in place
- single-trade shutdown cleanup has been proven
- the remaining known fail-open shutdown and live-close edges have been hardened
- persisted state is checked even when hot state is empty, and an unreadable database cannot report clean
- the execution actor is explicitly drained and joined during orderly shutdown
- a subprocess crash/restart test proves WAL recovery of multiple open positions and balance after the original process is gone

What is still missing:
- a clean **multi-open** shutdown proof bundle with real open inventory at exit

Why it remains incomplete:
- the latest validation attempts ended as `starvation/dead_window`
- the harness stayed truthful and clean, but the market windows did not exercise the multi-open cleanup path

Program decision:
- do not spend more active engineering time on shutdown proofing for now
- treat shutdown proofing as parked unless a clearly live validation window is available
- do not present shutdown cleanup as fully proven across multi-open inventory until that evidence exists
