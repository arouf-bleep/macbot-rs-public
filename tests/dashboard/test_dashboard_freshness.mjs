#!/usr/bin/env node
// Two properties this dashboard has to keep.
//
// 1. A panel whose data goes missing blanks its fields instead of leaving the
//    previous reading on screen. A stale number drawn as a current one is the
//    failure mode that made the whole dashboard feel untrustworthy.
// 2. The age of the analysis keeps advancing between payloads, so a page that
//    has stopped receiving visibly keeps ageing rather than freezing at
//    whatever the last tick said.

import { readFileSync } from "node:fs";
import vm from "node:vm";
import assert from "node:assert/strict";

const src = readFileSync("static/dashboard.js", "utf8");

// ── A tiny DOM, enough for the functions under test ─────────────────────────
function makeDom(ids) {
  const els = new Map();
  for (const id of ids) {
    els.set(id, {
      id,
      tagName: "SPAN",
      textContent: "STALE-VALUE",
      style: { width: "77%" },
      className: "",
      classList: { add() {}, remove() {}, toggle() {} },
    });
  }
  return {
    getElementById: (id) => els.get(id) ?? null,
    querySelector: () => null,
    querySelectorAll: () => [],
    els,
  };
}

function loadFns(names, dom) {
  // Pull the named functions plus their helpers out of the module and evaluate
  // them against a stub document. Extracting by brace matching keeps this
  // honest: if a function is renamed the extraction fails rather than silently
  // testing nothing.
  const wanted = [...names, "blankFields", "fmtAge", "currentAnalysisAge", "noteAnalysisFreshness"];
  let code = "const AGE_UNKNOWN = -1;\nlet lastAnalysisFreshness = null;\n";
  for (const decl of ["ML_FIELDS", "DEEP_FIELDS", "LIQ_FIELDS", "NO_ARB_FIELDS"]) {
    const m = new RegExp(`const ${decl} = \\[[\\s\\S]*?\\];`).exec(src);
    assert.ok(m, `${decl} not found — the blanking field lists have moved`);
    code += m[0] + "\n";
  }
  for (const name of wanted) {
    const decl = new RegExp(`(?:^|\\n)function ${name}\\s*\\(`).exec(src);
    assert.ok(decl, `function ${name} not found — rename it here rather than losing coverage`);
    const open = src.indexOf("{", decl.index);
    let depth = 0;
    let end = -1;
    for (let i = open; i < src.length; i++) {
      if (src[i] === "{") depth++;
      else if (src[i] === "}") {
        depth--;
        if (depth === 0) { end = i; break; }
      }
    }
    assert.ok(end > 0, `could not extract ${name}`);
    code += src.slice(decl.index, end + 1) + "\n";
  }
  const ctx = { document: dom, Date, Math, console };
  ctx.globalThis = ctx;
  vm.createContext(ctx);
  vm.runInContext(code + "\n;({" + wanted.join(",") + "});", ctx);
  return vm.runInContext("({" + wanted.join(",") + ", get lastAnalysisFreshness(){return lastAnalysisFreshness}})", ctx);
}

// ── 1. Missing data blanks the panel ────────────────────────────────────────
{
  const cases = [
    ["updateMlFromAnalysis", ["ml-p-up", "ml-fusion-p-up"], undefined],
    ["updateDeepFromAnalysis", ["deep-p-up", "deep-blended", "deep-mc-unc", "deep-skip"], undefined],
    ["updateLiquidationPanel", ["liq-30s-long", "liq-5m-ratio"], undefined],
    ["updateNoArbSummary", ["no-arb-summary", "no-arb-max"], undefined],
  ];
  for (const [fn, ids, arg] of cases) {
    const dom = makeDom(ids);
    const mod = loadFns([fn], dom);
    mod[fn](arg);
    for (const id of ids) {
      assert.equal(
        dom.getElementById(id).textContent,
        "—",
        `${fn} left a stale value in ${id} when its data was missing`
      );
    }
  }
  console.log("  missing data blanks its panel: 4 panels checked");
}

// ── 2. A bar blanks by width, not by text ───────────────────────────────────
{
  const dom = makeDom(["liq-ratio-bar", "liq-30s-long"]);
  const mod = loadFns(["updateLiquidationPanel"], dom);
  mod.updateLiquidationPanel(undefined);
  assert.equal(dom.getElementById("liq-ratio-bar").style.width, "0%",
    "a bar must collapse rather than keep its old width");
  console.log("  a bar collapses instead of holding its last width");
}

// ── 3. Age advances between payloads ────────────────────────────────────────
{
  const dom = makeDom([]);
  const mod = loadFns([], dom);
  mod.noteAnalysisFreshness({ age_secs: 10, max_age_secs: 45, never_run: false, interval_secs: 60 });
  const first = mod.currentAnalysisAge();
  assert.ok(first >= 10 && first < 11, `expected ~10s, got ${first}`);

  const realNow = Date.now;
  try {
    Date.now = () => realNow() + 7000;
    const later = mod.currentAnalysisAge();
    assert.ok(
      later >= 17 && later < 18,
      `age must advance with wall clock between payloads; expected ~17s, got ${later}`
    );
  } finally {
    Date.now = realNow;
  }
  console.log("  analysis age advances between payloads");
}

// ── 4. Never-run and unknown ages do not render as zero ─────────────────────
{
  const dom = makeDom([]);
  const mod = loadFns([], dom);
  mod.noteAnalysisFreshness({ age_secs: null, max_age_secs: 45, never_run: true, interval_secs: 60 });
  assert.equal(mod.currentAnalysisAge(), -1,
    "a never-run analysis must report unknown, not an age of zero");
  assert.equal(mod.fmtAge(-1), "", "unknown age must render as nothing, not '0s'");
  console.log("  a never-run analysis reports unknown rather than zero");
}

// ── 5. Every tab has a loader, and the fallback uses the same map ───────────
{
  const html = readFileSync("static/index.html", "utf8");
  const tabs = [...html.matchAll(/data-tab="([a-z]+)"/g)].map((m) => m[1]);
  assert.ok(tabs.length >= 5, `expected the tab bar to have tabs, found ${tabs.length}`);

  const mapBody = /const TAB_LOADERS = \{([\s\S]*?)\n\};/.exec(src);
  assert.ok(mapBody, "TAB_LOADERS not found — the per-tab loader map has moved");
  const covered = [...mapBody[1].matchAll(/^\s*([a-z]+)\s*:/gm)].map((m) => m[1]);

  const missing = tabs.filter((t) => !covered.includes(t));
  assert.deepEqual(
    missing, [],
    `these tabs refresh nothing when the stream is down: ${missing.join(", ")}`
  );

  // The fallback must delegate rather than keep its own list, or the two drift
  // apart exactly as they did before.
  const fallback = /Fallback: when the stream[\s\S]*?\}, 10_000\);/.exec(src);
  assert.ok(fallback, "stream-down fallback not found");
  assert.ok(
    /refreshActiveTab\(\)/.test(fallback[0]),
    "the fallback must call refreshActiveTab, not enumerate tabs itself"
  );
  assert.ok(
    !/activeTab === '/.test(fallback[0]),
    "the fallback names specific tabs again, which is how five of them were missed"
  );
  console.log(`  every tab refreshes when the stream is down: ${tabs.length} tabs covered`);
}

// ── 6. A flag badge never says "mismatch" ──────────────────────────────────
{
  // statusBadge exists for execution parity, where "mismatch" is a real
  // outcome. Reused for risk flags it printed that word beside the kill
  // switch, which reads as "the dashboard and the bot disagree" when the
  // switch was simply on.
  const dom = makeDom([]);
  const mod = loadFns(["flagBadge"], dom);
  for (const [bad, badWord, goodWord] of [[true, "engaged", "clear"], [false, "engaged", "clear"]]) {
    const html = mod.flagBadge(bad, badWord, goodWord);
    assert.ok(
      !/mismatch<|>mismatch/.test(html),
      `flagBadge emitted the word "mismatch" as its label: ${html}`
    );
    assert.ok(html.includes(bad ? badWord : goodWord), `flagBadge did not use the given word: ${html}`);
  }
  assert.ok(
    !/statusBadge\((?:r|macroGuard)[^)]*\?\s*'mismatch'/.test(src),
    "a risk flag is rendering through statusBadge('mismatch') again"
  );
  console.log("  risk flags say their state, not the word \"mismatch\"");
}

// ── 7. Halted trading outranks the loop flag in the control summary ────────
{
  // A running loop with the kill switch engaged is not trading. The summary
  // read "Loop running" in green while every entry was blocked.
  const block = /const controlBits = \[\][\s\S]*?controlSummaryEl\.className[\s\S]*?;/.exec(src);
  assert.ok(block, "control summary block not found");
  assert.ok(
    /killSwitchState/.test(block[0]),
    "the control summary ignores the kill switch, so it can report running while halted"
  );
  assert.ok(
    /Halted by kill switch/.test(block[0]),
    "the control summary must name the halt rather than only colour it"
  );
  console.log("  the control summary reports halted trading");
}

// ── 8. Durations are readable at their magnitude ───────────────────────────
{
  const dom = makeDom([]);
  const mod = loadFns(["fmtMinutesUntil"], dom);
  const cases = [
    [2564.2, /^42h 44m$/], // was "2564.2m", which is 42 hours 44 minutes
    [0.5, /30s/],
    [45, /45m/],
    [4000, /2d/],
    [-12, /^\+12m$/],      // an event that has passed
  ];
  for (const [mins, expect] of cases) {
    const got = mod.fmtMinutesUntil(mins);
    assert.match(got, expect, `fmtMinutesUntil(${mins}) gave "${got}"`);
  }
  assert.equal(mod.fmtMinutesUntil(null), "\u2014", "a missing duration must render as a dash");
  console.log("  durations read at their own magnitude");
}

// ── 9. No switch in the config form is ignored by the backend ──────────────
{
  const rust = readFileSync("src/config.rs", "utf8");
  const declared = new Set(
    [...rust.matchAll(/^\s*pub ([a-z_][a-z0-9_]*):/gm)].map((m) => m[1])
  );
  const offered = [...src.matchAll(/\{\s*key:\s*'([a-z_]+)'/g)].map((m) => m[1]);
  assert.ok(offered.length > 20, `expected a populated config form, found ${offered.length} keys`);
  const ghosts = offered.filter((k) => !declared.has(k));
  assert.deepEqual(
    ghosts, [],
    `the config form offers switches the Config struct does not have: ${ghosts.join(", ")}`
  );
  console.log(`  every config control maps to a real field: ${offered.length} checked`);
}

console.log("dashboard freshness: all properties hold");
