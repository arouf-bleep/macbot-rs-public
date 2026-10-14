import assert from 'node:assert/strict';
import { createHmac, webcrypto } from 'node:crypto';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';

function storage(values) {
  return {
    getItem: key => values.get(key),
    setItem: (key, value) => values.set(key, value),
    removeItem: key => values.delete(key),
  };
}

const localValues = new Map();
const sessionValues = new Map();
const location = { href: 'http://127.0.0.1:18943/#api_key=test-key&api_secret=test-secret&tab=maker' };
const context = vm.createContext({
  URL, URLSearchParams, TextEncoder, crypto: webcrypto,
  window: { location, crypto: webcrypto },
  localStorage: storage(localValues),
  sessionStorage: storage(sessionValues),
  history: { replaceState: (_state, _title, url) => { location.href = url; } },
  document: { title: 'Test' },
});
vm.runInContext(readFileSync(new URL('../../static/api-auth.js', import.meta.url), 'utf8'), context);
const auth = context.macbotApiAuth;
assert.deepEqual(Object.keys(await auth.buildAuthHeaders('POST', '/api/config', '{}')), []);
auth.persistApiAuthFromUrl();
assert.equal(new URL(location.href).hash, '#tab=maker');
assert.equal(auth.getApiAuth().apiKey, 'test-key');
assert.equal(localValues.has('macbot.apiAuth'), false);
assert.equal(sessionValues.has('macbot.apiAuth'), true);
const body = '{"execution_mode":"maker"}';
const headers = await auth.buildAuthHeaders('POST', '/api/config?dry_run=1', body);
assert.match(headers['x-api-nonce'], /^[0-9a-f]{64}$/);
const canonical = JSON.stringify([headers['x-api-ts'],headers['x-api-nonce'],'POST','/api/config?dry_run=1','',body]);
assert.equal(headers['x-api-signature'], createHmac('sha256', 'test-secret').update(canonical).digest('hex'));
assert.equal(headers['x-api-key'], 'test-key');
assert.notEqual(headers['x-api-signature'], createHmac('sha256', 'test-secret').update(canonical + ' ').digest('hex'));
const nextHeaders = await auth.buildAuthHeaders('POST', '/api/config?dry_run=1', body);
assert.notEqual(nextHeaders['x-api-nonce'], headers['x-api-nonce']);
assert.notEqual(nextHeaders['x-api-signature'], headers['x-api-signature']);
const keyed = await auth.buildAuthHeaders('POST','/api/config?dry_run=1',body,'operation-1');
assert.equal(keyed['x-idempotency-key'],'operation-1');
const keyedCanonical = JSON.stringify([keyed['x-api-ts'],keyed['x-api-nonce'],'POST','/api/config?dry_run=1','operation-1',body]);
assert.equal(keyed['x-api-signature'],createHmac('sha256','test-secret').update(keyedCanonical).digest('hex'));
assert.notEqual(keyed['x-api-signature'],createHmac('sha256','test-secret').update(keyedCanonical.replace('operation-1','operation-2')).digest('hex'));

// Shared with Rust/Python: UTF-8, quotes, colons, an escaped newline and a
// trailing body newline must all survive the canonical JSON framing exactly.
class VectorDate extends Date { static now() { return 1700000000000; } }
const vectorCrypto = { subtle: webcrypto.subtle, getRandomValues: array => array.fill(0) };
const vectorValues = new Map();
const vectorContext = vm.createContext({Date:VectorDate,TextEncoder,crypto:vectorCrypto,
  window:{crypto:vectorCrypto},sessionStorage:storage(vectorValues)});
vm.runInContext(readFileSync(new URL('../../static/api-auth.js', import.meta.url),'utf8'),vectorContext);
vectorContext.macbotApiAuth.setApiAuth('fixture-key','public-fixture-secret');
const vectorHeaders = await vectorContext.macbotApiAuth.buildAuthHeaders('POST','/api/config?note=colon%3Aquote',
  '{"note":"é:🙂\\n"}\n','key:quote"end');
assert.equal(vectorHeaders['x-api-signature'],'82d0e2bdb1555f0d3e91d2e4df2fb533ec7f823074b93de4b2bfc437f65218e2');

auth.clearApiAuth();
assert.equal(auth.getApiAuth().apiKey, '');
assert.equal(sessionValues.has('macbot.apiAuth'), false);

assert.equal(auth.setApiAuth('  session-key  ', '', false), true);
assert.equal(auth.getApiAuth().apiKey, 'session-key');
assert.equal(localValues.has('macbot.apiAuth'), false);

assert.equal(auth.setApiAuth('remembered-key', 'remembered-secret', true), true);
assert.equal(sessionValues.has('macbot.apiAuth'), false);
assert.equal(auth.getApiAuth().apiSecret, 'remembered-secret');

localValues.set('macbot.apiAuth', '{invalid');
assert.equal(auth.getApiAuth().apiKey, '');

const indexHtml = readFileSync(new URL('../../static/index.html', import.meta.url), 'utf8');
assert.match(indexHtml, /id="dashboard-auth-modal"/);
assert.match(indexHtml, /id="dashboard-api-key"[^>]+type="password"/);
assert.match(indexHtml, /id="dashboard-api-secret"[^>]+type="password"/);
assert.doesNotMatch(indexHtml, /test-key|test-secret/);

const dashboardJs = readFileSync(new URL('../../static/dashboard.js', import.meta.url), 'utf8');
assert.doesNotMatch(dashboardJs, /Open the page with #api_key/);
console.log('Dashboard unlock storage, credential import, and HMAC tests passed.');

const noStorage = vm.createContext({});
vm.runInContext(readFileSync(new URL('../../static/api-auth.js', import.meta.url), 'utf8'), noStorage);
assert.equal(noStorage.macbotApiAuth.setApiAuth('key','secret',false),false);
const remembered = new Map([['macbot.apiAuth', JSON.stringify({apiKey:'old',apiSecret:'old-secret'})]]);
const blocked = vm.createContext({localStorage:storage(remembered),sessionStorage:{setItem(){throw new Error('blocked');}}});
vm.runInContext(readFileSync(new URL('../../static/api-auth.js', import.meta.url), 'utf8'), blocked);
assert.equal(blocked.macbotApiAuth.setApiAuth('new','new-secret',false),false);
assert.equal(blocked.macbotApiAuth.getApiAuth().apiKey,'old');
