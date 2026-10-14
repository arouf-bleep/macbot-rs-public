#!/usr/bin/env node
// Guard against the dashboard reading analysis fields the server never emits.
//
// Two whole panels were dead for an unknown length of time because they read
// `a.extra.ml` and `a.extra.deep_model` while the payload carried `ml` and
// `deep_model` at the top level. Nothing failed: the guard clauses returned,
// the fields showed an em dash, and an em dash looks exactly like "no data
// yet". This check turns that silence into a build failure.
//
// The emitted set is read from the Rust source rather than a running server,
// so the check needs no process and no network.

import { readFileSync } from "node:fs";

const JS_SOURCES = ["static/dashboard.js"];
const RUST_SOURCES = [
  "src/analysis.rs",
  "src/bot/analyze.rs",
  "src/api/realtime.rs",
  "src/api/bot_routes.rs",
];

// Identifiers that are JavaScript, not analysis fields.
const NOISE = new Set([
  "length", "map", "filter", "forEach", "push", "slice", "join", "reduce",
  "some", "every", "find", "includes", "indexOf", "toFixed", "toString",
  "trim", "replace", "split", "keys", "entries", "values", "sort", "concat",
  "then", "catch", "finally", "call", "apply", "bind", "hasOwnProperty",
]);

// Only functions that actually receive the analysis object are checked. Other
// code also names its parameter `a` (the accounting-truth payload, for one),
// and flagging those would make this check noise rather than signal.
const ANALYSIS_CONSUMERS = [
  "updateSignalPanel",
  "updateWeights",
  "updateMlFromAnalysis",
  "updateDeepFromAnalysis",
  "updatePhase2Panels",
  "updateTapeMarket",
  "updateTapeOpenPositions",
  "updateTapeSignals",
  "updateTapeHealth",
];

// Fields the dashboard reads deliberately even though the analysis payload may
// omit them, because a sibling key covers the same value. Each entry must say
// why, so the allowlist cannot quietly become a dumping ground.
const ALLOWED_ABSENT = new Map([
  ["price", "YES price; market_data.price is the emitted path and is read first"],
  ["mins_left", "legacy alias in a ?? chain ending at time_remaining_minutes"],
  ["minutes_left", "legacy alias in a ?? chain ending at time_remaining_minutes"],
]);

const emitted = new Set();
for (const path of RUST_SOURCES) {
  let src;
  try {
    src = readFileSync(path, "utf8");
  } catch {
    continue;
  }
  // Keys appear as `"name": value` inside the json! macros that build the payload.
  for (const match of src.matchAll(/"([a-z_][a-z0-9_]*)"\s*:/gi)) {
    emitted.add(match[1]);
  }
}

if (emitted.size < 40) {
  console.error(
    `check_analysis_fields: only ${emitted.size} emitted keys found in the Rust sources. ` +
    "The payload builders have probably moved; update RUST_SOURCES rather than " +
    "letting this check pass vacuously."
  );
  process.exit(1);
}

const problems = [];
let readCount = 0;

// Extract one function body by brace matching from its declaration.
function functionBody(src, name) {
  const decl = new RegExp(`function\\s+${name}\\s*\\(`).exec(src);
  if (!decl) return null;
  const open = src.indexOf("{", decl.index);
  if (open === -1) return null;
  let depth = 0;
  for (let i = open; i < src.length; i++) {
    if (src[i] === "{") depth++;
    else if (src[i] === "}") {
      depth--;
      if (depth === 0) return { start: open, text: src.slice(open, i + 1) };
    }
  }
  return null;
}

const missingConsumers = [];
for (const path of JS_SOURCES) {
  const src = readFileSync(path, "utf8");
  for (const name of ANALYSIS_CONSUMERS) {
    const body = functionBody(src, name);
    if (!body) {
      missingConsumers.push(`${path}: ${name}`);
      continue;
    }
    const lineOffset = src.slice(0, body.start).split("\n").length;
    body.text.split("\n").forEach((line, index) => {
      // `a?.extra?.ml` must be caught as readily as `a.extra.ml`: the original
      // bug was written with optional chaining, and a check that misses the
      // shape it exists for is worse than no check.
      for (const match of line.matchAll(/\ba\??\.([a-z_][a-z0-9_]*)/gi)) {
        const key = match[1];
        if (NOISE.has(key)) continue;
        readCount++;
        if (emitted.has(key)) continue;
        if (ALLOWED_ABSENT.has(key)) continue;
        problems.push({ path, line: lineOffset + index, key, text: line.trim(), fn: name });
      }
    });
  }
}

if (missingConsumers.length > 0) {
  console.error(
    "check_analysis_fields: these analysis consumers no longer exist, so this " +
    "check is no longer covering them:\n  " + missingConsumers.join("\n  ") +
    "\nRename them in ANALYSIS_CONSUMERS rather than letting coverage lapse."
  );
  process.exit(1);
}

if (readCount === 0) {
  console.error(
    "check_analysis_fields: no analysis field reads found at all. The convention " +
    "of naming the analysis object `a` has probably changed; update this check " +
    "rather than letting it pass vacuously."
  );
  process.exit(1);
}

if (problems.length > 0) {
  console.error("check_analysis_fields: dashboard reads fields the payload never emits:\n");
  for (const p of problems) {
    console.error(`  ${p.path}:${p.line}  ${p.fn}() reads a.${p.key}`);
    console.error(`      ${p.text}`);
  }
  console.error(
    "\nEither the payload key changed name, or the read is dead. A dead read " +
    "renders as an em dash, which is indistinguishable from 'no data yet'."
  );
  process.exit(1);
}

// ── Stream envelope ─────────────────────────────────────────────────────────
// handleSSEUpdate branches on keys of the stream payload. Three of them
// (markets, no_arb, trades) branched on keys the envelope has never carried,
// so the branches were unreachable and the panels behind them were filled by
// their REST loaders instead. Same failure mode as the analysis reads: silent.

const ENVELOPE_SOURCE = "src/api/realtime.rs";
const envelopeSrc = readFileSync(ENVELOPE_SOURCE, "utf8");

// Only the envelope's own top-level keys count. Taking every quoted key in the
// file sweeps in the keys of nested objects, and then a branch on a nested
// name like `data.trades` looks valid when the envelope has no such key --
// which is exactly the bug this section exists to catch.
function topLevelKeysOfBlock(src, mustContain) {
  for (const start of [...src.matchAll(/json!\s*\(\s*\{/g)].map((m) => m.index)) {
    const open = src.indexOf("{", start);
    let depth = 0;
    let end = -1;
    for (let i = open; i < src.length; i++) {
      if (src[i] === "{") depth++;
      else if (src[i] === "}") {
        depth--;
        if (depth === 0) { end = i; break; }
      }
    }
    if (end === -1) continue;
    const block = src.slice(open, end + 1);
    if (!mustContain.every((k) => block.includes(`"${k}"`))) continue;

    // Walk the block and record quoted keys that sit at depth 1.
    const keys = new Set();
    depth = 0;
    for (let i = 0; i < block.length; i++) {
      const ch = block[i];
      if (ch === "{" || ch === "[") depth++;
      else if (ch === "}" || ch === "]") depth--;
      else if (ch === '"' && depth === 1) {
        const close = block.indexOf('"', i + 1);
        if (close === -1) break;
        const word = block.slice(i + 1, close);
        const after = block.slice(close + 1).match(/^\s*:/);
        if (after && /^[a-z_][a-z0-9_]*$/i.test(word)) keys.add(word);
        i = close;
      }
    }
    if (keys.size > 0) return keys;
  }
  return new Set();
}

const envelopeKeys = topLevelKeysOfBlock(envelopeSrc, ["analysis", "live_btc_price", "stats"]);

if (envelopeKeys.size < 15) {
  console.error(
    `check_analysis_fields: only ${envelopeKeys.size} envelope keys found in ` +
    `${ENVELOPE_SOURCE}. The builder has probably moved; update this check ` +
    "rather than letting it pass vacuously."
  );
  process.exit(1);
}

const handler = (() => {
  for (const path of JS_SOURCES) {
    const body = functionBody(readFileSync(path, "utf8"), "handleSSEUpdate");
    if (body) return { path, body, src: readFileSync(path, "utf8") };
  }
  return null;
})();

if (!handler) {
  console.error(
    "check_analysis_fields: handleSSEUpdate not found, so the stream envelope " +
    "is unchecked. Rename it here rather than letting coverage lapse."
  );
  process.exit(1);
}

const envelopeProblems = [];
let envelopeReads = 0;
const handlerLineOffset = handler.src.slice(0, handler.body.start).split("\n").length;
handler.body.text.split("\n").forEach((line, index) => {
  for (const match of line.matchAll(/\bdata\??\.([a-z_][a-z0-9_]*)/gi)) {
    const key = match[1];
    if (NOISE.has(key)) continue;
    envelopeReads++;
    if (envelopeKeys.has(key)) continue;
    envelopeProblems.push({
      line: handlerLineOffset + index,
      key,
      text: line.trim(),
    });
  }
});

if (envelopeReads === 0) {
  console.error(
    "check_analysis_fields: handleSSEUpdate reads no envelope fields, which " +
    "cannot be right. Update this check rather than letting it pass vacuously."
  );
  process.exit(1);
}

if (envelopeProblems.length > 0) {
  console.error("check_analysis_fields: handleSSEUpdate branches on keys the stream never sends:\n");
  for (const p of envelopeProblems) {
    console.error(`  ${handler.path}:${p.line}  data.${p.key}`);
    console.error(`      ${p.text}`);
  }
  console.error("\nAn unreachable branch is not a safe fallback; it is dead weight.");
  process.exit(1);
}

// ── A legacy alias is only safe if the chain reaches the real key ──────────
//
// ALLOWED_ABSENT excuses `mins_left` and `minutes_left` because they sit in a
// chain ending at `time_remaining_minutes`. That was true of two read sites and
// false of a third, which read the two aliases alone and so rendered an em dash
// while the figure sat in the payload. The excuse has to be checked, not taken
// on trust.
{
  const jsSource = readFileSync(JS_SOURCES[0], "utf8");
  const offenders = [];
  jsSource.split("\n").forEach((line, i) => {
    if (!/\ba\.mins_left\b|\ba\.minutes_left\b/.test(line)) return;
    if (line.includes("time_remaining_minutes")) return;
    offenders.push(`${i + 1}: ${line.trim()}`);
  });
  if (offenders.length > 0) {
    console.error(
      "check_analysis_fields: these read the legacy mins aliases without falling " +
      "back to time_remaining_minutes, which is the key the server actually " +
      `sends, so they render as absent while the value exists:\n  ${offenders.join("\n  ")}`
    );
    process.exit(1);
  }
}

// ── The consumer list must not silently fall behind the code ───────────────
//
// ANALYSIS_CONSUMERS is hand-maintained, so a panel added outside it was never
// checked and nothing failed — the same silence this script exists to end,
// reproduced one level up. Two assertions close it: every listed consumer must
// still exist, and every function that takes the analysis object must be
// listed.
{
  const jsSource = readFileSync(JS_SOURCES[0], "utf8");
  const declared = new Set(
    [...jsSource.matchAll(/function\s+([A-Za-z_$][\w$]*)\s*\(/g)].map(m => m[1])
  );
  const stale = ANALYSIS_CONSUMERS.filter(name => !declared.has(name));
  if (stale.length > 0) {
    console.error(
      "check_analysis_fields: these consumers no longer exist, so their fields " +
      `are unchecked: ${stale.join(", ")}`
    );
    process.exit(1);
  }

  // The analysis object is conventionally the parameter named `a`. Anything
  // with that signature is a consumer whether or not somebody remembered.
  const byConvention = new Set(
    [...jsSource.matchAll(/function\s+([A-Za-z_$][\w$]*)\s*\(\s*a\s*[,)]/g)].map(m => m[1])
  );
  const unlisted = [...byConvention].filter(name => !ANALYSIS_CONSUMERS.includes(name));
  if (unlisted.length > 0) {
    console.error(
      "check_analysis_fields: these functions take the analysis object but are " +
      `not in ANALYSIS_CONSUMERS, so their field reads are unchecked:\n  ${unlisted.join("\n  ")}\n\n` +
      "Add them to the list, or rename the parameter if it is not the analysis."
    );
    process.exit(1);
  }
}

console.log(
  `static/dashboard.js: ${readCount} analysis field reads checked against ` +
  `${emitted.size} emitted keys, and ${envelopeReads} stream field reads ` +
  `against ${envelopeKeys.size} envelope keys, all resolve ` +
  `(${ANALYSIS_CONSUMERS.length} consumers, none unlisted)`
);
