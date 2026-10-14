# Recovery without inventing orders or inventory

This procedure applies to a single bot instance and its own database. Keep live
entries frozen throughout investigation. Do not delete ledger rows, edit state
flags, reset the database, or submit a replacement order because a response was
lost. Restoring an older backup cannot undo orders executed at the venue.

## Preserve and inspect

1. Freeze entries with the authenticated kill-switch control. Stop the automatic
   loop. Cancelling a quote does not prove that it had no fills.
2. Save a new verified database backup. The runtime default is `trades.db`;
   `MACBOT_DB` can select another file. For example:

   ```bash
   python3 scripts/sqlite_backup.py --source trades.db --dest artifacts/backups/recovery.sqlite
   ```

   The CLI refuses source/destination aliases, unsupported schemas and active
   destination sidecars. An overwrite replaces an inactive backup only after
   validating the new snapshot. Backups contain private trading history.
3. Set `MACBOT_API_KEY` privately in your shell; do not put it in command history.
   Export the authenticated local evidence:

   ```bash
   python3 scripts/recovery_report.py --out artifacts/recovery/status.json
   python3 scripts/recovery_report.py --order-id ORDER_ID --out artifacts/recovery/order.json
   ```

   The status export lists pending/audit-uncertain taker orders, pending/uncertain maker orders,
   open live inventory, paper reservations, manual market stops and bounded redemption summaries. The order
   export includes original intents, terminal history, receipt match records,
   allocations, fills and audit records. Each table explicitly reports whether
   the 1,000-row limit truncated it. A truncated export is incomplete; retain
   the verified database and inspect it read-only. These exports do not query
   the venue or verify a wallet balance.

   A redemption transaction hash selects scoped stored receipt/lot/fill/realization
   evidence through the same order export. Per-trade fill responses expose
   `redemption_evidence` separately from book-price parity, which cannot prove
   collateral redemption. Stored proof is local evidence, not a fresh chain audit.

## Lost response or confirmation still pending

A pending taker intent or `CLOSING` exit remains reserved. An authenticated
`POST /api/resync/full` can complete accounting when confirmed account trades
and canonical finalized receipts become available. Maker recovery runs in the
execution actor. Missing evidence remains pending; neither timeout nor an empty
page authorizes another submission. Resync does not clear the kill switch.

## Terminal order audit uncertainty

Maker `AUDIT_UNCERTAIN` or taker `audit_uncertain` means that an earlier terminal observation could no longer be
verified. Save evidence first. With entries frozen, request an authenticated
`POST /api/recovery/orders/ORDER_ID/recheck` (the old maker route remains compatible). The same timestamped HMAC signature
required for every protected mutation is required here; use `api_auth.post_json`
with your key and secret rather than an unsigned curl request.

This narrow recheck succeeds only when the original nonempty terminal baseline
still matches complete authenticated trade history, the venue order is terminal
with the same quantity, and every recorded finalized receipt still matches
inventory, cost and fee. It commits the state change and an audit entry together.
It changes no trade, creates no order and leaves both risk stops in place.
A lost recheck response is handled by inspecting the persisted state and audit.

Changed or new executions, missing baselines, missing finalized receipts,
rejected submissions without a terminal venue record and contradictory history
remain blocked. The recheck cannot waive them. Settled taker audits rotate eight
order IDs every30seconds, so complete-history revisit time grows with history.
Older rows obtain their first baseline only after current complete receipt/history
checks; this does not verify unseen past changes. There is no general repair wizard
for external wallet activity, legacy unverified inventory or unsupported dust.
Those cases require independent reconciliation by the operator; the source sale
includes no assistance. Do not treat the recheck as funded-deployment acceptance.

## Record a supported external redemption

The source does not sign or submit redemption transactions. After independently
redeeming a standard CTF condition through the supported collateral adapter into
pUSD, an operator can request read-only receipt reconciliation. Keep the service
in live mode with its loop stopped and kill switch engaged, and use the same
authenticated request signing helper to call:

```text
POST /api/recovery/redemptions/reconcile
{"transaction_hash":"FINALIZED_POLYGON_TX_HASH","condition_id":"CTF_CONDITION_ID","trade_ids":[1,2]}
```

The identifiers select evidence, never a caller-supplied quantity, price or payout.
Every managed live lot of those redeemed tokens must be included, with exact
receipt-backed entry basis and dated partial-sale history. The verifier proves
canonical finalized account transfers, every burn, collateral wrapping/payout,
resolved condition identity, and that each cumulative entry preceded redemption
in chain order. A20second read deadline refuses slow evidence before accounting;
the atomic receipt/lot commit itself is not cancelled. Repeated identical receipt
and lot selection cannot credit profit twice. Changed evidence or selection refuses.

Configuration changes, loop start and kill-switch clearing are excluded during
this repair; emergency stop/freeze remain available. The result reports actual
stop and collateral-read state. Missing spendable-balance verification keeps the
service degraded. Nothing automatically clears the kill switch.

This path refuses negative-risk, V2 PositionManager/Router, V3/direct-USDC.e variants, unknown contracts,
mixed foreign holdings and unverified legacy basis. It records collateral profit
excluding native gas and external redemption costs. Local stored proof is not a
fresh independent chain audit. Use the transaction hash with the scoped evidence
export to retain its receipt, lot outcomes and dated risk events. Other redemption
variants require independent operator reconciliation and remain unsupported.

## Cash-origin changes

Do not use `POST /api/config` to change `initial_balance`. Every patch containing that key is rejected in full, even with a sensitive-config override; other keys in the same patch remain unchanged. The settings screen shows it read-only. Configure the starting value before startup or use the existing signed `POST /api/balance/reset` for a non-live runtime reset. That reset runs through the execution actor, ordering accounting inspection, origin persistence and cash publication with entries and exits. The reset requires a current recognized paper, shadow or simulation mode and no immutable boot-time live-wallet client, as do entries, simulated maker fills, paper full/partial sales and pair settlement. Live mode with shadow enabled is refused. Changing a wallet-backed process to a non-live config does not convert the cash origin or permit paper credits/debits. Legacy mixed paper lots stay unresolved in that process: handle their paper accounting only in an isolated process without a live-wallet client, with live recovery kept separate. It is not a repair for uncertain live orders or proof of spendable venue collateral. Partial or live merge is not supported.

## Restore drill

Stop every process using the original and target databases. Preserve the original
and its sidecars together; never overwrite an active database. Make another
verified copy from the backup into a **new** restore path, then start a research
instance with `MACBOT_DB` pointing to that path and live execution disabled.
Confirm ledger rows, pending reservations and market stand-down controls.

The `meta` table also stores consumed signed-request nonces. An older backup
cannot contain requests received after its snapshot. Before accepting signed
writes, rotate the HMAC secret (including any environment override), or keep
every pre-restore process stopped for at least eleven minutes before starting the
restored service. The eleven-minute interval covers future-dated signatures at the
maximum 300-second timestamp skew, including its inclusive whole-second boundary. Keeping the old secret and restarting an
older snapshot immediately can permit captured requests to replay.

An offline drill cannot identify orders placed after a backup. Before any funded
restart, account-wide venue and wallet reconciliation must establish that the
snapshot covers all actual exposure. Do not clear stops just because the restored
process starts. The included tests cover populated-ledger snapshots, process
crash/restart, and mock-venue recovery; real funded restore remains unverified.
