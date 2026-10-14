# Prometheus Alerts (Macbot)

This folder contains an alert ruleset for the `/api/metrics` endpoint:

- `macbot-alert-rules.yml`

## Wire Into Prometheus

1. Open your Prometheus config (Homebrew default):
   - `/opt/homebrew/etc/prometheus.yml`
2. Add a `rule_files` section that points to this rules file:

```yaml
rule_files:
  - /absolute/path/to/macbot-rs/ops/prometheus/macbot-alert-rules.yml
```

3. Keep your scrape job for Macbot:

```yaml
scrape_configs:
  - job_name: macbot
    metrics_path: /api/metrics
    follow_redirects: false
    http_headers:
      x-api-key:
        files: [/absolute/private/path/macbot-read-key]
    static_configs:
      - targets: ['localhost:8000']
```

The key file must contain only the read key, be readable by Prometheus and stay private. This uses the documented `http_headers.files` option in [Prometheus 2.55 configuration](https://prometheus.io/docs/prometheus/2.55/configuration/configuration/#scrape_config). For an older version, check its supported configuration before exposing metrics. No HMAC secret is required for scraping.

4. Reload or restart Prometheus:

```bash
brew services restart prometheus
```

## Validate Rules

If `promtool` is installed:

```bash
promtool check rules ops/prometheus/macbot-alert-rules.yml
```

Check loaded rules:

```bash
curl -s http://localhost:9090/api/v1/rules | jq '.status'
```

## Alert Intent

- `page`: loop stopped, kill-switch active, SLO guard trips, order rejects spike, DB insert failures.
- `warning`: stale/lagging L2 feed, API failure spikes, maker quote drained, reconcile drift events.
- `info`: no trade activity while loop is running.

Tune thresholds to your risk tolerance and cadence after one week of baseline telemetry.

New operator-facing counters worth watching:

- `macbot_analysis_snapshot_persist_fail_total`
- `macbot_shadow_kill_switch_trips_total`
- `macbot_trade_entry_block_kill_switch_total`
- `macbot_trade_entry_block_drawdown_total`
- `macbot_trade_entry_block_alpha_freeze_total`
- `macbot_trade_entry_block_degraded_total`
- `macbot_trade_entry_block_circuit_breaker_total`
- `macbot_trade_entry_block_l2_stale_total`
- `macbot_outbox_dropped_total`
- `macbot_actor_cmd_drop_total`

These are especially useful during canary and rollback drills because they tell you whether the bot is intentionally standing down versus silently doing nothing.

Prometheus alerts should be read alongside `/api/ready.last_reconcile_summary` and `/api/ready.last_reconcile_error` during restart drills. The alerts tell you something is wrong; the readiness surface tells you whether reconcile actually repaired it cleanly.
