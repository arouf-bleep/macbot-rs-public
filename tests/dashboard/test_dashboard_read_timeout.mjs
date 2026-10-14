// A broken read must release the dedupe slot and let a later read recover.
// Writes must not acquire a timeout that could suggest an unsafe retry.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';

const source = readFileSync(new URL('../../static/dashboard.js', import.meta.url), 'utf8');
const match = source.match(/^async function api\([^)]*\) \{[\s\S]*?^\}/m);
assert.ok(match, 'shipped request helper is present');
const timers = new Map();
let nextTimer = 0;
let attempts = 0;
let hangs = true;
const context = vm.createContext({
  AbortController, Date, Error,
  inflightReadRequests: new Map(), readCooldownUntil: new Map(),
  readRequestKey: (_method, path) => path,
  buildAuthHeaders: async (_method,_path,_body,idempotencyKey) => idempotencyKey
    ? {'x-idempotency-key':idempotencyKey} : {}, apiUrl: path => path,
  notePathFetched: () => {},
  setTimeout: (fn, ms) => { assert.equal(ms, 15000); timers.set(++nextTimer, fn); return nextTimer; },
  clearTimeout: id => timers.delete(id),
  fetch: async (_url, opts) => {
    attempts += 1;
    if (opts.method !== 'GET') {
      assert.equal(opts.signal, undefined, 'write outcome is not aborted by the read policy');
      assert.equal(opts.headers['x-idempotency-key'],'signed-operation','the signer and transmitted request receive the same operation key');
      return { ok: true, json: async () => ({ saved: true }) };
    }
    if (hangs) return new Promise((_resolve, reject) => {
      opts.signal.addEventListener('abort', () => reject(new Error('read aborted')));
    });
    return { ok: true, json: async () => ({ recovered: true }) };
  },
});
vm.runInContext(match[0], context);
const first = context.api('GET', '/api/report');
const second = context.api('GET', '/api/report');
const failed = Promise.allSettled([first, second]);
await new Promise(resolve => setImmediate(resolve));
assert.equal(attempts, 1, 'parallel reads share one request');
assert.equal(timers.size, 1);
[...timers.values()][0]();
const results = await failed;
assert.ok(results.every(r => r.status === 'rejected'));
assert.equal(context.inflightReadRequests.size, 0, 'timeout releases the stuck slot');
assert.equal(timers.size, 0, 'timeout is cleaned up');
hangs = false;
assert.equal((await context.api('GET', '/api/report')).recovered, true);
assert.equal(timers.size, 0, 'successful read cleans its timer too');
assert.equal((await context.api('POST', '/api/config', { enabled: true }, 'signed-operation')).saved, true);
assert.equal(timers.size, 0);
console.log('Dashboard read timeout and recovery tests passed.');

const elements = new Map();
const element = id => {
  if (!elements.has(id)) elements.set(id, { textContent: 'old reading', disabled: false });
  return elements.get(id);
};
const rendering = vm.createContext({
  Date, Array, Boolean, String, document: { getElementById: element },
  window: {}, lastSseAt: 0, researchOnlyState: false, loopRunningState: null,
  shouldPollReadLoop: () => false, updateDashboardStrip: () => {},
});
const handler = source.match(/^function handleSSEUpdate\(data\) \{[\s\S]*?^\}/m);
const buttons = source.match(/^function updateLoopButtons\(running, researchOnly\) \{[\s\S]*?^\}/m);
assert.ok(handler && buttons);
vm.runInContext(`${handler[0]}\n${buttons[0]}`, rendering);
rendering.handleSSEUpdate({ live_btc_price: null });
assert.equal(element('btc-current-price').textContent, '—', 'expired current price is blanked');
assert.equal(element('btc-price-change').textContent, '—');
rendering.updateLoopButtons(false, true);
for (const id of ['btn-start', 'btn-execute', 'btn-refresh']) assert.equal(element(id).disabled, true);
rendering.updateLoopButtons(false);
assert.equal(element('btn-start').disabled, true, 'a payload without the mode cannot re-enable the demo');
rendering.updateLoopButtons(false, false);
assert.equal(element('btn-start').disabled, false);
rendering.updateLoopButtons(true, false);
assert.equal(element('btn-start').disabled, true, 'running loops cannot be started twice');
console.log('Dashboard stale price and research-control tests passed.');
