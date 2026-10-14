// Pins the dashboard stream reconnect policy: a rejected credential is reported
// as such, every other failure keeps retrying, and retries back off to a cap
// instead of polling a dead server forever. A regression here either leaves the
// dashboard permanently dark after a network blip or hammers a down server.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';

// The dashboard's behaviour now lives in its own file rather than inline in
// the page, so read it directly. Reading the real source, not a copy, is the
// point: this test must fail if the shipped policy changes.
const script = readFileSync(new URL('../../static/dashboard.js', import.meta.url), 'utf8');

// Pull the self-contained pieces of the reconnect policy out of the shipped
// source so the test exercises the real code rather than a copy of it.
function extract(pattern, label) {
  const match = script.match(pattern);
  assert.ok(match, `could not find ${label} in static/dashboard.js`);
  return match[0];
}
const constants = extract(/^const SSE_RETRY_MIN_MS[\s\S]*?^let sseRetryDelay = 0;/m, 'retry constants');
const isAuthFailure = extract(/^function isAuthFailure\(err\) \{[\s\S]*?^\}/m, 'isAuthFailure');
const scheduleSseRetry = extract(/^function scheduleSseRetry\(authIssue\) \{[\s\S]*?^\}/m, 'scheduleSseRetry');

const scheduled = [];
const context = vm.createContext({
  setTimeout: (_fn, delay) => { scheduled.push(delay); return scheduled.length; },
  clearTimeout: () => {},
  connectSSE: () => {},
  reconnectTimer: null,
});
vm.runInContext(`${constants}\n${isAuthFailure}\n${scheduleSseRetry}`, context);

// Rejected credentials: retrying with the same key cannot help.
assert.equal(context.isAuthFailure({ status: 401 }), true, '401 is an auth failure');
assert.equal(context.isAuthFailure({ status: 403 }), true, '403 is an auth failure');
assert.equal(context.isAuthFailure(new Error('stream token failed: 401 invalid api key')), true,
  'message fallback for callers that throw without a status');

// Transient faults: the server or network is down, not the credentials.
assert.equal(context.isAuthFailure({ status: 500 }), false, '500 is transient');
assert.equal(context.isAuthFailure({ status: 502 }), false, '502 is transient');
assert.equal(context.isAuthFailure({ status: 0 }), false, 'opaque network failure is transient');
assert.equal(context.isAuthFailure(new TypeError('Failed to fetch')), false, 'offline is transient');
assert.equal(context.isAuthFailure(undefined), false, 'unknown failure is transient');

// Transient backoff doubles from the 3s floor and is capped at 30s.
for (let i = 0; i < 4; i += 1) context.scheduleSseRetry(false);
assert.deepEqual(scheduled, [3000, 6000, 12000, 24000], 'backoff doubles from the 3s floor');
for (let i = 0; i < 8; i += 1) context.scheduleSseRetry(false);
assert.equal(Math.max(...scheduled), 30000, 'backoff is capped at 30s');
assert.ok(scheduled.every(d => d >= 3000), 'no retry is scheduled faster than the floor');

// A missing credential keeps retrying too, on a higher floor, because recovery
// there waits on a human pasting a key.
scheduled.length = 0;
vm.runInContext('sseRetryDelay = 0;', context);
context.scheduleSseRetry(true);
context.scheduleSseRetry(true);
assert.deepEqual(scheduled, [10000, 20000], 'auth retries start at the 10s floor and back off');
assert.ok(scheduled.length > 0, 'an auth failure still schedules a retry');

// The policy above is only useful if the connect path actually routes through it.
// Guard the call sites too: the helpers can be correct while nothing calls them.
const withoutHelper = script.replace(scheduleSseRetry, '');
assert.equal(/setTimeout\(\s*connectSSE/.test(withoutHelper), false,
  'every reconnect must go through scheduleSseRetry so the backoff applies');
const callSites = (withoutHelper.match(/scheduleSseRetry\(/g) || []).length;
assert.ok(callSites >= 3,
  `expected the token-failure, auth-failure and drop-out paths to call scheduleSseRetry, found ${callSites}`);

console.log('Dashboard SSE reconnect policy tests passed.');
