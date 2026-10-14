#!/usr/bin/env python3
"""
Install project-specific Codex skills into $CODEX_HOME/skills (or ~/.codex/skills).
"""

from __future__ import annotations

import os
from pathlib import Path


def skill_doc(name: str, description: str, title: str, body: str) -> str:
    desc = description.replace("\\", "\\\\").replace('"', '\\"')
    return f"""---
name: {name}
description: "{desc}"
---

# {title}

Use this skill when the request matches the description above.

## Workflow
1. Read the latest runtime/config state first (`/api/config`, `/api/risk/status`, `/api/maker/state`).
2. Inspect existing implementation before proposing changes (`rg`, `cargo check`).
3. Make minimal, production-safe edits and keep maker/taker semantics explicit.
4. Validate with command + API checks; report metrics before/after.

## Output Contract
- State assumptions and scope.
- List changed files and why.
- Provide exact verification commands and expected output.

## Skill-Specific Guidance
{body}
"""


SKILLS: dict[str, tuple[str, str, str]] = {
    "maker-oms-engineer": (
        "Build and refine maker OMS logic: quote model, cancel/replace policy, and inventory skew in a single-writer execution actor.",
        "Maker OMS Engineer",
        """- Prefer actor-driven state transitions over ad-hoc async writes.
- Guardrails are mandatory: min quote lifetime, drift threshold, replace-per-minute ceiling.
- Keep `execution_mode` behavior strict: `maker` never falls through to taker path.""",
    ),
    "l2-feed-debugger": (
        "Diagnose and fix L2 ingestion issues (stale feed, schema drift, sequencing, subscription mismatches) for Polymarket CLOB WS.",
        "L2 Feed Debugger",
        """- Distinguish event age (`l2_feed_age_ms`) from network RTT.
- Validate env wiring: `MACBOT_CLOB_WS_URL`, `MACBOT_CLOB_WS_CHANNEL`, `MACBOT_L2_STALE_SECONDS`, `MACBOT_CLOB_BASE`.
- Verify snapshot->delta lifecycle and zero-size level removals.""",
    ),
    "execution-reconciliation": (
        "Harden execution reliability with idempotency, outbox/fill-ledger reconciliation, and exchange-vs-local drift repair flows.",
        "Execution Reconciliation",
        """- Reconcile by stable IDs, never by timestamp only.
- Ensure retries are idempotent and safe on partial failures.
- Expose explicit repair outcomes in logs/metrics.""",
    ),
    "risk-guard-tuner": (
        "Tune runtime risk armor: kill switch, stale-data gates, drawdown limits, slippage/order-age/exposure controls, degraded no-trade mode.",
        "Risk Guard Tuner",
        """- Keep fail-closed defaults in live mode.
- Every new guard needs telemetry and a visible UI state.
- Changes must include test/backtest evidence and rollback knobs.""",
    ),
    "backtest-walkforward-lab": (
        "Run robust backtesting and walk-forward sweeps, compare parameter sets, and detect overfitting/regime fragility.",
        "Backtest Walkforward Lab",
        """- Separate in-sample tuning from out-of-sample evaluation.
- Report median/dispersion and stability, not just best run.
- Include cost model assumptions (fees, gas, liquidity proxy).""",
    ),
    "telemetry-grafana-builder": (
        "Design Prometheus metrics and Grafana dashboards/alerts for execution, latency, risk trips, and strategy health.",
        "Telemetry Grafana Builder",
        """- Prefer low-cardinality labels.
- Add alert thresholds with intent (paging vs warning).
- Keep dashboard panels tied to actionable runbooks.""",
    ),
    "ui-trading-terminal": (
        "Maintain a professional trading terminal UI aligned with backend semantics for maker/taker execution and live telemetry.",
        "UI Trading Terminal",
        """- Do not show ambiguous metrics; label semantics explicitly.
- Preserve dense information hierarchy and keyboard-first operation.
- Keep realtime updates smooth and non-blocking.""",
    ),
    "live-security-checklist": (
        "Apply live-mode security hardening: auth gates, secret handling, TLS assumptions, log redaction, and preflight safety checks.",
        "Live Security Checklist",
        """- Verify auth-required endpoints and request signing.
- Confirm secrets are never returned in API responses or plaintext logs.
- Require explicit preflight checklist before enabling live mode.""",
    ),
    "release-canary-playbook": (
        "Plan and execute staged releases with canary checks, health gates, rollback triggers, and post-release verification.",
        "Release Canary Playbook",
        """- Promote in phases with objective health checks.
- Define rollback conditions before deploy.
- Capture before/after telemetry snapshots.""",
    ),
    "python-brain-bridge": (
        "Implement and validate Python model-to-Rust executor IPC bridges with latency budgets, heartbeat, and fail-closed behavior.",
        "Python Brain Bridge",
        """- Keep IPC payload minimal and versioned.
- Enforce heartbeat timeout and deterministic fallback policy.
- Measure end-to-end decision latency, not just model inference.""",
    ),
    "exchange-adapter-polymarket": (
        "Handle Polymarket/CLOB-specific API schema quirks, payload transformations, and compatibility wrappers safely.",
        "Exchange Adapter Polymarket",
        """- Isolate exchange-specific logic behind adapter interfaces.
- Track upstream schema assumptions with defensive parsing.
- Add integration probes for critical endpoints.""",
    ),
    "db-sqlite-performance": (
        "Optimize SQLite for trading workloads: WAL behavior, query/index tuning, contention hotspots, and durability tradeoffs.",
        "DB SQLite Performance",
        """- Prioritize predictable latency over peak throughput.
- Inspect query plans before adding indexes.
- Keep write paths atomic and crash-safe.""",
    ),
    "incident-postmortem": (
        "Produce structured incident postmortems for trading/runtime failures with timeline, root cause, impact, and prevention actions.",
        "Incident Postmortem",
        """- Include a concrete UTC timeline and blast radius.
- Separate trigger, contributing factors, and detection gaps.
- End with owner-assigned prevention items.""",
    ),
    "ui-terminal-art-direction": (
        "Define and enforce strong visual art direction for a high-end trading terminal (typography, depth, palette, hierarchy).",
        "UI Terminal Art Direction",
        """- Use a cohesive design token system.
- Keep contrast and readability high under dense data.
- Avoid generic styles; retain a distinctive but consistent identity.""",
    ),
    "ui-motion-system": (
        "Implement meaningful motion primitives for realtime trading UI without harming readability or performance.",
        "UI Motion System",
        """- Animate state transitions, not constant noise.
- Keep timings short and consistent.
- Respect reduced-motion accessibility preferences.""",
    ),
    "ui-data-viz-trading": (
        "Design and tune trading-specific visualizations: PnL curves, spread/depth cues, telemetry strips, and market tables.",
        "UI Data Viz Trading",
        """- Encode risk and direction with unambiguous color semantics.
- Use smoothing/aggregation only when clearly disclosed.
- Make abnormal states immediately scannable.""",
    ),
    "ui-realtime-interactions": (
        "Optimize realtime UI interaction loops, update batching, and incremental rendering for fast, stable dashboards.",
        "UI Realtime Interactions",
        """- Prefer diff updates over full re-renders.
- Separate critical telemetry cadence from heavy refresh work.
- Protect user interactions from stream jitter.""",
    ),
    "ui-command-palette-hotkeys": (
        "Build power-user command palette and keyboard workflows for rapid control of bot actions and views.",
        "UI Command Palette Hotkeys",
        """- Keep hotkeys discoverable and conflict-safe.
- Require confirmations for dangerous actions.
- Ensure controls mirror API guardrails.""",
    ),
    "ui-responsive-terminal": (
        "Adapt dense trading terminal layouts across desktop/mobile while preserving signal clarity and action speed.",
        "UI Responsive Terminal",
        """- Prioritize critical telemetry above fold on small screens.
- Avoid hidden critical states behind deep menus.
- Test at multiple widths with live data volume.""",
    ),
    "ui-accessibility-finance": (
        "Enforce accessibility standards for finance UIs: contrast, focus order, color-safe semantics, and reduced-motion support.",
        "UI Accessibility Finance",
        """- Ensure color is never the sole status indicator.
- Validate keyboard navigation across primary actions.
- Keep text size and contrast robust under dark themes.""",
    ),
    "ui-design-qa": (
        "Run final UI QA sweeps for trading dashboards: alignment, overflow, state consistency, loading/error handling, and semantic correctness.",
        "UI Design QA",
        """- Check empty/loading/error states explicitly.
- Verify labels match backend field semantics.
- Capture before/after screenshots for regressions.""",
    ),
}


def main() -> None:
    codex_home = Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex")))
    skills_root = codex_home / "skills"
    skills_root.mkdir(parents=True, exist_ok=True)

    created = []
    for skill_name, (desc, title, body) in SKILLS.items():
        skill_dir = skills_root / skill_name
        skill_dir.mkdir(parents=True, exist_ok=True)
        skill_md = skill_dir / "SKILL.md"
        skill_md.write_text(skill_doc(skill_name, desc, title, body), encoding="utf-8")
        created.append(skill_name)

    print(f"Installed/updated {len(created)} skills in {skills_root}")
    for s in sorted(created):
        print(f"- {s}")


if __name__ == "__main__":
    main()
