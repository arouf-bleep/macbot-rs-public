import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';

const source = readFileSync(new URL('../../static/dashboard.js', import.meta.url), 'utf8');
function extract(pattern, label) {
  const match = source.match(pattern);
  assert.ok(match, `${label} must exist in shipped dashboard source`);
  return match[0];
}
const escape = extract(/^const ledgerEsc = [\s\S]*?;$/m, 'HTML escaping');
const address = extract(/^const API_BASE = [\s\S]*?^\}\)\(\);/m, 'API origin binding');
const apiUrl = extract(/^function apiUrl\(path\) \{[\s\S]*?^\}/m, 'API URL resolution');
function addressContext(href) {
  const url = new URL(href);
  const context = vm.createContext({ URL, URLSearchParams, window: { location: url } });
  vm.runInContext(`${address}\n${apiUrl}`, context);
  return context;
}
const sameOrigin = addressContext('http://127.0.0.1:18081/');
assert.equal(sameOrigin.apiUrl('/api/config'), 'http://127.0.0.1:18081/api/config');
const poisoned = addressContext('http://127.0.0.1:18081/?api=https%3A%2F%2Fattacker.invalid');
assert.equal(poisoned.apiUrl('/api/config'), 'http://127.0.0.1:18081/api/config');
assert.throws(() => poisoned.apiUrl('https://attacker.invalid/api/config'), /origin/);
assert.throws(() => poisoned.apiUrl('//attacker.invalid/api/config'), /origin/);

let toastMarkup = '';
let configMarkup = '';
const node = { addEventListener() {}, remove() {} };
const context = vm.createContext({
  Date,
  document: {
    createElement: () => ({
      set innerHTML(value) { toastMarkup = value; },
      firstElementChild: node,
    }),
    getElementById: id => id === 'config-panel'
      ? { set innerHTML(value) { configMarkup = value; } }
      : { appendChild() {}, addEventListener() {} },
  },
  bootstrap: { Toast: function () { this.show = () => {}; } },
});
const toast = extract(/^function toast\([\s\S]*?^\}/m, 'toast renderer');
const renderConfig = extract(/^function renderConfigPanel\([\s\S]*?^\}/m, 'config renderer');
const getNested = extract(/^function getNestedVal\([\s\S]*?^\}/m, 'config reader');
vm.runInContext(`${escape}\n${toast}\n${getNested}\n${renderConfig}`, context);
const payload = '\"><img src=x onerror="steal()">';
context.toast(payload, 'danger');
assert.ok(toastMarkup.includes('&lt;img'));
assert.ok(!toastMarkup.includes('<img'));

// A notification must own its actual newly appended node, even if another toast
// arrives in the same millisecond. Match Bootstrap's strong instance registry:
// removing DOM alone does not release its retained element and event handlers.
const retainedToasts = new Map();
const renderedToasts = [];
const cleanupOrder = [];
const toastsContext = vm.createContext({
  Date: class extends Date { static now() { return 1234; } },
  document: {
    createElement() {
      const listeners = new Map();
      const element = {
        addEventListener(name, callback, options) { listeners.set(name, {callback, options}); },
        remove() { cleanupOrder.push('remove'); this.removed = true; },
        dispatchHidden() {
          const listener = listeners.get('hidden.bs.toast');
          if (!listener) return;
          if (listener.options?.once) listeners.delete('hidden.bs.toast');
          listener.callback();
        },
      };
      return { set innerHTML(_value) {}, firstElementChild: element };
    },
    getElementById(id) {
      assert.equal(id, 'toast-container', 'renderer must not resolve colliding time-based IDs');
      return { appendChild(element) { renderedToasts.push(element); } };
    },
  },
  bootstrap: { Toast: function(element) {
    assert.ok(!retainedToasts.has(element), 'each notification needs its own instance');
    retainedToasts.set(element, this);
    this.show = () => { this.shown = true; };
    this.dispose = () => { cleanupOrder.push('dispose'); retainedToasts.delete(element); };
  } },
});
vm.runInContext(`${escape}\n${toast}`, toastsContext);
toastsContext.toast('first');
toastsContext.toast('second');
assert.equal(renderedToasts.length, 2);
assert.notEqual(renderedToasts[0], renderedToasts[1]);
assert.equal(retainedToasts.size, 2);
for (const element of renderedToasts) { element.dispatchHidden(); element.dispatchHidden(); }
assert.equal(retainedToasts.size, 0, 'hidden toasts must leave no Bootstrap retained instance');
assert.deepEqual(cleanupOrder, ['dispose', 'remove', 'dispose', 'remove']);
assert.ok(renderedToasts.every(element => element.removed));
context.renderConfigPanel({ signal_weights_path: payload, macro_event_calendar_path: payload });
assert.ok(configMarkup.includes('value="&quot;&gt;&lt;img'));
assert.ok(!configMarkup.includes('<img'));

const fills = extract(/^function renderPartialFillsMarkup\([\s\S]*?^\}/m, 'fill renderer');
const fillsContext = vm.createContext({
  fmtN: value => Number(value).toFixed(4), tsShort: value => value,
  describeFillDetails: fill => fill.details,
});
vm.runInContext(`${escape}\n${fills}`, fillsContext);
const markup = fillsContext.renderPartialFillsMarkup([{ phase: payload, avg_price: 0.5, delta_size: 5, details: payload }]);
assert.ok(markup.includes('&lt;img'));
assert.ok(!markup.includes('<img'));
const zeroFill = fillsContext.renderPartialFillsMarkup([{phase:'exit',avg_price:0,intended_price:0.9,delta_size:2,cumulative_size:8}]);
assert.match(zeroFill, />0\.0000</);
assert.match(zeroFill, /2\.0000 shares/);
assert.doesNotMatch(zeroFill, /0\.9000|8\.0000 shares/);
assert.match(fillsContext.renderPartialFillsMarkup([]), /No fill evidence/);

const collapsed = extract(/^function readCollapsedSignalGroups\([\s\S]*?^\}/m, 'stored preferences');
const readBoundary = extract(/^function readSessionBoundaryFromStorage\([\s\S]*?^\}/m, 'stored session boundary');
const writeBoundary = extract(/^function writeSessionBoundaryToStorage\([\s\S]*?^\}/m, 'session persistence');
const denied = vm.createContext({Set,localStorage:{getItem(){throw new Error('blocked');},setItem(){throw new Error('blocked');}},
  normalizeSessionStartId:value=>Number(value)||0,normalizeSessionStartAt:value=>value||'',SESSION_START_KEY:'id',SESSION_START_AT_KEY:'at'});
vm.runInContext(`${collapsed}\n${readBoundary}\n${writeBoundary}`,denied);
assert.ok(denied.readCollapsedSignalGroups().has('MICRO'));
assert.equal(denied.readSessionBoundaryFromStorage().tradeId,0);
assert.doesNotThrow(()=>denied.writeSessionBoundaryToStorage({tradeId:3,startedAt:'today'}));

const execute = extract(/^async function executeCurrentTrade\([\s\S]*?^\}/m,'execution response handling');
let result={status:'not_executed',reason:'risk guard'},lastToast,loaded=0;
const actions=vm.createContext({Number,confirm:()=>true,POST:async()=>result,toast:(message,tone)=>{lastToast={message,tone};},
  loadTrades:async()=>{loaded++;},fmtN:String});
vm.runInContext(`${extract(/^function isPendingEntry\([\s\S]*?^\}/m,'pending entry classification')}\n${execute}`,actions);
await actions.executeCurrentTrade();
assert.match(lastToast.message,/No trade opened/);assert.equal(lastToast.tone,'warning');assert.equal(loaded,0);
result={};await actions.executeCurrentTrade();assert.match(lastToast.message,/unknown/);assert.equal(loaded,0);
for (const pending of [
  {status:'pending',execution_status:'pending',reason:'Awaiting confirmed fills',trade:{id:2,shares:0}},
  {id:2,status:'OPEN',mode:'live',shares:0,order_id:'accepted-buy',entry_price:0.5},
]) {
  result=pending;await actions.executeCurrentTrade();assert.equal(lastToast.tone,'warning');
  assert.match(lastToast.message,/Entry needs confirmation/);assert.doesNotMatch(lastToast.message,/executed/);
}
assert.equal(loaded,2,'pending results refresh the visible durable ledger');
result={id:4,status:'OPEN',direction:'BUY_YES',entry_price:.4};await actions.executeCurrentTrade();assert.equal(lastToast.tone,'success');assert.equal(loaded,3);

console.log('Dashboard same-origin credentials and hostile text rendering tests passed.');
