#!/usr/bin/env node
// Exercise the actual dashboard cache, SSE handler, and expiry timer with
// synthetic books and a deterministic clock. No server or exchange required.
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import assert from 'node:assert/strict';

const src = readFileSync('static/dashboard.js', 'utf8');
const names = ['expireLiveBook', 'noteLiveBook', 'liveMidFor', 'liveOddsFor',
  'refreshOpenPositionsIfBookMoved', 'handleSSEUpdate', 'renderDataStatus', 'updateTapeMarket'];
const functions = names.map(name => {
  const match = src.match(new RegExp(`function ${name}\\([\\s\\S]*?\\n}`));
  assert.ok(match, `${name} missing; this regression check must execute production code`);
  return match[0];
});
const epoch = Date.parse('2026-09-18T00:00:00Z');
let now = epoch;
class TestDate extends Date { static now() { return now; } }
const context = vm.createContext({ Date: TestDate, Map, Number, String, Array, Boolean,
  document: { getElementById: () => null }, window: { _lastAnalysis: {} },
});
vm.runInContext(`
  const LIVE_BOOK_MAX_AGE_MS = 5000;
  const liveBook = new Map();
  const lastRenderedLiveMid = new Map();
  let allTrades = [{id: 1, status: 'OPEN', token_id: 'yes'}];
  let lastSseAt = 0;
  let makerHandsOffMarkets = [], makerVenueRefusal = null;
  let rowsRendered = 0, oddsRendered = 0;
  const tapeFields = {};
  function tapeText(id, value) { tapeFields[id] = value; }
  function tapeWindowOf() { return ''; }
  function shouldPollReadLoop() { return false; }
  function renderOpenTrades() {
    rowsRendered++;
    lastRenderedLiveMid.set(1, liveMidFor('yes'));
  }
  function updateSignalPanel() { oddsRendered++; }
  function updateRiskFromSSE() {}
  function renderMakerState() {}
  function updateDashboardStrip() {}
  ${functions.join('\n')}
  globalThis.api = { noteLiveBook, liveMidFor, liveOddsFor, liveBook, lastRenderedLiveMid,
    handleSSEUpdate, renderDataStatus, updateTapeMarket, tapeFields,
    reset() { liveBook.clear(); lastRenderedLiveMid.clear(); rowsRendered = 0; oddsRendered = 0; },
    counts() { return { rowsRendered, oddsRendered }; }
  };
`, context);
const api = context.api;
const book = overrides => ({ token_id: 'yes', market_id: 'm', is_yes_token: true,
  best_bid: 0.70, best_ask: 0.80, stale: false, snapshot_loaded: true,
  updated_at: new Date(now).toISOString(), ...overrides });
let checks = 0;
const check = (name, run) => { now = epoch; api.reset(); run(); checks++; console.log(`  ${name}`); };

check('a fresh two-sided book supplies one midpoint for the token and odds', () => {
  api.noteLiveBook([book()]);
  assert.equal(api.liveMidFor('yes'), 0.75);
  assert.equal(api.liveOddsFor('m').yes, 0.75);
  assert.equal(api.liveOddsFor('m').no, null, 'one token cannot establish the other token price');
  api.noteLiveBook([book({token_id:'no',is_yes_token:false,best_bid:0.30,best_ask:0.40})]);
  assert.ok(Math.abs(api.liveOddsFor('m').no - 0.35) < 1e-12, 'NO reads its own book, not the YES complement');
});
check('absent, nonnumeric, and invalid book sides never invent a live midpoint', () => {
  for (const bad of [null, undefined, '', ' ', false, [], {}, NaN, Infinity, -1, 2]) {
    api.noteLiveBook([book()]);
    api.noteLiveBook([book({ best_bid: bad })]);
    assert.equal(api.liveMidFor('yes'), null, `accepted invalid bid ${String(bad)}`);
    api.noteLiveBook([book()]);
    api.noteLiveBook([book({ best_ask: bad })]);
    assert.equal(api.liveMidFor('yes'), null, `accepted invalid ask ${String(bad)}`);
  }
  api.noteLiveBook([book({ best_bid: null, best_ask: null })]);
  assert.equal(api.liveOddsFor('m'), null);
});
check('numeric zero is distinguished from a missing side', () => {
  api.noteLiveBook([book({ best_bid: 0, best_ask: 0.02 })]);
  assert.equal(api.liveMidFor('yes'), 0.01);
});
check('a repeated old snapshot cannot renew source freshness', () => {
  const initial = book();
  api.noteLiveBook([initial]);
  now += 4900;
  api.noteLiveBook([initial]);
  now += 200;
  assert.equal(api.liveMidFor('yes'), null);
  api.noteLiveBook([initial]);
  assert.equal(api.liveOddsFor('m'), null);
});
check('missing, invalid, and future source timestamps cannot claim freshness', () => {
  for (const updated_at of [null, undefined, '', 'not-a-date', new Date(epoch + 1000).toISOString()]) {
    api.noteLiveBook([book()]);
    api.noteLiveBook([book({ updated_at })]);
    assert.equal(api.liveMidFor('yes'), null);
  }
});
check('stale or unloaded tokens immediately invalidate their previous mark', () => {
  for (const invalid of [{ stale: true }, { snapshot_loaded: false }]) {
    api.noteLiveBook([book()]);
    api.noteLiveBook([book(invalid)]);
    assert.equal(api.liveMidFor('yes'), null);
  }
});
check('an unknown outcome never masquerades as NO', () => {
  api.noteLiveBook([book({ is_yes_token: null })]);
  assert.equal(api.liveMidFor('yes'), 0.75);
  assert.equal(api.liveOddsFor('m'), null);
});
check('each SSE global stale flag clears cached marks even without a maker payload', () => {
  for (const data of [{ l2_feed_stale: true }, { risk: { l2_feed_stale: true } },
    { feed_truth: { l2_feed_stale: true } }, { maker: { l2_stale: true, l2_top_of_book: [book()] } }]) {
    api.noteLiveBook([book()]);
    api.lastRenderedLiveMid.set(1, 0.75);
    api.handleSSEUpdate(data);
    assert.equal(api.liveMidFor('yes'), null);
    assert.equal(api.lastRenderedLiveMid.get(1), null, 'position renderer did not fall back');
  }
  assert.equal(api.counts().oddsRendered, 4, 'odds did not repaint on stale-only frames');
});
check('the freshness ticker expires and repaints marks even with no arriving frames', () => {
  api.noteLiveBook([book()]);
  api.lastRenderedLiveMid.set(1, 0.75);
  now += 5001;
  api.renderDataStatus();
  assert.equal(api.liveMidFor('yes'), null);
  assert.equal(api.counts().rowsRendered, 1);
  assert.equal(api.counts().oddsRendered, 1);
  api.renderDataStatus();
  assert.equal(api.counts().rowsRendered, 1, 'unchanged absent book rerendered');
});
check('a fresh source after recovery restores the live price', () => {
  api.handleSSEUpdate({ l2_feed_stale: true });
  now += 6000;
  api.handleSSEUpdate({ l2_feed_stale: false, maker: { l2_stale: false, l2_top_of_book: [book()] } });
  assert.equal(api.liveMidFor('yes'), 0.75);
});
check('the Tape displays each token book without manufacturing a complementary quote', () => {
  const analysis = { price: 0.60, market_data: { market_id: 'm' } };
  api.updateTapeMarket(analysis);
  assert.equal(api.tapeFields['tape-odds'], '0.60 / —');
  api.noteLiveBook([book()]);
  api.updateTapeMarket(analysis);
  assert.equal(api.tapeFields['tape-odds'], '0.75 / —');
  api.noteLiveBook([book({token_id:'no',is_yes_token:false,best_bid:0.30,best_ask:0.40})]);
  api.updateTapeMarket(analysis);
  assert.equal(api.tapeFields['tape-odds'], '0.75 / 0.35');
});
console.log(`dashboard live-book: ${checks} properties hold`);
