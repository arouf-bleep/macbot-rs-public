#!/usr/bin/env python3
"""Preview the existing dashboard with synthetic, read-only operator samples.

Run from any directory with ``python3 scripts/preview_dashboard.py`` and open the
printed local URL. No bot, model, database, credentials or exchange is loaded.
These samples illustrate the interface; real-engine acceptance evidence is
separate. Only the standard library is required.
"""
from __future__ import annotations

import argparse
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import math
import mimetypes
from pathlib import Path
import re
from socketserver import TCPServer
import threading
import time
from urllib.parse import parse_qs, unquote, urlsplit

ROOT = Path(__file__).resolve().parent.parent
READ_KEY = 'dashboard-preview'
SCENARIOS = {
    'normal': ('Sample overview', 'Mixed sample results and one open position. No trading engine is running.'),
    'halted': ('Stale-feed halt', 'Sample feed is stale. New entries are blocked and old prices are marked as old.'),
    'recovery': ('Unresolved exit', 'Sample sale has no confirmed receipt. Exposure stays visible; its profit is unknown.'),
}
EVIDENCE_NOTE = 'Synthetic interface samples only; not exchange receipts, real-engine test evidence or a performance claim.'
# Deliberately reviewed dashboard assets only. A file accidentally placed in
# static/ (including a credential, database, or raw index without our banner)
# is never exposed by this preview.
STATIC_ASSETS = frozenset({
    'api-auth.js', 'dashboard.js', 'dashboard.css', 'redesign.css',
    'vendor/bootstrap.min.css', 'vendor/bootstrap.bundle.min.js',
    'vendor/bootstrap-icons.min.css', 'vendor/chart.umd.min.js', 'vendor/fonts.css',
    'vendor/fonts/bootstrap-icons.woff', 'vendor/fonts/bootstrap-icons.woff2',
    'vendor/fonts/-F63fjptAgt5VM-kVkqdyU8n1iIq131nj-otFQ.woff2',
    'vendor/fonts/-F63fjptAgt5VM-kVkqdyU8n1isq131nj-otFQ.woff2',
    'vendor/fonts/-F63fjptAgt5VM-kVkqdyU8n1iAq131nj-otFQ.woff2',
    'vendor/fonts/-F63fjptAgt5VM-kVkqdyU8n1iEq131nj-otFQ.woff2',
    'vendor/fonts/-F63fjptAgt5VM-kVkqdyU8n1i8q131nj-o.woff2',
    'vendor/fonts/-F6qfjptAgt5VM-kVkqdyU8n3twJwl1FgsAXHNlYzg.woff2',
    'vendor/fonts/-F6qfjptAgt5VM-kVkqdyU8n3twJwlRFgsAXHNlYzg.woff2',
    'vendor/fonts/-F6qfjptAgt5VM-kVkqdyU8n3twJwl9FgsAXHNlYzg.woff2',
    'vendor/fonts/-F6qfjptAgt5VM-kVkqdyU8n3twJwl5FgsAXHNlYzg.woff2',
    'vendor/fonts/-F6qfjptAgt5VM-kVkqdyU8n3twJwlBFgsAXHNk.woff2',
    'vendor/fonts/-F6qfjptAgt5VM-kVkqdyU8n3vAOwl1FgsAXHNlYzg.woff2',
    'vendor/fonts/-F6qfjptAgt5VM-kVkqdyU8n3vAOwlRFgsAXHNlYzg.woff2',
    'vendor/fonts/-F6qfjptAgt5VM-kVkqdyU8n3vAOwl9FgsAXHNlYzg.woff2',
    'vendor/fonts/-F6qfjptAgt5VM-kVkqdyU8n3vAOwl5FgsAXHNlYzg.woff2',
    'vendor/fonts/-F6qfjptAgt5VM-kVkqdyU8n3vAOwlBFgsAXHNk.woff2',
    'vendor/fonts/V8mDoQDjQSkFtoMM3T6r8E7mPb54C_k3HqUtEw.woff2',
    'vendor/fonts/V8mDoQDjQSkFtoMM3T6r8E7mPb94C_k3HqUtEw.woff2',
    'vendor/fonts/V8mDoQDjQSkFtoMM3T6r8E7mPbF4C_k3HqU.woff2',
})


def iso(value: datetime) -> str:
    return value.astimezone(timezone.utc).isoformat(timespec='milliseconds').replace('+00:00', 'Z')


def samples(scenario: str, now: datetime, anchor: datetime) -> dict:
    """Build bounded fixtures; all headline arithmetic comes from these rows."""
    if scenario not in SCENARIOS:
        raise ValueError('Unknown demo scenario')
    stale = scenario == 'halted'
    recovering = scenario == 'recovery'
    trades = []
    for offset, exit_price in enumerate((0.56, 0.47, 0.52, 0.43)):
        opened = anchor - timedelta(minutes=90 - 20 * offset)
        trades.append({
            'id': 101 + offset, 'timestamp': iso(opened),
            'exit_timestamp': iso(opened + timedelta(minutes=10)),
            'market_id': f'synthetic-window-{offset + 1}',
            'question': f'Synthetic Bitcoin Up/Down — sample window {offset + 1}',
            'direction': 'BUY_YES', 'entry_price': 0.5, 'exit_price': exit_price,
            'amount': 5.0, 'shares': 10.0, 'entry_cost': 5.0, 'entry_shares': 10.0,
            'pnl': round(10 * (exit_price - 0.5), 2), 'fee_paid': 0.04,
            'accounting_version': 0, 'status': 'CLOSED', 'mode': 'simulation',
            'source': 'synthetic_sample', 'strategy': 'scalp' if offset % 2 else 'conviction',
            'confidence': 0.6, 'exit_type': 'manual', 'partial_closed': 0,
            'reason': EVIDENCE_NOTE, 'end_date': iso(opened + timedelta(minutes=15)),
            'run_id': 'synthetic-dashboard-preview', 'token_id': f'synthetic-token-{offset + 1}',
        })
    trades.append({
        'id': 105, 'timestamp': iso(anchor - timedelta(minutes=2)),
        'market_id': 'synthetic-current',
        'question': 'Synthetic Bitcoin Up/Down — current sample window',
        'direction': 'BUY_YES', 'entry_price': 0.48, 'amount': 4.8, 'shares': 10.0,
        'entry_cost': 4.8, 'entry_shares': 10.0, 'fee_paid': 0.04,
        'accounting_version': 0, 'pnl': None, 'exit_price': None, 'exit_timestamp': None,
        'status': 'CLOSING' if recovering else 'OPEN', 'mode': 'simulation',
        'source': 'synthetic_sample', 'strategy': 'conviction', 'confidence': 0.6,
        'partial_closed': 0, 'reason': EVIDENCE_NOTE, 'run_id': 'synthetic-dashboard-preview',
        'token_id': 'synthetic-yes', 'order_id': 'synthetic-unconfirmed-exit' if recovering else None,
        'end_date': iso(anchor + timedelta(minutes=8)),
    })
    closed = trades[:-1]
    net = [round(row['pnl'] - row['fee_paid'], 2) for row in closed]
    cumulative, peak, drawdown, curve = 0.0, 0.0, 0.0, []
    for row, pnl in zip(closed, net):
        cumulative = round(cumulative + pnl, 2)
        peak = max(peak, cumulative)
        drawdown = max(drawdown, round(peak - cumulative, 2))
        curve.append({'t': row['exit_timestamp'], 'cum': cumulative})
    today = now.date().isoformat()
    daily = round(sum(p for row, p in zip(closed, net) if row['exit_timestamp'][:10] == today), 2)
    evidence = {'day_utc': today, 'dated_today_pnl': daily, 'exact_today_pnl': daily,
                'evidence_complete': True, 'conservative_unallocated_loss': 0.0,
                'unallocated_trade_ids': [], 'synthetic': True}
    balance = round(1000 + cumulative - 4.8 - 0.04, 2)
    stats = {'total_trades': len(trades), 'total_closed': len(closed), 'closed_trades': len(closed),
             'wins': sum(value > 0 for value in net), 'losses': sum(value < 0 for value in net),
             'win_rate': sum(value > 0 for value in net) / len(net), 'total_pnl_net': cumulative,
             'total_pnl': round(sum(row['pnl'] for row in closed), 2), 'max_drawdown_net': drawdown,
             'max_drawdown': drawdown, 'total_open': 1, 'open_trades': 1, 'sharpe_ratio_net': None,
             'today_pnl': daily, 'daily_pnl_evidence': evidence,
             'pnl_series_net': [{'ts': p['t'], 'pnl': p['cum']} for p in curve]}
    age = 75 if stale else 0
    freshness = {'age_secs': age, 'max_age_secs': 30, 'interval_secs': 5,
                 'stale': stale, 'never_run': False, 'synthetic': True}
    analysis = {'signal': 'HOLD', 'confidence': 0.6, 'p_up': 0.58, 'price': 0.54,
                'reason': SCENARIOS[scenario][1] + ' Illustrative signals; no inference runs.',
                'time_remaining_minutes': max(0, (anchor + timedelta(minutes=8) - now).total_seconds() / 60),
                'regime': 'RANGE', 'phase': 'sample',
                'btc_price': 68042.0, 'btc_start_price': 68000.0, 'btc_start_source': 'synthetic',
                'net_ev': None, 'liquidity': 1200, 'timestamp': iso(now - timedelta(seconds=age)),
                'analysis_freshness': freshness,
                'market_data': {'market_id': 'synthetic-current', 'question': trades[-1]['question'],
                                'price': 0.54, 'liquidity': 1200, 'end_date': trades[-1]['end_date']},
                'signals': {'momentum': 0.18, 'order_flow': -0.12, 'mean_reversion': 0.08}}
    reason = ({'source': 'synthetic_feed_stale', 'detail': SCENARIOS[scenario][1],
               'timestamp': iso(now)} if stale else
              {'source': 'synthetic_pending_receipt', 'detail': SCENARIOS[scenario][1],
               'timestamp': iso(now)} if recovering else None)
    risk = {'kill_switch': stale or recovering, 'kill_switch_reason': reason,
            'degraded_no_trade': recovering, 'l2_feed_stale': stale, 'l2_feed_age_ms': 75000 if stale else 0,
            'analysis_stale': stale, 'analysis_age_secs': age, 'execution_mode': 'maker',
            'actor_cmd_drop_total': 0, 'outbox_dropped_total': 0, 'slo_guard_trips_total': 0,
            'last_balance_drift_usd': None, 'current_run_id': 'synthetic-dashboard-preview',
            'rollout_stage': 'sample only', 'macro_guard': {'enabled': False}}
    books = [{'token_id': token, 'market_id': 'synthetic-current', 'is_yes_token': yes,
              'best_bid': bid, 'best_ask': ask, 'updated_at': iso(now - timedelta(seconds=age)),
              'snapshot_loaded': True, 'stale': stale, 'spread_bps': 200}
             for token, yes, bid, ask in [('synthetic-yes', True, 0.53, 0.55),
                                          ('synthetic-no', False, 0.45, 0.47)]]
    feed = {'target_market_count': 1, 'token_count': 2, 'snapshot_loaded_count': 2,
            'two_sided_count': 2, 'one_sided_count': 0, 'stale_count': 2 if stale else 0,
            'quoteable_count': 0 if stale or recovering else 2,
            'primary_not_ready_reason': 'feed_stale' if stale else 'pending_receipt' if recovering else '',
            'l2_stale_code': 'stale_timeout' if stale else '', 'not_ready_reasons': []}
    maker = {'quote_count': 0, 'market_count': 1, 'quotes': [], 'l2_top_of_book': books,
             'l2_summary': feed, 'l2_stale': stale, 'l2_stale_reason': SCENARIOS[scenario][1],
             'hands_off_markets': [], 'quote_model': [], 'decision': 'read_only_sample'}
    no_trade = {**feed, 'label': 'global_l2_feed_stale' if stale else
               'pending_receipt' if recovering else 'research_demo', 'decision': 'read_only_sample'}
    model = {'model': {'loaded': False, 'version': 'No model runs in this sample demo'},
             'ensemble': {'phase': 'shadow', 'deep_weight': 0.0, 'deep_total_samples': 0},
             'last_prediction': {}, 'synthetic': True}
    price_bus = {'status': 'offline' if stale else 'live', 'source': 'synthetic_sample',
                 'symbols_live': 0 if stale else 3, 'symbols_total': 3,
                 **{symbol: {'age_ms': age * 1000} for symbol in ('btc', 'eth', 'sol')}}
    snapshot = {'ts': iso(now), 'analysis': analysis, 'analysis_freshness': freshness,
                'stats': stats, 'daily_pnl': daily, 'daily_pnl_evidence': evidence, 'balance': balance,
                'mode': 'simulation', 'execution_mode': 'maker', 'loop_running': False,
                'backtest_only': True, 'maker_hold_positions': True, 'risk': risk,
                'l2_feed_stale': stale, 'l2_feed_age_ms': risk['l2_feed_age_ms'], 'maker': maker,
                'feed_truth': feed, 'no_trade_truth': no_trade, 'deep_model_status': model,
                'price_bus': price_bus, 'live_btc_price': None if stale else 68042.0,
                'execution': {'open_positions': 1, 'open_notional_usd': 4.8}, 'trades': trades}
    ledger = {'generated_at': iso(now), 'provenance': {'mode': 'SYNTHETIC SAMPLE',
              'execution_mode': 'Read only', 'strategy': 'Illustration',
              'implementation_id': 'UI samples; not engine verification', 'model_hash': 'None'},
              'window': {'closed_trades': 4, 'distinct_markets': 4,
                         'days_elapsed': (datetime.fromisoformat(closed[-1]['exit_timestamp'].replace('Z', '+00:00')).date()
                                          - datetime.fromisoformat(closed[0]['exit_timestamp'].replace('Z', '+00:00')).date()).days + 1,
                         'required_trades': 200, 'required_markets': 60, 'required_days': 21,
                         'first_close': closed[0]['exit_timestamp'], 'last_close': closed[-1]['exit_timestamp'],
                         'evaluable': False},
              'totals': {'booked_pnl': round(sum(row['pnl'] for row in closed), 2), 'entry_fees': 0.16,
                         'net_pnl': cumulative, 'win_rate': stats['win_rate'] * 100,
                         'max_drawdown': drawdown, 'trades_with_recorded_fee': 4},
              'curve': curve, 'rows': [{'closed_at': row['exit_timestamp'], 'market': row['question'],
                                      'direction': row['direction'], 'entry_price': row['entry_price'],
                                      'exit_price': row['exit_price'], 'pnl': row['pnl'],
                                      'fee': row['fee_paid'], 'net': pnl, 'strategy': row['strategy']}
                                     for row, pnl in reversed(list(zip(closed, net)))],
              'rejections': {'total': 3, 'reasons': [{'reason': 'Synthetic insufficient edge', 'count': 2},
                                                  {'reason': 'Synthetic stale feed', 'count': 1}]}}
    return {'trades': trades, 'stats': stats, 'snapshot': snapshot, 'analysis': analysis,
            'risk': risk, 'maker': maker, 'feed': feed, 'no_trade': no_trade,
            'model': model, 'ledger': ledger, 'balance': balance, 'daily': daily}


def payload(path: str, scenario: str, now: datetime, anchor: datetime) -> dict | None:
    data = samples(scenario, now, anchor)
    routes = {
        '/api/health': {'status': 'synthetic demo', 'ok': True},
        '/api/ready': {'ready': False, 'status': 'interface samples only; no engine'},
        '/api/config': {'mode': 'simulation', 'execution_mode': 'maker', 'auth_required': True,
                        'api_key': READ_KEY, 'api_hmac_secret': '', 'initial_balance': 1000,
                        'trade_amount': 5, 'max_open_positions': 2, 'maker_hold_positions': True,
                        'daily_loss_limit': 25, 'analysis_stale_seconds': 30,
                        'session_start_trade_id': 0, 'session_start_at': ''},
        '/api/trades': {'trades': data['trades']},
        '/api/trades/analysis': data['stats'], '/api/analysis': data['analysis'],
        '/api/stats': {'performance': data['stats'], 'daily_pnl': data['daily'],
                       'balance': data['balance'], 'mode': 'simulation',
                       'loop_running': False, 'backtest_only': True},
        '/api/loop/status': {'running': False, 'backtest_only': True},
        '/api/risk/status': data['risk'], '/api/deep-model/status': data['model'],
        '/api/ml/state': {'active': False, 'samples_seen': 0, 'learning_rate': None,
                         'accuracy': {}, 'weights': []},
        '/api/maker/state': {'maker': data['maker'], 'feed_truth': data['feed'],
                            'no_trade_truth': data['no_trade'], 'execution_mode': 'maker',
                            'l2_feed_stale': scenario == 'halted',
                            'l2_feed_age_ms': data['risk']['l2_feed_age_ms']},
        '/api/maker/ab-summary': {'note': EVIDENCE_NOTE},
        '/api/maker/rollout-status': {'note': 'No rollout or funded acceptance runs in this demo'},
        '/api/accounting/truth': {'current_balance': data['balance'],
                                  'derived_restart_balance': data['balance'],
                                  'authoritative_live_balance': None, 'drift_vs_authoritative_usd': None},
        '/api/attribution': {'by_mode': [{'mode': 'synthetic simulation', 'trades': 4,
                                         'win_rate': 0.5, 'total_pnl': -0.36, 'avg_pnl': -0.09}]},
        '/api/integrity/report': {'ok': False, 'incomplete': True, 'mismatches': [],
                                  'error': 'Synthetic samples are not real-engine integrity evidence'},
        '/api/report': data['ledger'], '/api/rejections': data['ledger']['rejections'],
        '/api/decisions': {'decisions': [{'at': row['timestamp'], 'kind': 'sample',
                                        'market': row['question'], 'call': row['direction'],
                                        'confidence': row['confidence'], 'pnl': row['pnl'],
                                        'outcome': 'Synthetic unresolved exit' if row['status'] == 'CLOSING'
                                        else 'Synthetic sample: ' + row['status'].lower()}
                                       for row in reversed(data['trades'])]},
        '/api/btc-chart': {'start_price': 68000, 'current_price': None if scenario == 'halted' else 68042,
                           'market_question': data['trades'][-1]['question'],
                           'candles': [{'ts': int((now - timedelta(seconds=(75 if scenario == 'halted' else 0) + 15 * (40 - i))).timestamp()),
                                        'close': round(68000 + i + 12 * math.sin(i / 4), 2)}
                                       for i in range(41)]},
        '/api/markets/no-arb': {'status': 'insufficient_data', 'market_count': 1, 'group_count': 0,
                               'max_violation_cents': None, 'worst_group': None},
        '/api/markets': {'markets': []}, '/api/outcomes': {'outcomes': []},
        '/api/signal-stats': {'signals': {}},
        '/api/recovery': {'orders': [{'order_id': 'synthetic-unconfirmed-exit', 'status': 'pending',
                                     'trade_id': 105, 'reason': SCENARIOS[scenario][1]}]
                          if scenario == 'recovery' else []},
        '/api/stream/token': {'token': f'dashboard-preview.{scenario}', 'ttl_seconds': 120},
    }
    match = re.fullmatch(r'/api/trades/(10[1-5])/fills', path)
    if match:
        trade = next(row for row in data['trades'] if row['id'] == int(match[1]))
        fills = [{'id': trade['id'] * 2, 'timestamp': trade['timestamp'], 'phase': 'entry',
                  'delta_size': 10, 'delta_price': trade['entry_price'], 'source': 'synthetic_sample',
                  'details': EVIDENCE_NOTE}]
        if trade['status'] == 'CLOSED':
            fills.append({'id': trade['id'] * 2 + 1, 'timestamp': trade['exit_timestamp'],
                          'phase': 'exit', 'delta_size': 10, 'delta_price': trade['exit_price'],
                          'source': 'synthetic_sample', 'details': EVIDENCE_NOTE})
        routes[path] = {'fills': fills, 'parity_check': {
            'entry_reference_price': trade['entry_price'], 'entry_exchange_vwap': None,
            'entry_drift_bps': None, 'entry_status': 'missing_exchange_fill',
            'exit_reference_price': trade['exit_price'], 'exit_exchange_vwap': None,
            'exit_drift_bps': None, 'exit_status': 'missing_exchange_fill', 'overall_status': 'incomplete'}}
    result = routes.get(path)
    return None if result is None else {**result, 'demo_synthetic': True, 'demo_read_only': True,
                                        'demo_scenario': scenario, 'demo_evidence_note': EVIDENCE_NOTE}


def demo_html(root: Path, scenario: str) -> bytes:
    index = (root / 'static' / 'index.html').read_text(encoding='utf-8')
    _, description = SCENARIOS[scenario]
    links = ''.join(f'<a href="/?scenario={key}" aria-current="{str(key == scenario).lower()}">{label}</a>'
                    for key, (label, _) in SCENARIOS.items())
    banner = f'''<aside id="dashboard-preview-banner" role="region" aria-label="Synthetic dashboard preview">
<strong>SYNTHETIC DASHBOARD PREVIEW · READ ONLY</strong><p>{description}</p>
<p>All prices, signals, trades and fees are made-up samples. No bot, model or exchange runs.
These views are not proof of profitability or real-engine acceptance.</p><nav aria-label="Sample scenarios">{links}</nav></aside>'''
    style = '''<style>#dashboard-preview-banner{position:sticky;top:0;z-index:300;padding:16px 24px;background:#151d2b;border-bottom:2px solid #87aeff;
font:14px/1.5 system-ui;color:#eef3ff}#dashboard-preview-banner strong{font-size:15px;letter-spacing:.04em}
#dashboard-preview-banner p{margin:5px 0}#dashboard-preview-banner nav{display:flex;flex-wrap:wrap;gap:10px;margin-top:10px}
#dashboard-preview-banner a{padding:5px 12px;border:1px solid #87aeff;border-radius:5px;color:#eef3ff;text-decoration:none}
#dashboard-preview-banner a[aria-current=true]{background:#284476}#dashboard-preview-banner a:focus-visible{outline:3px solid #fff}
#dashboard-preview-banner~.navbar{position:relative}</style>'''
    # This bootstrap exists only in this server's HTML response. The checked-in
    # dashboard and production API/authentication behavior remain unchanged.
    bootstrap = '''<script>
(function(){
 const scenario=SCENARIO;
 // The dashboard also stores session boundaries, archived trades and view
 // preferences directly. Shadow both complete Storage surfaces before it
 // starts, so even an origin previously used by a real bot stays untouched.
 const memoryStorage=()=>{
   const values=new Map();
   return Object.freeze({
     get length(){return values.size;},
     key:index=>[...values.keys()][Number(index)]??null,
     getItem:key=>values.get(String(key))??null,
     setItem:(key,value)=>{values.set(String(key),String(value));},
     removeItem:key=>{values.delete(String(key));},clear:()=>{values.clear();}
   });
 };
 ['localStorage','sessionStorage'].forEach(name=>{
   Object.defineProperty(window,name,{value:memoryStorage(),configurable:true});
 });
 // No storage access, including at a port previously used by the real bot.
 // Replace the complete helper: its original header builder closes over the
 // original storage-reading function, so overriding getApiAuth alone is unsafe.
 window.macbotApiAuth=Object.freeze({
   getApiAuth:()=>({apiKey:'dashboard-preview',apiSecret:''}),
   buildAuthHeaders:async()=>({'x-api-key':'dashboard-preview'}),
   persistApiAuthFromUrl:()=>{},setApiAuth:()=>false,clearApiAuth:()=>{}
 });
 const originalFetch=window.fetch.bind(window);
 window.fetch=(input,options)=>{
   const url=new URL(input instanceof Request?input.url:input,location.href);
   if(url.origin!==location.origin)return Promise.reject(new Error('Dashboard preview refuses external requests'));
   if(url.pathname.startsWith('/api/'))url.searchParams.set('scenario',scenario);
   return originalFetch(input instanceof Request?new Request(url,input):url,options);
 };
 const OriginalEventSource=window.EventSource;
 window.EventSource=class extends OriginalEventSource{
   constructor(input,options){
     const url=new URL(input,location.href);
     if(url.origin!==location.origin)throw new Error('Dashboard preview refuses external streams');
     if(url.pathname.startsWith('/api/'))url.searchParams.set('scenario',scenario);
     super(url,options);
   }
 };
 const blockWrites=()=>{
   document.querySelectorAll('button[onclick],#nav-exec-mode,#config-panel input,#config-panel select').forEach(el=>{
     const authButton=['btn-dashboard-auth','btn-dashboard-lock','btn-dashboard-unlock'].includes(el.id);
     if(el.tagName==='BUTTON'&&!authButton&&!/loopControl|resetDrawdown|resetKillSwitch|closeTrade|closeAll|executeCurrentTrade|executeTrade|triggerAnalysis|saveConfig|resetBalance|updateExecutionMode|newSession|openDashboardUnlock|lockDashboardBrowser/.test(el.getAttribute('onclick')||''))return;
     if(!el.disabled)el.disabled=true;
     if(el.title!=='Read-only synthetic sample')el.title='Read-only synthetic sample';
   });
   const hint=document.getElementById('tab-hint');
   if(hint&&hint.textContent!=='Synthetic samples · no trading or model inference')hint.textContent='Synthetic samples · no trading or model inference';
   const copy={
     'btn-dashboard-auth':'Public sample data',
     'dash-session-boundary-sub':'Fixed in-memory sample records. No database is opened.',
     'ledger-title':'Synthetic sample record'
   };
   Object.entries(copy).forEach(([id,text])=>{const el=document.getElementById(id);if(el&&el.textContent!==text)el.textContent=text;});
 };
 document.addEventListener('DOMContentLoaded',()=>{
   blockWrites();new MutationObserver(blockWrites).observe(document.body,{childList:true,subtree:true,attributes:true,attributeFilter:['disabled']});
 });
})();
</script>'''.replace('SCENARIO', json.dumps(scenario))
    marker = '<script src="/static/dashboard.js"></script>'
    if marker not in index or '<body>' not in index:
        raise ValueError('Dashboard integration markers changed; dashboard preview needs review')
    return index.replace('<title>Macbot</title>', '<title>Macbot — synthetic dashboard preview</title>')\
        .replace('</head>', style + '</head>').replace('<body>', '<body>' + banner)\
        .replace(marker, bootstrap + marker).encode('utf-8')


class PreviewServer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, port: int = 18082, root: Path = ROOT):
        self.root = root.resolve()
        self.anchor = datetime.now(timezone.utc)
        self.stopping = threading.Event()
        self.stream_slots = threading.BoundedSemaphore(8)
        super().__init__(('127.0.0.1', port), PreviewHandler)

    def server_bind(self):
        # HTTPServer's default hostname lookup can consult external DNS. This
        # preview uses its literal loopback address and never needs discovery.
        TCPServer.server_bind(self)
        self.server_name = '127.0.0.1'
        self.server_port = self.server_address[1]

    def server_close(self):
        self.stopping.set()
        super().server_close()


class PreviewHandler(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def setup(self):
        super().setup()
        self.connection.settimeout(10)

    def log_message(self, format, *args):
        pass

    def send_payload(self, status: int, body: bytes, content_type: str):
        self.send_response(status)
        self.send_header('Content-Type', content_type)
        self.send_header('Content-Length', str(len(body)))
        self.send_header('Cache-Control', 'no-store')
        self.send_header('X-Content-Type-Options', 'nosniff')
        self.send_header('X-Demo-Synthetic', 'true')
        self.send_header('Referrer-Policy', 'no-referrer')
        self.send_header('Connection', 'close')
        if content_type.startswith('text/html'):
            self.send_header('Content-Security-Policy', "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; font-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'")
        self.end_headers()
        self.close_connection = True
        if self.command != 'HEAD':
            self.wfile.write(body)

    def send_json(self, status: int, data: dict):
        self.send_payload(status, json.dumps(data, allow_nan=False).encode(), 'application/json; charset=utf-8')

    def local_request(self) -> bool:
        port = self.server.server_port
        allowed = {f'127.0.0.1:{port}', f'localhost:{port}'}
        return self.headers.get('Host', '') in allowed and (
            not self.headers.get('Origin') or self.headers['Origin'] in {f'http://{host}' for host in allowed})

    def do_GET(self):
        try:
            if not self.local_request():
                self.send_json(403, {'error': 'Dashboard preview accepts only its local origin'})
                return
            parsed = urlsplit(self.path)
            query = parse_qs(parsed.query, keep_blank_values=True)
            chosen = query.get('scenario', ['normal'])
            if len(chosen) != 1 or chosen[0] not in SCENARIOS:
                self.send_json(400, {'error': 'Choose normal, halted or recovery'})
                return
            scenario = chosen[0]
            if parsed.path == '/':
                self.send_payload(200, demo_html(self.server.root, scenario), 'text/html; charset=utf-8')
            elif parsed.path.startswith('/static/'):
                relative = unquote(parsed.path[len('/static/'):])
                if relative not in STATIC_ASSETS:
                    self.send_json(404, {'error': 'Static file not found'})
                    return
                static = self.server.root / 'static'
                source = static / relative
                candidate = source.resolve()
                components = [static.joinpath(*Path(relative).parts[:n])
                              for n in range(len(Path(relative).parts) + 1)]
                if any(part.is_symlink() for part in components) or not candidate.is_relative_to(static) or not candidate.is_file():
                    self.send_json(404, {'error': 'Static file not found'})
                    return
                if candidate.stat().st_size > 10 * 1024 * 1024:
                    self.send_json(413, {'error': 'Static file exceeds demo limit'})
                    return
                self.send_payload(200, candidate.read_bytes(), mimetypes.guess_type(candidate.name)[0] or 'application/octet-stream')
            elif parsed.path == '/api/stream':
                tokens = query.get('stream_token', [])
                if len(tokens) != 1 or tokens[0] not in {f'dashboard-preview.{key}' for key in SCENARIOS}:
                    self.send_json(401, {'error': 'Synthetic stream token required'})
                    return
                token_scenario = tokens[0].split('.', 1)[1]
                if 'scenario' in query and token_scenario != scenario:
                    self.send_json(400, {'error': 'Demo scenario and token disagree'})
                    return
                self.stream(token_scenario)
            elif parsed.path.startswith('/api/'):
                if parsed.path not in ('/api/health', '/api/ready') and self.headers.get('x-api-key') != READ_KEY:
                    self.send_json(401, {'error': 'Use the public synthetic demo key dashboard-preview'})
                    return
                data = payload(parsed.path, scenario, datetime.now(timezone.utc), self.server.anchor)
                self.send_json(404 if data is None else 200, data or {'error': 'No synthetic fixture for this endpoint'})
            else:
                self.send_json(404, {'error': 'Not found'})
        except (OSError, ValueError) as error:
            if isinstance(error, (BrokenPipeError, ConnectionResetError)):
                return
            self.send_json(500, {'error': 'Dashboard preview could not render its local fixture'})

    def do_HEAD(self):
        self.do_GET()

    def refuse_write(self):
        self.send_response(405)
        self.send_header('Allow', 'GET, HEAD')
        self.send_header('Content-Type', 'application/json')
        body = b'{"error":"Read-only synthetic demo: all writes are refused","demo_synthetic":true}'
        self.send_header('Content-Length', str(len(body)))
        self.send_header('Connection', 'close')
        self.end_headers()
        self.close_connection = True
        self.wfile.write(body)

    do_POST = do_PUT = do_PATCH = do_DELETE = do_OPTIONS = do_TRACE = do_CONNECT = refuse_write

    def stream(self, scenario: str):
        if self.command == 'HEAD':
            self.send_payload(200, b'', 'text/event-stream')
            return
        if not self.server.stream_slots.acquire(blocking=False):
            self.send_json(503, {'error': 'Synthetic stream slots are full'})
            return
        try:
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Cache-Control', 'no-store')
            self.send_header('X-Demo-Synthetic', 'true')
            self.send_header('Connection', 'close')
            self.end_headers()
            self.close_connection = True
            # Finite sessions reconnect automatically; no abandoned browser
            # can hold a sample thread forever.
            until = time.monotonic() + 30
            while not self.server.stopping.is_set() and time.monotonic() < until:
                state = samples(scenario, datetime.now(timezone.utc), self.server.anchor)['snapshot']
                state.update(demo_synthetic=True, demo_read_only=True, demo_scenario=scenario,
                             demo_evidence_note=EVIDENCE_NOTE)
                self.wfile.write(('data: ' + json.dumps(state, allow_nan=False) + '\n\n').encode())
                self.wfile.flush()
                self.server.stopping.wait(1)
        except (OSError, ValueError):
            pass
        finally:
            self.server.stream_slots.release()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--port', type=int, default=18082)
    args = parser.parse_args()
    if not 1 <= args.port <= 65535:
        parser.error('--port must be between 1 and 65535')
    try:
        with PreviewServer(args.port) as server:
            print(f'Synthetic, read-only dashboard preview: http://127.0.0.1:{server.server_port}/', flush=True)
            print(EVIDENCE_NOTE + ' Stop with Ctrl+C.', flush=True)
            server.serve_forever()
    except KeyboardInterrupt:
        return 0
    except OSError as error:
        print(f'Dashboard preview could not start: {error}')
        return 1
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
