#!/usr/bin/env node
// Absent must render as absent.
//
// Three separate ways the dashboard used to turn "we do not know" into a
// definite claim, all of which looked like real readings:
//
//   1. fmtMoney/fmtPct/fmtN checked null but not NaN, so anything non-numeric
//      rendered as "$NaN" — while their four siblings guarded it correctly.
//   2. `if (x != null) el.textContent = ...` left the previous reading on
//      screen when the field went missing.
//   3. Panels fed by their own polled endpoints were never marked stale,
//      because the mark read from a hardcoded list of four tabs out of nine.

import { readFileSync } from "node:fs";
import vm from "node:vm";
import assert from "node:assert/strict";

const src = readFileSync("static/dashboard.js", "utf8");

function extract(pattern, label) {
  const m = pattern.exec(src);
  assert.ok(m, `${label} not found — it was renamed or removed, so this check is testing nothing`);
  return m[0];
}

// Arrow-function consts, up to their closing "};"
const arrow = name => extract(new RegExp(`const ${name} = [\\s\\S]*?\\n};`), name);
// Plain function declarations, up to a closing brace at column zero
const fn = name => extract(new RegExp(`function ${name}\\([\\s\\S]*?\\n}`), name);

const code = [
  arrow("fmtMoney"),
  arrow("fmtPct"),
  arrow("fmtN"),
  extract(/const PANEL_SOURCES = \[[\s\S]*?\];/, "PANEL_SOURCES"),
  "const sourceFetchedAt = new Map();",
  fn("notePathFetched"),
  fn("panelSourceIsStale"),
  // `const` in a vm script is a lexical binding, not a context property, so
  // hand the bindings out explicitly rather than reading them off the context.
  `globalThis.exported = { fmtMoney, fmtPct, fmtN, notePathFetched,
                           panelSourceIsStale, PANEL_SOURCES, sourceFetchedAt };`,
].join("\n");

const ctx = vm.createContext({});
vm.runInContext(code, ctx);
const { fmtMoney, fmtPct, fmtN, notePathFetched, panelSourceIsStale, PANEL_SOURCES, sourceFetchedAt } =
  ctx.exported;

const DASH = "—";
const checks = [];
const check = (name, body) => { body(); checks.push(name); };

check("a non-numeric value is absent, not $NaN", () => {
  for (const bad of ["abc", {}, NaN, Infinity, -Infinity]) {
    assert.equal(fmtMoney(bad), DASH, `fmtMoney(${String(bad)})`);
    assert.equal(fmtPct(bad), DASH, `fmtPct(${String(bad)})`);
    assert.equal(fmtN(bad), DASH, `fmtN(${String(bad)})`);
  }
});

check("null and undefined stay absent", () => {
  for (const empty of [null, undefined]) {
    assert.equal(fmtMoney(empty), DASH);
    assert.equal(fmtPct(empty), DASH);
    assert.equal(fmtN(empty), DASH);
  }
});

check("real numbers still render", () => {
  assert.equal(fmtMoney(12.345), "$12.35");
  assert.equal(fmtPct(12.34), "12.3%");
  assert.equal(fmtN(0.98765, 3), "0.988");
  assert.equal(fmtMoney(0), "$0.00", "a genuine zero is a reading and must show");
});

check("every tab with its own data source is covered", () => {
  // The five that the hardcoded analysis list left permanently unmarked.
  const covered = new Set(PANEL_SOURCES.map(s => s.tab));
  for (const tab of ["maker", "execution", "risk", "report", "config"]) {
    assert.ok(covered.has(tab), `${tab} has no freshness source, so it can never be marked stale`);
  }
});

check("a panel that has never loaded is not treated as fresh", () => {
  assert.equal(panelSourceIsStale("maker"), true);
});

check("a panel goes stale once its own budget elapses", () => {
  const now = 1_000_000;
  const maker = PANEL_SOURCES.find(s => s.tab === "maker");
  sourceFetchedAt.set("maker", now);
  assert.equal(panelSourceIsStale("maker", now + 1000), false, "just fetched");
  assert.equal(
    panelSourceIsStale("maker", now + maker.budgetMs + 1),
    true,
    "past its budget and still claiming to be current",
  );
});

check("a fetch marks only the panel it actually feeds", () => {
  sourceFetchedAt.clear();
  notePathFetched("/api/maker/state");
  assert.equal(panelSourceIsStale("maker"), false);
  assert.equal(panelSourceIsStale("risk"), true, "an unrelated fetch refreshed the wrong panel");
});

check("a tab with no declared source is never falsely marked", () => {
  assert.equal(panelSourceIsStale("overview"), false);
});


// ── The minutes-left tile must not announce a market that is not there ─────
//
// Between markets the stream still carries `time_remaining_minutes: 0.0`
// (REST sends null for the same instant), and "0.0m" reads as a market
// expiring this second. The countdown clocks gate on whether a market exists;
// the tile has to gate the same way. Exercised through the real function with
// a permissive DOM, not by inspecting the source.
{
  const extractFn = name => {
    const decl = new RegExp(`(?:^|\\n)function ${name}\\s*\\(`).exec(src);
    assert.ok(decl, `${name} not found — renamed, so this check is testing nothing`);
    let depth = 0, started = false, end = -1;
    for (let i = decl.index; i < src.length; i++) {
      const ch = src[i];
      if (ch === "{") { depth++; started = true; }
      else if (ch === "}") { depth--; if (started && depth === 0) { end = i; break; } }
    }
    assert.ok(end > 0, `could not extract ${name}`);
    return src.slice(decl.index, end + 1);
  };
  const mkEl = () => ({
    textContent: "", innerHTML: "", value: "", hidden: false,
    style: {}, dataset: {},
    classList: { add() {}, remove() {}, toggle() {}, contains: () => false },
    setAttribute() {}, removeAttribute() {}, querySelector: () => null, querySelectorAll: () => [],
    appendChild() {}, closest: () => null,
  });
  const runTile = (a, book = null) => {
    const tile = mkEl();
    const els = {};
    const dom = {
      getElementById: id => (id === "sig-mins-left" ? tile : (els[id] ||= Object.assign(mkEl(), {
        side: mkEl(), querySelector: sel => (sel === ".odd-side" ? els[id].side : null),
      }))),
      querySelector: () => null,
      querySelectorAll: () => [],
    };
    const code = [
      "const AGE_UNKNOWN = -1; let lastAnalysisFreshness = null;",
      arrow("fmtMoney"), arrow("fmtPct"), arrow("fmtN"), arrow("fmtSigned"),
      extractFn("fmtAge"), extractFn("currentAnalysisAge"),
      "const LIVE_BOOK_MAX_AGE_MS = 5000; const liveBook = new Map();",
      extractFn("expireLiveBook"), extractFn("noteLiveBook"), extractFn("liveMidFor"), extractFn("liveOddsFor"),
      extractFn("updateWeights"),
      extractFn("updateSignalPanel"),
      "globalThis.__run = (a, book) => { liveBook.clear(); if (book) noteLiveBook(book.map(e => ({ updated_at: new Date().toISOString(), ...e }))); updateSignalPanel(a); };",
    ].join("\n");
    const c = vm.createContext({ document: dom, Date, Math, console, window: {}, Number, String, Boolean, Array, Map });
    try {
      vm.runInContext(code, c);
      c.__run(a, book);
    } catch (e) {
      // A throw partway through is still informative for the tile, but say so.
      assert.ok(tile.textContent !== "", `updateSignalPanel threw before writing the tile: ${e.message}`);
    }
    return { tile: tile.textContent, yes: els["odds-yes"]?.textContent, yesSide: els["odd-yes"]?.side?.textContent,
      signal: els['signal-badge']?.textContent, confidence: els['sig-confidence']?.textContent };
  };

  check('absent analysis does not invent HOLD or zero confidence', () => {
    const result = runTile({});
    assert.equal(result.signal, DASH);
    assert.equal(result.confidence, DASH);
  });

  check("between markets the tile is absent, not \"0.0m\"", () => {
    assert.equal(runTile({ time_remaining_minutes: 0.0, market_data: {} }).tile, DASH);
  });
  check("with a market the tile shows the minutes", () => {
    assert.equal(runTile({ time_remaining_minutes: 6.0, market_data: { market_id: "0xabc" } }).tile, "6.0m");
  });
  // One token, one price. The same token read 0.78 in the odds tile (analysis
  // snapshot) and 0.83 in the positions row (per-row REST fetch) at the same
  // moment, with nothing saying which was older.
  check("the odds tile prefers the live book and says so", () => {
    const a = { time_remaining_minutes: 6.0, market_data: { market_id: "0xabc", price: 0.22 }, p_up: 0.26 };
    const book = [{ token_id: "t-yes", market_id: "0xabc", is_yes_token: true, best_bid: 0.16, best_ask: 0.18 }];
    const r = runTile(a, book);
    assert.equal(r.yes, "0.17", "live mid 0.17 must win over the 0.22 snapshot");
    assert.equal(r.yesSide, "Yes · up · live");
  });
  check("without a live book the tile shows the snapshot and its age", () => {
    const a = { time_remaining_minutes: 6.0, market_data: { market_id: "0xabc", price: 0.22 }, p_up: 0.26 };
    const r = runTile(a, null);
    assert.equal(r.yes, "0.22");
    assert.notEqual(r.yesSide, "Yes · up · live", "must not claim live without a book");
  });
  check("a live entry for a different market is not used", () => {
    const a = { time_remaining_minutes: 6.0, market_data: { market_id: "0xabc", price: 0.22 }, p_up: 0.26 };
    const book = [{ token_id: "t-yes", market_id: "0xOTHER", is_yes_token: true, best_bid: 0.90, best_ask: 0.92 }];
    assert.equal(runTile(a, book).yes, "0.22");
  });

  check("a question alone counts as a market, as it does for the clocks", () => {
    assert.equal(runTile({ time_remaining_minutes: 6.0, question: "Bitcoin Up or Down" }).tile, "6.0m");
  });
}


// ── Save Config sends only what changed ─────────────────────────────────────
//
// The panel contains `mode`, which the API refuses on every request, so
// posting the whole panel meant Save Config returned 403 on every press — a
// change to one number included — and had never once worked.
{
  const fnSrc = name => {
    const decl = new RegExp(`(?:^|\\n)function ${name}\\s*\\(`).exec(src);
    assert.ok(decl, `${name} not found`);
    let depth = 0, started = false, end = -1;
    for (let i = decl.index; i < src.length; i++) {
      if (src[i] === "{") { depth++; started = true; }
      else if (src[i] === "}") { depth--; if (started && depth === 0) { end = i; break; } }
    }
    return src.slice(decl.index, end + 1);
  };
  const field = (key, type, value, extra = {}) => ({ dataset: { key, ...extra }, type, value: String(value), checked: value === true });
  const fields = [
    field("circuit_breaker_losses", "number", 3),
    field("take_profit_threshold", "number", 0.85),
    field("kelly_sizing", "checkbox", true),
    field("mode", "select-one", "simulation"),
    field("finnhub_api_key", "text", "", { redacted: "true" }),
    // Even stale markup or a manually restored data-key must not make this
    // actor-owned accounting origin part of a generic settings write.
    field("initial_balance", "number", 999),
  ];
  const dom = { querySelectorAll: sel => (sel === "#config-panel [data-key]" ? fields : []) };
  const code = [fnSrc("readConfigFromPanel"), fnSrc("configValuesEqual"), fnSrc("readAllConfigFields"),
    "globalThis.__read = b => JSON.stringify(readConfigFromPanel(b));"].join("\n");
  const ctx2 = vm.createContext({ document: dom, currentConfig: {}, Math, String, Object, Number, JSON });
  vm.runInContext(code, ctx2);
  // A vm object carries the vm's Object.prototype and fails strict deep-equal
  // out here, so results cross the boundary as JSON text.
  const c = { __read: b => JSON.parse(ctx2.__read(b)) };
  const baseline = { circuit_breaker_losses: 3, take_profit_threshold: 0.85, kelly_sizing: true, mode: "simulation" };

  check("an unchanged panel sends nothing — not `mode`, not anything", () => {
    assert.deepEqual(c.__read(baseline), {});
  });
  check("changing one number sends exactly that key", () => {
    fields[0].value = "4";
    assert.deepEqual(c.__read(baseline), { circuit_breaker_losses: 4 });
    fields[0].value = "3";
  });
  check("startup-only mode cannot leak into a config save through stale markup", () => {
    fields[3].value = "live";
    assert.deepEqual(c.__read(baseline), {});
    fields[3].value = "simulation";
  });
  check("without a baseline, editable settings are sent and startup values are excluded", () => {
    const all = c.__read(null);
    assert.ok("circuit_breaker_losses" in all);
    assert.ok(!("mode" in all));
    assert.ok(!("initial_balance" in all));
  });
  check("an altered initial balance is excluded from config changes", () => {
    assert.deepEqual(c.__read({ ...baseline, initial_balance: 100 }), {});
  });

  check("clearing optional weights path sends the empty string while redacted blank credentials stay untouched", () => {
    const pathField = field("signal_weights_path", "text", "");
    fields.push(pathField);
    const previous = { ...baseline, signal_weights_path: "/private/previous-weights.json", finnhub_api_key: "***" };
    assert.deepEqual(c.__read(previous), { signal_weights_path: "" });
    assert.deepEqual(c.__read({ ...previous, signal_weights_path: "" }), {});
    fields.pop();
  });

  let configMarkup = "";
  const renderCtx = vm.createContext({
    document: { getElementById: () => ({ set innerHTML(value) { configMarkup = value; } }) },
    ledgerEsc: value => String(value).replaceAll('"', '&quot;').replaceAll('<', '&lt;'),
  });
  vm.runInContext(`${fn("getNestedVal")}\n${fn("renderConfigPanel")}`, renderCtx);
  renderCtx.renderConfigPanel({ initial_balance: 100, trade_amount: 5 });
  check("initial balance remains visible read-only with reset guidance", () => {
    const input = configMarkup.match(/<input[^>]*id="cfg-initial_balance"[^>]*>/)?.[0];
    assert.ok(input);
    assert.match(input, /\breadonly\b/);
    assert.match(input, /value="100"/);
    assert.doesNotMatch(input, /data-key=/);
    assert.match(configMarkup, /Change it during setup or use the signed balance reset API/);
  });
  check("startup-only mode and calendar path remain visible but cannot be edited", () => {
    for (const key of ['mode', 'macro_event_calendar_path']) {
      const input = configMarkup.match(new RegExp(`<input[^>]*id="cfg-${key}"[^>]*>`))?.[0];
      assert.ok(input);
      assert.match(input, /\breadonly\b/);
      assert.doesNotMatch(input, /data-key=/);
    }
  });
  check("everyday settings stay visible and experiment controls start collapsed", () => {
    assert.match(configMarkup, /Operating settings/);
    assert.match(configMarkup, /<details class="config-advanced"><summary>Strategy experiments and data sources/);
    assert.doesNotMatch(configMarkup, /<details[^>]*\bopen\b/);
    assert.match(configMarkup, /assumptions, not measured exchange execution/);
  });

  const sent = [];
  ctx2.POST = async (path, patch) => { sent.push({ path, patch: JSON.parse(JSON.stringify(patch)) }); return baseline; };
  ctx2.toast = () => {};
  ctx2.currentConfig = { ...baseline, initial_balance: 100 };
  vm.runInContext(extract(/^async function saveConfig\([\s\S]*?^\}/m, "config save action"), ctx2);
  await ctx2.saveConfig();
  assert.equal(sent.length, 0, "a changed read-only balance alone must not submit config");
  fields[0].value = "4";
  await ctx2.saveConfig();
  assert.deepEqual(sent, [{ path: "/api/config", patch: { circuit_breaker_losses: 4 } }]);
  checks.push("saving another setting never sends the read-only balance");
}


// ── The stats bar must not invent a 0% win rate ─────────────────────────────
//
// `wins ?? 0` divided by a real trade count rendered "0.0%" whenever the
// payload that ran last carried win_rate without wins — a confident claim
// from a missing field, beside a real total that made it look measured.
{
  const fnSrc = name => {
    const decl = new RegExp(`(?:^|\\n)function ${name}\\s*\\(`).exec(src);
    assert.ok(decl, `${name} not found`);
    let depth = 0, started = false, end = -1;
    for (let i = decl.index; i < src.length; i++) {
      if (src[i] === "{") { depth++; started = true; }
      else if (src[i] === "}") { depth--; if (started && depth === 0) { end = i; break; } }
    }
    return src.slice(decl.index, end + 1);
  };
  const mkEl = () => ({ textContent: "", className: "", style: {}, classList: { add() {}, remove() {}, toggle() {} } });
  const runBar = stats => {
    const els = {};
    const dom = { getElementById: id => (els[id] ||= mkEl()), querySelector: () => null, querySelectorAll: () => [] };
    const code = [
      arrow("fmtMoney"), arrow("fmtPct"), arrow("fmtN"), arrow("fmt\\$"), arrow("clsPnl"),
      fnSrc("updateStatsBar"),
      "globalThis.__run = s => updateStatsBar(s, 0);",
    ].join("\n");
    const c = vm.createContext({ document: dom, window: {}, Number, Math, String, Boolean });
    try { vm.runInContext(code, c); c.__run(stats); } catch (e) { assert.fail(`updateStatsBar threw: ${e.message}`); }
    return els["st-winrate"]?.textContent;
  };
  check("win rate comes from the rate when wins is absent, not from 0/total", () => {
    assert.equal(runBar({ closed_trades: 10, win_rate: 0.6 }), "60.0%");
  });
  check("win rate comes from wins when present", () => {
    assert.equal(runBar({ closed_trades: 10, wins: 7 }), "70.0%");
  });
  check("no wins and no rate is absent, not 0%", () => {
    assert.equal(runBar({ closed_trades: 10 }), DASH);
  });
  check('no closed trades means no observed win rate', () => {
    assert.equal(runBar({ closed_trades: 0, wins: 0, win_rate: 0 }), DASH);
  });
  check('open trades are not the win-rate denominator', () => {
    assert.equal(runBar({ total_trades: 15, closed_trades: 10, wins: 7 }), '70.0%');
  });
}

for (const name of checks) console.log(`  ${name}`);
console.log(`dashboard absent-values: ${checks.length} properties hold`);
