#!/usr/bin/env node
// Execute production renderers: an uncertain sell must remain visible,
// with no fresh P&L claim and no second Close button.
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import assert from 'node:assert/strict';

const source = readFileSync('static/dashboard.js', 'utf8');
const extract = name => {
  const match = source.match(new RegExp(`function ${name}\\([\\s\\S]*?\\n}`));
  assert.ok(match, `missing production renderer ${name}`);
  return match[0];
};
const rows = { innerHTML: '' };
const tape = { innerHTML: '' };
const count = { textContent: '' };
const history = {innerHTML: '', dataset: {}};
const context = vm.createContext({
  Map, String, Number, Array,
  document: {
    getElementById: id => id === 'open-trades-tbody' ? rows : id === 'history-tbody' ? history : count,
    querySelector: () => tape,
  },
});
vm.runInContext(`
  let allTrades = [];
  const lastRenderedLiveMid = new Map();
  let freshMid = 0.8;
  function liveMidFor() { return freshMid; }
  function fmtN(value) { return value == null ? '—' : String(value); }
  const fmtMoney = fmtN, fmt$ = fmtN, ledgerUsd = fmtN, fmtPct = fmtN;
  const expandedTradeIds = new Set(), partialFillsCache = new Map();
  function getHistoryViewState() { return {filter:'all',stratFilter:'all',sorted:allTrades}; }
  function computeHistoryRenderKey() { return JSON.stringify(allTrades); }
  function prefetchPartialFills() {}
  function tsShort(value) { return value || '—'; }
  function clsPnl() { return ''; }
  function ledgerEsc(value) { return String(value); }
  function tapeWindowOf() { return 'market'; }
  ${extract('isPendingEntry')}
  ${extract('entryPendingLabel')}
  ${extract('renderOpenTrades')}
  ${extract('renderHistory')}
  ${extract('updateTapeOpenPositions')}
  globalThis.render = (status, partialState = null) => {
    allTrades = [{id: 42, status, market_id: 'market', token_id: 'token',
      partial_close_state: partialState,
      direction: 'BUY_YES', entry_price: 0.5, shares: 5, amount: 2.5,
      live_price: 0.9, unrealized_pnl: 12345}];
    renderOpenTrades();
    renderHistory();
    updateTapeOpenPositions({price: 0.8, market_data: {id: 'market'}});
  };
  globalThis.renderTrade = trade => { allTrades = [trade]; renderOpenTrades(); renderHistory(); updateTapeOpenPositions({}); };
  globalThis.expire = () => { freshMid = null; globalThis.render('OPEN'); };
`, context);

context.render('CLOSING');
assert.equal(count.textContent, 1);
assert.match(rows.innerHTML, /Exit needs reconciliation/);
assert.doesNotMatch(rows.innerHTML, /closeTrade\(/);
assert.doesNotMatch(rows.innerHTML, /12345/);
assert.match(tape.innerHTML, /Exit needs venue reconciliation/);
assert.doesNotMatch(tape.innerHTML, /No open positions/);
context.render('OPEN');
assert.match(rows.innerHTML, /closeTrade\(42\)/);
assert.doesNotMatch(tape.innerHTML, /reconciliation/);
assert.doesNotMatch(rows.innerHTML, /12345|>0\.9</);
assert.match(rows.innerHTML, /1\.5000000000000002|1\.5/);
assert.doesNotMatch(tape.innerHTML, /12345|>0\.9</);
for (const state of ['in_flight', 'unknown']) {
  context.render('OPEN', state);
  assert.match(rows.innerHTML, /Partial sale awaiting confirmed outcome/);
  assert.match(tape.innerHTML, /Partial sale awaiting confirmed outcome/);
  assert.doesNotMatch(rows.innerHTML, /closeTrade\(|12345|1\.5|>0\.9</);
  assert.doesNotMatch(tape.innerHTML, /12345|1\.5|>0\.9</);
  assert.equal(count.textContent, 1, 'a partial intent still retains exposure');
}
context.expire();
assert.doesNotMatch(rows.innerHTML, /12345|1\.5|>0\.9</);
assert.doesNotMatch(tape.innerHTML, /12345|1\.5|>0\.9</);
const unresolvedBuy = {id:43,mode:'live',status:'OPEN',direction:'BUY_YES',token_id:'token',
  entry_price:0.5,amount:5,shares:0,order_id:'durable-buy',entry_execution_status:'pending'};
for (const status of ['pending','audit_uncertain','unavailable']) {
  context.renderTrade({...unresolvedBuy,entry_execution_status:status});
  assert.match(rows.innerHTML, /Entry (awaiting confirmation|evidence needs revalidation|confirmation unavailable)/);
  assert.match(tape.innerHTML, /Entry (awaiting confirmation|evidence needs revalidation|confirmation unavailable)/);
  assert.match(history.innerHTML, /ENTRY PENDING/);
  assert.doesNotMatch(rows.innerHTML, /closeTrade\(|>0\.5<|>0\.8<|>3<|1\.5/);
  assert.doesNotMatch(tape.innerHTML, />0\.8<|>3</);
  assert.doesNotMatch(history.innerHTML, /closeTrade\(/);
}
context.renderTrade({...unresolvedBuy,entry_execution_status:undefined});
assert.match(rows.innerHTML,/Entry confirmation unavailable/,'old cached zero-share BUY stays conservative');
// A partial confirmed BUY still has no final entry outcome; its visible
// recorded quantity must not make the unresolved remainder look settled.
context.renderTrade({...unresolvedBuy,shares:2});
assert.match(rows.innerHTML,/Entry awaiting confirmation/);
assert.doesNotMatch(rows.innerHTML,/closeTrade\(|>0\.6</);
// OPEN + retained order ID is also possible after a rejected SELL. Neither
// that lifecycle nor a simulated order is a pending BUY without entry proof.
context.renderTrade({...unresolvedBuy,shares:5,entry_execution_status:undefined});
assert.match(rows.innerHTML,/closeTrade\(43\)/);
assert.doesNotMatch(rows.innerHTML,/Entry awaiting|ENTRY PENDING/);
context.renderTrade({...unresolvedBuy,mode:'simulation',shares:5,entry_execution_status:undefined});
assert.match(rows.innerHTML,/closeTrade\(43\)/);
// Unknown/missing shares must not acquire imaginary inventory from budget.
context.renderTrade({...unresolvedBuy,mode:'simulation',shares:undefined,entry_execution_status:undefined});
assert.doesNotMatch(rows.innerHTML,/>3</);
console.log('dashboard unresolved entries/exits: truthful quantities, lifecycle labels and no resubmission');
