# Polymarket contract compatibility evidence — 10 October 2026

This records adapter evidence, not a claim that a funded account has traded successfully. No real orders or private credentials were used for these checks.

## Wallets and signing

The adapter separates the account signer from the wallet that holds assets. Private request authentication uses the signer address. Orders use the funding wallet as maker. Proxy and Safe orders retain the account signer in the signed order; Deposit Wallet orders use the funding wallet as both maker and order signer, with the ERC-7739 nested signature wrapper.

The legacy constructor still selects an EOA. `ClobAuth::new_wallet` accepts signature types 0–3; smart wallets require an explicit, nonzero funding address. EOA funding addresses must equal their signer. Deposit Wallet signatures are refused for Exchange V1.

Exchange V3 uses the V2 JSON fields, including `tokenId`, but signs a distinct version-3 domain at its dedicated exchange. The market's asset identity must select this route. The official current order workflow identifies PositionManager markets through the book's `version: "v2"`; CTF books omit that field. A global `/version` response alone cannot distinguish them.

The independent fixture generator uses the official Python SDK 1.1.0 at commit `292c11005d748c21342a9457d7c0ac89afc2e3f2`. Its 36 cases cover BUY/SELL, EOA/Proxy/Safe/Deposit wallets where supported, legacy/current/position protocols, negative-risk CTF exchanges, and canonical order hashes. These fixtures contain only a public dummy key and public test data. Regeneration produced identical bytes.

## Exact settlement accounting

Order acceptance and order-limit prices do not establish settled quantities, paid fees, or proceeds. `SettlementVerifier` reads a configured Polygon RPC and requires chain 137, a successful transaction receipt, a finalized block, and agreement with the canonical block hash. It accepts only the expected exchange's `OrderFilled` events for the expected order hash, wallet, asset, and side.

The decoder preserves gross share and collateral amounts and the actual fee in its native denomination. V1 BUY fees reduce received shares; V1 SELL fees reduce collateral proceeds. V2/V3 fees are collateral: BUY fees increase spending and SELL fees reduce proceeds. Fee-curve parameters are useful for budgeting, not a replacement for historical receipt evidence.

Each returned fill carries transaction hash and log index for persistent deduplication. Repeated identical logs are counted once; conflicting copies fail. `OrdersMatched` events are excluded because they describe quantities already represented by the taker's `OrderFilled` event. Null receipts, missing finality support, reverted transactions, removed logs, unsupported exchanges, incorrect identities, reorged blocks, and quantities beyond the accounting precision limit fail closed.

Transaction hashes are compared and deduplicated case-insensitively. An `UNMATCHED` order remains pending: the [official order lifecycle](https://docs.polymarket.com/concepts/order-lifecycle) describes this status as potentially resting after a delay, so it cannot establish cancellation or release a retry budget.

The provider has no implicit RPC endpoint. The operator must configure a trusted HTTPS Polygon RPC that supports finalized-block reads. HTTP is accepted only for loopback test servers. This is a trust dependency: ordinary JSON-RPC is not an independent cryptographic proof that a malicious provider is truthful.

Collateral budgeting refreshes the authenticated wallet's venue cache, then reads the distinct balance endpoint. It accepts exact, nonnegative integer micro units, including zero. It does not fall back to another wallet or add unrelated collateral balances. Funded expiry does not use Gamma outcomes as proof of paid redemption: inventory and accumulated realized P&L remain visible while entries are frozen pending transaction evidence.

## Checks performed

- Six authentication tests passed, including the existing 48 SDK signing vectors and the new 36 wallet/order-hash vectors.
- Five settlement tests passed, covering every protocol and side, duplicate and unrelated events, bad identities, pending/reverted/reorged receipts, precision limits, and a local RPC fault test.
- Six balance tests passed, covering Safe wallet versus signer identity, refresh-before-read ordering, exact units and zero, invalid balances, refresh failures, pending maker/taker reservations and unavailable reservation data. The terminal-reservation check caught and fixed an integer-versus-real SQLite empty-sum bug.
- A live expiry regression passed, retaining inventory, entry fees and earlier partial-sale P&L across repeated guards and restart without inventing an exit or payout.
- SDK fixture regeneration and byte comparison passed.

These checks validate the signing and receipt contracts. They do not prove strategy profitability, wallet deployment, spending approvals, venue account permissions, or a real funded execution. The untrained bundled model still provides no trading edge.

## Primary references

- [Official wallet and authentication workflow](https://docs.polymarket.com/trading/wallets-auth)
- [Official order routing, signing, and spending workflow](https://docs.polymarket.com/trading/place-orders)
- [Official Python CLOB SDK reference](https://github.com/Polymarket/py-clob-client-v2/tree/292c11005d748c21342a9457d7c0ac89afc2e3f2)
- [Legacy OrderFilled ABI](https://github.com/Polymarket/ctf-exchange/blob/main/src/exchange/interfaces/ITrading.sol)
- [Legacy fee denomination](https://github.com/Polymarket/ctf-exchange/blob/main/src/exchange/libraries/CalculatorHelper.sol)
- [Exchange V2 event ABI](https://github.com/Polymarket/ctf-exchange-v2/blob/main/src/exchange/mixins/Events.sol)
- [Exchange V2 settlement transfers](https://github.com/Polymarket/ctf-exchange-v2/blob/main/src/exchange/mixins/Trading.sol)
- [Exchange V3 event ABI, domain, and settlement transfers](https://github.com/Polymarket/polymarket-v2-external/blob/main/src/exchange/Exchange.sol)

Some narrative documentation in the upstream V3 contract repository still describes an older signing domain. The implementation, current API documentation, and independently generated SDK vectors agree on `Polymarket CTF Exchange`, domain version `3`; those are the references used here.
