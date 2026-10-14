# Security policy

## Supported state

This repository is a research asset. Simulation and backtest-only modes are the supported evaluation paths. Live deployment requires an independent security and operational review.

## Secrets

- Never commit `.env`, private keys, exchange credentials, wallet material, webhook URLs, production databases, or generated config snapshots containing secrets.
- Use a dedicated, minimally funded wallet for any canary.
- Keep `MACBOT_BIND_ALL=0` unless a TLS-terminating, authenticated reverse proxy is in place.
- Replace the development API key and HMAC secret before exposing the service off-host.
- Runtime settings saves keep environment-only credentials out of the configuration file. An explicit credential update persists only the supplied fields; changing a provider key does not copy unrelated API credentials or a private RPC URL from the process environment. Explicit API-key/HMAC environment overrides continue to govern request authentication.
- Protected requests require credentials even when the TCP peer is localhost. A reverse proxy must forward the authentication headers; forwarded IP headers do not grant access.
- Dashboard credentials stay in browser session storage by default and persist in local storage only when Remember is selected. Use a trusted browser profile and clear `macbot.apiAuth` when retiring its access.
- The dashboard sends credentials only to its own origin. Active streams are rechecked against credential and IP policy changes.
- Detailed runtime/history/metrics reads require a read key; mutations also require HMAC signatures. Health and minimal readiness remain public.
- Each signed mutation includes a fresh random 32-byte hex `x-api-nonce`. The signature covers the compact UTF-8 JSON array `[timestamp_string, nonce, METHOD, path_and_query, idempotency_key_or_empty, exact_body_text]`; nonce claims are committed before the action and survive process restarts. A retry signs a fresh nonce and retains its execution idempotency key. Replay storage failures refuse the action.
- `api_max_skew_seconds` must be between 1 and 300 (default 30). Transport credentials still require HTTPS outside localhost: signatures do not encrypt requests or protect a compromised client.
- Restoring an older database also restores older replay claims. Rotate the HMAC secret before accepting signed writes, or keep every pre-restore process stopped for at least eleven minutes before starting the restored service. This covers a future-dated signature at the maximum allowed skew, including the inclusive whole-second boundary. Restore is not reconciliation of orders placed after the snapshot.
- Treat all transferred runtime data as sensitive until it has been reviewed and sanitized.

## Reporting

Report suspected vulnerabilities privately to the current repository owner. Do not include live credentials, private keys, or customer/trading data in a report.
