#!/usr/bin/env python3
"""Fixture truth and actual HTTP boundaries for the read-only operator preview."""
from __future__ import annotations

from datetime import datetime, timedelta, timezone
import http.client
import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import threading
import unittest
from unittest import mock

import preview_dashboard as demo

NOW = datetime(2026, 10, 11, 12, tzinfo=timezone.utc)


class SampleTruthTests(unittest.TestCase):
    def test_balances_fees_and_headlines_derive_from_the_same_mixed_sample(self):
        data = demo.samples('normal', NOW, NOW)
        closed = [t for t in data['trades'] if t['status'] == 'CLOSED']
        net = sum(t['pnl'] - t['fee_paid'] for t in closed)
        self.assertAlmostEqual(net, -0.36)
        self.assertEqual(data['stats']['wins'], 2)
        self.assertEqual(data['stats']['losses'], 2)
        self.assertAlmostEqual(data['ledger']['totals']['net_pnl'], net)
        self.assertAlmostEqual(data['ledger']['curve'][-1]['cum'], net)
        open_row = data['trades'][-1]
        self.assertAlmostEqual(data['balance'], 1000 + net - open_row['amount'] - open_row['fee_paid'])
        self.assertFalse(data['ledger']['window']['evaluable'])
        self.assertFalse(data['model']['model']['loaded'])

    def test_daily_sample_arithmetic_uses_utc_dates(self):
        data = demo.samples('normal', NOW, NOW - timedelta(days=1))
        self.assertEqual(data['daily'], 0)
        self.assertEqual(data['stats']['daily_pnl_evidence']['day_utc'], '2026-10-11')
        self.assertEqual(data['stats']['total_pnl_net'], -0.36)

    def test_halt_discards_fresh_price_claims_and_recovery_retains_unknown_exposure(self):
        halted = demo.samples('halted', NOW, NOW)
        self.assertTrue(halted['risk']['kill_switch'])
        self.assertTrue(halted['snapshot']['analysis_freshness']['stale'])
        self.assertEqual(halted['risk']['kill_switch_reason']['source'], 'synthetic_feed_stale')
        self.assertIn('stale', halted['risk']['kill_switch_reason']['detail'])
        self.assertIsNone(halted['snapshot']['live_btc_price'])
        self.assertIsNone(demo.payload('/api/btc-chart', 'halted', NOW, NOW)['current_price'])
        self.assertTrue(all(book['stale'] for book in halted['maker']['l2_top_of_book']))
        recovery = demo.samples('recovery', NOW, NOW)
        pending = recovery['trades'][-1]
        self.assertEqual(pending['status'], 'CLOSING')
        self.assertIsNone(pending['pnl'])
        self.assertIsNone(pending['exit_price'])
        self.assertEqual(recovery['snapshot']['execution']['open_positions'], 1)
        self.assertTrue(recovery['risk']['degraded_no_trade'])
        self.assertEqual(recovery['risk']['kill_switch_reason']['source'], 'synthetic_pending_receipt')
        self.assertFalse(recovery['snapshot']['loop_running'])

    def test_every_fixture_declares_synthetic_provenance_and_no_real_exchange_parity(self):
        for scenario in demo.SCENARIOS:
            for path in ('/api/config', '/api/stats', '/api/analysis', '/api/report',
                         '/api/risk/status', '/api/trades/101/fills', '/api/recovery'):
                payload = demo.payload(path, scenario, NOW, NOW)
                self.assertTrue(payload['demo_synthetic'])
                self.assertTrue(payload['demo_read_only'])
                self.assertEqual(payload['demo_scenario'], scenario)
                json.dumps(payload, allow_nan=False)
            parity = demo.payload('/api/trades/101/fills', scenario, NOW, NOW)['parity_check']
            self.assertIsNone(parity['entry_exchange_vwap'])
            self.assertEqual(parity['overall_status'], 'incomplete')
        self.assertIsNone(demo.payload('/api/trades/999/fills', 'normal', NOW, NOW))
        with self.assertRaises(ValueError):
            demo.samples('<script>', NOW, NOW)

    def test_existing_dashboard_gets_a_prominent_injected_sample_label(self):
        html = demo.demo_html(demo.ROOT, 'recovery').decode()
        self.assertIn('SYNTHETIC DASHBOARD PREVIEW', html)
        self.assertIn('READ ONLY', html)
        self.assertIn('not proof of profitability', html)
        self.assertIn('aria-current="true">Unresolved exit', html)
        self.assertIn('window.fetch=', html)
        self.assertIn('new MutationObserver(blockWrites)', html)
        self.assertLess(html.index('window.macbotApiAuth=Object.freeze'),
                        html.index('<script src="/static/dashboard.js"></script>'))
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / 'static').mkdir()
            (root / 'static/index.html').write_text('<body>No dashboard marker</body>')
            with self.assertRaisesRegex(ValueError, 'integration markers'):
                demo.demo_html(root, 'normal')

    def test_all_current_dashboard_asset_references_are_explicitly_allowed(self):
        index = (demo.ROOT / 'static/index.html').read_text()
        references = re.findall(r'(?:src|href)="/static/([^"]+)"', index)
        for css in ('vendor/fonts.css', 'vendor/bootstrap-icons.min.css'):
            text = (demo.ROOT / 'static' / css).read_text()
            references.extend(str(Path(css).parent / url.split('?')[0])
                              for url in re.findall(r'url\([\'"]?([^\)\'\"]+)', text))
        self.assertTrue(references)
        for relative in references:
            self.assertIn(relative, demo.STATIC_ASSETS)
            self.assertTrue((demo.ROOT / 'static' / relative).is_file())
        self.assertNotIn('index.html', demo.STATIC_ASSETS)

    def test_browser_bootstrap_never_reads_or_changes_credentials_and_blocks_external_transport(self):
        node = shutil.which('node')
        if not node:
            self.skipTest('Node.js is required for the project dashboard checks')
        scripts = re.findall(r'<script>(.*?)</script>', demo.demo_html(demo.ROOT, 'recovery').decode(), re.S)
        bootstrap = next(script for script in scripts if 'window.macbotApiAuth=Object.freeze' in script)
        harness = r'''
const assert=require('node:assert/strict'),vm=require('node:vm');
const input=JSON.parse(require('node:fs').readFileSync(0,'utf8'));
const calls=[],streams=[];
const sessionButton={id:'',tagName:'BUTTON',disabled:false,getAttribute:()=> 'newSession()'};
const executeButton={id:'',tagName:'BUTTON',disabled:false,getAttribute:()=> 'executeCurrentTrade()'};
const refreshButton={id:'',tagName:'BUTTON',disabled:false,getAttribute:()=> 'loadRiskAndAttribution()'};
const callbacks=[];
class EventSource{constructor(url,options){streams.push({url:String(url),options});}}
const context={URL,URLSearchParams,Request,Promise,EventSource,
  location:{href:'http://127.0.0.1:18082/?scenario=recovery#api_key=private&api_secret=private',origin:'http://127.0.0.1:18082'},
  fetch:async(url,options)=>{calls.push({url:String(url instanceof Request?url.url:url),options});return 'sample';},
  document:{querySelectorAll:()=>[sessionButton,executeButton,refreshButton],getElementById:()=>null,addEventListener:(name,callback)=>callbacks.push(callback),body:{}},
  MutationObserver:class{observe(){}},history:{replaceState(){throw Error('Must not alter credential fragment');}},
  setInterval:()=>0,setTimeout:()=>0,clearTimeout:()=>{},console};
let storageAccesses=0;
const originalStorage={localStorage:new Map([
  ['macbot.apiAuth','PRIVATE_AUTH'],['archivedTrades','PRIVATE_ARCHIVED_HISTORY'],
  ['sessionStartId','999'],['sessionStartAt','2026-10-10T00:00:00Z'],
  ['macbot.activeTab','execution'],['collapsedSignalGroups','["private"]']
]),sessionStorage:new Map([['macbot.apiAuth','PRIVATE_SESSION_AUTH']])};
const beforeStorage=JSON.stringify(Object.values(originalStorage).map(store=>[...store]));
for(const name of ['localStorage','sessionStorage'])Object.defineProperty(context,name,{configurable:true,get(){storageAccesses++;throw Error('Private storage must remain untouched');}});
context.window=context;vm.createContext(context);
vm.runInContext(input.auth,context);vm.runInContext(input.bootstrap,context);
callbacks.forEach(callback=>callback());
assert.equal(sessionButton.disabled,true);assert.equal(executeButton.disabled,true);assert.equal(refreshButton.disabled,false);
(async()=>{
assert.equal(context.macbotApiAuth.getApiAuth().apiKey,'dashboard-preview');
assert.equal(context.macbotApiAuth.getApiAuth().apiSecret,'');
assert.equal((await context.macbotApiAuth.buildAuthHeaders('POST','/api/config','{}'))['x-api-key'],'dashboard-preview');
context.macbotApiAuth.persistApiAuthFromUrl();context.macbotApiAuth.clearApiAuth();
assert.equal(context.macbotApiAuth.setApiAuth('private','private',true),false);
assert.equal(storageAccesses,0);
await assert.rejects(context.fetch('https://example.invalid/api/config'),/external requests/);
await assert.rejects(context.fetch('http://127.0.0.1:18081/api/config'),/external requests/);
assert.equal(calls.length,0);
await context.fetch('/api/stats?scenario=normal');
assert.equal(new URL(calls[0].url).searchParams.get('scenario'),'recovery');
await context.fetch(new Request('http://127.0.0.1:18082/api/config'));
assert.equal(new URL(calls[1].url).searchParams.get('scenario'),'recovery');
assert.throws(()=>new context.EventSource('https://example.invalid/api/stream'),/external streams/);
assert.throws(()=>new context.EventSource('http://127.0.0.1:18081/api/stream'),/external streams/);
assert.equal(streams.length,0);
new context.EventSource('/api/stream?stream_token=dashboard-preview.recovery');
assert.equal(new URL(streams[0].url).searchParams.get('scenario'),'recovery');
assert.equal(new URL(streams[0].url).searchParams.get('stream_token'),'dashboard-preview.recovery');
// Execute the actual dashboard's complete top-level startup. This loads its
// archive and immediately reads/writes its stored session boundary.
context.document.getElementById=()=>null;context.document.querySelector=()=>null;
context.document.querySelectorAll=()=>[];context.addEventListener=()=>{};
vm.runInContext(input.dashboard,context);
assert.equal(vm.runInContext('archivedTrades.length',context),0);
assert.equal(vm.runInContext('sessionStartId',context),0);
vm.runInContext("applySessionBoundary({tradeId:101,startedAt:'2026-10-11T12:00:00Z'}); archivedTrades=[{id:777,source:'synthetic_sample'}]; saveArchivedTradesToStorage();",context);
assert.equal(context.localStorage.getItem('sessionStartId'),'101');
assert.equal(context.localStorage.length,3);
assert.equal(JSON.parse(context.localStorage.getItem('archivedTrades'))[0].source,'synthetic_sample');
assert.equal(context.sessionStorage.length,0);
assert.equal(storageAccesses,0);
assert.equal(JSON.stringify(Object.values(originalStorage).map(store=>[...store])),beforeStorage);
process.stdout.write('credential and origin boundary passed\n');
})().catch(error=>{process.stderr.write(String(error));process.exitCode=1;});
'''
        result = subprocess.run([node, '-e', harness], input=json.dumps({
            'auth': (demo.ROOT / 'static/api-auth.js').read_text(), 'bootstrap': bootstrap,
            'dashboard': (demo.ROOT / 'static/dashboard.js').read_text()}),
            text=True, capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('credential and origin boundary passed', result.stdout)


class DemoHttpBoundaryTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory(prefix='macbot-dashboard-preview-http-')
        cls.root = Path(cls.temporary.name)
        (cls.root / 'static').mkdir()
        (cls.root / 'static/index.html').write_text(
            '<title>Macbot</title><head></head><body>'
            '<script src="/static/api-auth.js"></script>'
            '<script src="/static/dashboard.js"></script></body>')
        (cls.root / 'static/dashboard.css').write_text('body { color: white; }')
        (cls.root / 'static/.env').write_text('PRIVATE_STATIC_SECRET')
        (cls.root / 'static/unexpected.json').write_text('PRIVATE_STATIC_SECRET')
        (cls.root / 'config.json').write_text('PRIVATE_CONFIG_MUST_NOT_BE_READ')
        (cls.root / 'trades.db').write_text('PRIVATE_DATABASE_MUST_NOT_BE_READ')
        with mock.patch('socket.getfqdn', side_effect=AssertionError('Demo must not perform DNS discovery')):
            cls.server = demo.PreviewServer(0, cls.root)
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()

    @classmethod
    def tearDownClass(cls):
        cls.server.stopping.set()
        cls.server.shutdown()
        cls.server.server_close()
        cls.thread.join(timeout=2)
        cls.temporary.cleanup()

    def request(self, method, path, body=None, headers=None):
        client = http.client.HTTPConnection('127.0.0.1', self.server.server_port, timeout=3)
        response = None
        try:
            client.request(method, path, body=body, headers=headers or {})
            response = client.getresponse()
            return response.status, dict(response.getheaders()), response.read()
        finally:
            if response is not None:
                response.close()
            client.close()

    def read(self, path):
        status, _, body = self.request('GET', path, headers={'x-api-key': demo.READ_KEY})
        self.assertEqual(status, 200)
        return json.loads(body)

    def test_actual_socket_is_loopback_and_unsafe_origins_are_refused(self):
        self.assertEqual(self.server.server_address[0], '127.0.0.1')
        status, _, _ = self.request('GET', '/', headers={'Host': 'attacker.invalid'})
        self.assertEqual(status, 403)
        status, _, _ = self.request('GET', '/', headers={'Origin': 'https://attacker.invalid'})
        self.assertEqual(status, 403)
        status, headers, body = self.request('GET', '/')
        self.assertEqual(status, 200)
        self.assertIn("connect-src 'self'", headers['Content-Security-Policy'])
        self.assertIn('frame-ancestors', headers['Content-Security-Policy'])
        self.assertIn(b'SYNTHETIC DASHBOARD PREVIEW', body)

    def test_all_mutations_are_refused_even_with_signature_headers(self):
        before = self.read('/api/trades')
        for method in ('POST', 'PUT', 'PATCH', 'DELETE', 'OPTIONS', 'TRACE', 'CONNECT'):
            for path in ('/api/trade/force', '/api/trades/105/close', '/api/config', '/api/loop/start'):
                status, headers, body = self.request(method, path, '{}', {
                    'content-type': 'application/json', 'x-api-key': demo.READ_KEY,
                    'x-api-secret': 'anything', 'x-api-signature': 'anything'})
                self.assertEqual(status, 405, (method, path))
                self.assertEqual(headers['Allow'], 'GET, HEAD')
                self.assertIn(b'all writes are refused', body)
        self.assertEqual(self.read('/api/trades'), before)
        self.assertEqual((self.root / 'config.json').read_text(), 'PRIVATE_CONFIG_MUST_NOT_BE_READ')
        self.assertEqual((self.root / 'trades.db').read_text(), 'PRIVATE_DATABASE_MUST_NOT_BE_READ')

    def test_static_serving_cannot_escape_to_configuration_or_database(self):
        for path in ('/static/../config.json', '/static/%2e%2e/trades.db',
                     '/static/%2fetc/passwd', '/static/..%5cconfig.json', '/config.json',
                     '/static/.env', '/static/unexpected.json', '/static/index.html'):
            status, _, body = self.request('GET', path)
            self.assertEqual(status, 404, path)
            self.assertNotIn(b'PRIVATE_', body)
        (self.root / 'static/redesign.css').symlink_to(self.root / 'config.json')
        status, _, _ = self.request('GET', '/static/redesign.css')
        self.assertEqual(status, 404)
        (self.root / 'static/api-auth.js').symlink_to(self.root / 'static/.env')
        status, _, body = self.request('GET', '/static/api-auth.js')
        self.assertEqual(status, 404)
        self.assertNotIn(b'PRIVATE_', body)
        status, _, body = self.request('HEAD', '/static/dashboard.css')
        self.assertEqual(status, 200)
        self.assertEqual(body, b'')

    def test_unknown_scenarios_or_paths_are_not_silently_invented(self):
        for suffix in ('invalid', 'normal&scenario=halted', '%3Cscript%3E', ''):
            status, _, _ = self.request('GET', '/?scenario=' + suffix)
            self.assertEqual(status, 400)
        self.assertTrue(self.read('/api/stats?scenario=halted')['demo_synthetic'])
        status, _, _ = self.request('GET', '/api/trades')
        self.assertEqual(status, 401)
        status, _, _ = self.request('GET', '/api/unknown', headers={'x-api-key': demo.READ_KEY})
        self.assertEqual(status, 404)

    def test_real_sse_frame_has_consistent_recovery_state_and_bounded_slots(self):
        token = self.read('/api/stream/token?scenario=recovery')['token']
        # Check capacity before an example client disconnects. Otherwise its
        # delayed slot release could race this assertion on a slow runner.
        acquired = 0
        try:
            while self.server.stream_slots.acquire(blocking=False):
                acquired += 1
            self.assertEqual(acquired, 8)
            status, _, _ = self.request('GET', '/api/stream?stream_token=dashboard-preview.normal')
            self.assertEqual(status, 503)
        finally:
            for _ in range(acquired):
                self.server.stream_slots.release()
        client = http.client.HTTPConnection('127.0.0.1', self.server.server_port, timeout=3)
        response = None
        try:
            client.request('GET', '/api/stream?stream_token=' + token)
            response = client.getresponse()
            self.assertEqual(response.status, 200)
            frame = response.readline().decode()
            state = json.loads(frame.removeprefix('data: '))
            self.assertTrue(state['demo_synthetic'])
            self.assertEqual(state['demo_scenario'], 'recovery')
            self.assertEqual(state['trades'][-1]['status'], 'CLOSING')
            self.assertIsNone(state['trades'][-1]['pnl'])
        finally:
            if response is not None:
                response.close()
            client.close()
        status, _, _ = self.request('GET', '/api/stream?stream_token=dashboard-preview.recovery&scenario=normal')
        self.assertEqual(status, 400)


if __name__ == '__main__':
    unittest.main()
