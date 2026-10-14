// Dashboard behaviour: transport, auth, state and rendering.
// Extracted verbatim from index.html. Loaded at the same position in the
// document, after api-auth.js, so execution order is unchanged.
'use strict';

// ── State ────────────────────────────────────────────────────────────────────
let allTrades = [];
let currentConfig = {};
let pnlChart = null;
let btcChart = null;
let btcStartPriceVal = 0;

// Real-time BTC price ring buffer for chart (fed by SSE)
const BTC_CHART_MAX_POINTS = 300; // ~5 minutes at 1 point/sec
const btcPriceBuffer = [];        // [{ts: Date, price: number}]
let btcChartInitialized = false;   // true after first REST load or first SSE price
let sseSource = null;
let reconnectTimer = null;
let lastSseAt = 0;
// Back off on repeated stream failures instead of polling at a fixed interval,
// so a server that stays down is not hit every few seconds indefinitely. The
// delay resets as soon as a stream actually opens. A missing credential keeps a
// higher floor, because recovery there waits on a human pasting a key.
const SSE_RETRY_MIN_MS = 3000;
const SSE_RETRY_AUTH_MIN_MS = 10000;
const SSE_RETRY_MAX_MS = 30000;
let sseRetryDelay = 0;

function scheduleSseRetry(authIssue) {
  const floor = authIssue ? SSE_RETRY_AUTH_MIN_MS : SSE_RETRY_MIN_MS;
  const delay = Math.min(Math.max(floor, sseRetryDelay), SSE_RETRY_MAX_MS);
  sseRetryDelay = Math.min(delay * 2, SSE_RETRY_MAX_MS);
  clearTimeout(reconnectTimer);
  reconnectTimer = setTimeout(connectSSE, delay);
  return delay;
}
let activeTab = 'overview';
let initialLoadComplete = false;
let executionQualityBusy = false;
let riskPanelBusy = false;
let integrityBusy = false;
let loopRunningState = null;
let researchOnlyState = false;
let runtimeModeState = '';
let currentExecModeState = '';
let makerHoldState = null;   // from the stream; null until the first payload
let makerHandsOffMarkets = [];   // markets a person closed by hand; no quotes there until they roll
let makerVenueRefusal = null;    // the venue_min_size reason when the quote size is under the venue minimum

// ── Utility ──────────────────────────────────────────────────────────────────
const ledgerEsc = value => String(value ?? '')
  .replace(/&/g, '&amp;')
  .replace(/</g, '&lt;')
  .replace(/>/g, '&gt;')
  .replace(/"/g, '&quot;');

const fmt$ = v => {
  if (v === null || v === undefined) return '—';
  const n = Number(v);
  if (!Number.isFinite(n)) return '—';
  const sign = n > 0 ? '+' : n < 0 ? '-' : '';
  return sign + '$' + Math.abs(n).toFixed(2);
};
const fmtMoney = v => {
  if (v === null || v === undefined) return '—';
  const n = Number(v);
  // fmtMs, fmtSigned, fmtBps and fmtEtaSeconds all guard this; these three did
  // not, so anything non-numeric rendered as "$NaN" rather than as absent.
  if (!Number.isFinite(n)) return '—';
  return '$' + n.toFixed(2);
};
const fmtPct = v => {
  if (v === null || v === undefined) return '—';
  const n = Number(v);
  if (!Number.isFinite(n)) return '—';
  return n.toFixed(1) + '%';
};
const fmtN = (v, dp = 2) => {
  if (v === null || v === undefined) return '—';
  const n = Number(v);
  if (!Number.isFinite(n)) return '—';
  return n.toFixed(dp);
};
const fmtMs = v => {
  if (v === null || v === undefined) return '—';
  const n = Number(v);
  if (!Number.isFinite(n)) return '—';
  if (n >= 1000) return (n / 1000).toFixed(2) + 's';
  return n.toFixed(0) + 'ms';
};
const fmtSigned = (v, dp = 1, suffix = '') => {
  if (v === null || v === undefined) return '—';
  const n = Number(v);
  if (!Number.isFinite(n)) return '—';
  const sign = n > 0 ? '+' : '';
  return `${sign}${n.toFixed(dp)}${suffix}`;
};
const fmtEtaSeconds = v => {
  if (v === null || v === undefined) return '—';
  const n = Math.max(0, Number(v));
  if (!Number.isFinite(n)) return '—';
  if (n >= 3600) return `${(n / 3600).toFixed(1)}h`;
  if (n >= 60) return `${Math.round(n / 60)}m`;
  return `${Math.round(n)}s`;
};
const fmtCandidateLabel = label => {
  if (!label) return '—';
  return String(label)
    .replace(/^widen_(\d+)bps$/i, 'WIDEN $1bps')
    .replace(/_/g, ' ')
    .toUpperCase();
};
const fmtTruthLabel = label => {
  if (!label) return '—';
  return String(label)
    .replace(/^l2_stale\((.+)\)$/i, 'l2_stale $1')
    .replace(/[()]/g, ' ')
    .replace(/_/g, ' ')
    .replace(/\s+/g, ' ')
    .trim()
    .toUpperCase();
};
const truthTone = label => {
  const s = String(label || '').toLowerCase();
  if (!s || s === '—') return '';
  if ([
    'success',
    'active',
    'quoteable_books',
    'ready',
    'proven',
    'single'
  ].includes(s)) return 'maker-live';
  if ([
    'dead_window',
    'no_target_markets',
    'global_l2_feed_stale',
    'l2_feed_stale',
    'l2_stale',
    'token_bbo_stale',
    'stale_timeout',
    'warmup_timeout',
    'session_error',
    'actor_backpressure',
    'market_quality',
    'market_quality_failure',
    'lifecycle_bug',
    'broken'
  ].includes(s)) return 'maker-stale';
  if ([
    'no_snapshot',
    'snapshot_missing',
    'one_sided_books',
    'one_sided_book',
    'execution_mode_taker',
    'inactive',
    'inactive/unexercised',
    'policy_suppression',
    'low_quoteability',
    'low_activity',
    'parked',
    'pending',
    'warmup_partial',
    'warmup_awaiting_snapshots',
    'rearm_wait'
  ].includes(s)) return 'maker-warn';
  return '';
};
const topReasonSummary = reasons => {
  const items = Array.isArray(reasons) ? reasons : [];
  if (!items.length) return '—';
  return items
    .slice(0, 3)
    .map(item => `${item.count || 0}× ${fmtTruthLabel(item.label || 'unknown')}`)
    .join(' · ');
};
const shortId = (s, n = 10) => {
  if (!s) return '—';
  const str = String(s);
  return str.length > n ? str.slice(0, n) + '…' : str;
};
const clsPnl = v => {
  if (v === null || v === undefined) return 'pnl-zero';
  return Number(v) > 0 ? 'pnl-pos' : Number(v) < 0 ? 'pnl-neg' : 'pnl-zero';
};
const tsShort = ts => {
  if (!ts) return '—';
  try { return new Date(ts).toLocaleTimeString([], {hour:'2-digit', minute:'2-digit', second:'2-digit'}); }
  catch { return ts.slice(0, 19); }
};
const fmtBps = v => {
  if (v === null || v === undefined) return '—';
  const n = Number(v);
  if (!Number.isFinite(n)) return '—';
  return (n >= 0 ? '+' : '') + n.toFixed(1) + 'bps';
};

function activeReadCooldownMs() {
  const now = Date.now();
  let remaining = 0;
  for (const until of readCooldownUntil.values()) {
    remaining = Math.max(remaining, Number(until || 0) - now);
  }
  return remaining > 0 ? remaining : 0;
}

function computeReadLoopSummary() {
  if (document.hidden) {
    return { label: 'BACKGROUND PAUSED', tone: 'maker-warn', detail: 'Foreground the tab to resume deferred reads.' };
  }
  const cooldownMs = activeReadCooldownMs();
  if (cooldownMs > 0) {
    return {
      label: `BACKOFF ${Math.ceil(cooldownMs / 1000)}S`,
      tone: 'maker-warn',
      detail: 'Read retries are honoring API backoff before polling again.',
    };
  }
  if (sseSource && lastSseAt && (Date.now() - lastSseAt) <= 5_000) {
    return { label: 'SSE LIVE', tone: 'maker-live', detail: 'Realtime stream is feeding the dashboard.' };
  }
  if (lastSseAt && (Date.now() - lastSseAt) <= 15_000) {
    return { label: 'SSE QUIET', tone: 'maker-warn', detail: 'Stream is connected but the last event is getting old.' };
  }
  return { label: 'FALLBACK POLL', tone: 'maker-stale', detail: 'The dashboard is relying on periodic REST refreshes.' };
}

// Whether trading is currently permitted. Tracked at module scope because the
// page header states it and the risk panel states it, and until now only the
// risk panel knew: the header cheerfully reported "Loop running" while the
// kill switch was engaged and nothing was allowed to trade.
let killSwitchState = null;
let degradedState = null;
let killSwitchCause = null;

/// A badge for a boolean condition.
///
/// These used to render through statusBadge(cond ? 'mismatch' : 'ok'), a helper
/// built for execution parity where "mismatch" is a real outcome. Reused here
/// it printed the literal word "mismatch" beside the kill switch, which reads
/// as "the dashboard and the bot disagree" when in fact nothing disagreed and
/// the switch was simply on. Say the state instead.
function flagBadge(isBad, badWord, goodWord) {
  const cls = isBad ? 'eq-status eq-mismatch' : 'eq-status eq-ok';
  return `<span class="${cls}">${isBad ? badWord : goodWord}</span>`;
}

function statusBadge(status) {
  const s = String(status || '').toLowerCase();
  if (s === 'ok' || s === 'match' || s === 'resolved') {
    return `<span class="eq-status eq-ok">${ledgerEsc(status || 'ok')}</span>`;
  }
  if (s === 'mismatch') {
    return `<span class="eq-status eq-mismatch">${ledgerEsc(status || 'mismatch')}</span>`;
  }
  return `<span class="eq-status eq-incomplete">${ledgerEsc(status || 'incomplete')}</span>`;
}

// ── Toast notifications ───────────────────────────────────────────────────────
function toast(msg, type = 'info') {
  const colors = { success: 'var(--clr-green)', danger: 'var(--clr-red)', info: 'var(--clr-blue)', warning: 'var(--clr-yellow)' };
  const div = document.createElement('div');
  div.innerHTML = `
    <div class="toast align-items-center border-0 mb-1" role="alert" style="background:var(--clr-surface); border-left:3px solid ${colors[type]||colors.info} !important; border:1px solid var(--clr-border);">
      <div class="d-flex">
        <div class="toast-body" style="font-size:var(--fs-md);">${ledgerEsc(msg)}</div>
        <button type="button" class="btn-close btn-close-white me-2 m-auto" data-bs-dismiss="toast"></button>
      </div>
    </div>`;
  const el = div.firstElementChild;
  document.getElementById('toast-container').appendChild(el);
  const t = new bootstrap.Toast(el, { delay: 3500 });
  el.addEventListener('hidden.bs.toast', () => {
    // Bootstrap keeps a strong element-to-instance map until disposal.
    t.dispose();
    el.remove();
  }, { once: true });
  t.show();
}

// ── API helpers ───────────────────────────────────────────────────────────────
// Stored dashboard credentials belong to this origin. A URL query must never
// redirect them to another host; serve API and UI together behind a proxy.
const API_BASE = (() => {
  const param = new URLSearchParams(window.location.search).get('api');
  if (!param) return '';
  try {
    const target = new URL(param, window.location.href);
    if (target.origin !== window.location.origin) return '';
    return target.origin;
  } catch { return ''; }
})();

function apiUrl(path) {
  if (!path) return path;
  const target = new URL(API_BASE ? `${API_BASE}${path}` : path, window.location.href);
  if (target.origin !== window.location.origin) throw new Error('Dashboard API must use this page’s origin');
  return target.href;
}

const {
  persistApiAuthFromUrl,
  getApiAuth,
  setApiAuth,
  clearApiAuth,
  buildAuthHeaders,
} = globalThis.macbotApiAuth;

function updateDashboardAuthUi() {
  const { apiKey, apiSecret } = getApiAuth();
  const button = document.getElementById('btn-dashboard-auth');
  const label = document.getElementById('dashboard-auth-label');
  const lockButton = document.getElementById('btn-dashboard-lock');
  if (button) {
    button.classList.toggle('is-read', Boolean(apiKey && !apiSecret));
    button.classList.toggle('is-write', Boolean(apiKey && apiSecret));
    button.title = apiKey
      ? (apiSecret ? 'Live data and protected controls are unlocked' : 'Live data is unlocked; protected controls are locked')
      : 'Unlock live data for this browser';
  }
  if (label) label.textContent = apiKey ? (apiSecret ? 'Controls unlocked' : 'Live unlocked') : 'Unlock';
  if (lockButton) lockButton.style.display = apiKey || apiSecret ? '' : 'none';
}

function openDashboardUnlock() {
  const modalEl = document.getElementById('dashboard-auth-modal');
  if (!modalEl) return;
  document.getElementById('dashboard-api-key').value = '';
  document.getElementById('dashboard-api-secret').value = '';
  document.getElementById('dashboard-auth-remember').checked = false;
  document.getElementById('dashboard-auth-error').textContent = '';
  updateDashboardAuthUi();
  const modal = bootstrap.Modal.getOrCreateInstance(modalEl);
  modal.show();
  modalEl.addEventListener('shown.bs.modal', () => document.getElementById('dashboard-api-key')?.focus(), { once: true });
}

async function submitDashboardUnlock(event) {
  event.preventDefault();
  const keyInput = document.getElementById('dashboard-api-key');
  const secretInput = document.getElementById('dashboard-api-secret');
  const rememberInput = document.getElementById('dashboard-auth-remember');
  const error = document.getElementById('dashboard-auth-error');
  const submit = document.getElementById('btn-dashboard-unlock');
  const apiKey = keyInput.value.trim();
  const apiSecret = secretInput.value;
  if (!apiKey) {
    error.textContent = 'Enter the API key configured for this bot.';
    keyInput.focus();
    return;
  }

  error.textContent = '';
  submit.disabled = true;
  submit.textContent = 'Checking…';
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 15000);
  try {
    // Validate read access without sending or persisting the optional secret.
    const response = await fetch(apiUrl('/api/config'), {
      cache: 'no-store',
      headers: { 'x-api-key': apiKey },
      signal: controller.signal,
    });
    if (!response.ok) {
      error.textContent = response.status === 401 || response.status === 403
        ? 'That API key was rejected.'
        : `The bot could not verify the key (HTTP ${response.status}).`;
      return;
    }
    if (!setApiAuth(apiKey, apiSecret, rememberInput.checked)) {
      error.textContent = 'This browser blocked credential storage.';
      return;
    }
    window.location.reload();
  } catch {
    error.textContent = 'Could not reach the bot. Check that it is running and try again.';
  } finally {
    clearTimeout(timeout);
    submit.disabled = false;
    submit.textContent = 'Unlock';
  }
}

function lockDashboardBrowser() {
  clearApiAuth();
  window.location.reload();
}

function hasWriteAuth() {
  const auth = getApiAuth();
  return Boolean(auth.apiKey && auth.apiSecret && window.crypto?.subtle);
}

function updateWriteAuthUi() {
  updateDashboardAuthUi();
  const select = document.getElementById('nav-exec-mode');
  const hint = document.getElementById('nav-exec-auth-hint');
  const canWrite = hasWriteAuth();
  if (select) {
    select.dataset.writeAuth = canWrite ? 'ready' : 'missing';
    select.title = canWrite
      ? 'Execution mode can be changed from this browser session.'
      : 'Execution mode changes require an API key and secret. Use Unlock to add them.';
  }
  if (hint) {
    hint.style.display = canWrite ? 'none' : 'inline';
    hint.textContent = canWrite ? '' : 'controls locked';
    hint.title = canWrite
      ? ''
      : 'Use Unlock to add the API key and secret for protected controls.';
  }
}

function explainWriteAuthRequirement(actionLabel = 'This action') {
  updateWriteAuthUi();
  toast(
    `${actionLabel} requires control access. Use Unlock to add the API key and secret.`,
    'warning'
  );
}

let cachedStreamAuth = { token: '', expiresAt: 0 };
let dashboardAuthHintShown = false;
const READ_RATE_LIMIT_BACKOFF_MS = 10_000;
const inflightReadRequests = new Map();
const readCooldownUntil = new Map();
const deferredReadTimers = new Set();
let deferredInitialReadsScheduled = false;

function titleCase(s) {
  return String(s || '').charAt(0).toUpperCase() + String(s || '').slice(1).toLowerCase();
}

function isAuthFailure(err) {
  // A rejected credential will not fix itself. Anything else (offline, DNS,
  // proxy, 5xx) is transient and must keep retrying.
  const status = err?.status;
  if (status === 401 || status === 403) return true;
  if (typeof status === 'number') return false;
  const msg = String(err?.message || err || '');
  return /\b(401|403)\b/.test(msg) || msg.toLowerCase().includes('auth failed');
}


function isBenignReadAbort(err) {
  const msg = String(err?.message || err || '');
  return msg.includes('Failed to fetch')
    || msg.includes('NetworkError')
    || msg.includes('The user aborted a request');
}

function logReadError(context, err) {
  if (isBenignReadAbort(err)) return;
  console.error(`${context}:`, err);
}

function shouldPollReadLoop() {
  return !document.hidden;
}

function runReadTask(task) {
  return Promise.resolve()
    .then(() => task())
    .catch(() => {});
}

function scheduleDeferredRead(task, delayMs = 0) {
  const timer = setTimeout(() => {
    deferredReadTimers.delete(timer);
    if (!shouldPollReadLoop()) return;
    void runReadTask(task);
  }, Math.max(0, delayMs));
  deferredReadTimers.add(timer);
  return timer;
}

function scheduleInitialDeferredReads() {
  if (deferredInitialReadsScheduled) return;
  deferredInitialReadsScheduled = true;

  // The markets table this used to feed was removed in the redesign; its
  // loader kept fetching /api/markets on every load and rendering into an
  // element that no longer existed.
  scheduleDeferredRead(() => Promise.allSettled([
    loadBtcChart(),
  ]), 350);

  scheduleDeferredRead(() => Promise.allSettled([
    loadNoArb(),
  ]), 900);

  scheduleDeferredRead(() => Promise.allSettled([
    loadIntegritySummary(),
  ]), 1_600);

  if (activeTab === 'maker') {
    scheduleDeferredRead(() => Promise.allSettled([
      loadMakerAbSummary(),
      loadMakerRolloutStatus(),
    ]), 2_200);
  }

  if (activeTab === 'overview') {
    scheduleDeferredRead(() => Promise.allSettled([
      loadMlState(),
      loadDeepModelState(),
    ]), 3_000);
  }
}

function updateDashboardStrip() {
  const boundaryEl = document.getElementById('dash-session-boundary');
  const boundarySubEl = document.getElementById('dash-session-boundary-sub');
  const sessionSummaryEl = document.getElementById('dash-session-summary');
  const sessionSubEl = document.getElementById('dash-session-sub');
  const archiveSummaryEl = document.getElementById('dash-archive-summary');
  const archiveSubEl = document.getElementById('dash-archive-sub');
  const controlSummaryEl = document.getElementById('dash-control-summary');
  const controlSubEl = document.getElementById('dash-control-sub');
  if (!boundaryEl || !boundarySubEl || !sessionSummaryEl || !sessionSubEl || !archiveSummaryEl || !archiveSubEl || !controlSummaryEl || !controlSubEl) {
    return;
  }

  if (!sessionStartId && !sessionStartAt) {
    boundaryEl.textContent = 'All history';
    boundaryEl.className = 'v text-muted';
    boundarySubEl.textContent = 'No persisted session cutoff. Current history spans the full DB.';
  } else {
    const boundaryBits = [];
    if (sessionStartId) boundaryBits.push(`#${sessionStartId}`);
    if (sessionStartAt) boundaryBits.push(tsShort(sessionStartAt));
    boundaryEl.textContent = boundaryBits.join(' · ') || 'SESSION CUT';
    boundaryEl.className = 'v maker-live';
    boundarySubEl.textContent = sessionStartAt
      ? `Persisted restart-safe boundary from ${new Date(sessionStartAt).toLocaleString([], { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' })}.`
      : 'Persisted trade-id cutoff with no timestamp metadata.';
  }

  const currentTrades = currentSessionTrades(allTrades);
  const currentClosed = currentTrades.filter(t => t.status === 'CLOSED');
  const currentOpen = currentTrades.filter(t => t.status === 'OPEN' || t.status === 'CLOSING');
  const currentClosedPnl = sumRealizedNetPnl(currentClosed);
  sessionSummaryEl.textContent = `${currentTrades.length} trades · ${currentOpen.length} open`;
  sessionSummaryEl.className = `v ${currentTrades.length ? '' : 'text-muted'}`.trim();
  sessionSubEl.textContent = `${currentClosed.length} closed · ${fmt$(currentClosedPnl)} closed P&L, net`;
  sessionSubEl.className = `sub ${currentClosedPnl > 0 ? 'maker-live' : currentClosedPnl < 0 ? 'maker-stale' : ''}`.trim();

  const archived = mergeArchivedTrades(archivedTrades, getBoundaryArchivedTrades());
  const archivedPnl = sumRealizedNetPnl(archived);
  archiveSummaryEl.textContent = archived.length ? `${archived.length} trades` : 'None';
  archiveSummaryEl.className = `v ${archived.length ? '' : 'text-muted'}`.trim();
  archiveSubEl.textContent = archived.length
    ? `${fmt$(archivedPnl)} archived P&L`
    : 'No prior-session trades are currently parked.';
  archiveSubEl.className = `sub ${archivedPnl > 0 ? 'maker-live' : archivedPnl < 0 ? 'maker-stale' : ''}`.trim();

  const controlBits = [];
  if (researchOnlyState) controlBits.push('Research demo');
  if (runtimeModeState) controlBits.push(titleCase(String(runtimeModeState)));
  if (currentExecModeState) controlBits.push(titleCase(String(currentExecModeState)));
  // A maker that holds and a maker that flattens are different strategies
  // wearing the same mode name. Say which one is running.
  if (['maker', 'hybrid'].includes(String(currentExecModeState || '').toLowerCase()) && makerHoldState !== null) {
    controlBits.push(makerHoldState ? 'Holding' : 'Flattening');
  }
  // After a manual close the maker leaves that market alone until it rolls.
  // Without saying so, the page looks idle for no reason.
  const liveMarketId = window._lastAnalysis?.market_data?.market_id;
  if (liveMarketId && makerHandsOffMarkets.includes(String(liveMarketId))) {
    controlBits.push('Standing down after manual close');
  }
  // The venue will not take an order under its minimum, and the simulator
  // no longer pretends otherwise. This is the operator's knob, so it goes
  // where the operator looks.
  if (makerVenueRefusal) {
    controlBits.push('Quote size under venue minimum — raise Quote Size in Config › Maker');
  }

  // A running loop with the kill switch engaged is not trading. Reporting only
  // the loop meant this card read "Loop running" in green while every entry was
  // blocked, which is the most consequential thing this page can get wrong.
  const halted = killSwitchState === true;
  if (halted) {
    controlBits.push('Halted by kill switch');
  } else if (degradedState === true) {
    controlBits.push('Degraded, not trading');
  } else {
    controlBits.push(loopRunningState === null ? 'Loop unknown' : (loopRunningState ? 'Loop running' : 'Loop stopped'));
  }
  controlSummaryEl.textContent = controlBits.join(' · ');
  controlSummaryEl.className = `v ${
    halted ? 'maker-stale' : degradedState === true ? 'maker-warn' : loopRunningState ? 'maker-live' : 'maker-warn'
  }`.trim();

  const readState = computeReadLoopSummary();
  controlSubEl.textContent = `${readState.label} · ${titleCase(String(activeTab || 'overview'))} tab · ${readState.detail}`;
  controlSubEl.className = `sub ${readState.tone}`.trim();
}

function readRequestKey(method, path) {
  return `${String(method).toUpperCase()}:${path}`;
}

function retryAfterMs(headerValue) {
  const secs = Number(headerValue);
  if (Number.isFinite(secs) && secs > 0) {
    return Math.min(Math.max(secs * 1000, 1000), 60_000);
  }
  return READ_RATE_LIMIT_BACKOFF_MS;
}

function makeRateLimitError(path, waitMs) {
  const err = new Error(`429: rate limit cooldown ${Math.ceil(waitMs / 1000)}s`);
  err.status = 429;
  err.path = path;
  err.cooldownMs = waitMs;
  return err;
}

function notifyDashboardAuthRequired(context) {
  if (!dashboardAuthHintShown) {
    dashboardAuthHintShown = true;
    toast('This browser is locked. Use Unlock to load live data.', 'warning');
  }
  if (context === 'stream') {
    document.getElementById('sse-dot').className = '';
    document.getElementById('sse-status').textContent = 'browser locked';
  }
}

async function getStreamToken() {
  const auth = getApiAuth();
  if (!auth.apiKey) return '';
  if (cachedStreamAuth.token && cachedStreamAuth.expiresAt > Date.now() + 5000) {
    return cachedStreamAuth.token;
  }

  const payload = await api('GET', '/api/stream/token');
  cachedStreamAuth = {
    token: String(payload.token || ''),
    expiresAt: Date.now() + (Number(payload.ttl_seconds || 120) * 1000),
  };
  return cachedStreamAuth.token;
}

async function streamUrl(path) {
  const url = new URL(apiUrl(path), window.location.href);
  const token = await getStreamToken();
  if (token) {
    url.searchParams.set('stream_token', token);
  }
  return url.toString();
}

persistApiAuthFromUrl();
updateWriteAuthUi();

// ── Per-source freshness ────────────────────────────────────────────────────
// The stale mark used to read from a hardcoded list of four tabs, so the five
// fed by their own polled endpoints — maker, execution, risk, report, config —
// drew last-known values with nothing saying how old they were, no matter how
// long ago they were fetched. A panel is now marked from the age of the
// endpoint that actually feeds it, and adding a tab without a source here
// leaves it unmarked rather than silently claiming freshness it does not have.
// ── One price per token ─────────────────────────────────────────────────────
// The same token was shown at three prices from three sources: the analysis
// snapshot (up to a cycle old), a per-row REST fetch for open positions, and
// the L2 WebSocket top-of-book on the stream. A reader saw 0.78 in one panel
// and 0.83 in another with nothing saying which was older. The L2 book is the
// freshest and is already on every payload; every price on the page prefers
// it while it is fresh, and says where it came from otherwise.
const LIVE_BOOK_MAX_AGE_MS = 5000;
const liveBook = new Map();   // token_id → { mid, bid, ask, isYes, marketId, at }

function expireLiveBook(now = Date.now()) {
  let changed = false;
  for (const [tokenId, e] of liveBook) {
    if (now - e.at > LIVE_BOOK_MAX_AGE_MS || e.at > now) {
      liveBook.delete(tokenId);
      changed = true;
    }
  }
  return changed;
}

function noteLiveBook(entries, options = null) {
  if (options?.feedStale === true) {
    liveBook.clear();
    return;
  }
  const now = Date.now();
  expireLiveBook(now);
  if (!Array.isArray(entries)) return;
  for (const e of entries) {
    if (!e?.token_id) continue;
    const tokenId = String(e.token_id);
    const isPrice = value => (typeof value === 'number' || (typeof value === 'string' && value.trim() !== ''));
    const bid = Number(e?.best_bid), ask = Number(e?.best_ask);
    const at = typeof e.updated_at === 'string' ? Date.parse(e.updated_at) : NaN;
    if (e.stale || e.snapshot_loaded === false || !isPrice(e.best_bid) || !isPrice(e.best_ask)
        || !Number.isFinite(bid) || !Number.isFinite(ask) || bid < 0 || ask > 1 || ask < bid
        || !Number.isFinite(at) || at > now || now - at > LIVE_BOOK_MAX_AGE_MS) {
      liveBook.delete(tokenId);
      continue;
    }
    liveBook.set(tokenId, {
      mid: (bid + ask) / 2, bid, ask,
      isYes: typeof e.is_yes_token === 'boolean' ? e.is_yes_token : null,
      marketId: e.market_id ? String(e.market_id) : null, at,
    });
  }
}

// The live mid for a token, or null when the book has nothing fresh for it.
function liveMidFor(tokenId, now = Date.now()) {
  const e = tokenId ? liveBook.get(String(tokenId)) : null;
  return e && now >= e.at && now - e.at <= LIVE_BOOK_MAX_AGE_MS ? e.mid : null;
}

// Each displayed live price needs that outcome token's own fresh book.
function liveOddsFor(marketId, now = Date.now()) {
  if (!marketId) return null;
  let yes = null, no = null;
  for (const e of liveBook.values()) {
    if (e.marketId !== String(marketId) || e.at > now || now - e.at > LIVE_BOOK_MAX_AGE_MS) continue;
    if (e.isYes === true) yes = e.mid;
    else if (e.isYes === false) no = e.mid;
  }
  if (yes === null && no === null) return null;
  return { yes, no };
}

const PANEL_SOURCES = [
  { tab: 'maker',     prefix: '/api/maker/',          budgetMs: 150000 },
  { tab: 'execution', prefix: '/api/trades/',         budgetMs:  60000 },
  { tab: 'risk',      prefix: '/api/risk/',           budgetMs:  30000 },
  { tab: 'report',    prefix: '/api/report',          budgetMs: 180000 },
  { tab: 'config',    prefix: '/api/config',          budgetMs: 900000 },
];
const sourceFetchedAt = new Map();

function notePathFetched(path) {
  const p = String(path || '');
  for (const src of PANEL_SOURCES) {
    if (p.startsWith(src.prefix)) sourceFetchedAt.set(src.tab, Date.now());
  }
}

// Never fetched counts as not fresh: a panel that has never loaded is showing
// nothing, and nothing should not look like a current reading.
function panelSourceIsStale(tab, now = Date.now()) {
  const src = PANEL_SOURCES.find(s => s.tab === tab);
  if (!src) return false;
  const at = sourceFetchedAt.get(tab);
  if (at == null) return true;
  return now - at > src.budgetMs;
}

async function api(method, path, body, idempotencyKey = '') {
  const verb = String(method).toUpperCase();
  const key = verb === 'GET' ? readRequestKey(verb, path) : '';
  if (key) {
    const inflight = inflightReadRequests.get(key);
    if (inflight) return inflight;
    const until = readCooldownUntil.get(key) || 0;
    if (until > Date.now()) {
      throw makeRateLimitError(path, until - Date.now());
    }
  }

  const run = async () => {
    const opts = { method, headers: {} };
    if (body !== undefined) {
      opts.headers['Content-Type'] = 'application/json';
      opts.body = JSON.stringify(body);
    }
    Object.assign(opts.headers, await buildAuthHeaders(method, path, opts.body || '', idempotencyKey));
    // Bound reads so a dropped HTTP connection cannot strand an in-flight
    // dedupe entry or the dashboard's initial load forever. Mutating requests
    // keep their existing outcome handling; a timeout is not permission to retry.
    const controller = verb === 'GET' ? new AbortController() : null;
    if (controller) opts.signal = controller.signal;
    const timeout = controller ? setTimeout(() => controller.abort(), 15_000) : null;
    try {
      const r = await fetch(apiUrl(path), opts);
      if (!r.ok) {
        const text = await r.text().catch(() => r.statusText);
        if (key && r.status === 429) {
          const cooldownMs = retryAfterMs(r.headers.get('retry-after'));
          readCooldownUntil.set(key, Date.now() + cooldownMs);
        }
        const err = new Error(`${r.status}: ${text}`);
        err.status = r.status;
        err.path = path;
        throw err;
      }
      if (key) readCooldownUntil.delete(key);
      if (verb === 'GET') notePathFetched(path);
      return await r.json();
    } finally {
      if (timeout !== null) clearTimeout(timeout);
    }
  };

  if (!key) return run();
  const promise = run().finally(() => inflightReadRequests.delete(key));
  inflightReadRequests.set(key, promise);
  return promise;
}
const GET    = path       => api('GET',    path);
const POST   = (path, b, idempotencyKey = '') => api('POST', path, b, idempotencyKey);

function syncNavExecMode(mode) {
  const el = document.getElementById('nav-exec-mode');
  if (!el || !mode) return;
  const v = String(mode).toLowerCase();
  if (['taker', 'maker', 'hybrid'].includes(v)) {
    el.value = v;
    currentExecModeState = v;
    updateDashboardStrip();
  }
}

async function updateExecutionMode(mode) {
  const v = String(mode || '').toLowerCase();
  if (!['taker', 'maker', 'hybrid'].includes(v)) return;
  if (!hasWriteAuth()) {
    syncNavExecMode(currentConfig.execution_mode || '');
    explainWriteAuthRequirement('Execution mode changes');
    return;
  }
  try {
    const r = await POST('/api/config', { execution_mode: v });
    currentConfig = r;
    renderConfigPanel(currentConfig);
    syncNavExecMode(r.execution_mode || v);
    updateWriteAuthUi();
    toast(`Execution mode set to ${v.toUpperCase()}`, 'success');
  } catch (e) {
    syncNavExecMode(currentConfig.execution_mode || '');
    if (isAuthFailure(e) || String(e?.message || '').includes('invalid request signature')) {
      explainWriteAuthRequirement('Execution mode changes');
      return;
    }
    toast('Execution mode update failed: ' + e.message, 'danger');
  }
}

// ── Maker cockpit ────────────────────────────────────────────────────────────
async function loadMakerState() {
  try {
    const r = await GET('/api/maker/state');
    renderMakerState(r);
  } catch (e) {
    logReadError('loadMakerState', e);
  }
}

async function loadMakerAbSummary() {
  try {
    const r = await GET('/api/maker/ab-summary');
    renderMakerAbSummary(r);
  } catch (e) {
    logReadError('loadMakerAbSummary', e);
  }
}

async function loadMakerRolloutStatus() {
  try {
    const r = await GET('/api/maker/rollout-status');
    renderMakerRolloutStatus(r);
  } catch (e) {
    logReadError('loadMakerRolloutStatus', e);
  }
}

function renderMakerState(payload) {
  const maker = payload?.maker || {};
  const feedTruth = payload?.feed_truth || {};
  const noTradeTruth = payload?.no_trade_truth || {};
  const execMode = payload?.execution_mode || '—';
  const execModeRaw = String(payload?.execution_mode || '').toLowerCase();
  currentExecModeState = execModeRaw;
  const l2Stale = Boolean(payload?.l2_feed_stale ?? maker?.l2_stale);
  const l2Age = payload?.l2_feed_age_ms ?? null;
  const targetMarketCount = Number(feedTruth?.target_market_count ?? maker?.l2_summary?.target_market_count ?? maker?.market_count ?? 0);
  const tokenCount = Number(feedTruth?.token_count ?? maker?.l2_summary?.token_count ?? 0);
  const loadedCount = Number(feedTruth?.snapshot_loaded_count ?? maker?.l2_summary?.snapshot_loaded_count ?? 0);
  const twoSidedCount = Number(feedTruth?.two_sided_count ?? maker?.l2_summary?.two_sided_count ?? 0);
  const oneSidedCount = Number(feedTruth?.one_sided_count ?? maker?.l2_summary?.one_sided_count ?? 0);
  const staleCount = Number(feedTruth?.stale_count ?? maker?.l2_summary?.stale_count ?? 0);
  const quoteableCount = Number(feedTruth?.quoteable_count ?? maker?.l2_summary?.quoteable_count ?? 0);
  const primaryNotReadyReason = noTradeTruth?.primary_not_ready_reason || feedTruth?.primary_not_ready_reason || '';
  const notReadyReasons = Array.isArray(noTradeTruth?.not_ready_reasons)
    ? noTradeTruth.not_ready_reasons
    : (Array.isArray(feedTruth?.not_ready_reasons) ? feedTruth.not_ready_reasons : []);
  const l2StaleCode = noTradeTruth?.l2_stale_code || feedTruth?.l2_stale_code || '';
  const quotes = Array.isArray(maker?.quotes) ? maker.quotes : [];
  const bbo = Array.isArray(maker?.l2_top_of_book) ? maker.l2_top_of_book : [];
  const rollMicro = Array.isArray(maker?.roll_microstructure) ? maker.roll_microstructure : [];
  const telemetry = Array.isArray(maker?.recent_quote_telemetry) ? maker.recent_quote_telemetry : [];

  document.getElementById('maker-exec-mode').textContent = String(execMode).toUpperCase();
  syncNavExecMode(execModeRaw);

  const l2El = document.getElementById('maker-l2-status');
  const navL2 = document.getElementById('nav-l2-status');
  const navL2Age = document.getElementById('nav-l2-age');
  const isTaker = String(execMode).toLowerCase() === 'taker';
  if (isTaker) {
    l2El.textContent = 'N/A';
    l2El.className = 'v text-muted';
    if (navL2) {
      navL2.textContent = 'L2 N/A';
      navL2.className = 'text-muted';
    }
    if (navL2Age) navL2Age.textContent = 'taker';
    document.getElementById('maker-l2-age').textContent = 'taker';
  } else {
    l2El.textContent = l2Stale ? 'STALE' : 'LIVE';
    l2El.className = 'v ' + (l2Stale ? 'maker-stale' : 'maker-live');
    if (navL2) {
      navL2.textContent = l2Stale ? 'L2 STALE' : 'L2 LIVE';
      navL2.className = l2Stale ? 'maker-stale' : 'maker-live';
    }
    if (navL2Age) navL2Age.textContent = fmtMs(l2Age);
    document.getElementById('maker-l2-age').textContent = fmtMs(l2Age);
  }
  document.getElementById('maker-quote-count').textContent = String(maker?.quote_count ?? quotes.length ?? 0);
  document.getElementById('maker-replace-min').textContent = String(maker?.replace_events_last_min ?? 0);
  document.getElementById('maker-market-count').textContent = String(maker?.market_count ?? 0);
  const targetEl = document.getElementById('maker-target-markets');
  const booksEl = document.getElementById('maker-book-shape');
  const quoteableEl = document.getElementById('maker-quoteable-state');
  const noTradeEl = document.getElementById('maker-no-trade-label');
  const notReadyEl = document.getElementById('maker-not-ready');
  if (targetEl) targetEl.textContent = String(targetMarketCount);
  if (booksEl) {
    booksEl.textContent = tokenCount
      ? `${loadedCount}/${tokenCount} · ${twoSidedCount} 2S · ${oneSidedCount} 1S`
      : '0/0';
  }
  if (quoteableEl) {
    quoteableEl.textContent = tokenCount
      ? `${quoteableCount}/${tokenCount}`
      : '0/0';
    quoteableEl.className = `v ${quoteableCount > 0 ? 'maker-live' : (tokenCount > 0 ? 'maker-warn' : '')}`.trim();
  }
  if (noTradeEl) {
    const label = noTradeTruth?.label || (l2Stale ? 'global_l2_feed_stale' : 'maker_idle');
    noTradeEl.textContent = fmtTruthLabel(label);
    noTradeEl.className = `v ${truthTone(label)}`.trim();
  }
  if (notReadyEl) {
    const blockerLabel = isTaker
      ? (l2StaleCode || 'execution_mode_taker')
      : primaryNotReadyReason
      || (quoteableCount > 0 ? 'ready' : (targetMarketCount === 0 ? 'no_target_markets' : 'maker_idle'));
    notReadyEl.textContent = fmtTruthLabel(blockerLabel);
    notReadyEl.className = `v ${truthTone(blockerLabel)}`.trim();
  }
  const rollStateEl = document.getElementById('maker-roll-state');
  const rollSignalEl = document.getElementById('maker-roll-signal');
  const strongestRoll = rollMicro.length ? rollMicro[0] : null;
  if (strongestRoll && (strongestRoll.confidence || strongestRoll.sample_count)) {
    const signal = Number(strongestRoll.signal || 0);
    const conf = Number(strongestRoll.confidence || 0);
    const persistence = Number(strongestRoll.persistence || 0);
    if (rollStateEl) {
      if (conf > 0.15 && signal > 0) {
        rollStateEl.textContent = 'BUY YES';
        rollStateEl.className = 'v maker-live';
      } else if (conf > 0.15 && signal < 0) {
        rollStateEl.textContent = 'BUY NO';
        rollStateEl.className = 'v maker-stale';
      } else if (persistence < 0) {
        rollStateEl.textContent = 'BOUNCE';
        rollStateEl.className = 'v text-muted';
      } else {
        rollStateEl.textContent = 'QUIET';
        rollStateEl.className = 'v text-muted';
      }
    }
    if (rollSignalEl) {
      rollSignalEl.textContent = `${fmtN(signal, 2)} / ${fmtPct(conf * 100)}`;
      rollSignalEl.className = 'v ' + (signal > 0 ? 'maker-live' : signal < 0 ? 'maker-stale' : 'text-muted');
    }
  } else {
    if (rollStateEl) {
      rollStateEl.textContent = '—';
      rollStateEl.className = 'v';
    }
    if (rollSignalEl) {
      rollSignalEl.textContent = '—';
      rollSignalEl.className = 'v';
    }
  }
  const staleReasonText = isTaker
    ? 'L2 disabled in taker mode'
    : (maker?.l2_stale_reason || (l2Stale ? 'L2 feed stale' : 'L2 healthy'));
  document.getElementById('maker-stale-reason').textContent = isTaker
    ? staleReasonText
    : `${fmtTruthLabel(l2StaleCode || (l2Stale ? 'global_l2_feed_stale' : 'quoteable_books'))} · ${staleReasonText}`;
  const feedNoteEl = document.getElementById('maker-feed-note');
  if (feedNoteEl) {
    const feedLabel = l2StaleCode
      || (l2Stale ? 'global_l2_feed_stale' : (quoteableCount > 0 ? 'quoteable_books' : primaryNotReadyReason || 'maker_idle'));
    feedNoteEl.textContent = isTaker
      ? 'Feed truth: maker L2 is intentionally disabled while execution mode is taker.'
      : `Feed truth: ${fmtTruthLabel(feedLabel)} · ${targetMarketCount} targets · ${loadedCount}/${tokenCount} snapshots · ${twoSidedCount} two-sided · ${oneSidedCount} one-sided · ${staleCount} stale.`;
  }
  const noTradeNoteEl = document.getElementById('maker-no-trade-note');
  if (noTradeNoteEl) {
    const noteParts = [];
    const noTradeLabel = noTradeTruth?.label || (l2Stale ? 'global_l2_feed_stale' : 'maker_idle');
    noteParts.push(`No-trade: ${fmtTruthLabel(noTradeLabel)}`);
    if (primaryNotReadyReason && primaryNotReadyReason !== noTradeLabel) {
      noteParts.push(`blocker ${fmtTruthLabel(primaryNotReadyReason)}`);
    }
    if (notReadyReasons.length) noteParts.push(`top blockers ${topReasonSummary(notReadyReasons)}`);
    if (noTradeTruth?.decision) noteParts.push(`decision ${String(noTradeTruth.decision).replace(/_/g, ' ')}`);
    noTradeNoteEl.textContent = noteParts.join(' · ');
  }

  const bboTbody = document.getElementById('maker-bbo-tbody');
  if (!bbo.length) {
    bboTbody.innerHTML = '<tr><td colspan="4" class="text-muted text-center py-3">No BBO data</td></tr>';
  } else {
    bboTbody.innerHTML = bbo.slice(0, 10).map(x => `
      <tr>
        <td title="${ledgerEsc(x.token_id || '')}">${ledgerEsc(shortId(x.token_id, 12))}</td>
        <td>${fmtN(x.best_bid, 3)}</td>
        <td>${fmtN(x.best_ask, 3)}</td>
        <td>${x.spread_bps != null ? fmtN(x.spread_bps, 1) + 'bps' : '—'}</td>
      </tr>
    `).join('');
  }

  const qTbody = document.getElementById('maker-quotes-tbody');
  if (!quotes.length) {
    qTbody.innerHTML = '<tr><td colspan="4" class="text-muted text-center py-3">No active quotes</td></tr>';
  } else {
    qTbody.innerHTML = quotes.slice(0, 14).map(q => `
      <tr>
        <td>${ledgerEsc(String(q.side || '').toUpperCase())}</td>
        <td>${fmtN(q.price, 3)}</td>
        <td>${fmtN(q.size, 2)}</td>
        <td title="${ledgerEsc(q.order_id || '')}">${ledgerEsc(shortId(q.order_id, 12))}</td>
      </tr>
    `).join('');
  }


  const telemetryTbody = document.getElementById('maker-telemetry-tbody');
  if (telemetryTbody) {
    if (!telemetry.length) {
      telemetryTbody.innerHTML = '<tr><td colspan="8" class="text-muted text-center py-3">No quote telemetry yet</td></tr>';
    } else {
      telemetryTbody.innerHTML = telemetry.slice(0, 18).map(evt => {
        const adverse = evt.adverse_move_bps;
        const adverseTxt = adverse != null ? fmtBps(-Math.abs(adverse)) : '—';
        const adverseClr = adverse == null ? '' : (Number(adverse) > 0 ? 'pnl-neg' : 'pnl-zero');
        const basis = evt.current_basis_pct ?? evt.basis_pct;
        const basisTxt = basis != null ? `${Number(basis) >= 0 ? '+' : ''}${fmtN(basis, 3)}%` : '—';
        const basisClr = basis == null ? '' : (Number(basis) > 0 ? 'pnl-pos' : Number(basis) < 0 ? 'pnl-neg' : '');
        return `
          <tr>
            <td>${tsShort(evt.observed_at || evt.placed_at)}</td>
            <td>${ledgerEsc(String(evt.event || '—').toUpperCase())}</td>
            <td>${ledgerEsc(String(evt.side || '—').toUpperCase())}</td>
            <td title="${ledgerEsc(evt.reason || '')}">${ledgerEsc(shortId(evt.reason || '—', 16))}</td>
            <td>${fmtMs(evt.quote_age_ms)}</td>
            <td class="${adverseClr}">${adverseTxt}</td>
            <td class="${basisClr}">${basisTxt}</td>
            <td title="${ledgerEsc(evt.order_id || '')}">${ledgerEsc(shortId(evt.order_id, 12))}</td>
          </tr>
        `;
      }).join('');
    }
  }
  updateDashboardStrip();
}

// ── Tab controller ──────────────────────────────────────────────────────────
function setActiveTab(tab) {
  activeTab = tab;
  try { localStorage.setItem('macbot.activeTab', tab); } catch {}

  document.querySelectorAll('#terminal-tabs .terminal-tab').forEach(btn => {
    btn.classList.toggle('is-active', btn.dataset.tab === tab);
  });

  document.querySelectorAll('.tab-pane-section').forEach(el => {
    const targets = String(el.dataset.tabPane || '')
      .split(/\s+/)
      .map(x => x.trim())
      .filter(Boolean);
    el.classList.toggle('is-hidden', !targets.includes(tab));
  });

  const strip = document.getElementById('dashboard-strip');
  if (strip) strip.classList.toggle('is-hidden', tab === 'report' || tab === 'tape');

  const hint = document.getElementById('tab-hint');
  if (hint) {
    const hints = {
      overview: 'What the bot is doing right now: current signal, market surface, open positions, and recent trades.',
      tape: 'What it is doing now: the market clock, open positions, and every decision it made including the ones it declined.',
      report: 'What happened: window progress against the pre-registered sample floor, net figures after fees, and the markets the bot declined.',
      maker: 'Maker OMS internals: L2, grid quotes, and quote model decisions.',
      execution: 'Parity checks between local trade prices and exchange-derived fill VWAP.',
      risk: 'Risk guard telemetry, accounting truth snapshot, and attribution rollups.',
      signals: 'Feature inputs behind the decision: regime, microstructure, liquidations, order book, and cross-market.',
      model: 'Online learner and deep model state, calibration, and inference health.',
      config: 'Runtime configuration matrix with persistence controls.',
    };
    hint.textContent = hints[tab] || hints.overview;
  }

  if (!initialLoadComplete) return;
  setTapePolling(tab === 'tape');
  refreshActiveTab(tab);
  updateDashboardStrip();
}

// What each tab needs fetched, in one place.
//
// This used to be written twice: once here for tab switches, and again, partly,
// in the stream-down fallback, which listed only overview and execution. Every
// other tab therefore refreshed nothing at all whenever the stream was down,
// and adding a tab meant remembering to update a second list nobody would think
// to look for. One definition, two callers, no tab left out by omission.
const TAB_LOADERS = {
  overview: () => { loadAnalysis(); loadMlState(); loadDeepModelState(); loadTrades(); },
  tape:     () => { refreshTape(); },
  report:   () => { loadLedgerReport(); },
  signals:  () => { loadAnalysis(); loadDeepModelState(); },
  model:    () => { loadAnalysis(); loadMlState(); loadDeepModelState(); },
  execution:() => { loadExecutionQuality(); loadTrades(); },
  risk:     () => { loadRiskAndAttribution(); },
  maker:    () => { loadMakerState(); loadMakerAbSummary(); loadMakerRolloutStatus(); },
  config:   () => { loadConfig(); },
};

function refreshActiveTab(tab = activeTab) {
  const load = TAB_LOADERS[tab];
  if (load) load();
}

function initTerminalTabs() {
  document.querySelectorAll('#terminal-tabs .terminal-tab').forEach(btn => {
    btn.addEventListener('click', () => setActiveTab(btn.dataset.tab || 'overview'));
  });
  const known = Array.from(document.querySelectorAll('#terminal-tabs .terminal-tab'))
    .map(btn => btn.dataset.tab)
    .filter(Boolean);
  let saved = 'overview';
  try { saved = localStorage.getItem('macbot.activeTab') || 'overview'; } catch {}
  if (!known.includes(saved)) saved = 'overview';
  setActiveTab(saved);
}

// ── Execution quality ───────────────────────────────────────────────────────
async function loadExecutionQuality() {
  if (activeTab !== 'execution' || executionQualityBusy) return;
  executionQualityBusy = true;
  const tbody = document.getElementById('eq-tbody');
  try {
    const closed = [...allTrades]
      .filter(t => t.status === 'CLOSED')
      .sort((a, b) => b.id - a.id)
      .slice(0, 20);

    if (!closed.length) {
      tbody.innerHTML = '<tr><td colspan="9" class="text-muted text-center py-3">No closed trades available.</td></tr>';
      document.getElementById('eq-checked').textContent = '0';
      document.getElementById('eq-ok').textContent = '0';
      document.getElementById('eq-mismatch').textContent = '0';
      document.getElementById('eq-incomplete').textContent = '0';
      document.getElementById('eq-entry-drift').textContent = '—';
      document.getElementById('eq-exit-drift').textContent = '—';
      return;
    }

    const results = await Promise.allSettled(
      closed.map(t => GET(`/api/trades/${t.id}/fills`))
    );

    let checked = 0;
    let ok = 0;
    let mismatch = 0;
    let incomplete = 0;
    const entryDrifts = [];
    const exitDrifts = [];
    const rows = [];

    for (let i = 0; i < closed.length; i++) {
      const t = closed[i];
      const res = results[i];
      if (res.status !== 'fulfilled') {
        incomplete += 1;
        rows.push(`
          <tr>
            <td>#${t.id}</td>
            <td>${t.direction === 'BUY_YES' ? '<span class="dir-yes">YES</span>' : '<span class="dir-no">NO</span>'}</td>
            <td colspan="6" class="text-muted">Unable to load fill ledger</td>
            <td>${statusBadge('incomplete')}</td>
          </tr>
        `);
        continue;
      }

      checked += 1;
      const payload = res.value || {};
      const parity = payload.parity_check || {};
      const overall = String(parity.overall_status || 'incomplete').toLowerCase();

      if (overall === 'ok') ok += 1;
      else if (overall === 'mismatch') mismatch += 1;
      else incomplete += 1;

      if (parity.entry_drift_bps !== null && parity.entry_drift_bps !== undefined) {
        entryDrifts.push(Math.abs(Number(parity.entry_drift_bps)));
      }
      if (parity.exit_drift_bps !== null && parity.exit_drift_bps !== undefined) {
        exitDrifts.push(Math.abs(Number(parity.exit_drift_bps)));
      }

      rows.push(`
        <tr>
          <td>#${t.id}</td>
          <td>${t.direction === 'BUY_YES' ? '<span class="dir-yes">YES</span>' : '<span class="dir-no">NO</span>'}</td>
          <td>${fmtN(parity.entry_reference_price, 4)}</td>
          <td>${fmtN(parity.entry_exchange_vwap, 4)}</td>
          <td>${statusBadge(parity.entry_status)} <span class="ms-1 text-muted">${fmtBps(parity.entry_drift_bps)}</span></td>
          <td>${fmtN(parity.exit_reference_price, 4)}</td>
          <td>${fmtN(parity.exit_exchange_vwap, 4)}</td>
          <td>${statusBadge(parity.exit_status)} <span class="ms-1 text-muted">${fmtBps(parity.exit_drift_bps)}</span></td>
          <td>${statusBadge(parity.overall_status)}</td>
        </tr>
      `);
    }

    const avgEntry = entryDrifts.length
      ? (entryDrifts.reduce((a, b) => a + b, 0) / entryDrifts.length)
      : null;
    const avgExit = exitDrifts.length
      ? (exitDrifts.reduce((a, b) => a + b, 0) / exitDrifts.length)
      : null;

    document.getElementById('eq-checked').textContent = String(checked);
    document.getElementById('eq-ok').textContent = String(ok);
    document.getElementById('eq-mismatch').textContent = String(mismatch);
    document.getElementById('eq-incomplete').textContent = String(incomplete);
    document.getElementById('eq-entry-drift').textContent = avgEntry === null ? '—' : avgEntry.toFixed(1) + 'bps';
    document.getElementById('eq-exit-drift').textContent = avgExit === null ? '—' : avgExit.toFixed(1) + 'bps';

    tbody.innerHTML = rows.join('');
  } catch (e) {
    logReadError('loadExecutionQuality', e);
    tbody.innerHTML = '<tr><td colspan="9" class="text-muted text-center py-3">Execution quality load failed.</td></tr>';
  } finally {
    executionQualityBusy = false;
  }
}

// ── Risk + attribution panel ────────────────────────────────────────────────
async function loadRiskAndAttribution() {
  if (activeTab !== 'risk' || riskPanelBusy) return;
  riskPanelBusy = true;
  try {
    const [riskRes, attrRes, acctRes] = await Promise.allSettled([
      GET('/api/risk/status'),
      GET('/api/attribution'),
      GET('/api/accounting/truth'),
    ]);

    if (riskRes.status === 'fulfilled') {
      const r = riskRes.value || {};
      killSwitchState = !!r.kill_switch;
      degradedState = !!r.degraded_no_trade;
      killSwitchCause = r.kill_switch_reason || null;
      const isTaker = String(r.execution_mode || '').toLowerCase() === 'taker';
      document.getElementById('risk-kill').innerHTML = flagBadge(!!r.kill_switch, 'engaged', 'clear');
      const resetBtn = document.getElementById('btn-reset-kill');
      if (resetBtn) resetBtn.style.display = r.kill_switch ? 'inline-block' : 'none';
      document.getElementById('risk-degraded').innerHTML = flagBadge(!!r.degraded_no_trade, 'degraded', 'normal');
      if (isTaker) {
        document.getElementById('risk-l2').innerHTML = statusBadge('neutral') + ` <span class="ms-1">N/A</span>`;
        document.getElementById('risk-l2-age').textContent = 'taker';
      } else {
        document.getElementById('risk-l2').innerHTML = flagBadge(!!r.l2_feed_stale, 'stale', 'live');
        document.getElementById('risk-l2-age').textContent = fmtMs(r.l2_feed_age_ms);
      }
      document.getElementById('risk-actor-drops').textContent = String(r.actor_cmd_drop_total ?? 0);
      document.getElementById('risk-outbox-drops').textContent = String(r.outbox_dropped_total ?? 0);
      document.getElementById('risk-slo-trips').textContent = String(r.slo_guard_trips_total ?? 0);
      document.getElementById('risk-balance-drift').textContent =
        (r.last_balance_drift_usd === null || r.last_balance_drift_usd === undefined) ? 'n/a' : fmt$(r.last_balance_drift_usd);
      renderMacroGuardSummary(r.macro_guard);
      document.getElementById('risk-run-id').textContent =
        `Run ID: ${r.current_run_id || '—'} · Stage: ${r.rollout_stage || '—'} · Exec: ${(r.execution_mode || '—').toUpperCase()}`;
    }

    if (acctRes.status === 'fulfilled') {
      const a = acctRes.value || {};
      document.getElementById('acct-current-balance').textContent = fmtMoney(a.current_balance);
      document.getElementById('acct-derived-restart').textContent = fmtMoney(a.derived_restart_balance);
      document.getElementById('acct-authoritative').textContent =
        (a.authoritative_live_balance === null || a.authoritative_live_balance === undefined)
          ? 'n/a'
          : fmtMoney(a.authoritative_live_balance);
      document.getElementById('acct-drift').textContent =
        (a.drift_vs_authoritative_usd === null || a.drift_vs_authoritative_usd === undefined)
          ? 'n/a'
          : fmt$(a.drift_vs_authoritative_usd);
    }

    const tbody = document.getElementById('attrib-mode-tbody');
    if (attrRes.status === 'fulfilled') {
      const rows = Array.isArray(attrRes.value?.by_mode) ? attrRes.value.by_mode : [];
      if (!rows.length) {
        tbody.innerHTML = '<tr><td colspan="5" class="text-muted text-center py-3">No attribution rows available.</td></tr>';
      } else {
        tbody.innerHTML = rows.slice(0, 8).map(x => `
          <tr>
            <td>${ledgerEsc(String(x.mode || 'unknown').toUpperCase())}</td>
            <td>${x.trades ?? 0}</td>
            <td>${fmtPct((x.win_rate ?? 0) * 100)}</td>
            <td class="${clsPnl(x.total_pnl)}">${fmt$(x.total_pnl ?? 0)}</td>
            <td class="${clsPnl(x.avg_pnl)}">${fmt$(x.avg_pnl ?? 0)}</td>
          </tr>
        `).join('');
      }
    } else {
      tbody.innerHTML = '<tr><td colspan="5" class="text-muted text-center py-3">Attribution load failed.</td></tr>';
    }
  } catch (e) {
    logReadError('loadRiskAndAttribution', e);
  } finally {
    riskPanelBusy = false;
  }
}

function updatePriceBusPanels(priceBus) {
  const navStatus = document.getElementById('nav-pricebus-status');
  const navAge = document.getElementById('nav-pricebus-age');
  const feedStatus = document.getElementById('btc-feed-status');
  const feedDetail = document.getElementById('btc-feed-detail');
  if (!navStatus || !navAge || !feedStatus || !feedDetail) return;

  if (!priceBus) {
    navStatus.textContent = 'PX —';
    navStatus.style.color = '';
    navAge.textContent = '—';
    feedStatus.textContent = '—';
    feedStatus.style.color = '';
    feedDetail.textContent = 'Waiting for live price bus…';
    return;
  }

  const status = String(priceBus.status || 'offline').toLowerCase();
  const liveCount = Number(priceBus.symbols_live || 0);
  const totalCount = Number(priceBus.symbols_total || 3);
  const btc = priceBus.btc || {};
  const eth = priceBus.eth || {};
  const sol = priceBus.sol || {};
  const statusColor = status === 'live'
    ? 'var(--clr-green)'
    : status === 'degraded'
      ? 'var(--clr-yellow)'
      : 'var(--clr-red)';
  const source = String(priceBus.source || 'stream').replace(/_/g, ' ').toUpperCase();

  navStatus.textContent = `PX ${status.toUpperCase()} ${liveCount}/${totalCount}`;
  navStatus.style.color = statusColor;
  navAge.textContent = btc.age_ms != null ? `BTC ${fmtMs(btc.age_ms)}` : 'BTC —';

  feedStatus.textContent = `${status.toUpperCase()} ${liveCount}/${totalCount}`;
  feedStatus.style.color = statusColor;
  feedDetail.textContent =
    `${source} · BTC ${fmtMs(btc.age_ms)} · ETH ${fmtMs(eth.age_ms)} · SOL ${fmtMs(sol.age_ms)}`;
}

function updateRealtimeContext(analysis, priceBus) {
  if (priceBus) updatePriceBusPanels(priceBus);

  const priceToBeatEl = document.getElementById('btc-price-to-beat-distance');
  const priceToBeatDetailEl = document.getElementById('btc-price-to-beat-distance-detail');
  const basisEl = document.getElementById('btc-futures-basis');
  const basisDetailEl = document.getElementById('btc-futures-basis-detail');
  const markIndexEl = document.getElementById('btc-futures-mark-index');
  const markIndexDetailEl = document.getElementById('btc-futures-mark-index-detail');
  if (!priceToBeatEl || !priceToBeatDetailEl || !basisEl || !basisDetailEl || !markIndexEl || !markIndexDetailEl) return;

  const extra = analysis?.extra || {};
  const readRealtimeField = (key) => {
    const nested = extra?.[key];
    if (nested !== null && nested !== undefined) return nested;
    return analysis?.[key];
  };
  const priceToBeatDistance = readRealtimeField('price_to_beat_distance_pct');
  const basis = readRealtimeField('futures_basis_pct');
  const mark = readRealtimeField('futures_mark_price');
  const index = readRealtimeField('futures_index_price');

  if (priceToBeatDistance === null || priceToBeatDistance === undefined) {
    priceToBeatEl.textContent = '—';
    priceToBeatEl.style.color = '';
    priceToBeatDetailEl.textContent = 'Awaiting price-to-beat context';
  } else {
    const n = Number(priceToBeatDistance);
    priceToBeatEl.textContent = `${n >= 0 ? '+' : ''}${fmtN(n, 3)}%`;
    priceToBeatEl.style.color = n > 0 ? 'var(--clr-green)' : n < 0 ? 'var(--clr-red)' : '';
    priceToBeatDetailEl.textContent =
      n > 0 ? 'BTC is above the price to beat' : n < 0 ? 'BTC is below the price to beat' : 'BTC is sitting on the price to beat';
  }

  if (basis === null || basis === undefined) {
    basisEl.textContent = '—';
    basisEl.style.color = '';
    basisDetailEl.textContent = 'Awaiting futures premium index context';
  } else {
    const n = Number(basis);
    basisEl.textContent = `${n >= 0 ? '+' : ''}${fmtN(n, 3)}%`;
    basisEl.style.color = n > 0 ? 'var(--clr-green)' : n < 0 ? 'var(--clr-red)' : '';
    basisDetailEl.textContent = n > 0 ? 'Contango' : n < 0 ? 'Backwardation' : 'Flat basis';
  }

  if (mark === null || mark === undefined || index === null || index === undefined) {
    markIndexEl.textContent = '—';
    markIndexDetailEl.textContent = 'Mark/index not available yet';
  } else {
    markIndexEl.textContent = `${fmtMoney(mark)} / ${fmtMoney(index)}`;
    const premium = Number(mark) - Number(index);
    markIndexDetailEl.textContent =
      `${premium >= 0 ? '+' : ''}${fmtMoney(Math.abs(premium)).replace('$', '')} premium spread`;
  }
}

/// Render a duration in minutes as something a person reads at a glance.
///
/// "FOMC Statement · 2564.2m" is forty-three hours to one decimal place. The
/// precision is spurious and the unit is wrong for the magnitude.
function fmtMinutesUntil(mins) {
  // Number(null) is 0, so a missing duration would render as "0s", meaning
  // "happening right now". Absent has to stay absent.
  if (mins === null || mins === undefined || mins === '') return '\u2014';
  const n = Number(mins);
  if (!Number.isFinite(n)) return '\u2014';
  const abs = Math.abs(n);
  const sign = n < 0 ? '+' : '';  // negative means the event has passed
  if (abs < 1) return `${sign}${Math.round(abs * 60)}s`;
  if (abs < 90) return `${sign}${Math.round(abs)}m`;
  if (abs < 48 * 60) {
    const h = Math.floor(abs / 60);
    const m = Math.round(abs % 60);
    return m ? `${sign}${h}h ${m}m` : `${sign}${h}h`;
  }
  const d = Math.floor(abs / 1440);
  const h = Math.round((abs % 1440) / 60);
  return h ? `${sign}${d}d ${h}h` : `${sign}${d}d`;
}

function renderMacroGuardSummary(macroGuard) {
  const macroEl = document.getElementById('risk-macro-guard');
  const nextEl = document.getElementById('risk-macro-next');
  if (!macroEl || !nextEl) return;

  if (!macroGuard) {
    macroEl.textContent = '—';
    nextEl.textContent = '—';
    return;
  }

  const active = macroGuard.active || null;
  const next = macroGuard.next || null;
  if (macroGuard.blocking && active) {
    const detail = `${active.label} · ${fmtMinutesUntil(active.minutes_until_event)}`;
    macroEl.innerHTML = flagBadge(true, 'blocking', 'clear') + ` <span class="ms-1">${ledgerEsc(detail)}</span>`;
  } else if (macroGuard.enabled) {
    macroEl.innerHTML = statusBadge('ok') + ' <span class="ms-1">CLEAR</span>';
  } else {
    macroEl.innerHTML = statusBadge('neutral') + ' <span class="ms-1">OFF</span>';
  }

  if (next) {
    nextEl.textContent = `${next.label} · ${fmtMinutesUntil(next.minutes_until_event)}`;
  } else if (macroGuard.error) {
    nextEl.textContent = 'calendar error';
  } else {
    nextEl.textContent = '—';
  }
}

function updateIntegritySummary(report) {
  const navStatus = document.getElementById('nav-integrity-status');
  const navDetail = document.getElementById('nav-integrity-detail');
  const riskIntegrity = document.getElementById('risk-integrity');
  const riskMismatchCount = document.getElementById('risk-integrity-mismatches');
  if (!navStatus || !navDetail || !riskIntegrity || !riskMismatchCount) return;

  const ok = Boolean(report?.ok);
  const mismatchCount = Array.isArray(report?.mismatches) ? report.mismatches.length : 0;
  const statusColor = ok ? 'var(--clr-green)' : 'var(--clr-red)';

  const unknown = report?.incomplete === true || typeof report?.error === 'string';
  navStatus.textContent = unknown ? 'Integrity unknown' : ok ? 'Integrity OK' : 'Integrity Drift';
  navStatus.style.color = statusColor;
  navDetail.textContent = unknown ? 'evidence unavailable' : mismatchCount === 0 ? '0 drift' : `${mismatchCount} drift`;

  riskIntegrity.innerHTML = statusBadge(unknown ? 'neutral' : ok ? 'ok' : 'mismatch') +
    ` <span class="ms-1">${unknown ? 'UNKNOWN' : ok ? 'CLEAN' : 'DRIFT'}</span>`;
  riskMismatchCount.textContent = String(mismatchCount);
}

async function loadIntegritySummary() {
  if (integrityBusy) return;
  integrityBusy = true;
  try {
    const report = await GET('/api/integrity/report');
    updateIntegritySummary(report);
  } catch (e) {
    if (isAuthFailure(e)) {
      const navStatus = document.getElementById('nav-integrity-status');
      const navDetail = document.getElementById('nav-integrity-detail');
      if (navStatus) {
        navStatus.textContent = 'Integrity locked';
        navStatus.style.color = 'var(--clr-yellow)';
      }
      if (navDetail) navDetail.textContent = 'unlock';
    } else {
      logReadError('loadIntegritySummary', e);
    }
  } finally {
    integrityBusy = false;
  }
}

// ── SSE ───────────────────────────────────────────────────────────────────────
async function connectSSE() {
  if (sseSource) { sseSource.close(); sseSource = null; }
  clearTimeout(reconnectTimer);

  let url;
  try {
    url = await streamUrl('/api/stream');
  } catch (e) {
    console.error('connectSSE stream token:', e);
    if (isAuthFailure(e)) {
      notifyDashboardAuthRequired('stream');
      scheduleSseRetry(true);
      return;
    }
    scheduleSseRetry(false);
    return;
  }
  sseSource = new EventSource(url);

sseSource.onopen = () => {
  document.getElementById('sse-dot').className = 'connected';
  document.getElementById('sse-status').textContent = 'live';
  lastSseAt = Date.now();
  sseRetryDelay = 0;
  updateDashboardStrip();
};

  sseSource.onmessage = e => {
    try {
      const data = JSON.parse(e.data);
      handleSSEUpdate(data);
    } catch {}
  };

  sseSource.onerror = () => {
    document.getElementById('sse-dot').className = '';
    if (!getApiAuth().apiKey) {
      notifyDashboardAuthRequired('stream');
      document.getElementById('sse-status').textContent = 'browser locked';
    } else {
      document.getElementById('sse-status').textContent = 'reconnecting…';
    }
    sseSource.close();
    sseSource = null;
    updateDashboardStrip();
    scheduleSseRetry(!getApiAuth().apiKey);
  };
}

let _lastTradeRefresh = 0;
function handleSSEUpdate(data) {
  lastSseAt = Date.now();
  // Refresh open positions periodically from the authoritative trade API.
  if (shouldPollReadLoop() && Date.now() - _lastTradeRefresh > 15_000) {
    _lastTradeRefresh = Date.now();
    loadTrades();
  }
  if (data.balance !== undefined) {
    document.getElementById('nav-balance').textContent = fmtMoney(data.balance);
  }
  if (data.mode) {
    runtimeModeState = String(data.mode || '').toLowerCase();
    const badge = document.getElementById('mode-badge');
    badge.textContent = data.mode.toUpperCase();
    badge.className = 'mode-badge ' + (data.mode === 'live' ? 'mode-live' : 'mode-sim');
  }
  if (data.loop_running !== undefined) {
    updateLoopButtons(data.loop_running, data.backtest_only);
  }
  if (data.maker_hold_positions !== undefined) {
    makerHoldState = Boolean(data.maker_hold_positions);
  }
  const feedStale = data.l2_feed_stale === true || data.risk?.l2_feed_stale === true
    || data.maker?.l2_stale === true || data.feed_truth?.l2_feed_stale === true;
  if (feedStale || Array.isArray(data.maker?.l2_top_of_book)) {
    noteLiveBook(data.maker?.l2_top_of_book, { feedStale });
    refreshOpenPositionsIfBookMoved();
    if (!data.analysis && window._lastAnalysis) updateSignalPanel(window._lastAnalysis);
  }
  if (data.maker && Array.isArray(data.maker.hands_off_markets)) {
    makerHandsOffMarkets = data.maker.hands_off_markets.map(String);
  }
  if (data.maker && Array.isArray(data.maker.quote_model)) {
    const refused = data.maker.quote_model.find(m => String(m?.bid_reason || '').startsWith('venue_min_size('));
    makerVenueRefusal = refused ? String(refused.bid_reason) : null;
  }
  if (data.analysis) {
    window._lastAnalysis = data.analysis;
    updateSignalPanel(data.analysis);
    updateMlFromAnalysis(data.analysis);
    updateDeepFromAnalysis(data.analysis);
    updatePhase2Panels(data.analysis);
    updateRealtimeContext(data.analysis, data.price_bus);
    noteAnalysisFreshness(data.analysis_freshness);
    updateTape(data.analysis);
    // Update ML A/B stats
    if (data.analysis.ml) {
      const abLrAcc = document.getElementById('ab-lr-acc');
      if (abLrAcc) abLrAcc.textContent = data.analysis.ml.accuracy_20 != null ? (data.analysis.ml.accuracy_20 * 100).toFixed(1) + '%' : '—';
      const abLrSamples = document.getElementById('ab-lr-samples');
      if (abLrSamples) abLrSamples.textContent = data.analysis.ml.samples_seen || '—';
    }
  } else if (data.price_bus) {
    updateRealtimeContext(window._lastAnalysis, data.price_bus);
  }
  if (data.analysis_freshness) noteAnalysisFreshness(data.analysis_freshness);
  if ('live_btc_price' in data && data.live_btc_price > 0) {
    const startPx = (data.analysis && data.analysis.btc_start_price) || btcStartPriceVal;
    const startSrc = data.analysis && data.analysis.btc_start_source;
    updateBtcCurrentPrice(data.live_btc_price, startPx, startSrc);
  } else if ('live_btc_price' in data) {
    document.getElementById('btc-current-price').textContent = '—';
    document.getElementById('btc-price-change').textContent = '—';
  }
  if (data.stats) {
    updateStatsBar(data.stats, data.daily_pnl);
    // Circuit breaker cooldown display
    const cbBadge = document.getElementById('cb-cooldown-badge');
    const cbMins = document.getElementById('cb-cooldown-mins');
    if (cbBadge && data.stats.circuit_breaker_cooldown) {
      const cb = data.stats.circuit_breaker_cooldown;
      if (cb.active) {
        cbBadge.style.display = 'inline-block';
        cbMins.textContent = cb.minutes_left + 'm';
      } else {
        cbBadge.style.display = 'none';
      }
    }
  }
  if (data.deep_model_status) {
    updateDeepModelFromSSE(data.deep_model_status);
  }
  if (data.risk) {
    updateRiskFromSSE(data.risk);
  }
  if (data.maker) {
    // Wrap maker snapshot to match renderMakerState's expected shape
    renderMakerState({
      maker: data.maker,
      feed_truth: data.feed_truth,
      no_trade_truth: data.no_trade_truth,
      execution_mode: data.risk?.execution_mode,
      l2_feed_stale: data.risk?.l2_feed_stale,
      l2_feed_age_ms: data.risk?.l2_feed_age_ms,
    });
  }
  updateDashboardStrip();
}

function updateDeepModelFromSSE(r) {
  const model = r.model || {};
  const ensemble = r.ensemble || {};
  const lastPred = r.last_prediction || {};

  if (model.loaded) {
    const el = document.getElementById('deep-status');
    if (el) el.innerHTML = '<span style="color:var(--clr-green);">Loaded</span>';
    const badge = document.getElementById('deep-phase-badge');
    if (badge) {
      badge.textContent = (ensemble.phase || 'shadow').toUpperCase();
      badge.className = 'badge ' + (ensemble.phase === 'active' ? 'bg-success' : ensemble.phase === 'ramping' ? 'bg-warning' : 'bg-secondary');
    }
  }
  const verEl = document.getElementById('deep-version');
  if (verEl) verEl.textContent = model.version || '—';
  if (model.loaded_ago_secs != null) {
    const secs = model.loaded_ago_secs;
    const mins = Math.floor(secs / 60);
    const hrs = Math.floor(mins / 60);
    const el = document.getElementById('deep-loaded-ago');
    if (el) el.textContent = hrs > 0 ? hrs + 'h ' + (mins % 60) + 'm' : mins > 0 ? mins + 'm ' + (secs % 60) + 's' : secs + 's';
  }

  const wPct = ((ensemble.deep_weight || 0) * 100);
  const el1 = document.getElementById('deep-ens-phase');
  if (el1) el1.textContent = ensemble.phase || 'shadow';
  const el2 = document.getElementById('deep-ens-weight');
  if (el2) el2.textContent = wPct.toFixed(1) + '%';
  const el3 = document.getElementById('deep-ens-ramp');
  if (el3) el3.textContent = `${ensemble.ramp_step || 0} / 100`;
  const el4 = document.getElementById('deep-ens-samples');
  if (el4) el4.textContent = String(ensemble.deep_total_samples || 0);

  // Promotion progress bar
  const deepSamplesSSE = ensemble.deep_total_samples || 0;
  const promoPctSSE = Math.min(100, (deepSamplesSSE / 200) * 100);
  const promoBar = document.getElementById('deep-promo-bar');
  if (promoBar) promoBar.style.width = promoPctSSE + '%';
  const promoLabel = document.getElementById('deep-promo-label');
  if (promoLabel) promoLabel.textContent = deepSamplesSSE + ' / 200';

  const el5 = document.getElementById('deep-acc-50');
  if (el5) el5.textContent = ensemble.deep_accuracy_50 != null ? (ensemble.deep_accuracy_50 * 100).toFixed(1) + '%' : '—';
  const el6 = document.getElementById('deep-acc-200');
  if (el6) el6.textContent = ensemble.deep_accuracy_200 != null ? (ensemble.deep_accuracy_200 * 100).toFixed(1) + '%' : '—';
  const el7 = document.getElementById('deep-brier-50');
  if (el7) el7.textContent = ensemble.deep_brier_50 != null ? ensemble.deep_brier_50.toFixed(3) : '—';

  const el8 = document.getElementById('deep-p-up');
  if (el8) el8.textContent = lastPred.deep_p_up != null ? lastPred.deep_p_up.toFixed(3) : '—';
  const el9 = document.getElementById('deep-blended');
  if (el9) el9.textContent = lastPred.blended_p_up != null ? lastPred.blended_p_up.toFixed(3) : '—';
  const el10 = document.getElementById('deep-mc-unc');
  if (el10) el10.textContent = lastPred.mc_uncertainty != null ? lastPred.mc_uncertainty.toFixed(4) : '—';
  const el11 = document.getElementById('deep-skip');
  if (el11) el11.textContent = lastPred.should_skip != null ? (lastPred.should_skip ? 'YES' : 'no') : '—';

  const barEl = document.getElementById('deep-weight-bar');
  if (barEl) barEl.style.width = Math.min(wPct / 60 * 100, 100) + '%';
  const labelEl = document.getElementById('deep-weight-label');
  if (labelEl) labelEl.textContent = 'Deep Weight: ' + wPct.toFixed(1) + '%';

  // Model A/B comparison (#28)
  const abDeepBrier = document.getElementById('ab-deep-brier');
  if (abDeepBrier) abDeepBrier.textContent = ensemble.deep_brier_50 != null ? ensemble.deep_brier_50.toFixed(3) : '—';
  const abDeepAcc = document.getElementById('ab-deep-acc');
  if (abDeepAcc) abDeepAcc.textContent = ensemble.deep_accuracy_50 != null ? (ensemble.deep_accuracy_50 * 100).toFixed(1) + '%' : '—';
  const abDeepSamples = document.getElementById('ab-deep-samples');
  if (abDeepSamples) abDeepSamples.textContent = ensemble.deep_total_samples || '—';

  // Agreement indicator: do ML and Deep agree on direction?
  if (lastPred.deep_p_up != null) {
    const deepBullish = lastPred.deep_p_up > 0.5;
    const agreeEl = document.getElementById('ab-agreement');
    // ML p_up is stored from latest analysis
    const lastA = window._lastAnalysis;
    if (agreeEl && lastA && lastA.ml && lastA.ml.p_up != null) {
      const mlBullish = lastA.ml.p_up > 0.5;
      const agree = deepBullish === mlBullish;
      agreeEl.innerHTML = agree
        ? '<span style="color:var(--clr-green);">✓ Agree</span>'
        : '<span style="color:var(--clr-red);">✗ Disagree</span>';
    }
  }
}

function updateRiskFromSSE(r) {
  killSwitchState = !!r.kill_switch;
  degradedState = !!r.degraded_no_trade;
  killSwitchCause = r.kill_switch_reason || null;
  renderDataStatus();
  updateDashboardStrip();
  const isTaker = String(r.execution_mode || '').toLowerCase() === 'taker';
  const el1 = document.getElementById('risk-kill');
  if (el1) el1.innerHTML = flagBadge(!!r.kill_switch, 'engaged', 'clear');
  const resetBtn = document.getElementById('btn-reset-kill');
  if (resetBtn) resetBtn.style.display = r.kill_switch ? 'inline-block' : 'none';
  if (isTaker) {
    const el2 = document.getElementById('risk-l2');
    if (el2) el2.innerHTML = statusBadge('neutral') + ` <span class="ms-1">N/A</span>`;
    const el3 = document.getElementById('risk-l2-age');
    if (el3) el3.textContent = 'taker';
  } else {
    const el2 = document.getElementById('risk-l2');
    if (el2) el2.innerHTML = flagBadge(!!r.l2_feed_stale, 'stale', 'live');
    const el3 = document.getElementById('risk-l2-age');
    if (el3) el3.textContent = fmtMs(r.l2_feed_age_ms);
  }
  renderMacroGuardSummary(r.macro_guard);
}


function updateNoArbSummary(snapshot) {
  if (!snapshot) { blankFields(NO_ARB_FIELDS); return; }
  const badge = document.getElementById('no-arb-badge');
  const summary = document.getElementById('no-arb-summary');
  const groupsEl = document.getElementById('no-arb-groups');
  const maxEl = document.getElementById('no-arb-max');
  const worstEl = document.getElementById('no-arb-worst');
  const detailEl = document.getElementById('no-arb-detail');

  const status = snapshot.status || 'insufficient_data';
  if (badge) {
    if (status === 'clean') {
      badge.textContent = 'CLEAN';
      badge.className = 'badge bg-success';
    } else if (status === 'violations') {
      badge.textContent = 'VIOLATION';
      badge.className = 'badge bg-danger';
    } else {
      badge.textContent = 'INSUFFICIENT';
      badge.className = 'badge bg-secondary';
    }
  }

  if (summary) {
    const markets = snapshot.market_count || 0;
    const groups = snapshot.comparable_group_count || snapshot.group_count || 0;
    summary.textContent = `${markets} mkts / ${groups} ladders`;
  }
  if (groupsEl) groupsEl.textContent = snapshot.comparable_group_count ?? snapshot.group_count ?? '—';
  if (maxEl) {
    maxEl.textContent = snapshot.max_violation_cents != null
      ? `${fmtN(snapshot.max_violation_cents, 1)}c`
      : '—';
    maxEl.style.color = snapshot.max_violation_cents > 0 ? 'var(--clr-red)' : '';
  }

  const worstGroup = snapshot.worst_group;
  if (!worstGroup || !Array.isArray(worstGroup.violations) || !worstGroup.violations.length) {
    if (worstEl) worstEl.textContent = status === 'clean' ? 'clean' : '—';
    if (detailEl) {
      detailEl.textContent = status === 'clean'
        ? 'Above-at-expiry Gamma quotes are monotone within tolerance. This does not prove executable arbitrage.'
        : 'Need more overlapping BTC contracts with parseable strikes.';
    }
    return;
  }

  const worstViolation = worstGroup.violations.reduce((best, row) =>
    !best || row.violation_cents > best.violation_cents ? row : best
  , null);
  if (!worstViolation) return;

  if (worstEl) {
    worstEl.textContent = `${fmtN(worstViolation.lower_strike_price / 1000, 1)}k→${fmtN(worstViolation.higher_strike_price / 1000, 1)}k`;
  }

  if (detailEl) {
    const expiry = worstGroup.end_date ? new Date(worstGroup.end_date).toLocaleString() : 'unknown expiry';
    detailEl.textContent =
      `${expiry}: higher strike YES ${fmtN(worstViolation.higher_yes_price, 3)} > lower strike YES ${fmtN(worstViolation.lower_yes_price, 3)} by ${fmtN(worstViolation.violation_cents, 1)}c`;
  }
}

function updateStrategyStats() {
  let convPnl = 0, scalpPnl = 0;
  let convWins = 0, convTotal = 0, scalpWins = 0, scalpTotal = 0;
  const closed = allTrades.filter(t => t.status === 'CLOSED');
  closed.forEach(t => {
    const pnl = realizedNetPnl(t);
    if (pnl === null) return;
    if (t.strategy === 'conviction') {
      convPnl += pnl;
      convTotal++;
      if (pnl > 0) convWins++;
    } else if (t.strategy === 'scalp') {
      scalpPnl += pnl;
      scalpTotal++;
      if (pnl > 0) scalpWins++;
    }
  });
  const convPnlEl = document.getElementById('st-conv-pnl');
  convPnlEl.textContent = fmt$(convPnl);
  convPnlEl.className = 'value ' + clsPnl(convPnl);
  const scalpPnlEl = document.getElementById('st-scalp-pnl');
  scalpPnlEl.textContent = fmt$(scalpPnl);
  scalpPnlEl.className = 'value ' + clsPnl(scalpPnl);
  document.getElementById('st-conv-wr').textContent = convTotal > 0 ? fmtPct(convWins / convTotal * 100) : '—';
  document.getElementById('st-scalp-wr').textContent = scalpTotal > 0 ? fmtPct(scalpWins / scalpTotal * 100) : '—';
}

// ── Loop control ───────────────────────────────────────────────────────────────
function updateLoopButtons(running, researchOnly) {
  if (typeof researchOnly === 'boolean') researchOnlyState = researchOnly;
  loopRunningState = Boolean(running);
  document.getElementById('btn-start').disabled = running || researchOnlyState;
  document.getElementById('btn-stop').disabled  = !running;
  for (const id of ['btn-execute', 'btn-refresh']) {
    const el = document.getElementById(id);
    if (el) { el.disabled = researchOnlyState; el.title = researchOnlyState ? 'Unavailable in the research demo' : ''; }
  }
  updateDashboardStrip();
}

async function loopControl(action) {
  try {
    await POST(`/api/loop/${action}`);
    let status = null;
    try {
      status = await GET('/api/loop/status');
    } catch {}
    if (status && typeof status.running === 'boolean') {
      updateLoopButtons(status.running, status.backtest_only);
      if (status.running === (action === 'start')) {
        toast(action === 'start' ? 'Loop started' : 'Loop stopped',
              action === 'start' ? 'success' : 'warning');
      } else {
        toast(`Loop ${action} failed: still ${status.running ? 'running' : 'stopped'}`, 'danger');
      }
    } else {
      updateLoopButtons(action === 'start');
      toast(action === 'start' ? 'Loop started' : 'Loop stopped',
            action === 'start' ? 'success' : 'warning');
    }
  } catch (e) {
    toast('Loop control failed: ' + e.message, 'danger');
  }
}

async function resetDrawdown() {
  if (!confirm('Reset the available-cash drawdown peak? Daily losses and the kill switch will stay unchanged.')) return;
  try {
    const r = await POST('/api/drawdown/reset');
    toast(`Cash drawdown peak reset to ${fmt$(r.peak_balance)}. Daily losses and kill switch kept.`, 'success');
    await loadConfig();
    await loadStats();
  } catch (e) {
    toast('Reset drawdown failed: ' + e.message, 'danger');
  }
}

async function resetKillSwitch() {
  if (!confirm('Clear the kill switch? Other risk checks and loop controls still apply.')) return;
  try {
    const r = await POST('/api/risk/kill-switch', { active: false });
    killSwitchState = false;
    killSwitchCause = null;
    updateDashboardStrip();
    renderDataStatus();
    toast('Kill switch cleared. Other risk checks still apply.', 'success');
    const el = document.getElementById('risk-kill');
    if (el) el.innerHTML = statusBadge('ok') + ' <span class="ms-1">OFF</span>';
    document.getElementById('btn-reset-kill').style.display = 'none';
  } catch (e) {
    toast('Reset kill switch failed: ' + e.message, 'danger');
  }
}

// ── ML panel ──────────────────────────────────────────────────────────────────
async function loadMlState() {
  if (initialLoadComplete && !['overview', 'model'].includes(activeTab)) return;
  try {
    const r = await GET('/api/ml/state');
    const badge = document.getElementById('ml-status-badge');
    badge.textContent = r.active ? 'Active' : 'Shadow';
    badge.className = 'badge ' + (r.active ? 'bg-success' : 'bg-secondary');
    badge.style.fontSize = '0.7rem';

    document.getElementById('ml-status').textContent = r.active ? 'ML Active' : 'Shadow (collecting data)';
    document.getElementById('ml-samples').textContent = r.samples_seen;
    document.getElementById('ml-lr').textContent = r.learning_rate?.toFixed(4) || '—';

    const acc = r.accuracy || {};
    document.getElementById('ml-acc-20').textContent = acc.accuracy_20 != null ? (acc.accuracy_20 * 100).toFixed(1) + '%' : '—';
    document.getElementById('ml-acc-50').textContent = acc.accuracy_50 != null ? (acc.accuracy_50 * 100).toFixed(1) + '%' : '—';

    // Render weight bars
    const container = document.getElementById('ml-weights-bars');
    if (r.weights && r.weights.length) {
      const maxW = Math.max(...r.weights.map(w => Math.abs(w.weight)), 0.01);
      container.innerHTML = r.weights.map(w => {
        const pct = Math.abs(w.weight) / maxW * 100;
        const color = w.weight >= 0 ? '#00c853' : '#ff1744';
        const name = w.name.replace(/_/g, ' ');
        return `<div style="display:flex;align-items:center;margin:1px 0;">
          <span style="width:120px;text-align:right;padding-right:6px;color:#888;">${ledgerEsc(name)}</span>
          <div style="flex:1;background:#222;height:12px;border-radius:var(--r-sm);overflow:hidden;">
            <div style="width:${pct}%;background:${color};height:100%;"></div>
          </div>
          <span style="width:50px;text-align:right;padding-left:4px;">${w.weight >= 0 ? '+' : ''}${w.weight.toFixed(3)}</span>
        </div>`;
      }).join('');
    }
  } catch (e) {
    logReadError('loadMlState', e);
  }
}

/// Blank a panel's fields rather than leaving the previous reading in place.
///
/// A render that returns early when its data is missing leaves whatever the
/// last payload put there, indefinitely, drawn exactly like a current value.
/// An em dash is honest; a number from an earlier tick is not.
function blankFields(ids) {
  for (const id of ids) {
    const el = document.getElementById(id);
    if (!el) continue;
    if (el.tagName === 'TBODY') continue;
    // Bars and badges carry their state in style/class, not text.
    if (/-bar$/.test(id)) { el.style.width = '0%'; continue; }
    el.textContent = '\u2014';
  }
}

const ML_FIELDS = ['ml-p-up', 'ml-fusion-p-up'];
const DEEP_FIELDS = ['deep-p-up', 'deep-blended', 'deep-mc-unc', 'deep-skip'];
const LIQ_FIELDS = [
  'liq-30s-long', 'liq-30s-short', 'liq-30s-ratio',
  'liq-5m-long', 'liq-5m-short', 'liq-5m-ratio', 'liq-ratio-bar',
];
const NO_ARB_FIELDS = ['no-arb-summary', 'no-arb-groups', 'no-arb-max', 'no-arb-worst'];

function updateMlFromAnalysis(a) {
  const ml = a?.ml;
  if (!ml) { blankFields(ML_FIELDS); return; }
  const el = document.getElementById('ml-p-up');
  if (el) el.textContent = ml.p_up != null ? ml.p_up.toFixed(3) : '—';
  const fel = document.getElementById('ml-fusion-p-up');
  if (fel) fel.textContent = a.p_up != null ? a.p_up.toFixed(3) : '—';
}

// Poll ML state lightly; SSE covers the fast path.
setInterval(() => { if (shouldPollReadLoop()) loadMlState(); }, 30_000);

// ── Deep Model panel ─────────────────────────────────────────────────────────
async function loadDeepModelState() {
  // Serves two tabs: the deep-model cards on Overview and the liquidation
  // panel on Signals.
  if (initialLoadComplete && !['overview', 'signals', 'model'].includes(activeTab)) return;
  try {
    const r = await GET('/api/deep-model/status');
    updateLiquidationPanel(r);
    const model = r.model || {};
    const ensemble = r.ensemble || {};
    const lastPred = r.last_prediction || {};

    // Model info
    const badge = document.getElementById('deep-phase-badge');
    if (model.loaded) {
      document.getElementById('deep-status').innerHTML = '<span style="color:var(--clr-green);">Loaded</span>';
      badge.textContent = (ensemble.phase || 'shadow').toUpperCase();
      badge.className = 'badge ' + (ensemble.phase === 'active' ? 'bg-success' : ensemble.phase === 'ramping' ? 'bg-warning' : 'bg-secondary');
    } else {
      document.getElementById('deep-status').innerHTML = '<span class="text-muted">Not loaded</span>';
      badge.textContent = 'No Model';
      badge.className = 'badge bg-secondary';
    }
    document.getElementById('deep-version').textContent = model.version || '—';
    if (model.loaded_ago_secs != null) {
      const secs = model.loaded_ago_secs;
      const mins = Math.floor(secs / 60);
      const hrs = Math.floor(mins / 60);
      document.getElementById('deep-loaded-ago').textContent = hrs > 0 ? hrs + 'h ' + (mins % 60) + 'm' : mins > 0 ? mins + 'm ' + (secs % 60) + 's' : secs + 's';
    } else {
      document.getElementById('deep-loaded-ago').textContent = '—';
    }
    document.getElementById('deep-inference-ms').textContent = model.inference_ms != null ? model.inference_ms.toFixed(1) + 'ms' : '—';

    // Ensemble
    document.getElementById('deep-ens-phase').textContent = ensemble.phase || 'shadow';
    const wPct = ((ensemble.deep_weight || 0) * 100);
    document.getElementById('deep-ens-weight').textContent = wPct.toFixed(1) + '%';
    document.getElementById('deep-ens-ramp').textContent = `${ensemble.ramp_step || 0} / 100`;
    document.getElementById('deep-ens-samples').textContent = String(ensemble.deep_total_samples || 0);

    // Promotion progress bar
    const deepSamples = ensemble.deep_total_samples || 0;
    const promoPct = Math.min(100, (deepSamples / 200) * 100);
    document.getElementById('deep-promo-bar').style.width = promoPct + '%';
    document.getElementById('deep-promo-label').textContent = deepSamples + ' / 200';

    // Accuracy
    document.getElementById('deep-acc-50').textContent = ensemble.deep_accuracy_50 != null ? (ensemble.deep_accuracy_50 * 100).toFixed(1) + '%' : '—';
    document.getElementById('deep-acc-200').textContent = ensemble.deep_accuracy_200 != null ? (ensemble.deep_accuracy_200 * 100).toFixed(1) + '%' : '—';
    document.getElementById('deep-brier-50').textContent = ensemble.deep_brier_50 != null ? ensemble.deep_brier_50.toFixed(3) : '—';
    document.getElementById('deep-cal-pts').textContent = (ensemble.calibration_breakpoints || 0) + ' pts';

    // Last prediction
    document.getElementById('deep-p-up').textContent = lastPred.deep_p_up != null ? lastPred.deep_p_up.toFixed(3) : '—';
    document.getElementById('deep-blended').textContent = lastPred.blended_p_up != null ? lastPred.blended_p_up.toFixed(3) : '—';
    document.getElementById('deep-mc-unc').textContent = lastPred.mc_uncertainty != null ? lastPred.mc_uncertainty.toFixed(4) : '—';
    document.getElementById('deep-skip').textContent = lastPred.should_skip != null ? (lastPred.should_skip ? 'YES' : 'no') : '—';

    // Weight bar
    const maxW = 60; // MAX_DEEP_WEIGHT = 0.60
    const barPct = Math.min(wPct / maxW * 100, 100);
    document.getElementById('deep-weight-bar').style.width = barPct + '%';
    document.getElementById('deep-weight-label').textContent = 'Deep Weight: ' + wPct.toFixed(1) + '%';
  } catch (e) {
    logReadError('loadDeepModelState', e);
  }
}

function updateDeepFromAnalysis(a) {
  const dm = a?.deep_model;
  if (!dm) { blankFields(DEEP_FIELDS); return; }
  const el = document.getElementById('deep-p-up');
  // Writing only when the value is present leaves the last good reading on
  // screen, which is indistinguishable from a current one. Absent must show.
  if (el) el.textContent = fmtN(dm.deep_p_up, 3);
  const bel = document.getElementById('deep-blended');
  if (bel) bel.textContent = fmtN(dm.blended_p_up, 3);
  const mel = document.getElementById('deep-mc-unc');
  if (mel) mel.textContent = fmtN(dm.mc_uncertainty, 4);
  const sel = document.getElementById('deep-skip');
  if (sel) sel.textContent = dm.should_skip == null ? '—' : (dm.should_skip ? 'YES' : 'no');

  const badge = document.getElementById('deep-phase-badge');
  if (badge && dm.ensemble_phase) {
    badge.textContent = dm.ensemble_phase.toUpperCase();
    badge.className = 'badge ' + (dm.ensemble_phase === 'active' ? 'bg-success' : dm.ensemble_phase === 'ramping' ? 'bg-warning' : 'bg-secondary');
  }
}

// Poll deep model state lightly; SSE covers the fast path.
setInterval(() => { if (shouldPollReadLoop()) loadDeepModelState(); }, 30_000);

// ── Phase 2 Intelligence Panels ────────────────────────────────────────────────
function updatePhase2Panels(a) {
  if (!a) return;
  const x = a;

  // ── Regime Detection ──
  const regBadge = document.getElementById('regime-badge');
  if (regBadge && a.regime) {
    regBadge.textContent = a.regime;
    const colors = {TREND_UP:'var(--clr-green)',TREND_DOWN:'var(--clr-red)',CHOP:'var(--clr-yellow)',SQUEEZE:'var(--clr-purple)',BREAKOUT:'#85dcff',RANGE:'var(--clr-blue)'};
    regBadge.style.background = colors[a.regime] || 'rgba(255,255,255,0.12)';
    regBadge.style.color = (a.regime==='CHOP'||a.regime==='BREAKOUT') ? '#0f1219' : '#fff';
  }
  const regPhase = document.getElementById('regime-phase');
  if (regPhase) regPhase.textContent = x.phase || '—';
  const setReg = (id, v, fmt) => {
    const e = document.getElementById(id);
    if (!e) return;
    e.textContent = v == null ? '\u2014' : (fmt ? fmt(v) : fmtN(v, 4));
  };
  setReg('reg-vwap-slope', x.vwap_slope);
  setReg('reg-rsi', x.rsi, v => fmtN(v, 1));
  setReg('reg-macd', x.macd_hist);
  setReg('reg-obi', x.ob_imbalance, v => fmtN(v, 3));
  setReg('reg-tv', x.tick_velocity_60s, v => fmtN(v, 2));

  // ── Granger Causality Weights ──
  const gc = x.granger;
  if (gc) {
    const container = document.getElementById('granger-bars');
    if (container) {
      const signals = [
        {name:'OB Imbalance', val:gc.orderbook_imbalance},
        {name:'Tick Velocity', val:gc.tick_velocity},
        {name:'Funding Rate', val:gc.funding_rate},
        {name:'Aggressor Ratio', val:gc.aggressor_ratio},
        {name:'VPIN', val:gc.vpin},
      ];
      container.innerHTML = signals.map(s => {
        const pct = ((s.val - 0.65) / 0.50 * 100).toFixed(0); // 0.65→0%, 1.15→100%
        const clr = s.val >= 1.05 ? 'var(--clr-green)' : s.val <= 0.80 ? 'var(--clr-red)' : 'var(--clr-blue)';
        return `<div style="display:flex;align-items:center;gap:6px;font-size:var(--fs-sm);">
          <span style="width:100px;color:var(--clr-muted);">${s.name}</span>
          <div style="flex:1;height:7px;background:rgba(255,255,255,0.06);border-radius:var(--r-sm);overflow:hidden;">
            <div style="height:100%;width:${Math.max(2,Math.min(100,pct))}%;background:${clr};border-radius:var(--r-sm);transition:width 0.4s;"></div>
          </div>
          <span style="width:36px;text-align:right;font-family:'IBM Plex Mono',monospace;">${fmtN(s.val,2)}</span>
        </div>`;
      }).join('');
    }
  }

  // ── Counterfactual Scenario Tree ──
  const cf = x.counterfactual;
  if (cf) {
    const setTxt = (id, v) => { const e = document.getElementById(id); if (e) e.textContent = v; };
    const evClr = v => v >= 0 ? 'var(--clr-green)' : 'var(--clr-red)';
    const yevEl = document.getElementById('cf-yes-ev');
    if (yevEl) { yevEl.textContent = fmtN(cf.yes_ev_pct, 2) + '%'; yevEl.style.color = evClr(cf.yes_ev_pct); }
    const nevEl = document.getElementById('cf-no-ev');
    if (nevEl) { nevEl.textContent = fmtN(cf.no_ev_pct, 2) + '%'; nevEl.style.color = evClr(cf.no_ev_pct); }
    setTxt('cf-max-loss', fmtN(cf.max_loss_pct, 2) + '%');
    const actEl = document.getElementById('cf-action');
    if (actEl) {
      actEl.textContent = cf.recommended_action || '—';
      actEl.style.color = cf.recommended_action === 'Hold' ? 'var(--clr-yellow)' : 'var(--clr-green)';
    }
    if (cf.scenarios) {
      const tbody = document.getElementById('cf-tbody');
      if (tbody) {
        tbody.innerHTML = cf.scenarios.map(s => {
          const moveClr = s.btc_move_pct >= 0 ? 'var(--clr-green)' : 'var(--clr-red)';
          return `<tr>
            <td>${ledgerEsc(s.name)}</td>
            <td style="color:${moveClr}">${s.btc_move_pct >= 0 ? '+' : ''}${fmtN(s.btc_move_pct, 2)}%</td>
            <td>${fmtN(s.yes_price, 3)}</td>
            <td style="color:${s.yes_pnl_pct >= 0 ? 'var(--clr-green)' : 'var(--clr-red)'}">${fmtN(s.yes_pnl_pct, 1)}%</td>
            <td style="color:${s.no_pnl_pct >= 0 ? 'var(--clr-green)' : 'var(--clr-red)'}">${fmtN(s.no_pnl_pct, 1)}%</td>
            <td>${fmtN(s.probability * 100, 0)}%</td>
          </tr>`;
        }).join('');
      }
    }
  }

  // ── Latency Arb ──
  const arb = x.latency_arb;
  if (arb) {
    const badge = document.getElementById('arb-badge');
    if (badge) {
      if (arb.arb_detected) {
        badge.textContent = 'ARB DETECTED';
        badge.className = 'badge bg-warning text-dark';
      } else {
        badge.textContent = 'NO ARB';
        badge.className = 'badge bg-secondary';
      }
    }
    const confEl = document.getElementById('arb-conf');
    if (confEl) confEl.textContent = arb.arb_detected ? `${fmtPct(arb.confidence * 100)} conf` : '';
    const setA = (id, v) => { const e = document.getElementById(id); if (e) e.textContent = v; };
    setA('arb-move', fmtN(arb.binance_move_pct, 3) + '%');
    setA('arb-dir', arb.expected_direction > 0 ? 'BUY YES' : arb.expected_direction < 0 ? 'BUY NO' : '—');
    setA('arb-lag', arb.lag_cents != null ? fmtN(arb.lag_cents, 1) : '—');
    const dirEl = document.getElementById('arb-dir');
    if (dirEl) dirEl.style.color = arb.expected_direction > 0 ? 'var(--clr-green)' : arb.expected_direction < 0 ? 'var(--clr-red)' : '';
  }

  // ── Cross-Market Signals ──
  const cm = x.cross_market;
  if (cm) {
    const netEl = document.getElementById('cm-net');
    if (netEl) {
      netEl.textContent = (cm.net_signal >= 0 ? '+' : '') + fmtN(cm.net_signal, 2);
      netEl.style.color = cm.net_signal > 0.1 ? 'var(--clr-green)' : cm.net_signal < -0.1 ? 'var(--clr-red)' : '';
    }
    const confEl = document.getElementById('cm-conf');
    if (confEl) confEl.textContent = fmtPct(cm.confidence * 100);
  }
  const cms = x.cross_market_signals;
  if (cms) {
    const container = document.getElementById('cm-signals');
    if (container) {
      if (cms.length === 0) {
        container.innerHTML = '<span class="text-muted">No recent cross-market resolutions</span>';
      } else {
        container.innerHTML = cms.map(s => {
          const clr = s.resolved_up ? 'var(--clr-green)' : 'var(--clr-red)';
          const dir = s.resolved_up ? 'UP' : 'DOWN';
          return `<span class="badge me-1" style="background:${clr};color:#0f1219;font-size:var(--fs-xs);">${ledgerEsc(s.asset)} ${dir} (${Math.round(s.secs_ago)}s ago)</span>`;
        }).join('');
      }
    }
  }

  // ── VPIN Microstructure ──
  const vp = x.vpin;
  if (vp) {
    const valEl = document.getElementById('vpin-value');
    if (valEl) {
      valEl.textContent = fmtN(vp.vpin, 3);
      valEl.style.color = vp.vpin > 0.7 ? 'var(--clr-red)' : vp.vpin > 0.5 ? 'var(--clr-yellow)' : 'var(--clr-green)';
    }
    const bar = document.getElementById('vpin-bar');
    if (bar) bar.style.width = (vp.vpin * 100).toFixed(0) + '%';
    const setV = (id, v) => { const e = document.getElementById(id); if (e) e.textContent = fmtN(v, 4); };
    setV('vpin-imbalance', vp.trade_imbalance);
    setV('vpin-vwap-dev', vp.vwap_deviation);
    setV('vpin-signal', vp.signal);
    setV('vpin-conf', vp.confidence);
    const imbEl = document.getElementById('vpin-imbalance');
    if (imbEl) imbEl.style.color = vp.trade_imbalance > 0.2 ? 'var(--clr-green)' : vp.trade_imbalance < -0.2 ? 'var(--clr-red)' : '';
  }
}

function timeSince(isoStr) {
  try {
    const ms = Date.now() - new Date(isoStr).getTime();
    if (!isFinite(ms) || ms < 0) return '—';
    if (ms < 60000) return Math.round(ms / 1000) + 's ago';
    if (ms < 3600000) return Math.round(ms / 60000) + 'm ago';
    if (ms < 86400000) return Math.round(ms / 3600000) + 'h ago';
    return Math.round(ms / 86400000) + 'd ago';
  } catch { return '—'; }
}

// ── Liquidation panel (from deep_model_status SSE) ──
function updateLiquidationPanel(dms) {
  if (!dms?.liquidations) { blankFields(LIQ_FIELDS); return; }
  const liq = dms.liquidations;
  const fmtVol = v => v != null ? (v >= 1e6 ? (v/1e6).toFixed(1)+'M' : v >= 1e3 ? (v/1e3).toFixed(0)+'K' : fmtN(v,0)) : '—';
  const set = (id, v) => { const e = document.getElementById(id); if (e) e.textContent = v; };

  if (liq.window_30s) {
    set('liq-30s-long', fmtVol(liq.window_30s.long_vol));
    set('liq-30s-short', fmtVol(liq.window_30s.short_vol));
    set('liq-30s-ratio', liq.window_30s.ratio != null ? fmtN(liq.window_30s.ratio, 2) : '—');
    const ratioBar = document.getElementById('liq-ratio-bar');
    // A missing ratio used to draw a perfectly balanced bar, which claims
    // balance rather than admitting the reading is absent.
    const ratio30 = liq.window_30s?.ratio;
    if (ratioBar) ratioBar.style.width = ratio30 == null ? '0%' : (ratio30 * 100).toFixed(0) + '%';
  }
  if (liq.window_5m) {
    set('liq-5m-long', fmtVol(liq.window_5m.long_vol));
    set('liq-5m-short', fmtVol(liq.window_5m.short_vol));
    set('liq-5m-ratio', liq.window_5m.ratio != null ? fmtN(liq.window_5m.ratio, 2) : '—');
  }
  const spikeBadge = document.getElementById('liq-spike-badge');
  if (spikeBadge && liq.spike_z_score != null) {
    const z = liq.spike_z_score;
    spikeBadge.textContent = `z=${fmtN(z, 1)}`;
    spikeBadge.className = 'badge ms-2 ' + (z > 2.0 ? 'bg-danger' : z > 1.0 ? 'bg-warning text-dark' : 'bg-secondary');
    spikeBadge.style.fontSize = '0.6rem';
  }
}

// ── Signal panel ───────────────────────────────────────────────────────────────
function updateSignalPanel(a) {
  if (!a || a === null) return;

  // ── Market, clock and the two token prices ───────────────────────────────
  // Everything below already existed; this section gives the decision its
  // context so the panel answers "which market, how long, at what price"
  // before it answers "what did we decide".
  // The identifier is what says a market is in play. market_data can carry a
  // candidate question while the bot is still waiting, and keying on the
  // question alone made this panel announce "No active market" at 0:00 while
  // the Tape, reading the same payload correctly, showed the live market with
  // fourteen minutes left. Two panels, one payload, opposite answers.
  const md = a.market_data || {};
  const hasMarket = !!(md.market_id || md.id) || !!a.question;
  const mktQ = document.getElementById('mkt-question');
  if (mktQ) {
    const q = md.question || a.question || '';
    mktQ.textContent = hasMarket && q ? q : 'No active BTC Up/Down market';
    mktQ.classList.toggle('is-idle', !(hasMarket && q));
  }

  const mins = a.mins_left ?? a.minutes_left ?? a.time_remaining_minutes ?? null;
  const countdown = document.getElementById('mkt-countdown');
  const track = document.getElementById('mkt-track-fill');
  if (countdown) {
    const live = hasMarket && mins !== null && Number.isFinite(Number(mins)) && Number(mins) >= 0 && Number(mins) < 600;
    if (live) {
      const secs = Math.max(0, Number(mins) * 60);
      countdown.textContent = Math.floor(secs / 60) + ':' + String(Math.floor(secs % 60)).padStart(2, '0');
      // Elapsed, not remaining: the bar fills as the window closes.
      if (track) track.style.width = Math.max(0, Math.min(100, (1 - Number(mins) / 15) * 100)).toFixed(1) + '%';
    } else {
      countdown.textContent = '—';
      if (track) track.style.width = '0%';
    }
    countdown.classList.toggle('is-urgent', live && Number(mins) < 2);
  }

  // Each token's live price comes from its own fresh book. The analysis
  // snapshot can supply an older YES price, but cannot establish a NO quote.
  const snapYes = Number(a.price ?? (a.market_data && a.market_data.price));
  const hasSnap = Number.isFinite(snapYes) && snapYes > 0 && snapYes < 1;
  const live = liveOddsFor(a.market_data && a.market_data.market_id);
  const yesPx = live ? live.yes : snapYes;
  const noPx = live ? live.no : null;
  const hasPx = live ? true : hasSnap;
  const snapAge = typeof currentAnalysisAge === 'function' ? currentAnalysisAge() : AGE_UNKNOWN;
  const source = live ? 'live' : (hasSnap && snapAge !== AGE_UNKNOWN ? `${fmtAge(snapAge)} ago` : (hasSnap ? 'analysis' : ''));
  const pUp = Number(a.p_up);
  const hasModel = Number.isFinite(pUp) && pUp > 0 && pUp < 1;
  const setOdd = (tileId, priceId, modelId, side, price, model) => {
    const tile = document.getElementById(tileId);
    const pe = document.getElementById(priceId);
    const me = document.getElementById(modelId);
    const se = tile ? tile.querySelector('.odd-side') : null;
    if (pe) pe.textContent = Number.isFinite(price) ? price.toFixed(2) : '—';
    if (me) me.textContent = Number.isFinite(model) ? model.toFixed(2) : '—';
    if (se) se.textContent = source && Number.isFinite(price) ? `${side} · ${source}` : side;
    if (tile) tile.classList.toggle('is-live-price', Boolean(live) && Number.isFinite(price));
  };
  setOdd('odd-yes', 'odds-yes', 'odds-yes-model', 'Yes · up', hasPx ? yesPx : NaN, hasModel ? pUp : NaN);
  setOdd('odd-no', 'odds-no', 'odds-no-model', 'No · down', hasPx ? noPx : NaN, hasModel ? 1 - pUp : NaN);

  // Highlight the side the model favours, and only when it favours one.
  const yesCell = document.getElementById('odd-yes');
  const noCell = document.getElementById('odd-no');
  if (yesCell && noCell) {
    yesCell.classList.toggle('is-favoured', hasModel && pUp > 0.5);
    noCell.classList.toggle('is-favoured', hasModel && pUp < 0.5);
  }

  const signal = a.signal || '—';
  const badge = document.getElementById('signal-badge');
  badge.textContent = signal;
  badge.className = 'signal-badge signal-' + signal;

  const conf = a.confidence == null ? null : Number(a.confidence) * 100;
  document.getElementById('sig-confidence').textContent = fmtPct(conf);
  document.getElementById('sig-conf-bar').style.width = Math.min(conf, 100) + '%';
  document.getElementById('sig-conf-bar').className = 'progress-bar ' +
    (conf > 70 ? 'bg-success' : conf > 50 ? 'bg-warning' : 'bg-secondary');

  document.getElementById('sig-reason').textContent = a.reason || '—';
  document.getElementById('sig-regime').textContent = a.regime || '—';
  document.getElementById('sig-phase').textContent  = a.phase  || '—';
  document.getElementById('sig-ev').textContent     = fmtN(a.net_ev, 3);

  const btcChg = a.btc_change_pct ?? null;
  const btcEl  = document.getElementById('sig-btc-change');
  if (btcChg !== null) {
    btcEl.textContent = (btcChg >= 0 ? '+' : '') + fmtN(btcChg, 3) + '%';
    btcEl.className   = btcChg >= 0 ? 'small fw-bold pnl-pos' : 'small fw-bold pnl-neg';
  } else {
    btcEl.textContent = '—';
    btcEl.className = 'small fw-bold';
  }

  // `mins_left` and `minutes_left` are legacy aliases the server has never
  // sent; `time_remaining_minutes` is the emitted key. Without it on the end of
  // the chain this tile read as absent while the figure was right there in the
  // payload, and it had been doing so for as long as the panel existed.
  const minsLeft = a.mins_left ?? a.minutes_left ?? a.time_remaining_minutes ?? null;
  // Between markets the stream still carries 0.0 here (REST sends null), and
  // "0.0m" reads as a market expiring this second when there is no market at
  // all. Gate on the market the way the countdown clocks do.
  const sigMd = a.market_data || {};
  const sigHasMarket = !!(sigMd.market_id || sigMd.id) || !!a.question;
  document.getElementById('sig-mins-left').textContent =
    sigHasMarket && minsLeft !== null ? fmtN(minsLeft, 1) + 'm' : '—';

  // Weights
  updateWeights(a);

  // Altcoin correlation (#14)
  if (a.altcoin_correlation) {
    const ac = a.altcoin_correlation;
    const ethEl = document.getElementById('eth-change');
    const solEl = document.getElementById('sol-change');
    if (ethEl) {
      ethEl.textContent = (ac.eth_change_pct >= 0 ? '+' : '') + fmtN(ac.eth_change_pct, 3) + '%';
      ethEl.className = 'stat-value ' + (ac.eth_change_pct >= 0 ? 'pnl-pos' : 'pnl-neg');
    }
    if (solEl) {
      solEl.textContent = (ac.sol_change_pct >= 0 ? '+' : '') + fmtN(ac.sol_change_pct, 3) + '%';
      solEl.className = 'stat-value ' + (ac.sol_change_pct >= 0 ? 'pnl-pos' : 'pnl-neg');
    }
  }

  // Coinbase divergence (#30)
  if (a.coinbase_divergence_pct !== undefined && a.coinbase_divergence_pct !== null) {
    const div = a.coinbase_divergence_pct;
    const badge = document.getElementById('ob-depth-badge');
    if (badge) badge.textContent = 'CB div: ' + (div >= 0 ? '+' : '') + fmtN(div, 4) + '%';
  }
}

// Track which signal groups are collapsed — persist to localStorage, default all collapsed
function readCollapsedSignalGroups() {
  const defaults = ['MICRO', 'MOMENTUM', 'MACRO', 'TEMPORAL', 'LIQUIDATION'];
  try {
    const stored = JSON.parse(localStorage.getItem('collapsedSignalGroups') || 'null');
    return new Set(Array.isArray(stored) ? stored.filter(value => typeof value === 'string') : defaults);
  } catch { return new Set(defaults); }
}
const collapsedSignalGroups = readCollapsedSignalGroups();

function updateWeights(a) {
  const container = document.getElementById('weights-container');
  const debug = a.fusion_debug || {};
  const normalizedWeights = debug.normalized_weights || {};
  const configuredWeights = debug.configured_weights || {};
  const weightedContributions = debug.weighted_contributions || null;
  const signals = a.signals || null;
  if (!weightedContributions && !signals) {
    const allKeys = [
      'orderbook_imbalance','tick_velocity','vwap_confluence','price_divergence',
      'spike_detector','deribit_pcr','rsi_macd_ha','fear_greed','funding_rate',
      'oi_momentum','aggressor_ratio','coingecko_sentiment','finnhub_sentiment','lunarcrush_sentiment',
      'polymarket_flow','vpin_microstructure','options_advanced','whale_flow','cross_market','latency_arb','news_sentiment',
      'depth_imbalance_5','volatility_10','volatility_30','window_momentum',
      'hour_of_day','day_of_week','distance_from_round_1000','distance_from_round_500',
      'long_liq_volume','short_liq_volume','btc_final_momentum','kyle_lambda','hawkes_flow'
    ];
    const weights = {};
    let any = false;
    for (const k of allKeys) {
      if (configuredWeights[k] !== undefined) { weights[k] = configuredWeights[k]; any = true; }
      else if (a[k] !== undefined) { weights[k] = a[k]; any = true; }
    }
    if (!any) return;
    renderWeightBars(container, weights, {
      configuredWeights,
      normalizedWeights,
      primaryLabel: 'Configured weights',
      valueKind: 'weight',
      timeOfDayWeightingEnabled: !!debug.time_of_day_weighting_enabled,
    });
    return;
  }
  renderWeightBars(container, weightedContributions || signals, {
    configuredWeights,
    normalizedWeights,
    primaryLabel: weightedContributions ? 'Weighted live contributions' : 'Signed signal strengths',
    valueKind: weightedContributions ? 'contribution' : 'signal',
    timeOfDayWeightingEnabled: !!debug.time_of_day_weighting_enabled,
  });
}

function renderWeightBars(container, values, meta = {}) {
  const normalizedWeights = meta.normalizedWeights || {};
  const configuredWeights = meta.configuredWeights || {};
  const labels = {
    orderbook_imbalance: 'Orderbook Imbalance',
    tick_velocity:       'Tick Velocity',
    vwap_confluence:     'VWAP Confluence',
    price_divergence:    'Price Divergence',
    spike_detector:      'Spike Detector',
    deribit_pcr:         'Deribit PCR',
    rsi_macd_ha:         'RSI/MACD/HA',
    fear_greed:          'Fear & Greed',
    funding_rate:        'Funding Rate',
    oi_momentum:         'OI Momentum',
    aggressor_ratio:     'Aggressor Ratio',
    coingecko_sentiment: 'CoinGecko Sentiment',
    finnhub_sentiment:   'Finnhub Sentiment',
    lunarcrush_sentiment:'LunarCrush Sentiment',
    polymarket_flow:     'Polymarket Flow',
    vpin_microstructure: 'VPIN Microstructure',
    options_advanced:    'Options Advanced',
    whale_flow:          'Whale Flow',
    cross_market:        'Cross Market',
    latency_arb:         'Latency Arb',
    news_sentiment:      'News Sentiment',
    depth_imbalance_5:   'Depth Imbalance 5',
    volatility_10:       'Volatility 10',
    volatility_30:       'Volatility 30',
    window_momentum:     'Window Momentum',
    hour_of_day:         'Hour of Day',
    day_of_week:         'Day of Week',
    distance_from_round_1000: 'Dist Round 1000',
    distance_from_round_500:  'Dist Round 500',
    long_liq_volume:     'Long Liq Volume',
    short_liq_volume:    'Short Liq Volume',
    btc_final_momentum:  'BTC Final Momentum',
    kyle_lambda:         'Kyle Lambda',
    hawkes_flow:         'Hawkes Flow',
  };

  const signalGroups = {
    'MICRO': ['orderbook_imbalance', 'tick_velocity', 'aggressor_ratio', 'depth_imbalance_5', 'vpin_microstructure', 'polymarket_flow', 'kyle_lambda', 'hawkes_flow'],
    'MOMENTUM': ['rsi_macd_ha', 'price_divergence', 'spike_detector', 'vwap_confluence', 'oi_momentum', 'volatility_10', 'volatility_30', 'window_momentum', 'btc_final_momentum'],
    'MACRO': ['fear_greed', 'funding_rate', 'deribit_pcr', 'coingecko_sentiment', 'finnhub_sentiment', 'lunarcrush_sentiment', 'news_sentiment', 'options_advanced', 'whale_flow'],
    'CROSS-ASSET': ['cross_market', 'latency_arb'],
    'TEMPORAL': ['hour_of_day', 'day_of_week', 'distance_from_round_1000', 'distance_from_round_500'],
    'LIQUIDATION': ['long_liq_volume', 'short_liq_volume']
  };

  const allKeys = [...new Set([
    ...Object.keys(values || {}),
    ...Object.keys(normalizedWeights),
    ...Object.keys(configuredWeights),
  ])];
  const maxAbs = allKeys.reduce((max, key) => {
    const v = Math.abs(Number(values?.[key] ?? 0));
    return Number.isFinite(v) ? Math.max(max, v) : max;
  }, 0);
  const scaleBase = maxAbs > 0 ? maxAbs : 1;
  const activeCount = allKeys.filter(key => Math.abs(Number(values?.[key] ?? 0)) > 1e-6).length;

  function renderBar(key) {
    const label = labels[key] || key;
    const v = Number(values?.[key] ?? 0);
    const pct = Math.min((Math.abs(v) / scaleBase) * 100, 100);
    const cls = v > 0.05 ? 'weight-positive' : v < -0.05 ? 'weight-negative' : 'weight-neutral';
    const sign = v > 0 ? '+' : '';
    const metaBits = [];
    if (normalizedWeights[key] !== undefined) metaBits.push(`w ${fmtPct(normalizedWeights[key] * 100)}`);
    if (configuredWeights[key] !== undefined) metaBits.push(`cfg ${fmtN(configuredWeights[key], 3)}`);
    const metaLine = metaBits.length
      ? `<div class="text-muted" style="font-size:var(--fs-xs);">${metaBits.join(' • ')}</div>`
      : '';
    return `
      <div class="mb-2">
        <div class="d-flex justify-content-between mb-1">
          <span class="text-muted" style="font-size:var(--fs-sm);">${ledgerEsc(label)}</span>
          <span style="font-size:var(--fs-sm); ${v > 0.05 ? 'color:var(--clr-green)' : v < -0.05 ? 'color:var(--clr-red)' : 'color:var(--clr-muted)'}">${sign}${fmtN(v, 3)}</span>
        </div>
        ${metaLine}
        <div class="weight-bar-track">
          <div class="weight-bar-fill ${cls}" style="width:${pct}%"></div>
        </div>
      </div>`;
  }

  let html = `
    <div class="d-flex justify-content-between align-items-center mb-2">
      <div class="text-muted" style="font-size:var(--fs-xs);">${ledgerEsc(meta.primaryLabel || 'Fusion values')}${meta.timeOfDayWeightingEnabled ? ' • TOD on' : ''}</div>
      <div class="text-muted" style="font-size:var(--fs-xs);">${activeCount}/${allKeys.length} active</div>
    </div>
    <div class="text-muted mb-2" style="font-size:var(--fs-xs);">
      Bars are scaled to the strongest current value. Secondary values show normalized live weight and configured base weight.
    </div>`;
  for (const [groupName, keys] of Object.entries(signalGroups)) {
    const groupWeights = keys.filter(k => allKeys.includes(k));
    if (!groupWeights.length) continue;

    const isCollapsed = collapsedSignalGroups.has(groupName);
    const arrow = isCollapsed ? '&#9654;' : '&#9660;';
    html += `<div style="margin-bottom:6px;">
      <div onclick="toggleSignalGroup('${groupName}')" style="cursor:pointer; display:flex; align-items:center; gap:6px; padding:3px 0; border-bottom:1px solid rgba(255,255,255,0.08); margin-bottom:4px; user-select:none;">
        <span style="font-size:var(--fs-xs); color:var(--clr-muted); width:10px;">${arrow}</span>
        <span style="font-size:var(--fs-xs); font-weight:600; color:var(--clr-muted); text-transform:uppercase; letter-spacing:0.08em; font-family:'IBM Plex Mono',monospace;">${groupName}</span>
        <span style="font-size:var(--fs-xs); color:var(--clr-muted); margin-left:auto;">${groupWeights.length}</span>
      </div>
      <div id="signal-group-${groupName}" style="display:${isCollapsed ? 'none' : 'block'};">`;

    for (const key of groupWeights) {
      html += renderBar(key);
    }
    html += `</div></div>`;
  }

  // Render any ungrouped signals
  const allGrouped = new Set(Object.values(signalGroups).flat());
  for (const key of allKeys) {
    if (allGrouped.has(key)) continue;
    html += renderBar(key);
  }

  container.innerHTML = html || '<div class="text-muted small text-center mt-4">No fusion debug available</div>';
}

function toggleSignalGroup(groupName) {
  if (collapsedSignalGroups.has(groupName)) {
    collapsedSignalGroups.delete(groupName);
  } else {
    collapsedSignalGroups.add(groupName);
  }
  try { localStorage.setItem('collapsedSignalGroups', JSON.stringify([...collapsedSignalGroups])); } catch {}
  const el = document.getElementById('signal-group-' + groupName);
  if (el) el.style.display = collapsedSignalGroups.has(groupName) ? 'none' : 'block';
  const parent = el?.parentElement;
  if (parent) {
    const arrowEl = parent.querySelector('div > span:first-child');
    if (arrowEl) arrowEl.innerHTML = collapsedSignalGroups.has(groupName) ? '&#9654;' : '&#9660;';
  }
}

// ── Stats bar ─────────────────────────────────────────────────────────────────
function updateStatsBar(stats, dailyPnl) {
  window._lastStats = stats;
  if (!stats) return;
  const r = stats;

  // The bar is fed two payload shapes: /api/trades/analysis uses
  // total_closed/max_drawdown, /api/stats performance uses closed_trades and
  // max_drawdown_pct. Reading only the first set made the header show 0 trades
  // and 0.0% whenever the stats path ran last, contradicting every other panel.
  const first = (...vals) => vals.find(v => v !== undefined && v !== null);
  const totalT = first(r.total_trades, r.total_closed, r.closed_trades);
  const wrRaw  = r.win_rate;
  // `wins ?? 0` divided by a real total is a confident 0% the moment a payload
  // carries win_rate without wins — a definite claim from a missing field.
  // Derive from wins only when wins is actually present; otherwise take the
  // rate the payload gives; otherwise admit there is none.
  const closedT = first(r.total_closed, r.closed_trades, r.total_trades);
  const wr = Number(closedT) === 0 ? null : (r.wins !== undefined && r.wins !== null && closedT > 0)
    ? (Number(r.wins) / closedT * 100)
    : (wrRaw === undefined || wrRaw === null ? null : (Number(wrRaw) <= 1 ? Number(wrRaw) * 100 : Number(wrRaw)));
  const tPnl   = first(r.total_pnl_net, r.total_pnl);
  const sharpe = first(r.sharpe_ratio_net, r.sharpe_ratio, r.sharpe);
  // Only a dollar drawdown belongs in a dollar field; max_drawdown_pct is a
  // percent, and printing it as dollars turns "-$4.33" into "-$8.70".
  const maxdd  = first(r.max_drawdown_net, r.max_drawdown);
  const openP  = first(r.total_open, r.open_trades);

  document.getElementById('st-total').textContent   = totalT ?? '—';
  document.getElementById('st-winrate').textContent = fmtPct(wr);
  document.getElementById('st-pnl').textContent     = fmt$(tPnl);
  document.getElementById('st-pnl').className       = 'value ' + clsPnl(tPnl);
  document.getElementById('st-sharpe').textContent  = fmtN(sharpe, 2);
  if (maxdd !== undefined) {
    document.getElementById('st-maxdd').textContent = fmt$(-maxdd);
  } else if (r.max_drawdown_pct !== undefined) {
    document.getElementById('st-maxdd').textContent = fmtPct(-Number(r.max_drawdown_pct));
  } else {
    document.getElementById('st-maxdd').textContent = '—';
  }
  document.getElementById('st-open').textContent    = openP ?? '—';

  if (dailyPnl !== undefined) {
    document.getElementById('st-daily').textContent = fmt$(dailyPnl);
    document.getElementById('st-daily').className   = 'value ' + clsPnl(dailyPnl);
    const evidence = r.daily_pnl_evidence;
    document.getElementById('st-daily').title = evidence?.evidence_complete === false
      ? 'Exact realized P&L is unknown: legacy partial sales need dated evidence. New entries are blocked until reconciled.'
      : `Realized P&L on ${evidence?.day_utc || 'the current day'} UTC, including partial sales. Live taker uses settlement recognition time; maker uses fill time.`;
  }
}

async function loadStats() {
  try {
    const r = await GET('/api/stats');
    updateStatsBar(r.performance || {}, r.daily_pnl);
    if (r.balance !== undefined) {
      document.getElementById('nav-balance').textContent = fmtMoney(r.balance);
    }
    if (r.mode) {
      runtimeModeState = String(r.mode || '').toLowerCase();
      const badge = document.getElementById('mode-badge');
      badge.textContent = r.mode.toUpperCase();
      badge.className = 'mode-badge ' + (r.mode === 'live' ? 'mode-live' : 'mode-sim');
    }
    if (r.loop_running !== undefined) updateLoopButtons(r.loop_running, r.backtest_only);
    updateDashboardStrip();
  } catch (e) {
    logReadError('loadStats', e);
  }
}

async function loadAnalysis() {
  try {
    const a = await GET('/api/analysis');
    // Only the stream used to fill this cache, so any panel reading it showed
    // "waiting for a market" whenever the stream was unauthenticated or down,
    // while the REST-fed panels beside it showed the live market.
    window._lastAnalysis = a;
    noteAnalysisFreshness(a.analysis_freshness);
    updateSignalPanel(a);
    updatePhase2Panels(a);
    updateMlFromAnalysis(a);
    updateDeepFromAnalysis(a);
    updateRealtimeContext(a, null);
    updateTape(a);
  } catch (e) {
    logReadError('loadAnalysis', e);
  }
}

// ── PnL Chart ─────────────────────────────────────────────────────────────────
function initChart() {
  const ctx = document.getElementById('pnl-chart').getContext('2d');
  pnlChart = new Chart(ctx, {
    type: 'line',
    data: {
      labels: [],
      datasets: [{
        label: 'Cumulative P&L ($)',
        data: [],
        borderColor: '#3fb950',
        backgroundColor: 'rgba(63,185,80,0.08)',
        borderWidth: 2,
        pointRadius: 0,
        pointHoverRadius: 4,
        fill: true,
        tension: 0.3,
      }],
    },
    plugins: [{
      id: 'zeroLine',
      beforeDraw(chart) {
        const yScale = chart.scales.y;
        if (!yScale) return;
        const y = yScale.getPixelForValue(0);
        const ctx = chart.ctx;
        ctx.save();
        ctx.beginPath();
        ctx.moveTo(chart.chartArea.left, y);
        ctx.lineTo(chart.chartArea.right, y);
        ctx.strokeStyle = 'rgba(255,255,255,0.3)';
        ctx.lineWidth = 1;
        ctx.setLineDash([4, 4]);
        ctx.stroke();
        ctx.restore();
      }
    }],
    options: {
      responsive: true,
      maintainAspectRatio: false,
      interaction: { intersect: false, mode: 'index' },
      plugins: {
        legend: { display: false },
        tooltip: {
          backgroundColor: '#161b22',
          borderColor: '#30363d',
          borderWidth: 1,
          titleColor: '#8b949e',
          bodyColor: '#e6edf3',
          callbacks: {
            label: ctx => ' P&L: ' + fmt$(ctx.parsed.y),
          },
        },
      },
      scales: {
        x: {
          grid: { color: 'rgba(48,54,61,0.5)' },
          ticks: { color: '#8b949e', maxTicksLimit: 8, font: { size: 10 } },
        },
        y: {
          grid: { color: 'rgba(48,54,61,0.5)' },
          ticks: {
            color: '#8b949e', font: { size: 10 },
            callback: v => '$' + v.toFixed(2),
          },
        },
      },
    },
  });
}

function updateChart(series) {
  if (!pnlChart || !series || !series.length) return;
  const labels = series.map(p => tsShort(p.ts));
  const data   = series.map(p => p.pnl);
  const color  = data[data.length - 1] >= 0 ? '#3fb950' : '#f85149';
  pnlChart.data.labels = labels;
  pnlChart.data.datasets[0].data = data;
  pnlChart.data.datasets[0].borderColor = color;
  pnlChart.data.datasets[0].backgroundColor = color + '14';
  pnlChart.update('none');
}

// ── BTC Price Chart ───────────────────────────────────────────────────────────
function initBtcChart() {
  const el = document.getElementById('btc-chart');
  if (!el) return;
  const ctx = el.getContext('2d');

  btcChart = new Chart(ctx, {
    type: 'line',
    data: {
      labels: [],
      datasets: [
        {
          label: 'BTC Price',
          data: [],
          borderColor: '#f2a900',
          backgroundColor: function(context) {
            const chart = context.chart;
            const { ctx: c, chartArea } = chart;
            if (!chartArea) return 'rgba(242,169,0,0.0)';
            const g = c.createLinearGradient(0, chartArea.top, 0, chartArea.bottom);
            g.addColorStop(0, 'rgba(242,169,0,0.22)');
            g.addColorStop(1, 'rgba(242,169,0,0.0)');
            return g;
          },
          borderWidth: 2,
          pointRadius: 0,
          pointHoverRadius: 5,
          pointBackgroundColor: '#f2a900',
          fill: true,
          tension: 0.3,
          order: 1,
        },
        {
          label: 'Price to Beat',
          data: [],
          borderColor: 'rgba(139,148,158,0.5)',
          backgroundColor: 'transparent',
          borderWidth: 1.5,
          borderDash: [5, 5],
          pointRadius: 0,
          pointHoverRadius: 0,
          fill: false,
          tension: 0,
          order: 2,
        },
      ],
    },
    options: {
      responsive: true,
      maintainAspectRatio: false,
      animation: false,
      interaction: { intersect: false, mode: 'index' },
      plugins: {
        legend: { display: false },
        tooltip: {
          backgroundColor: '#161b22',
          borderColor: '#30363d',
          borderWidth: 1,
          titleColor: '#8b949e',
          bodyColor: '#e6edf3',
          callbacks: {
            title: items => items[0]?.label || '',
            label: function(ctx) {
              if (ctx.datasetIndex === 0) {
                return '  BTC  $' + ctx.parsed.y.toLocaleString('en-US', {minimumFractionDigits:2, maximumFractionDigits:2});
              }
              if (ctx.datasetIndex === 1 && btcStartPriceVal > 0) {
                return '  Start $' + btcStartPriceVal.toLocaleString('en-US', {minimumFractionDigits:2, maximumFractionDigits:2});
              }
              return null;
            },
          },
        },
      },
      scales: {
        x: {
          grid:  { color: 'rgba(48,54,61,0.4)' },
          ticks: { color: '#8b949e', maxTicksLimit: 8, font: { size: 10 } },
        },
        y: {
          position: 'right',
          grid:  { color: 'rgba(48,54,61,0.4)' },
          ticks: {
            color: '#8b949e',
            font:  { size: 10 },
            callback: v => '$' + Math.round(v).toLocaleString('en-US'),
          },
        },
      },
    },
  });
}

async function loadBtcChart() {
  try {
    const r = await GET('/api/btc-chart');
    _applyBtcChartData(r);
  } catch (e) {
    logReadError('loadBtcChart', e);
  }
}

function _applyBtcChartData(r) {
  if (!btcChart) return;
  const candles = r.candles || [];
  if (!candles.length) return;

  btcStartPriceVal = r.start_price || 0;
  const currentPx  = r.current_price || 0;

  // Seed the ring buffer from historical candles (one-time on page load)
  if (btcPriceBuffer.length === 0) {
    for (const c of candles) {
      btcPriceBuffer.push({ ts: new Date(c.ts * 1000), price: c.close });
    }
  }

  const labels  = btcPriceBuffer.map(p =>
    p.ts.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' })
  );
  const closes  = btcPriceBuffer.map(p => p.price);
  if (currentPx > 0 && closes.length) closes[closes.length - 1] = currentPx;

  const refLine = closes.map(() => btcStartPriceVal > 0 ? btcStartPriceVal : null);
  const radii = closes.map((_, i) => i === closes.length - 1 ? 4 : 0);

  btcChart.data.labels = labels;
  btcChart.data.datasets[0].data         = closes;
  btcChart.data.datasets[0].pointRadius  = radii;
  btcChart.data.datasets[1].data         = refLine;
  btcChart.update('none');

  _updateBtcHeader(currentPx, btcStartPriceVal, r.market_question || null);
}

function updateBtcCurrentPrice(price, startPrice, startSource) {
  if (!btcChart || !price) return;
  if (startPrice && startPrice > 0) btcStartPriceVal = startPrice;
  if (startSource) {
    const srcEl = document.getElementById('btc-start-source');
    if (srcEl) srcEl.textContent = startSource === 'oracle' ? '(oracle)' : '(' + startSource + ')';
  }

  // Append to ring buffer (throttle to ~1 point/sec to avoid overwhelming chart)
  const now = new Date();
  const lastEntry = btcPriceBuffer[btcPriceBuffer.length - 1];
  if (!lastEntry || (now - lastEntry.ts) >= 1000) {
    btcPriceBuffer.push({ ts: now, price });
    if (btcPriceBuffer.length > BTC_CHART_MAX_POINTS) {
      btcPriceBuffer.shift();
    }
  } else {
    // Update last point's price (sub-second update)
    btcPriceBuffer[btcPriceBuffer.length - 1].price = price;
  }

  // Redraw chart from buffer
  if (btcPriceBuffer.length >= 2) {
    const labels = btcPriceBuffer.map(p =>
      p.ts.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' })
    );
    const closes = btcPriceBuffer.map(p => p.price);
    const refLine = closes.map(() => btcStartPriceVal > 0 ? btcStartPriceVal : null);
    const radii = closes.map((_, i) => i === closes.length - 1 ? 4 : 0);

    btcChart.data.labels = labels;
    btcChart.data.datasets[0].data = closes;
    btcChart.data.datasets[0].pointRadius = radii;
    btcChart.data.datasets[1].data = refLine;
    btcChart.update('none');
  }

  _updateBtcHeader(price, btcStartPriceVal, null);
}

function _updateBtcHeader(currentPx, startPx, question) {
  if (currentPx > 0) {
    document.getElementById('btc-current-price').textContent =
      '$' + currentPx.toLocaleString('en-US', { minimumFractionDigits: 2, maximumFractionDigits: 2 });
  }
  if (startPx > 0) {
    document.getElementById('btc-start-price').textContent =
      '$' + startPx.toLocaleString('en-US', { minimumFractionDigits: 2, maximumFractionDigits: 2 });
    if (currentPx > 0) {
      const chg = (currentPx - startPx) / startPx * 100;
      const el  = document.getElementById('btc-price-change');
      el.textContent = (chg >= 0 ? '+' : '') + chg.toFixed(3) + '%';
      el.className   = 'fw-bold ' + (chg >= 0 ? 'pnl-pos' : 'pnl-neg');
    }
  }
  if (question !== null && question !== '') {
    document.getElementById('btc-market-question').textContent = question;
  }
}

// ── Trades ────────────────────────────────────────────────────────────────────
let _lastTradesJson = '';
let _lastTradeAnalysisRefresh = 0;
let _loadTradesPromise = null;
async function loadTrades() {
  if (_loadTradesPromise) return _loadTradesPromise;
  _loadTradesPromise = (async () => {
    try {
      const r = await GET('/api/trades');
      const newJson = JSON.stringify(r.trades || []);
      const changed = newJson !== _lastTradesJson;
      _lastTradesJson = newJson;
      allTrades = r.trades || [];
      renderArchivedTrades();
      renderOpenTrades();
      if (changed) renderHistory();   // skip re-render if unchanged — preserves expanded rows
      updateStrategyStats();
      renderHourHeatmap();
      updateDashboardStrip();
      if (changed || Date.now() - _lastTradeAnalysisRefresh > 30_000) {
        await loadTradeAnalysis();
      }
      if (initialLoadComplete && activeTab === 'execution') {
        await loadExecutionQuality();
      }
    } catch (e) {
      logReadError('loadTrades', e);
    } finally {
      _loadTradesPromise = null;
    }
  })();
  return _loadTradesPromise;
}

// Open positions rendered from the REST poll and then sat still while the
// book moved every 200 ms, so a row read 0.68 beside an odds tile reading
// 0.77 for the same token. Re-render when — and only when — the live mid for
// an open position's token has actually changed since the last render.
const lastRenderedLiveMid = new Map();   // trade id → mid used at last render
function refreshOpenPositionsIfBookMoved() {
  if (!Array.isArray(allTrades)) return;
  let moved = false;
  for (const t of allTrades) {
    if (t.status !== 'OPEN' && t.status !== 'CLOSING') continue;
    const mid = liveMidFor(t.token_id);
    if (lastRenderedLiveMid.get(t.id) !== mid) { moved = true; break; }
  }
  if (moved) renderOpenTrades();
}

// A durable BUY phase is explicit in REST/stream payloads. The zero-share
// fallback keeps older cached responses conservative without confusing a
// rejected SELL (OPEN residual + order_id) with an unfinished entry.
function isPendingEntry(trade) {
  if (String(trade?.mode).toLowerCase() !== 'live' || String(trade?.status).toUpperCase() !== 'OPEN') return false;
  if (['pending', 'audit_uncertain', 'unavailable'].includes(trade.entry_execution_status)) return true;
  return Number(trade.shares) === 0 && typeof trade.order_id === 'string' && trade.order_id.length > 0;
}

function entryPendingLabel(trade) {
  if (trade.entry_execution_status === 'audit_uncertain') return 'Entry evidence needs revalidation';
  if (trade.entry_execution_status === 'unavailable' || !trade.entry_execution_status) return 'Entry confirmation unavailable';
  return 'Entry awaiting confirmation';
}

function renderOpenTrades() {
  const open = allTrades.filter(t => t.status === 'OPEN' || t.status === 'CLOSING');
  const tbody = document.getElementById('open-trades-tbody');
  document.getElementById('st-open').textContent = open.length;

  if (!open.length) {
    tbody.innerHTML = '<tr><td colspan="8" class="text-muted text-center py-3">No open positions</td></tr>';
    return;
  }
  tbody.innerHTML = open.map(t => {
    const strat = t.strategy || 'conviction';
    const stratBadge = strat === 'scalp'
      ? '<span class="badge" style="background:rgba(255,180,50,0.15); color:#ffb432; font-size:var(--fs-xs);">SCALP</span>'
      : strat === 'resolution_sniper'
      ? '<span class="badge" style="background:rgba(255,180,50,0.15); color:#ffb432; font-size:var(--fs-xs);"><i class="bi bi-crosshair" style="font-size:var(--fs-xs);"></i> SNIPE</span>'
      : '<span class="badge" style="background:rgba(88,166,255,0.12); color:var(--clr-blue); font-size:var(--fs-xs);">HOLD</span>';

    // Calculate unrealized P&L client-side if not provided by server
    const partialPending = ['in_flight', 'unknown'].includes(t.partial_close_state);
    const closing = t.status === 'CLOSING' || partialPending;
    const entryPending = isPendingEntry(t);
    let upnl = null;
    // The same live book the odds tiles read, so one token has one price on
    // the page; the per-row REST fetch is the fallback.
    const bookMid = liveMidFor(t.token_id);
    lastRenderedLiveMid.set(t.id, bookMid);
    const liveP = closing || entryPending ? null : bookMid;
    const entryP = entryPending ? null : t.entry_price;
    const shares = t.shares == null ? null : Number(t.shares);
    if (!closing && !entryPending && Number.isFinite(shares) && liveP != null && entryP != null && shares > 0) {
      if (t.direction === 'BUY_YES') {
        upnl = (liveP - entryP) * shares;
      } else {
        // NO tokens: profit when NO price rises (YES price drops)
        upnl = (liveP - entryP) * shares;
      }
    }

    return `
    <tr>
      <td class="text-muted">#${t.id}</td>
      <td>${stratBadge}</td>
      <td><span class="${t.direction === 'BUY_YES' ? 'dir-yes' : 'dir-no'}">${t.direction === 'BUY_YES' ? 'YES' : 'NO'}</span></td>
      <td>${fmtN(entryP, 3)}</td>
      <td>${liveP !== null && liveP !== undefined ? fmtN(liveP, 3) : '—'}</td>
      <td class="${clsPnl(upnl)}" style="${upnl > 0 ? 'color:var(--clr-green);' : upnl < 0 ? 'color:var(--clr-red);' : ''}">${upnl !== null && upnl !== undefined ? fmt$(upnl) + (entryP > 0 && shares > 0 ? ' <span style=\"font-size:var(--fs-xs);opacity:0.8;\">(' + (upnl >= 0 ? '+' : '') + ((upnl / (entryP * shares)) * 100).toFixed(2) + '%)</span>' : '') : '—'}</td>
      <td>${fmtMoney(t.amount)}</td>
      <td>
        ${closing || entryPending ? `<span class="text-warning">${closing ? (partialPending ? 'Partial sale awaiting confirmed outcome' : 'Exit needs reconciliation') : entryPendingLabel(t)}</span>` : `<button class="btn btn-sm btn-outline-danger" style="font-size:var(--fs-sm); padding:1px 7px;" onclick="closeTrade(${t.id})">Close</button>`}
      </td>
    </tr>`;
  }).join('');
}

function getHistoryViewState() {
  const filter = document.getElementById('history-filter').value;
  const stratFilter = document.getElementById('history-strategy-filter').value;
  let trades = filter === 'ALL' ? allTrades :
    allTrades.filter(t => t.status === filter);
  trades = currentSessionTrades(trades);
  if (stratFilter !== 'ALL') {
    trades = trades.filter(t => (t.strategy || 'conviction') === stratFilter);
  }
  const sorted = [...trades].sort((a, b) => b.id - a.id);
  return { filter, stratFilter, sorted };
}

function computeHistoryRenderKey(filter, stratFilter, visibleTrades) {
  return JSON.stringify({
    filter,
    stratFilter,
    sessionStartId,
    sessionStartAt,
    expanded: [...expandedTradeIds].sort((a, b) => a - b),
    trades: visibleTrades.map(t => ({
      id: t.id,
      status: t.status,
      order_id: t.order_id,
      entry_execution_status: t.entry_execution_status,
      timestamp: t.timestamp,
      direction: t.direction,
      entry_price: t.entry_price,
      exit_price: t.exit_price,
      pnl: t.pnl,
      confidence: t.confidence,
      exit_type: t.exit_type,
      partial_closed: t.partial_closed,
      strategy: t.strategy,
      signals_json: t.signals_json,
      fee_paid: t.fee_paid,
      accounting_version: t.accounting_version,
      mode: t.mode,
      shares: t.shares,
      amount: t.amount,
      market_id: t.market_id,
      run_id: t.run_id,
      exit_timestamp: t.exit_timestamp,
    })),
  });
}

function syncHistoryRenderKey() {
  const tbody = document.getElementById('history-tbody');
  if (!tbody) return;
  const { filter, stratFilter, sorted } = getHistoryViewState();
  if (!sorted.length) {
    tbody.dataset.historyKey = 'empty';
    return;
  }
  const visibleTrades = sorted.slice(0, 100);
  tbody.dataset.historyKey = computeHistoryRenderKey(filter, stratFilter, visibleTrades);
}

function renderHistory() {
  const { filter, stratFilter, sorted } = getHistoryViewState();
  const tbody  = document.getElementById('history-tbody');
  const visibleTrades = sorted.slice(0, 100);
  const visiblePartialTradeIds = visibleTrades
    .filter(t => t.status === 'CLOSED' && t.partial_closed === 1 && t.entry_price > 0)
    .map(t => Number(t.id))
    .filter(id => Number.isFinite(id) && id > 0);
  prefetchPartialFills(visiblePartialTradeIds);

  if (!sorted.length) {
    tbody.dataset.historyKey = 'empty';
    tbody.innerHTML = '<tr><td colspan="11" class="text-muted text-center py-3">No trades found</td></tr>';
    return;
  }

  const historyRenderKey = computeHistoryRenderKey(filter, stratFilter, visibleTrades);
  if (tbody.dataset.historyKey === historyRenderKey) {
    return;
  }

  tbody.innerHTML = visibleTrades.map(t => {
    const pnl   = t.status === 'CLOSED' ? realizedNetPnl(t) : null;
    const entryPending = isPendingEntry(t);
    const isOpen = t.status === 'OPEN' && !entryPending;
    const strat = t.strategy || 'conviction';
    const stratBadge = strat === 'scalp'
      ? '<span class="badge" style="background:rgba(255,180,50,0.15); color:#ffb432; font-size:var(--fs-xs);">SCALP</span>'
      : strat === 'resolution_sniper'
      ? '<span class="badge" style="background:rgba(255,180,50,0.15); color:#ffb432; font-size:var(--fs-xs);"><i class="bi bi-crosshair" style="font-size:var(--fs-xs);"></i> SNIPE</span>'
      : '<span class="badge" style="background:rgba(88,166,255,0.12); color:var(--clr-blue); font-size:var(--fs-xs);">HOLD</span>';
    const exitType = t.exit_type || '—';
    const exitBadge = exitType.includes('scalp_take_profit') ? '<span style="color:var(--clr-green);">TP</span>'
      : exitType.includes('scalp_trailing') ? '<span style="color:var(--clr-yellow);">TRAIL</span>'
      : exitType.includes('scalp_emergency_sl') ? '<span style="color:var(--clr-red);">EMRG</span>'
      : exitType.includes('scalp_stop_loss') ? '<span style="color:var(--clr-red);">SL</span>'
      : exitType.includes('take_profit') ? '<span style="color:var(--clr-green);">TP</span>'
      : exitType.includes('stop_loss') ? '<span style="color:var(--clr-red);">SL</span>'
      : `<span class="text-muted">${ledgerEsc(exitType)}</span>`;
    const isPartial = t.partial_closed === 1;

    // Build expandable detail row
    let detailContent = entryPending
      ? `<div class="text-warning" style="margin-bottom:6px;">${entryPendingLabel(t)}. ${t.entry_execution_status === 'audit_uncertain' ? 'Recorded inventory is retained while entry evidence is revalidated.' : 'Amount is reserved intent; confirmed shares are shown separately.'}</div>`
      : '';
    // Partial close breakdown — show real fill data if available
    if (isPartial && t.status === 'CLOSED' && t.entry_price > 0) {
      const cachedFills = partialFillsCache.get(Number(t.id));
      const partialMarkup = cachedFills
        ? renderPartialFillsMarkup(cachedFills)
        : `<div style="margin-bottom:6px;"><span style="color:var(--clr-yellow);">⚡ Partial Close</span> — P&L is cumulative across multiple legs. <span class="text-muted" style="font-size:var(--fs-xs);">Loading fills...</span></div>`;
      detailContent += `<div style="margin-bottom:6px;" id="partial-info-${t.id}" data-loaded="${cachedFills ? '1' : '0'}">${partialMarkup}</div>`;
    }
    // Signals at entry
    if (t.signals_json) {
      try {
        const sigs = typeof t.signals_json === 'string' ? JSON.parse(t.signals_json) : t.signals_json;
        const sorted_sigs = Object.entries(sigs).sort((a, b) => Math.abs(b[1]) - Math.abs(a[1])).slice(0, 6);
        const sigPills = sorted_sigs.map(([k, v]) => {
          const clr = v > 0 ? 'var(--clr-green)' : v < 0 ? 'var(--clr-red)' : 'var(--clr-muted)';
          const short = k.replace('_sentiment','').replace('distance_from_','dist_').replace('orderbook_','ob_').replace('imbalance','imb');
          return `<span style="display:inline-block; margin:1px 3px; padding:1px 6px; border-radius:var(--r-sm); background:rgba(48,54,61,0.7); font-size:var(--fs-xs);"><span style="color:var(--clr-muted);">${ledgerEsc(short)}</span> <span style="color:${clr};">${v >= 0 ? '+' : ''}${v.toFixed(3)}</span></span>`;
        }).join('');
        detailContent += `<div style="margin-bottom:4px;"><span class="text-muted" style="font-size:var(--fs-xs);">SIGNALS AT ENTRY:</span> ${sigPills}</div>`;
      } catch {}
    }
    // Trade metadata
    const feePaid = t.fee_paid != null ? `$${t.fee_paid.toFixed(3)}` : '—';
    const shares = t.shares != null ? t.shares.toFixed(2) : '—';
    const amount = t.amount != null ? `$${t.amount.toFixed(2)}` : '—';
    const marketId = t.market_id ? t.market_id.slice(0, 12) + '…' : '—';
    const exitTs = t.exit_timestamp ? tsShort(t.exit_timestamp) : '—';
    const entryTs = t.timestamp ? tsShort(t.timestamp) : '—';
    detailContent += `<div style="display:flex; gap:16px; flex-wrap:wrap; font-size:var(--fs-xs);">
      <span><span class="text-muted">Amount:</span> ${amount}</span>
      <span><span class="text-muted">Shares:</span> ${shares}</span>
      <span><span class="text-muted">Entry fee:</span> ${feePaid}</span>
      <span><span class="text-muted">Entry Time:</span> ${entryTs}</span>
      <span><span class="text-muted">Exit Time:</span> ${exitTs}</span>
      <span><span class="text-muted">Market:</span> <code style="font-size:var(--fs-xs);">${ledgerEsc(marketId)}</code></span>
      ${t.run_id ? `<span><span class="text-muted">Run:</span> <code style="font-size:var(--fs-xs);">${ledgerEsc(t.run_id.slice(-12))}</code></span>` : ''}
    </div>`;

    return `
      <tr style="cursor:pointer;" onclick="toggleTradeDetail(this, ${t.id}, ${isPartial})">
        <td class="text-muted">#${t.id}</td>
        <td>${stratBadge}</td>
        <td>${tsShort(t.timestamp)}</td>
        <td><span class="${t.direction === 'BUY_YES' ? 'dir-yes' : 'dir-no'}">${t.direction === 'BUY_YES' ? 'YES↑' : 'NO↓'}</span></td>
        <td>${fmtN(entryPending ? null : t.entry_price, 3)}</td>
        <td>${t.exit_price !== null && t.exit_price !== undefined ? fmtN(t.exit_price, 3) : '—'}</td>
        <td class="${clsPnl(pnl)}">${pnl !== null && pnl !== undefined ? fmt$(pnl) : '—'}${isPartial ? ' <span style="color:var(--clr-yellow); font-size:var(--fs-xs);" title="Includes partial close">½</span>' : ''}</td>
        <td>${fmtPct(t.confidence == null ? null : t.confidence * 100)}</td>
        <td><span class="badge" style="background:${t.status === 'OPEN' ? 'rgba(88,166,255,0.15)' : 'rgba(48,54,61,0.5)'}; color:${t.status === 'OPEN' ? 'var(--clr-blue)' : 'var(--clr-muted)'}; font-size:var(--fs-xs);">${ledgerEsc(entryPending ? 'ENTRY PENDING' : t.status)}</span></td>
        <td>${exitBadge}</td>
        <td>
          ${isOpen ? `<button class="btn btn-sm btn-outline-danger" style="font-size:var(--fs-sm); padding:1px 7px;" onclick="event.stopPropagation(); closeTrade(${t.id})">Close</button>` : ''}
        </td>
      </tr>
      <tr style="display:${expandedTradeIds.has(t.id) ? 'table-row' : 'none'};" data-detail-id="${t.id}">
        <td colspan="11" style="padding:8px 16px; background:rgba(22,27,34,0.8); border-top:1px solid rgba(48,54,61,0.4);">
          ${detailContent}
        </td>
      </tr>`;
  }).join('');
  tbody.dataset.historyKey = historyRenderKey;

  // Re-fetch fill data for expanded partial trades (don't toggle — just fetch)
  for (const id of expandedTradeIds) {
    const el = document.getElementById(`partial-info-${id}`);
    if (el && el.dataset.loaded !== '1') {
      fetchPartialFills(id, el);
    }
  }
}

async function loadTradeAnalysis() {
  try {
    const r = await GET('/api/trades/analysis');
    _lastTradeAnalysisRefresh = Date.now();
    updateStatsBar(r);
    if (r.pnl_series_net) updateChart(r.pnl_series_net);
    else if (r.pnl_series) updateChart(r.pnl_series);
  } catch (e) {
    logReadError('loadTradeAnalysis', e);
  }
}

// Track which trade rows are expanded
const expandedTradeIds = new Set();
const partialFillsCache = new Map();
const partialFillsInflight = new Map();

function describeFillDetails(fill) {
  const raw = fill?.details || '';
  const parsed = fill?.details_json && typeof fill.details_json === 'object'
    ? fill.details_json
    : (() => {
        try { return raw ? JSON.parse(raw) : null; } catch { return null; }
      })();
  if (!parsed || typeof parsed !== 'object') return raw;

  const note = parsed.note || raw || '';
  const snap = parsed.snapshot || {};
  const parts = [];
  if (snap.best_bid != null) parts.push(`bid ${fmtN(snap.best_bid, 4)}`);
  if (snap.best_ask != null) parts.push(`ask ${fmtN(snap.best_ask, 4)}`);
  if (snap.midpoint != null) parts.push(`mid ${fmtN(snap.midpoint, 4)}`);
  if (!parts.length) return note;
  return `${note} [${parts.join(' · ')}]`;
}

function renderPartialFillsMarkup(fills) {
  if (fills && fills.length > 0) {
    const legs = fills.map(f => {
      const phase = f.phase || 'unknown';
      const icon = phase === 'entry' ? '📥' : phase.includes('partial') ? '⚡' : '📤';
      const price = fmtN(f.avg_price ?? f.intended_price, 4);
      const shares = fmtN(f.delta_size ?? f.cumulative_size, 2);
      const ts = f.timestamp ? tsShort(f.timestamp) : '';
      const details = describeFillDetails(f);
      const clr = phase === 'entry' ? 'var(--clr-blue)' : phase.includes('partial') ? 'var(--clr-yellow)' : 'var(--clr-muted)';
      return `<div style="font-size:var(--fs-xs); padding:1px 0;">
        ${icon} <span style="color:${clr}; font-weight:600;">${ledgerEsc(phase)}</span>
        @ <span style="color:var(--clr-fg);">${price}</span>
        · ${shares} shares
        ${ts ? `· <span class="text-muted">${ts}</span>` : ''}
        ${details ? `<span class="text-muted" style="font-size:var(--fs-xs);"> — ${ledgerEsc(details)}</span>` : ''}
      </div>`;
    }).join('');
    return `<div style="margin-bottom:6px;"><span style="color:var(--clr-yellow);">⚡ Trade Legs</span></div>${legs}`;
  }
  return `<div class="text-muted">No fill evidence available.</div>`;
}

async function loadPartialFillsData(tradeId) {
  if (partialFillsCache.has(tradeId)) return partialFillsCache.get(tradeId);
  if (partialFillsInflight.has(tradeId)) return partialFillsInflight.get(tradeId);
  const request = GET(`/api/trades/${tradeId}/fills`)
    .then(resp => {
      const fills = Array.isArray(resp?.fills) ? resp.fills : [];
      partialFillsCache.set(tradeId, fills);
      partialFillsInflight.delete(tradeId);
      return fills;
    })
    .catch(err => {
      partialFillsInflight.delete(tradeId);
      throw err;
    });
  partialFillsInflight.set(tradeId, request);
  return request;
}

function prefetchPartialFills(tradeIds) {
  tradeIds.forEach(tradeId => {
    if (partialFillsCache.has(tradeId) || partialFillsInflight.has(tradeId)) return;
    loadPartialFillsData(tradeId).catch(() => {});
  });
}

async function fetchPartialFills(tradeId, el) {
  if (!el || el.dataset.loaded === '1') return;
  try {
    const fills = await loadPartialFillsData(tradeId);
    el.innerHTML = renderPartialFillsMarkup(fills);
    el.dataset.loaded = '1';
  } catch {
    el.textContent = 'Could not load fill evidence. Reopen this trade to retry.';
    el.dataset.loaded = '0';
  }
}

async function toggleTradeDetail(row, tradeId, isPartial) {
  const detail = row.nextElementSibling;
  if (!detail) return;
  const wasHidden = detail.style.display === 'none';

  // Track expanded state across re-renders
  if (wasHidden) expandedTradeIds.add(tradeId);
  else expandedTradeIds.delete(tradeId);

  // Fetch fills on first expand for partial trades
  if (wasHidden && isPartial) {
    const el = document.getElementById(`partial-info-${tradeId}`);
    if (el && el.dataset.loaded !== '1' && partialFillsCache.has(tradeId)) {
      el.innerHTML = renderPartialFillsMarkup(partialFillsCache.get(tradeId));
      el.dataset.loaded = '1';
    }
    detail.style.display = 'table-row';
    syncHistoryRenderKey();
    await fetchPartialFills(tradeId, el);
    return;
  }

  detail.style.display = wasHidden ? 'table-row' : 'none';
  syncHistoryRenderKey();
}

const ARCHIVED_TRADES_KEY = 'archivedTrades';
const SESSION_START_KEY = 'sessionStartId';
const SESSION_START_AT_KEY = 'sessionStartAt';

function loadArchivedTradesFromStorage() {
  try {
    const raw = localStorage.getItem(ARCHIVED_TRADES_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    return Array.isArray(parsed) ? parsed : [];
  } catch {
    return [];
  }
}

function saveArchivedTradesToStorage() {
  try {
    localStorage.setItem(ARCHIVED_TRADES_KEY, JSON.stringify(archivedTrades));
  } catch {}
}

function archiveTradeKey(t) {
  return `${t.id || 0}:${t.timestamp || ''}:${t.market_id || ''}`;
}

function mergeArchivedTrades(existing, incoming) {
  const merged = new Map();
  [...existing, ...incoming].forEach(t => {
    if (!t) return;
    merged.set(archiveTradeKey(t), t);
  });
  return [...merged.values()].sort((a, b) => Number(b.id || 0) - Number(a.id || 0));
}

function parseTradeTimeMs(value) {
  if (!value) return null;
  const ms = Date.parse(value);
  return Number.isFinite(ms) ? ms : null;
}

function getTradeOpenedTimeMs(trade) {
  return parseTradeTimeMs(trade?.opened_at || trade?.timestamp || null);
}

function getTradeClosedTimeMs(trade) {
  return parseTradeTimeMs(trade?.exit_timestamp || trade?.closed_at || null);
}

function normalizeSessionStartId(value) {
  const parsed = Number(value || 0);
  return Number.isFinite(parsed) && parsed > 0 ? Math.trunc(parsed) : 0;
}

function normalizeSessionStartAt(value) {
  if (!value) return '';
  const parsed = String(value).trim();
  return Number.isFinite(Date.parse(parsed)) ? parsed : '';
}

function hasSessionBoundary(boundary) {
  return Boolean(boundary?.tradeId || boundary?.startedAt);
}

function readSessionBoundaryFromStorage() {
  try {
  let tradeId = normalizeSessionStartId(localStorage.getItem(SESSION_START_KEY) || '0');
  let startedAt = normalizeSessionStartAt(localStorage.getItem(SESSION_START_AT_KEY) || '');
  if (tradeId && !startedAt) {
    // Legacy boundaries only tracked an id cutoff. Assign a timestamp now so a
    // persisted boundary keeps trades that were still open at the cutoff in the
    // current session when they later close.
    startedAt = new Date().toISOString();
    localStorage.setItem(SESSION_START_AT_KEY, startedAt);
  }
  return { tradeId, startedAt };
  } catch { return { tradeId: 0, startedAt: '' }; }
}

function writeSessionBoundaryToStorage(boundary) {
  try {
  const tradeId = normalizeSessionStartId(boundary?.tradeId);
  const startedAt = normalizeSessionStartAt(boundary?.startedAt);
  if (tradeId > 0) localStorage.setItem(SESSION_START_KEY, String(tradeId));
  else localStorage.removeItem(SESSION_START_KEY);
  if (startedAt) localStorage.setItem(SESSION_START_AT_KEY, startedAt);
  else localStorage.removeItem(SESSION_START_AT_KEY);
  } catch {}
}

function applySessionBoundary(boundary) {
  sessionStartId = normalizeSessionStartId(boundary?.tradeId);
  sessionStartAt = normalizeSessionStartAt(boundary?.startedAt);
  writeSessionBoundaryToStorage({ tradeId: sessionStartId, startedAt: sessionStartAt });
}

function getConfigSessionBoundary(cfg) {
  return {
    tradeId: normalizeSessionStartId(cfg?.dashboard_session_start_trade_id),
    startedAt: normalizeSessionStartAt(cfg?.dashboard_session_start_at),
  };
}

async function persistSessionBoundary(boundary) {
  const patch = {
    dashboard_session_start_trade_id: normalizeSessionStartId(boundary?.tradeId),
    dashboard_session_start_at: normalizeSessionStartAt(boundary?.startedAt),
  };
  const updated = await POST('/api/config', patch);
  currentConfig = updated;
  renderConfigPanel(currentConfig);
  syncNavExecMode(currentConfig.execution_mode || '');
  applySessionBoundary(getConfigSessionBoundary(currentConfig));
}

function getSessionBoundaryMs() {
  return parseTradeTimeMs(sessionStartAt);
}

function isCurrentSessionTrade(trade) {
  if (!sessionStartId && !sessionStartAt) return true;
  if (!trade) return false;
  if (trade.status === 'OPEN' || trade.status === 'CLOSING') return true;

  const tradeId = Number(trade.id || 0);
  if (sessionStartId && tradeId > sessionStartId) return true;

  const boundaryMs = getSessionBoundaryMs();
  if (boundaryMs == null) {
    return !(trade.status === 'CLOSED' && sessionStartId && tradeId <= sessionStartId);
  }

  const openedMs = getTradeOpenedTimeMs(trade);
  if (openedMs != null && openedMs >= boundaryMs) return true;

  const closedMs = getTradeClosedTimeMs(trade);
  if (closedMs != null && closedMs >= boundaryMs) return true;

  return false;
}

function getBoundaryArchivedTrades() {
  if (!sessionStartId && !sessionStartAt) return [];
  return allTrades.filter(t => t.status === 'CLOSED' && !isCurrentSessionTrade(t));
}

function currentSessionTrades(trades) {
  if (!sessionStartId && !sessionStartAt) return trades;
  return trades.filter(isCurrentSessionTrade);
}

let archivedTrades = loadArchivedTradesFromStorage();
let sessionStartId = 0;
let sessionStartAt = '';
applySessionBoundary(readSessionBoundaryFromStorage());

async function newSession() {
  if (!confirm('Start new session? Closed trades will move to Archived, the session cutoff will persist across restarts, and paper balance/cash drawdown peak will reset. Daily losses and the kill switch will be kept. Database trade history will be kept.')) return;
  try {
    const currentTrades = allTrades.filter(t => t.status === 'CLOSED');
    const maxId = allTrades.reduce((max, t) => Math.max(max, t.id || 0), 0);
    await persistSessionBoundary({
      tradeId: maxId,
      startedAt: new Date().toISOString(),
    });

    archivedTrades = mergeArchivedTrades(archivedTrades, currentTrades);
    saveArchivedTradesToStorage();

    renderArchivedTrades();
    renderHistory();

    const resetWarnings = [];
    try {
      await POST('/api/balance/reset', {});
    } catch (e) {
      resetWarnings.push(`balance reset failed (${e.message})`);
    }
    try {
      await POST('/api/drawdown/reset');
    } catch (e) {
      resetWarnings.push(`drawdown reset failed (${e.message})`);
    }

    await loadTrades();
    await loadConfig();
    await loadStats();
    toast(
      resetWarnings.length
        ? `New session started, but ${resetWarnings.join(' · ')}.`
        : 'New session started. Closed trades moved to Archived, and the session cutoff was saved.',
      resetWarnings.length ? 'warning' : 'success',
    );
  } catch (e) {
    toast('New session failed: ' + e.message, 'danger');
  }
}

function renderArchivedTrades() {
  const section = document.getElementById('archived-trades-section');
  const tbody = document.getElementById('archived-tbody');
  const countEl = document.getElementById('archived-count');
  const pnlEl = document.getElementById('archived-pnl');
  if (!section || !tbody) return;

  const archived = mergeArchivedTrades(archivedTrades, getBoundaryArchivedTrades());

  if (archived.length === 0) {
    section.style.display = 'none';
    updateDashboardStrip();
    return;
  }

  section.style.display = 'block';
  countEl.textContent = archived.length;
  const totalPnl = sumRealizedNetPnl(archived);
  pnlEl.textContent = fmt$(totalPnl);
  pnlEl.style.color = totalPnl >= 0 ? 'var(--clr-green)' : 'var(--clr-red)';

  tbody.innerHTML = archived.map(t => {
    const pnl = realizedNetPnl(t);
    const pnlColor = pnl > 0 ? 'var(--clr-green)' : pnl < 0 ? 'var(--clr-red)' : '';
    const stratBadge = t.strategy === 'scalp'
      ? '<span class="badge" style="background:#7c3aed;font-size:var(--fs-xs);">SCALP</span>'
      : '<span class="badge" style="background:#374151;font-size:var(--fs-xs);">HOLD</span>';
    const dirBadge = (t.direction || '').includes('YES')
      ? '<span style="color:var(--clr-green);">YES&uarr;</span>'
      : '<span style="color:var(--clr-red);">NO&darr;</span>';
    const tradeTime = t.opened_at || t.timestamp || '';
    const time = tradeTime ? new Date(tradeTime).toLocaleTimeString() : '';
    return `<tr>
      <td>#${t.id}</td><td>${stratBadge}</td><td>${time}</td><td>${dirBadge}</td>
      <td>${fmtN(t.entry_price, 3)}</td><td>${fmtN(t.exit_price, 3)}</td>
      <td style="color:${pnlColor};font-weight:600;">${fmt$(pnl)}</td>
      <td>${fmtPct(t.confidence == null ? null : t.confidence * 100)}</td>
      <td>CLOSED</td><td>${ledgerEsc(t.exit_type||'—')}</td>
    </tr>`;
  }).join('');
  updateDashboardStrip();
}

async function closeTrade(id) {
  try {
    const r = await POST(`/api/trades/${id}/close`);
    // Say what it sold for. "closed" alone hid a close that had marked the
    // position out at its own entry price.
    const t = r?.trade;
    const px = t?.exit_price == null ? NaN : Number(t.exit_price);
    const pnl = realizedNetPnl(t);
    const detail = Number.isFinite(px) ? ` @ ${px.toFixed(3)}` + (pnl !== null && Number.isFinite(pnl) ? ` · ${fmt$(pnl)}` : '') : '';
    toast(`Trade #${id} closed${detail}`, pnl !== null && Number.isFinite(pnl) && pnl < 0 ? 'warning' : 'success');
    await loadTrades();
  } catch (e) {
    toast('Close failed: ' + e.message, 'danger');
  }
}

async function closeAllTrades() {
  if (!confirm('Close all open positions?')) return;
  try {
    const r = await POST('/api/trades/close-all');
    toast(`Closed ${r.closed} trades. Total P&L: ${fmt$(r.total_pnl)}`, 'success');
    await loadTrades();
  } catch (e) {
    toast('Close all failed: ' + e.message, 'danger');
  }
}


async function loadNoArb() {
  try {
    const r = await GET('/api/markets/no-arb');
    updateNoArbSummary(r);
  } catch (e) {
    logReadError('loadNoArb', e);
  }
}

// ── Analysis ──────────────────────────────────────────────────────────────────
async function triggerAnalysis() {
  try {
    const r = await POST('/api/analyze');
    updateSignalPanel(r);
    toast('Analysis refreshed', 'info');
  } catch (e) {
    toast('Analysis failed: ' + e.message, 'danger');
  }
}

async function executeCurrentTrade() {
  if (!confirm('Execute trade from current analysis?')) return;
  try {
    const r = await POST('/api/trade', {});
    if (r && ['hold', 'not_executed'].includes(r.status)) {
      toast(`No trade opened — ${r.reason || 'filtered by a runtime guard'}`, 'warning');
    } else if (r && (r.status === 'pending' || isPendingEntry(r))) {
      toast(`Entry needs confirmation — ${r.reason || 'inspect recovery evidence before retrying'}`, 'warning');
      await loadTrades();
    } else if (r && Number.isInteger(r.id) && r.status === 'OPEN') {
      toast(`Trade #${r.id} executed — ${r.direction} @ ${fmtN(r.entry_price, 3)}`, 'success');
      await loadTrades();
    } else {
      toast('Trade result is unknown. Check trade history before retrying.', 'warning');
    }
  } catch (e) {
    toast('Execute failed: ' + e.message, 'danger');
  }
}

// ── Config ────────────────────────────────────────────────────────────────────
async function loadConfig() {
  try {
    const previousBoundary = `${sessionStartId}|${sessionStartAt}`;
    currentConfig = await GET('/api/config');
    const persistedBoundary = getConfigSessionBoundary(currentConfig);
    if (hasSessionBoundary(persistedBoundary)) {
      applySessionBoundary(persistedBoundary);
    } else {
      const legacyBoundary = readSessionBoundaryFromStorage();
      applySessionBoundary(legacyBoundary);
      if (hasSessionBoundary(legacyBoundary) && hasWriteAuth()) {
        try {
          await persistSessionBoundary(legacyBoundary);
        } catch (persistErr) {
          console.warn('session boundary migration failed', persistErr);
        }
      }
    }
    renderConfigPanel(currentConfig);
    syncNavExecMode(currentConfig.execution_mode || '');
    dashboardAuthHintShown = false;
    if (`${sessionStartId}|${sessionStartAt}` !== previousBoundary) {
      renderArchivedTrades();
      renderHistory();
    }
    updateDashboardStrip();
  } catch (e) {
    logReadError('loadConfig', e);
    if (isAuthFailure(e)) {
      notifyDashboardAuthRequired('config');
      const panel = document.getElementById('config-panel');
      if (panel) {
        panel.innerHTML = '<div class="text-muted small">Read-only API key required to load config.</div>';
      }
    }
  }
}

function renderConfigPanel(cfg) {
  const panel = document.getElementById('config-panel');

  // Everyday operating controls stay visible; research choices have their
  // own disclosure so a new installation is not a wall of tuning knobs.
  const sections = [
    {
      title: 'Trading',
      fields: [
        { key: 'initial_balance',   label: 'Initial Balance ($)',  type: 'readonly', help: 'Read-only while running. Change it during setup or use the signed balance reset API.' },
        { key: 'trade_amount',      label: 'Trade Amount ($)',     type: 'number', step: 0.5 },
        { key: 'min_entry_price',   label: 'Min Entry Price',      type: 'number', step: 0.01 },
        { key: 'max_entry_price',   label: 'Max Entry Price',      type: 'number', step: 0.01 },
        { key: 'auto_trading',      label: 'Auto Trading',         type: 'bool' },
        { key: 'mode',              label: 'Mode',                 type: 'readonly', help: 'Set the operating mode in the private configuration before launch.' },
        { key: 'sim_live_execution',label: 'Sim Live Execution',   type: 'bool' },
      ],
    },
    {
      title: 'Maker',
      fields: [
        { key: 'maker_hold_positions',                label: 'Hold Positions (collector)', type: 'bool' },
        { key: 'maker_live_quoting',                  label: 'Live Quoting',                type: 'bool' },
        { key: 'maker_quote_notional_usd',            label: 'Quote Size ($)',              type: 'number', step: 0.5 },
        { key: 'maker_max_inventory_notional_usd',    label: 'Max Inventory ($)',           type: 'number', step: 1 },
        { key: 'maker_market_quality_max_spread_bps', label: 'Max Spread (bps)',            type: 'number', step: 100 },
        { key: 'maker_buy_no_max_p_up',               label: 'Side Margin (max p_up for NO)', type: 'number', step: 0.05 },
      ],
    },
    {
      title: 'Risk',
      fields: [
        { key: 'take_profit',           label: 'Take Profit',       type: 'bool' },
        { key: 'take_profit_threshold', label: 'TP Threshold',      type: 'number', step: 0.01 },
        { key: 'stop_loss',             label: 'Stop Loss',         type: 'bool' },
        { key: 'stop_loss_threshold',   label: 'SL Threshold',      type: 'number', step: 0.01 },
        { key: 'partial_take_profit',   label: 'Partial TP',        type: 'bool' },
        { key: 'partial_tp_threshold',  label: 'Partial TP Threshold', type: 'number', step: 0.01 },
        { key: 'daily_loss_limit',      label: 'Daily Loss Limit ($)', type: 'number', step: 1 },
        { key: 'circuit_breaker_losses',label: 'CB Losses',         type: 'number', step: 1 },
        { key: 'macro_event_guard_enabled', label: 'Macro Guard', type: 'bool' },
        { key: 'macro_event_guard_minutes_before', label: 'Macro Mins Before', type: 'number', step: 1 },
        { key: 'macro_event_guard_minutes_after', label: 'Macro Mins After', type: 'number', step: 1 },
      ],
    },
    {
      title: 'Kelly Sizing',
      advanced: true,
      fields: [
        { key: 'kelly_sizing',   label: 'Kelly Sizing',  type: 'bool' },
        { key: 'kelly_max_mult', label: 'Kelly Max Mult',type: 'number', step: 0.1 },
        { key: 'kelly_fraction', label: 'Kelly Fraction',type: 'number', step: 0.05 },
      ],
    },
    {
      title: 'Filters',
      advanced: true,
      fields: [
        { key: 'min_liquidity',        label: 'Min Liquidity ($)',     type: 'number', step: 10 },
        { key: 'live_confidence_cap',  label: 'Live Conf Cap',         type: 'number', step: 0.01 },
        { key: 'max_spread',           label: 'Max Spread',            type: 'number', step: 0.001 },
        { key: 'min_depth_usd',        label: 'Min Depth USD',         type: 'number', step: 10 },
        { key: 'buy_no_trend_up_guard_enabled', label: 'BUY_NO Trend-Up Guard', type: 'bool' },
        { key: 'buy_no_trend_up_guard_shadow_only', label: 'BUY_NO Guard Shadow Only', type: 'bool' },
        { key: 'buy_no_trend_up_max_p_up', label: 'BUY_NO Guard Max p_up', type: 'number', step: 0.01 },
        { key: 'time_of_day_weight',   label: 'Time of Day Weight',    type: 'bool' },
      ],
    },
    {
      title: 'Hail Mary',
      advanced: true,
      fields: [
        { key: 'hail_mary',              label: 'Hail Mary',          type: 'bool' },
        { key: 'hail_mary_stake',        label: 'HM Stake ($)',       type: 'number', step: 0.5 },
        { key: 'hail_mary_max_price',    label: 'HM Max Price',       type: 'number', step: 0.01 },
        { key: 'hail_mary_min_btc_change', label: 'HM Min BTC Δ%',   type: 'number', step: 0.01 },
      ],
    },
    {
      title: 'Refresh',
      fields: [
        { key: 'price_refresh_rate',     label: 'Price Refresh Rate (s)',type: 'number', step: 1 },
        { key: 'signal_refresh_interval',label: 'Signal Refresh (s)',    type: 'number', step: 1 },
      ],
    },
    {
      title: 'Signals',
      advanced: true,
      fields: [
        { key: 'pcr_enabled',                 label: 'Deribit PCR',         type: 'bool' },
        { key: 'fear_greed_enabled',          label: 'Fear & Greed',        type: 'bool' },
        { key: 'funding_rate_enabled',        label: 'Funding Rate',        type: 'bool' },
        { key: 'oi_momentum_enabled',         label: 'OI Momentum',         type: 'bool' },
        { key: 'aggressor_ratio_enabled',     label: 'Aggressor Ratio',     type: 'bool' },
        { key: 'lunarcrush_sentiment_enabled',label: 'LunarCrush Sentiment',type: 'bool' },
      ],
    },
    {
      title: 'Calibration',
      advanced: true,
      fields: [
        { key: 'analysis_snapshot_enabled', label: 'Analysis Snapshots', type: 'bool' },
        { key: 'signal_weights_path',       label: 'Signal Weights Path', type: 'text' },
        { key: 'macro_event_calendar_path', label: 'Macro Event Calendar', type: 'readonly', help: 'Set the calendar path in the private configuration before launch.' },
        { key: 'finnhub_api_key',           label: 'Finnhub API Key',     type: 'password', redacted: true },
        { key: 'lunarcrush_api_key',        label: 'LunarCrush API Key',  type: 'password', redacted: true },
      ],
    },
    {
      title: 'Paper execution assumptions',
      advanced: true,
      help: 'These settings model delay and displayed liquidity. They are assumptions, not measured exchange execution or queue priority.',
      fields: [
        { key: 'sim_order_latency_ms', label: 'Order Delay (ms)', type: 'number', step: 1 },
        { key: 'sim_available_depth_fraction', label: 'Available Depth Fraction', type: 'number', step: 0.05 },
        { key: 'maker_sim_queue_ahead_shares', label: 'Queue Ahead (shares)', type: 'number', step: 1 },
        { key: 'maker_sim_max_fill_fraction', label: 'Max Maker Fill Fraction', type: 'number', step: 0.05 },
        { key: 'maker_sim_fill_on_touch', label: 'Allow Touch Fills', type: 'bool' },
      ],
    },
  ];

  let runtimeHtml = '';
  let advancedHtml = '';
  for (const sec of sections) {
    let html = `<div class="col-12 col-xl-6">`;
    html += `<div class="config-cluster">`;
    html += `<div class="config-cluster-title">${sec.title}</div>`;
    if (sec.help) html += `<p class="text-muted small">${ledgerEsc(sec.help)}</p>`;
    html += `<div class="row g-2">`;
    for (const f of sec.fields) {
      const val = getNestedVal(cfg, f.key);
      html += `<div class="col-md-6">`;
      html += `<label class="form-label" for="cfg-${f.key}">${f.label}</label>`;
      if (f.type === 'readonly') {
        html += `<input type="text" class="form-control form-control-sm" id="cfg-${f.key}" readonly aria-describedby="cfg-${f.key}-help" value="${ledgerEsc(val !== undefined ? val : '')}">`;
        html += `<div class="text-muted small" id="cfg-${f.key}-help">${ledgerEsc(f.help || 'Read-only while running.')}</div>`;
      } else if (f.type === 'bool') {
        html += `
          <div class="form-check form-switch mt-1">
            <input class="form-check-input" type="checkbox" id="cfg-${f.key}" data-key="${f.key}" ${val ? 'checked' : ''}>
          </div>`;
      } else if (f.type === 'select') {
        const opts = f.options.map(o => `<option value="${o}" ${val === o ? 'selected' : ''}>${o}</option>`).join('');
        html += `<select class="form-select form-select-sm" id="cfg-${f.key}" data-key="${f.key}">${opts}</select>`;
      } else if (f.type === 'text' || f.type === 'password') {
        const isRedacted = f.redacted && val === '***';
        const displayVal = isRedacted ? '' : (val !== undefined ? val : '');
        const placeholder = isRedacted ? 'redacted' : '';
        // Secrets are masked and kept out of browser autofill.
        const extra = f.type === 'password' ? 'autocomplete="off"' : '';
        html += `<input type="${f.type}" class="form-control form-control-sm" id="cfg-${f.key}" data-key="${f.key}" ${f.redacted ? 'data-redacted=\"true\"' : ''} ${extra} value="${ledgerEsc(displayVal)}" placeholder="${placeholder}">`;
      } else {
        html += `<input type="number" step="${f.step || 1}" class="form-control form-control-sm" id="cfg-${f.key}" data-key="${f.key}" value="${ledgerEsc(val !== undefined ? val : '')}">`;
      }
      html += `</div>`;
    }
    html += `</div></div></div>`;
    if (sec.advanced) advancedHtml += html;
    else runtimeHtml += html;
  }
  panel.innerHTML = `<div class="col-12 config-section-intro"><strong>Operating settings</strong><p class="text-muted small mb-0">Routine controls for trading, maker quotes, risk and refresh. Saving changes requires the API key and secret.</p></div>${runtimeHtml}`
    + `<div class="col-12"><details class="config-advanced"><summary>Strategy experiments and data sources</summary><p class="text-muted small mt-2">Optional tuning and paper execution assumptions. Opening this section does not change any setting.</p><div class="row g-3">${advancedHtml}</div></details></div>`;
}

function getNestedVal(obj, key) {
  return obj[key];
}

// Only what changed goes to the server.
//
// The panel used to post every field, and it contains `mode`, which the API
// refuses on principle ("sensitive config keys must be changed outside the
// API"). So Save Config returned 403 on every press — a change to a single
// number included — and the tab's only write action had never once worked.
// Sending the diff keeps each config write small enough to read. Startup-only
// values are displayed read-only and excluded even if old markup is restored.
function readConfigFromPanel(baseline = currentConfig) {
  const all = readAllConfigFields();
  if (!baseline || typeof baseline !== 'object') return all;
  const patch = {};
  for (const [key, value] of Object.entries(all)) {
    if (!configValuesEqual(baseline[key], value)) patch[key] = value;
  }
  return patch;
}

function configValuesEqual(stored, entered) {
  if (stored === entered) return true;
  if (typeof stored === 'number' && typeof entered === 'number') return Math.abs(stored - entered) < 1e-12;
  if (stored == null && entered === '') return true;
  return String(stored) === String(entered);
}

function readAllConfigFields() {
  const patch = {};
  document.querySelectorAll('#config-panel [data-key]').forEach(el => {
    const key = el.dataset.key;
    if (['initial_balance', 'mode', 'auth_required', 'api_key', 'api_hmac_secret', 'private_rpc_url', 'macro_event_calendar_path'].includes(key)) return;
    if (el.type === 'checkbox') {
      patch[key] = el.checked;
    } else if (el.type === 'text' || el.type === 'password') {
      const v = el.value.trim();
      const redacted = el.dataset.redacted === 'true';
      if (redacted && v === '') return;
      patch[key] = v;
    } else if (el.type === 'number') {
      const v = el.value.trim();
      if (v !== '') patch[key] = Number(v);
    } else {
      patch[key] = el.value;
    }
  });
  return patch;
}

async function saveConfig() {
  const patch = readConfigFromPanel();
  if (Object.keys(patch).length === 0) {
    toast('Nothing changed', 'info');
    return;
  }
  try {
    const r = await POST('/api/config', patch);
    currentConfig = r;
    toast('Config saved', 'success');
  } catch (e) {
    toast('Save config failed: ' + e.message, 'danger');
  }
}

// ── Session Performance Heatmap ───────────────────────────────────────────────
function renderHourHeatmap() {
  const container = document.getElementById('hour-heatmap');
  if (!container) return;

  // Bucket closed trades by UTC hour of entry
  const hourBuckets = new Array(24).fill(null).map(() => ({ totalPnl: 0, count: 0 }));
  const closed = allTrades.filter(t => t.status === 'CLOSED' && t.timestamp);
  for (const t of closed) {
    try {
      const d = new Date(t.timestamp);
      const h = d.getUTCHours();
      const pnl = realizedNetPnl(t);
      if (pnl === null || !Number.isInteger(h)) continue;
      hourBuckets[h].totalPnl += pnl;
      hourBuckets[h].count += 1;
    } catch {}
  }

  // Find max absolute avg for scaling
  const avgs = hourBuckets.map(b => b.count > 0 ? b.totalPnl / b.count : 0);
  const maxAbs = Math.max(...avgs.map(Math.abs), 0.01);

  let html = '';
  for (let h = 0; h < 24; h++) {
    const b = hourBuckets[h];
    const avg = b.count > 0 ? b.totalPnl / b.count : 0;
    let bg, title;
    if (b.count === 0) {
      bg = 'rgba(255,255,255,0.06)';
      title = `Hour ${h} UTC: no trades`;
    } else {
      const intensity = Math.min(Math.abs(avg) / maxAbs, 1);
      const alpha = 0.15 + intensity * 0.65;
      if (avg >= 0) {
        bg = `rgba(125,229,181,${alpha.toFixed(2)})`;
      } else {
        bg = `rgba(255,146,160,${alpha.toFixed(2)})`;
      }
      title = `Hour ${h} UTC: ${b.count} trades, avg ${avg >= 0 ? '+' : ''}$${avg.toFixed(2)}, total ${b.totalPnl >= 0 ? '+' : ''}$${b.totalPnl.toFixed(2)}`;
    }
    html += `<div style="flex:1; min-width:18px; height:28px; background:${bg}; border-radius:var(--r-sm); cursor:default; transition:background 0.3s;" title="${title}"></div>`;
  }
  container.innerHTML = html;
}

function renderMakerAbSummary(payload) {
  const front = payload?.front_runner || null;
  const latest = payload?.latest || null;
  const notes = Array.isArray(payload?.notes) ? payload.notes : [];

  const candidateEl = document.getElementById('maker-ab-candidate');
  const verdictEl = document.getElementById('maker-ab-verdict');
  const adverseEl = document.getElementById('maker-ab-adverse');
  const quotesEl = document.getElementById('maker-ab-quotes');
  const noteEl = document.getElementById('maker-ab-note');

  if (candidateEl) candidateEl.textContent = fmtCandidateLabel(front?.candidate_label);

  if (verdictEl) {
    const verdict = String(front?.verdict || latest?.verdict || '—');
    verdictEl.textContent = verdict === '—' ? verdict : verdict.replace(/_/g, ' ');
    verdictEl.style.color =
      verdict.includes('regression') || verdict.includes('starved') ? 'var(--clr-red)' :
      verdict.includes('promising') || verdict.includes('improvement') || verdict.includes('mixed') ? 'var(--clr-green)' :
      '';
  }

  const adverse = front?.avg_adverse_move_delta_bps;
  if (adverseEl) {
    adverseEl.textContent = adverse != null ? fmtSigned(adverse, 1, 'bps') : '—';
    adverseEl.style.color = adverse < 0 ? 'var(--clr-green)' : adverse > 0 ? 'var(--clr-red)' : '';
  }

  const quotes = front?.avg_places_delta_pct;
  if (quotesEl) {
    quotesEl.textContent = quotes != null ? fmtSigned(quotes, 1, '%') : '—';
    quotesEl.style.color = quotes < 0 ? 'var(--clr-yellow)' : quotes > 0 ? 'var(--clr-green)' : '';
  }

  if (noteEl) {
    if (latest?.verdict === 'awaiting_eligible_start') {
      noteEl.textContent = `Latest batch waiting for eligible start (${fmtEtaSeconds(latest?.next_eligible_delay_seconds)}).`;
    } else if (front) {
      const engaged = front?.engaged_comparable_windows ?? 0;
      const improved = front?.engaged_improved_adverse_move ?? 0;
      const regressed = front?.engaged_regressed_adverse_move ?? 0;
      noteEl.textContent = `${fmtCandidateLabel(front?.candidate_label)} front-runner: ${engaged} engaged windows, ${improved} improved, ${regressed} regressed.`;
    } else if (notes.length) {
      noteEl.textContent = notes[0];
    } else {
      noteEl.textContent = 'No maker A/B summary available yet.';
    }
  }
}

function renderMakerRolloutStatus(payload) {
  const readiness = payload?.readiness || null;
  const gate = payload?.gate || null;
  const runtime = payload?.gate_runtime || null;
  const shutdown = payload?.shutdown_validation?.latest_bundle || null;
  const shutdownAttempts = payload?.shutdown_validation?.latest_attempts || null;
  const front = readiness?.front_runner || null;
  const blockers = Array.isArray(readiness?.blockers) ? readiness.blockers : [];
  const rounds = Array.isArray(gate?.rounds) ? gate.rounds : [];

  const verdictEl = document.getElementById('maker-rollout-verdict');
  const phaseEl = document.getElementById('maker-rollout-phase');
  const stopEl = document.getElementById('maker-rollout-stop');
  const blockersEl = document.getElementById('maker-rollout-blockers');
  const roundsEl = document.getElementById('maker-rollout-rounds');
  const noteEl = document.getElementById('maker-rollout-note');
  const shutdownEl = document.getElementById('maker-shutdown-proof');
  const shutdownNoteEl = document.getElementById('maker-shutdown-note');

  const verdict = String((gate?.verdict || readiness?.verdict || '—'));
  if (verdictEl) {
    verdictEl.textContent = verdict === '—' ? verdict : verdict.replace(/_/g, ' ');
    verdictEl.style.color =
      verdict.includes('ready') ? 'var(--clr-green)' :
      verdict.includes('blocked') ? 'var(--clr-yellow)' :
      '';
  }

  if (phaseEl) {
    const phase = String(runtime?.phase || '—');
    phaseEl.textContent = phase === '—' ? phase : phase.replace(/_/g, ' ');
    phaseEl.style.color = runtime?.active ? 'var(--clr-green)' : '';
  }

  if (stopEl) {
    const stop = String((runtime?.active ? runtime?.stop_reason : gate?.stop_reason) || gate?.stop_reason || '—');
    stopEl.textContent = stop === '—' ? stop : stop.replace(/_/g, ' ');
  }

  if (blockersEl) {
    blockersEl.textContent = blockers.length ? String(blockers.length) : '0';
    blockersEl.style.color = blockers.length ? 'var(--clr-yellow)' : 'var(--clr-green)';
  }

  if (roundsEl) {
    roundsEl.textContent = String(rounds.length || 0);
  }

  if (shutdownEl) {
    const terminalClass = String(shutdown?.terminal_class || payload?.shutdown_validation?.status || '—');
    let shutdownLabel = '—';
    if (terminalClass === 'success' && Number(shutdown?.cleanup_trade_count || 0) >= 2) {
      shutdownLabel = 'PROVEN';
    } else if (terminalClass === 'success') {
      shutdownLabel = 'SINGLE';
    } else if (terminalClass === 'starvation/dead_window' || terminalClass === 'inactive/unexercised') {
      shutdownLabel = 'PARKED';
    } else if (terminalClass === 'lifecycle_bug') {
      shutdownLabel = 'BROKEN';
    } else if (terminalClass && terminalClass !== '—') {
      shutdownLabel = fmtTruthLabel(terminalClass);
    }
    shutdownEl.textContent = shutdownLabel;
    shutdownEl.className = `v ${truthTone(shutdownLabel.toLowerCase())}`.trim();
  }

  if (shutdownNoteEl) {
    if (shutdown) {
      const pieces = [];
      const terminalClass = String(shutdown?.terminal_class || '');
      if (terminalClass === 'starvation/dead_window' || terminalClass === 'inactive/unexercised') {
        pieces.push('Shutdown proof parked: code-complete, evidence-incomplete.');
      }
      if (shutdown.run_classification_note) pieces.push(shutdown.run_classification_note);
      if (shutdown.cleanup_trade_count != null) pieces.push(`cleanup trades ${shutdown.cleanup_trade_count}`);
      if (shutdown.peak_active_trade_count != null) pieces.push(`peak active ${shutdown.peak_active_trade_count}`);
      if (shutdown.run_exit_cause) pieces.push(`exit ${String(shutdown.run_exit_cause).replace(/_/g, ' ')}`);
      if (shutdownAttempts?.classification_counts) {
        const counts = shutdownAttempts.classification_counts || {};
        pieces.push(`attempts proof=${counts.proof || 0} starvation=${counts.starvation || 0} other=${counts.other || 0}`);
      }
      shutdownNoteEl.textContent = pieces.join(' · ') || 'Shutdown proof status available but sparse.';
    } else {
      shutdownNoteEl.textContent = 'No shutdown proof summary available yet.';
    }
  }

  if (noteEl) {
    if (runtime?.active) {
      noteEl.textContent = `Gate runner active: ${String(runtime?.phase || 'working').replace(/_/g, ' ')}.`;
    } else if (front && blockers.length) {
      noteEl.textContent = `${fmtCandidateLabel(front?.label)} blocked by ${blockers.length} rollout gate condition${blockers.length === 1 ? '' : 's'}.`;
    } else if (front) {
      noteEl.textContent = `${fmtCandidateLabel(front?.label)} is clear on the current rollout gate.`;
    } else if (gate) {
      noteEl.textContent = `Gate runner status: ${String(gate?.stop_reason || verdict).replace(/_/g, ' ')}.`;
    } else {
      noteEl.textContent = 'No maker rollout gate status available yet.';
    }
  }
}

// ── Initial load ──────────────────────────────────────────────────────────────
async function initialLoad() {
  initChart();
  initBtcChart();
  initTerminalTabs();
  connectSSE();
  renderArchivedTrades();
  updateDashboardStrip();

  // Front-load only the visible-above-the-fold reads, then stagger the rest.
  await Promise.allSettled([
    loadConfig(),
    loadStats(),
    loadAnalysis(),
  ]);

  await Promise.allSettled([
    loadTrades(),
    loadMakerState(),
  ]);

  initialLoadComplete = true;
  if (activeTab === 'execution') {
    scheduleDeferredRead(() => loadExecutionQuality(), 600);
  } else if (activeTab === 'risk') {
    scheduleDeferredRead(() => loadRiskAndAttribution(), 600);
  } else if (activeTab === 'tape') {
    setTimeout(() => { refreshTape(); }, 600);
  } else if (activeTab === 'report') {
    // setActiveTab skips on-demand reads before this point, so a restored
    // Report tab would otherwise sit on "Loading…" forever. Staggered behind
    // the front-loaded reads, but not routed through the polling gate: that
    // gate drops tasks while the page is hidden, and a page restored into a
    // background tab would then never load at all.
    setTimeout(() => { void loadLedgerReport(); }, 600);
  }

  scheduleInitialDeferredReads();
  startFreshnessTicker();
}

// Only poll heavy/tab-gated queries; everything else comes via SSE
setInterval(() => { if (shouldPollReadLoop() && activeTab === 'execution') loadExecutionQuality(); }, 20_000);  // heavy query, tab-gated
setInterval(() => { if (shouldPollReadLoop() && activeTab === 'risk') loadRiskAndAttribution(); }, 10_000);  // heavy query, tab-gated
setInterval(() => { if (shouldPollReadLoop()) loadIntegritySummary(); }, 30_000);
setInterval(() => { if (shouldPollReadLoop() && activeTab === 'maker') loadMakerAbSummary(); }, 60_000);
setInterval(() => { if (shouldPollReadLoop() && activeTab === 'maker') loadMakerRolloutStatus(); }, 60_000);
// Fallback: when the stream is not delivering, refresh whatever the reader is
// actually looking at, plus the figures the page chrome shows on every tab.
setInterval(() => {
  if (!shouldPollReadLoop()) return;
  const stale = !lastSseAt || (Date.now() - lastSseAt) > 5_000;
  if (!stale) return;
  loadStats();
  refreshActiveTab();
  updateDashboardStrip();
}, 10_000);

window.addEventListener('DOMContentLoaded', initialLoad);
window.addEventListener('visibilitychange', () => {
  if (shouldPollReadLoop()) {
    scheduleInitialDeferredReads();
    // Deferred reads are skipped outright while the tab is hidden, and the
    // initial batch only ever runs once. Without this a page opened in a
    // background tab sits on "Loading…" for as long as it stays open.
    if (activeTab === 'report' && !ledgerLoaded) void loadLedgerReport();
    if (activeTab === 'tape' && !tapeLoaded) refreshTape();
  }
  updateDashboardStrip();
});

// ── Report (ledger) ─────────────────────────────────────────────────────────
// The evidence view. Everything here is read, not operated, so it loads on
// demand rather than on the SSE tick.

let ledgerBusy = false;
let ledgerLoaded = false;
let ledgerLastCurve = null;
let ledgerResizeBound = false;

// Market questions come from an external feed, so they are escaped before they
// reach innerHTML.


const ledgerUsd = (v, dp = 2) => {
  const n = Number(v);
  if (v === null || v === undefined || !Number.isFinite(n)) return '—';
  const sign = n > 0 ? '+' : n < 0 ? '−' : '';
  return `${sign}$${Math.abs(n).toFixed(dp)}`;
};

const ledgerClock = iso => {
  if (!iso) return '—';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return '—';
  const pad = n => String(n).padStart(2, '0');
  return `${d.getUTCFullYear()}-${pad(d.getUTCMonth() + 1)}-${pad(d.getUTCDate())} ${pad(d.getUTCHours())}:${pad(d.getUTCMinutes())}`;
};

// Questions read "Bitcoin Up or Down — September 14, 8PM ET". The window is the
// only part that distinguishes one row from the next.
const ledgerMarketLabel = question => {
  const text = String(question || '').trim();
  if (!text) return '—';
  const dash = text.search(/[—–-]\s/);
  const tail = dash > 0 ? text.slice(dash + 1).trim() : text;
  return tail.length > 46 ? `${tail.slice(0, 45)}…` : tail;
};

const ledgerSide = direction => String(direction || '')
  .replace(/^BUY_/i, '')
  .toUpperCase() || '—';

function ledgerSetHtml(id, html) {
  const el = document.getElementById(id);
  if (el) el.innerHTML = html;
}

function renderLedgerProvenance(prov) {
  const p = prov || {};
  const chips = [
    ['Mode', p.mode],
    ['Execution', p.execution_mode],
    ['Strategy', p.strategy],
    ['Config', p.calibration_hash],
    ['Build', p.implementation_id],
    ['Model', p.model_hash || (p.model_loaded ? 'loaded' : 'none')],
  ];
  ledgerSetHtml('ledger-prov', chips
    .filter(([, v]) => v)
    .map(([k, v]) => `<span class="ledger-chip">${ledgerEsc(k)} <b>${ledgerEsc(v)}</b></span>`)
    .join(''));
}

function renderLedgerWindow(win) {
  const w = win || {};
  const sub = document.getElementById('ledger-window-sub');
  if (sub) {
    const span = w.days_elapsed
      ? `${ledgerClock(w.first_close)} to ${ledgerClock(w.last_close)} UTC`
      : 'No closed trades recorded.';
    sub.textContent = span;
  }

  const gate = document.getElementById('ledger-gate');
  if (!gate) return;
  gate.hidden = false;
  gate.classList.toggle('is-met', !!w.evaluable);

  if (w.evaluable) {
    gate.innerHTML = '<b>Sample floor met.</b> The pre-registered minimum of '
      + `${w.required_trades} closed trades across ${w.required_markets} markets over `
      + `${w.required_days} days is satisfied. Apply the acceptance criteria in the evidence protocol.`;
    return;
  }

  const short = [];
  if ((w.closed_trades ?? 0) < (w.required_trades ?? 0)) {
    short.push(`${w.closed_trades ?? 0} of ${w.required_trades} trades`);
  }
  if ((w.distinct_markets ?? 0) < (w.required_markets ?? 0)) {
    short.push(`${w.distinct_markets ?? 0} of ${w.required_markets} markets`);
  }
  if ((w.days_elapsed ?? 0) < (w.required_days ?? 0)) {
    short.push(`${w.days_elapsed ?? 0} of ${w.required_days} days`);
  }
  gate.innerHTML = '<b>Not yet evaluable.</b> The pre-registered sample floor is not met: '
    + `${ledgerEsc(short.join(', '))}. Figures below describe the window so far. `
    + 'They are not a result.';
}

function realizedNetPnl(trade) {
  if (trade?.pnl === null || trade?.pnl === undefined) return null;
  const pnl = Number(trade.pnl);
  const fee = Number(trade.fee_paid ?? 0);
  if (!Number.isFinite(pnl) || !Number.isFinite(fee)) return null;
  return pnl - (Number(trade.accounting_version) === 1 ? 0 : fee);
}

function sumRealizedNetPnl(trades) {
  let total = 0;
  for (const trade of trades) {
    const pnl = realizedNetPnl(trade);
    if (pnl === null) return null;
    total += pnl;
  }
  return total;
}

function renderLedgerFigures(totals, win) {
  const t = totals || {};
  const w = win || {};
  const net = Number(t.net_pnl ?? 0);
  const dd = Number(t.max_drawdown ?? 0);
  const closed = Number(w.closed_trades ?? 0);
  const partialFees = closed > 0 && Number(t.trades_with_recorded_fee ?? 0) < closed;
  const figs = [
    {
      n: ledgerUsd(net),
      cls: net > 0 ? 'pos' : net < 0 ? 'neg' : '',
      l: partialFees ? 'Net, fees partly recorded' : 'Net after fees',
    },
    { n: ledgerUsd(t.booked_pnl), cls: '', l: 'Booked P&L' },
    { n: fmtMoney(t.entry_fees), cls: '', l: 'Entry fees' },
    { n: `${w.closed_trades ?? 0}`, cls: '', l: `Closed of ${w.required_trades ?? 0} required` },
    { n: `${w.distinct_markets ?? 0}`, cls: '', l: `Markets of ${w.required_markets ?? 0}` },
    {
      n: t.win_rate === null || t.win_rate === undefined ? '—' : `${Number(t.win_rate).toFixed(1)}%`,
      cls: '',
      l: 'Win rate, net',
    },
    { n: dd > 0 ? `−$${dd.toFixed(2)}` : '$0.00', cls: dd > 0 ? 'neg' : '', l: 'Max drawdown' },
  ];
  ledgerSetHtml('ledger-figs', figs.map(f =>
    `<div class="ledger-fig"><span class="n ${f.cls}">${ledgerEsc(f.n)}</span><span class="l">${f.l}</span></div>`
  ).join(''));
}

// Drawn at the element's real pixel width so the stroke stays uniform and the
// labels are not stretched. A stretched viewBox is the usual cause of both.
function drawLedgerCurve(curve) {
  const svg = document.getElementById('ledger-curve');
  if (!svg) return;
  ledgerLastCurve = Array.isArray(curve) ? curve : [];

  const points = ledgerLastCurve;
  // Floor exists only to keep the labels legible if the element reports zero.
  const width = Math.max(260, Math.round(svg.clientWidth || svg.parentElement?.clientWidth || 680));
  const height = 120;
  svg.setAttribute('viewBox', `0 0 ${width} ${height}`);

  if (points.length < 2) {
    svg.innerHTML = `<text x="0" y="${height / 2}" fill="var(--clr-faint)" `
      + 'font-family="IBM Plex Mono, monospace" font-size="11">'
      + 'Not enough closed trades to plot a curve.</text>';
    return;
  }

  const padL = 46;
  const padR = 62;
  const padT = 14;
  const padB = 22;
  const values = points.map(p => Number(p.cum) || 0);
  let lo = Math.min(0, ...values);
  let hi = Math.max(0, ...values);
  if (hi - lo < 1e-9) { hi += 0.5; lo -= 0.5; }
  const pad = (hi - lo) * 0.12;
  lo -= pad;
  hi += pad;

  const x = i => padL + (i / (points.length - 1)) * (width - padL - padR);
  const y = v => padT + (1 - (v - lo) / (hi - lo)) * (height - padT - padB);

  const last = values[values.length - 1];
  const stroke = last >= 0 ? 'var(--clr-green)' : 'var(--clr-red)';
  const line = points.map((p, i) => `${x(i).toFixed(1)},${y(Number(p.cum) || 0).toFixed(1)}`).join(' ');
  const area = `${padL},${y(0).toFixed(1)} ${line} ${x(points.length - 1).toFixed(1)},${y(0).toFixed(1)}`;
  const zeroY = y(0).toFixed(1);
  const endX = x(points.length - 1);
  const endY = y(last);

  svg.innerHTML = `
    <polygon points="${area}" fill="${stroke}" opacity="0.10"></polygon>
    <line x1="${padL}" y1="${zeroY}" x2="${width - padR}" y2="${zeroY}"
          stroke="var(--clr-border-strong)" stroke-width="1"></line>
    <polyline points="${line}" fill="none" stroke="${stroke}" stroke-width="1.75"
              stroke-linejoin="round" stroke-linecap="round"></polyline>
    <circle cx="${endX.toFixed(1)}" cy="${endY.toFixed(1)}" r="3.25" fill="${stroke}"></circle>
    <text x="${(endX + 8).toFixed(1)}" y="${(endY + 3.5).toFixed(1)}" fill="${stroke}"
          font-family="IBM Plex Mono, monospace" font-size="11">${ledgerEsc(ledgerUsd(last))}</text>
    <text x="${padL - 8}" y="${(Number(zeroY) + 3.5).toFixed(1)}" text-anchor="end" fill="var(--clr-faint)"
          font-family="IBM Plex Mono, monospace" font-size="10">$0.00</text>
    <text x="${padL}" y="${height - 5}" fill="var(--clr-faint)"
          font-family="IBM Plex Mono, monospace" font-size="10">${ledgerEsc(ledgerClock(points[0].t).slice(0, 10))}</text>
    <text x="${(width - padR).toFixed(1)}" y="${height - 5}" text-anchor="end" fill="var(--clr-faint)"
          font-family="IBM Plex Mono, monospace" font-size="10">${ledgerEsc(ledgerClock(points[points.length - 1].t).slice(0, 10))}</text>
  `;
}

function renderLedgerTrades(rows) {
  const tbody = document.querySelector('#ledger-trades tbody');
  if (!tbody) return;
  if (!Array.isArray(rows) || rows.length === 0) {
    tbody.innerHTML = '<tr><td colspan="7" class="ledger-empty">No closed trades yet.</td></tr>';
    return;
  }
  tbody.innerHTML = rows.map(r => {
    const net = Number(r.net ?? 0);
    const cls = net > 0 ? 'pos' : net < 0 ? 'neg' : '';
    return `
      <tr>
        <td>${ledgerEsc(ledgerClock(r.closed_at))}</td>
        <td class="txt" title="${ledgerEsc(r.market)}">${ledgerEsc(ledgerMarketLabel(r.market))}</td>
        <td>${ledgerEsc(ledgerSide(r.direction))}</td>
        <td class="num">${ledgerEsc(fmtN(r.entry_price, 2))}</td>
        <td class="num">${ledgerEsc(r.exit_price === null || r.exit_price === undefined ? '—' : fmtN(r.exit_price, 2))}</td>
        <td class="num">${fmtN(r.fee, 3)}</td>
        <td class="num ${cls}">${ledgerEsc(ledgerUsd(net))}</td>
      </tr>`;
  }).join('');
}

function renderLedgerRejections(summary, error, closedTrades) {
  const tbody = document.querySelector('#ledger-rejections tbody');
  const note = document.getElementById('ledger-rej-note');
  if (!tbody) return;

  if (error) {
    // A failed summary must not read as "no rejections", which is the most
    // flattering possible way to render an error.
    tbody.innerHTML = `<tr><td colspan="3" class="ledger-empty">${ledgerEsc(error)}</td></tr>`;
    if (note) note.textContent = 'Rejection counts unavailable.';
    return;
  }

  const total = Number(summary?.total ?? 0);
  const reasons = Array.isArray(summary?.reasons) ? summary.reasons : [];

  if (note) {
    const considered = total + Number(closedTrades || 0);
    note.textContent = considered > 0
      ? `${total.toLocaleString()} declined of ${considered.toLocaleString()} candidates (${(total / considered * 100).toFixed(1)}%).`
      : 'No decisions recorded yet.';
  }

  if (reasons.length === 0) {
    tbody.innerHTML = '<tr><td colspan="3" class="ledger-empty">No rejections recorded.</td></tr>';
    return;
  }

  const top = reasons[0]?.count || 1;
  tbody.innerHTML = reasons.map(r => {
    const count = Number(r.count || 0);
    // The dominant reason is the one that decides whether there is a strategy
    // here at all, so it carries the warning stripe.
    const sev = count >= top * 0.5 ? 'ledger-sev warn' : 'ledger-sev';
    const share = total > 0 ? `${(count / total * 100).toFixed(1)}%` : '—';
    return `
      <tr>
        <td class="txt"><span class="${sev}"></span>${ledgerEsc(r.reason)}</td>
        <td class="num">${count.toLocaleString()}</td>
        <td class="num">${share}</td>
      </tr>`;
  }).join('');
}

function renderLedgerFooter(payload) {
  const el = document.getElementById('ledger-foot');
  if (!el) return;
  const closed = Number(payload?.window?.closed_trades ?? 0);
  const covered = Number(payload?.totals?.trades_with_recorded_fee ?? 0);
  const parts = [];

  // Maker and live fills record no local fee, so on a maker-heavy sample the
  // headline figure is only partly net. Say so rather than let the label imply
  // a completeness the data does not have.
  if (closed > 0 && covered < closed) {
    parts.push(`Fees are recorded for ${covered} of ${closed} closed trades; `
      + `the remaining historical or simulated rows do not establish complete fee coverage. `
      + `Their displayed result may omit costs.`);
  } else if (closed > 0) {
    parts.push('Net follows each trade’s accounting version. Confirmed live entry and exit cash fees are already included; entry fees are shown separately for reference.');
  }

  parts.push('Rejection reasons are grouped by category; the stored text also records the specific threshold that fired.');
  parts.push(`Generated ${ledgerClock(payload?.generated_at)} UTC.`);
  el.textContent = parts.join(' ');
}

async function loadLedgerReport() {
  if (ledgerBusy) return;
  ledgerBusy = true;
  try {
    const payload = await GET('/api/report');
    renderLedgerProvenance(payload.provenance);
    renderLedgerWindow(payload.window);
    renderLedgerFigures(payload.totals, payload.window);
    drawLedgerCurve(payload.curve);
    renderLedgerTrades(payload.rows);
    renderLedgerRejections(
      payload.rejections,
      payload.rejection_error,
      payload.window?.closed_trades,
    );
    renderLedgerFooter(payload);
    ledgerLoaded = true;

    if (!ledgerResizeBound) {
      ledgerResizeBound = true;
      let pending = null;
      window.addEventListener('resize', () => {
        clearTimeout(pending);
        pending = setTimeout(() => {
          if (activeTab === 'report' && ledgerLastCurve) drawLedgerCurve(ledgerLastCurve);
        }, 150);
      });
    }
  } catch (err) {
    const sub = document.getElementById('ledger-window-sub');
    if (sub) sub.textContent = `Could not load the report: ${err.message || err}`;
    const gate = document.getElementById('ledger-gate');
    if (gate) gate.hidden = true;
  } finally {
    ledgerBusy = false;
  }
}

// ── Tape (watching view) ────────────────────────────────────────────────────
// Composes the live feed already flowing through the SSE handler into a single
// screen. It holds no state of its own beyond the decision feed, which is the
// one thing the stream does not carry.

let tapeLoaded = false;
let tapeDecisionsBusy = false;

const tapeText = (id, value, cls) => {
  const el = document.getElementById(id);
  if (!el) return;
  el.textContent = value;
  if (cls !== undefined) el.className = cls;
};

// Questions read "Bitcoin Up or Down - April 3, 4:00PM-4:15PM ET". Only the
// window distinguishes one from the next, and the column is narrow.
const tapeWindowOf = question => {
  const text = String(question || '').trim();
  if (!text) return '—';
  const dash = text.search(/[—–-]\s/);
  const tail = dash > 0 ? text.slice(dash + 1).trim() : text;
  const times = tail.match(/(\d{1,2}:\d{2}\s*(?:AM|PM)?\s*[—–-]\s*\d{1,2}:\d{2}\s*(?:AM|PM)?)/i);
  return times ? times[1].replace(/\s+/g, '') : (tail.length > 28 ? `${tail.slice(0, 27)}…` : tail);
};

function updateTapeMarket(a) {
  if (!a) return;

  // The identifier is what says a market is actually in play; a question alone
  // can be a candidate the bot is still waiting on.
  const md = a.market_data || {};
  const hasMarket = !!(md.market_id || md.id) || !!a.question;
  const question = md.question || a.question || '';
  tapeText('tape-market', hasMarket && question ? question : 'Waiting for a market…');
  tapeText('tape-market-window', hasMarket && question ? tapeWindowOf(question) : '—');

  const mins = a.mins_left ?? a.minutes_left ?? a.time_remaining_minutes ?? null;
  const n = Number(mins);
  // Without the market gate an idle bot reads as a market with zero seconds
  // left, which is the single most alarming thing this panel can say.
  const live = hasMarket && mins !== null && Number.isFinite(n) && n >= 0 && n < 600;
  const clock = document.getElementById('tape-countdown');
  const fill = document.getElementById('tape-track-fill');
  if (clock) {
    if (live) {
      const secs = Math.max(0, n * 60);
      clock.textContent = `${Math.floor(secs / 60)}:${String(Math.floor(secs % 60)).padStart(2, '0')}`;
    } else {
      clock.textContent = '--:--';
    }
    clock.classList.toggle('is-urgent', live && n < 2);
  }
  if (fill) {
    // Fills as the window closes, so the bar reads as time spent.
    fill.style.width = live ? `${Math.max(0, Math.min(100, (1 - n / 15) * 100)).toFixed(1)}%` : '0%';
    fill.classList.toggle('is-urgent', live && n < 2);
  }

  const strike = Number(a.btc_start_price);
  tapeText('tape-strike', Number.isFinite(strike) && strike > 0
    ? `$${strike.toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`
    : '—');

  const spot = Number(a.btc_price);
  const spotEl = document.getElementById('tape-spot');
  if (spotEl) {
    if (Number.isFinite(spot) && spot > 0) {
      const delta = Number.isFinite(strike) && strike > 0 ? (spot / strike - 1) * 100 : null;
      spotEl.textContent = delta === null
        ? `$${spot.toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`
        : `$${spot.toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 })}  ${delta >= 0 ? '+' : '−'}${Math.abs(delta).toFixed(2)}%`;
      spotEl.className = delta === null ? '' : (delta > 0 ? 'pos' : delta < 0 ? 'neg' : '');
    } else {
      spotEl.textContent = '—';
      spotEl.className = '';
    }
  }

  // Token quotes can differ from complementary probabilities; show each
  // observed price and leave the other side unknown until its book arrives.
  const liveOdds = liveOddsFor(md.market_id || md.id);
  const snapshotYes = Number(a.price ?? md.price);
  const yes = liveOdds ? liveOdds.yes : snapshotYes;
  const no = liveOdds ? liveOdds.no : null;
  const quote = value => Number.isFinite(value) && value > 0 && value < 1
    ? value.toFixed(2) : '—';
  tapeText('tape-odds', `${quote(yes)} / ${quote(no)}`);
}

function updateTapeOpenPositions(a) {
  const tbody = document.querySelector('#tape-open tbody');
  if (!tbody) return;
  const open = (allTrades || []).filter(t => ['OPEN', 'CLOSING'].includes(String(t.status).toUpperCase()));
  if (open.length === 0) {
    tbody.innerHTML = '<tr><td colspan="6" class="tape-empty">No open positions.</td></tr>';
    return;
  }

  tbody.innerHTML = open.slice(0, 8).map(t => {
    const partialPending = ['in_flight', 'unknown'].includes(t.partial_close_state);
    if (String(t.status).toUpperCase() === 'CLOSING' || partialPending) {
      return `<tr><td class="txt">#${t.id}</td><td colspan="5">${partialPending ? 'Partial sale awaiting confirmed outcome' : 'Exit needs venue reconciliation'}</td></tr>`;
    }
    if (isPendingEntry(t)) {
      return `<tr><td class="txt">#${t.id}</td><td colspan="5">${entryPendingLabel(t)}</td></tr>`;
    }
    const side = String(t.direction || '').replace(/^BUY_/i, '').toUpperCase();
    // Use the same fresh outcome-token book as the overview. Historical
    // metadata and the other outcome's price cannot establish a live mark.
    const mark = liveMidFor(t.token_id);
    const pnl = mark === null ? null : (mark - Number(t.entry_price)) * Number(t.shares || 0);
    const cls = pnl === null ? '' : pnl > 0 ? 'pos' : pnl < 0 ? 'neg' : '';
    return `
      <tr>
        <td class="txt" title="${ledgerEsc(t.question || '')}">${ledgerEsc(tapeWindowOf(t.question))}</td>
        <td>${ledgerEsc(side || '—')}</td>
        <td class="num">${fmtN(t.entry_price, 2)}</td>
        <td class="num">${fmtMoney(t.amount)}</td>
        <td class="num">${mark === null ? '—' : mark.toFixed(2)}</td>
        <td class="num ${cls}">${pnl === null ? '—' : ledgerUsd(pnl)}</td>
      </tr>`;
  }).join('');
}

function updateTapePerformance(report, balance) {
  const totals = report?.totals || {};
  const win = report?.window || {};
  const net = Number(totals.net_pnl ?? 0);

  const big = document.getElementById('tape-session-pnl');
  if (big) {
    big.textContent = ledgerUsd(net);
    big.className = `tape-big ${net > 0 ? 'pos' : net < 0 ? 'neg' : ''}`;
  }

  const closed = Number(win.closed_trades ?? 0);
  const open = (allTrades || []).filter(t => ['OPEN', 'CLOSING'].includes(String(t.status).toUpperCase())).length;
  tapeText('tape-session-trades', `${closed} closed · ${open} open`);
  tapeText('tape-session-wr', totals.win_rate === null || totals.win_rate === undefined
    ? '—'
    : fmtPct(totals.win_rate));

  const dd = Number(totals.max_drawdown ?? 0);
  tapeText('tape-session-dd', dd > 0 ? `−$${dd.toFixed(2)}` : '$0.00', dd > 0 ? 'neg' : '');
  if (balance !== undefined && balance !== null) tapeText('tape-session-bal', fmtMoney(balance));
}

function updateTapeSignals(a) {
  const host = document.getElementById('tape-bars');
  if (!host) return;
  const breakdown = a?.signals;
  const entries = breakdown && typeof breakdown === 'object' ? Object.entries(breakdown) : [];
  const usable = entries
    .filter(([, v]) => Number.isFinite(Number(v)) && Number(v) !== 0)
    .sort((x, y) => Math.abs(Number(y[1])) - Math.abs(Number(x[1])))
    .slice(0, 8);

  if (usable.length === 0) {
    host.innerHTML = '<p class="tape-empty">No signal breakdown yet.</p>';
    return;
  }

  // Scale to the strongest signal present so the shapes stay comparable
  // instead of all pinning at the edge.
  const peak = Math.max(...usable.map(([, v]) => Math.abs(Number(v))), 0.01);
  host.innerHTML = usable.map(([name, raw]) => {
    const v = Number(raw);
    const pct = Math.min(50, (Math.abs(v) / peak) * 50);
    const label = name.replace(/_/g, ' ').replace(/\b\w/g, c => c.toUpperCase());
    const bar = v >= 0
      ? `<i style="left:50%;width:${pct.toFixed(1)}%"></i>`
      : `<i class="dn" style="right:50%;width:${pct.toFixed(1)}%"></i>`;
    return `
      <div class="tape-bar">
        <span class="k" title="${ledgerEsc(label)}">${ledgerEsc(label)}</span>
        <span class="g">${bar}</span>
        <span class="v">${v >= 0 ? '+' : '−'}${Math.abs(v).toFixed(2)}</span>
      </div>`;
  }).join('');
}

function updateTapeHealth(a) {
  const host = document.getElementById('tape-health');
  if (!host) return;

  const sseAgeS = lastSseAt ? (Date.now() - lastSseAt) / 1000 : null;
  const streamOk = sseAgeS !== null && sseAgeS < 10;
  const loopOn = loopRunningState === true;

  const rows = [
    {
      k: 'Stream',
      v: sseAgeS === null ? 'not connected' : `${streamOk ? 'live' : 'stale'} · ${sseAgeS.toFixed(1)}s`,
      cls: sseAgeS === null ? 'warn' : streamOk ? 'pos' : 'warn',
    },
    {
      k: 'Trading loop',
      v: loopRunningState === null ? '—' : loopOn ? 'running' : 'stopped',
      cls: loopRunningState === null ? '' : loopOn ? 'pos' : 'warn',
    },
    {
      k: 'Mode',
      v: runtimeModeState ? runtimeModeState : '—',
      cls: runtimeModeState === 'live' ? 'warn' : '',
    },
    {
      k: 'Market feed',
      v: a && (a.market_data || a.question) ? 'market found' : 'no market',
      cls: a && (a.market_data || a.question) ? 'pos' : 'warn',
    },
  ];

  host.innerHTML = rows.map(r =>
    `<div class="tape-row"><dt>${ledgerEsc(r.k)}</dt><dd class="${r.cls}">${ledgerEsc(r.v)}</dd></div>`
  ).join('');
}

function renderTapeDecisions(list) {
  const tbody = document.querySelector('#tape-decisions tbody');
  const note = document.getElementById('tape-dec-note');
  if (!tbody) return;

  if (!Array.isArray(list) || list.length === 0) {
    tbody.innerHTML = '<tr><td colspan="5" class="tape-empty">No decisions recorded yet.</td></tr>';
    if (note) note.textContent = '';
    return;
  }

  const declined = list.filter(d => d.kind === 'declined').length;
  if (note) note.textContent = `${declined} of the last ${list.length} declined`;

  const hhmmss = iso => {
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return '—';
    const pad = n => String(n).padStart(2, '0');
    return `${pad(d.getUTCHours())}:${pad(d.getUTCMinutes())}:${pad(d.getUTCSeconds())}`;
  };

  tbody.innerHTML = list.map(d => {
    const pnl = d.pnl === null || d.pnl === undefined ? null : Number(d.pnl);
    const outcomeCls = d.kind === 'declined' ? '' : pnl === null ? '' : pnl > 0 ? 'pos' : pnl < 0 ? 'neg' : '';
    return `
      <tr class="${d.kind === 'declined' ? 'is-declined' : ''}">
        <td>${ledgerEsc(hhmmss(d.at))}</td>
        <td class="txt" title="${ledgerEsc(d.market)}">${ledgerEsc(tapeWindowOf(d.market))}</td>
        <td>${ledgerEsc(d.call)}</td>
        <td class="num">${d.confidence === null || d.confidence === undefined ? '—' : Number(d.confidence).toFixed(2)}</td>
        <td class="txt ${outcomeCls}" title="${ledgerEsc(d.outcome)}">${ledgerEsc(d.outcome)}</td>
      </tr>`;
  }).join('');
}

async function loadTapeDecisions() {
  if (tapeDecisionsBusy) return;
  tapeDecisionsBusy = true;
  try {
    const payload = await GET('/api/decisions');
    renderTapeDecisions(payload.decisions);
    tapeLoaded = true;
  } catch (err) {
    const tbody = document.querySelector('#tape-decisions tbody');
    if (tbody) {
      tbody.innerHTML = `<tr><td colspan="5" class="tape-empty">Could not load decisions: ${ledgerEsc(err.message || err)}</td></tr>`;
    }
  } finally {
    tapeDecisionsBusy = false;
  }
}

// Called from the SSE handler, so the tape tracks the live feed without a
// second poll loop of its own.
function updateTape(analysis) {
  if (activeTab !== 'tape') return;
  const a = analysis || window._lastAnalysis;
  updateTapeMarket(a);
  updateTapeOpenPositions(a);
  updateTapeSignals(a);
  updateTapeHealth(a);
}

function refreshTape() {
  updateTape(window._lastAnalysis);
  // A watching view must not re-render a stale snapshot. When the stream is
  // not delivering, the analysis is refetched rather than replayed.
  const streamLive = lastSseAt && (Date.now() - lastSseAt) < 10_000;
  if (!streamLive) void loadAnalysis();
  void loadTapeDecisions();
  // Read directly rather than waiting on the stream, which needs an API key
  // and may never connect.
  GET('/api/report')
    .then(report => GET('/api/stats')
      .then(s => updateTapePerformance(report, s.balance))
      .catch(() => updateTapePerformance(report)))
    .catch(() => {});
}

// A watching view that only updates on tab entry is a screenshot. The stream
// carries the market and the signals; the decision feed and the closed-trade
// figures are polled, and only while this tab is the one on screen.
let tapeTimer = null;
function setTapePolling(on) {
  if (tapeTimer) { clearInterval(tapeTimer); tapeTimer = null; }
  if (!on) return;
  tapeTimer = setInterval(() => {
    if (activeTab !== 'tape' || document.hidden) return;
    refreshTape();
  }, 20_000);
}

// ── Data freshness ──────────────────────────────────────────────────────────
// The page draws a figure computed a minute ago exactly like one computed a
// moment ago. These functions are the only thing standing between a reader and
// that mistake, so they state the situation plainly rather than hinting at it.

let lastAnalysisFreshness = null;
let freshnessTicker = null;

const AGE_UNKNOWN = -1;

function fmtAge(secs) {
  if (secs === null || secs === undefined || secs < 0) return '';
  const n = Math.max(0, Math.round(secs));
  if (n < 60) return `${n}s`;
  if (n < 3600) return `${Math.floor(n / 60)}m ${n % 60}s`;
  return `${Math.floor(n / 3600)}h ${Math.floor((n % 3600) / 60)}m`;
}

function noteAnalysisFreshness(freshness) {
  if (!freshness) return;
  // Record when this arrived so the age can keep counting between payloads
  // instead of freezing at whatever the last tick reported.
  lastAnalysisFreshness = { ...freshness, receivedAt: Date.now() };
}

/// Age of the current analysis in seconds, advanced locally between payloads.
function currentAnalysisAge() {
  const f = lastAnalysisFreshness;
  if (!f || f.never_run || f.age_secs === null || f.age_secs === undefined) return AGE_UNKNOWN;
  return f.age_secs + (Date.now() - f.receivedAt) / 1000;
}

function renderDataStatus() {
  // Source age keeps advancing even when the stream stops delivering frames.
  if (expireLiveBook()) {
    refreshOpenPositionsIfBookMoved();
    if (window._lastAnalysis) updateSignalPanel(window._lastAnalysis);
  }
  const wrap = document.getElementById('data-status');
  const label = document.getElementById('sse-status');
  const ageEl = document.getElementById('data-age');
  const banner = document.getElementById('stale-banner');
  if (!wrap || !label) return;

  const streamAgeMs = lastSseAt ? Date.now() - lastSseAt : null;
  const streamLive = streamAgeMs !== null && streamAgeMs < 10_000;
  const age = currentAnalysisAge();
  const maxAge = lastAnalysisFreshness?.max_age_secs ?? null;
  const interval = lastAnalysisFreshness?.interval_secs ?? null;
  const neverRun = !!lastAnalysisFreshness?.never_run;
  const aged = age !== AGE_UNKNOWN && maxAge !== null && age > maxAge;

  wrap.classList.remove('is-live', 'is-stale', 'is-down');

  if (!streamLive) {
    // Nothing is arriving. Say so, rather than leaving a blank dot to carry it.
    wrap.classList.add('is-down');
    label.textContent = getApiAuth && !getApiAuth().apiKey ? 'browser locked' : 'not receiving';
    if (ageEl) ageEl.textContent = age === AGE_UNKNOWN ? '' : `figures ${fmtAge(age)} old`;
  } else if (neverRun) {
    wrap.classList.add('is-stale');
    label.textContent = 'no analysis yet';
    if (ageEl) ageEl.textContent = '';
  } else if (aged) {
    wrap.classList.add('is-stale');
    label.textContent = 'stale';
    if (ageEl) ageEl.textContent = fmtAge(age);
  } else {
    wrap.classList.add('is-live');
    label.textContent = 'live';
    if (ageEl) ageEl.textContent = age === AGE_UNKNOWN ? '' : fmtAge(age);
  }

  // Panels fed by the analysis cycle carry the mark, so a stale reading looks
  // stale wherever it is drawn rather than only in the corner of the page.
  const analysisFed = ['overview', 'tape', 'signals', 'model'];
  document.querySelectorAll('.tab-pane-section').forEach(el => {
    const tabs = String(el.dataset.tabPane || '').split(/\s+/);
    const fed = tabs.some(t => analysisFed.includes(t));
    const ownSourceStale = tabs.some(t => panelSourceIsStale(t));
    el.classList.toggle(
      'is-stale-data',
      (fed && (!streamLive || aged || neverRun)) || ownSourceStale,
    );
  });

  if (!banner) return;

  // Trading being halted outranks anything the stream is doing. A reader who
  // sees only "figures are 40s old" while entries are blocked has been told
  // the less important of two facts.
  if (killSwitchState === true || degradedState === true) {
    banner.hidden = false;
    banner.classList.add('is-down');
    let why = killSwitchState === true
      ? 'The kill switch is engaged, so no new positions will be opened.'
      : 'The bot is in degraded no-trade mode, so no new positions will be opened.';
    // Name the cause where one was recorded. "Halted" without a reason sends
    // the reader to the logs, which may have rotated.
    if (killSwitchState === true && killSwitchCause) {
      const src = String(killSwitchCause.source || '').replace(/_/g, ' ');
      const detail = String(killSwitchCause.detail || '').slice(0, 160);
      why = `Halted by ${src}${detail ? `: ${detail}` : ''}.`;
    }
    banner.innerHTML = `<b>Not trading.</b> <span>${ledgerEsc(why)}`
      + `${streamLive ? '' : ' The live stream is also not connected.'}</span>`;
    return;
  }

  if (!streamLive) {
    banner.hidden = false;
    banner.classList.add('is-down');
    const browserLocked = typeof getApiAuth === 'function' && !getApiAuth().apiKey;
    const why = browserLocked
      ? 'This browser is locked. Unlock it to receive the protected live stream.'
      : 'The live stream is not connected. Figures below are from the last update received.';
    banner.innerHTML = `<b>Not receiving live data.</b> <span>${ledgerEsc(why)}`
      + `${age === AGE_UNKNOWN ? '' : ` Last analysis was ${ledgerEsc(fmtAge(age))} ago.`}</span>`
      + `${browserLocked ? '<button class="btn btn-sm btn-outline-light dashboard-inline-unlock" type="button" onclick="openDashboardUnlock()">Unlock dashboard</button>' : ''}`;
  } else if (neverRun) {
    banner.hidden = false;
    banner.classList.remove('is-down');
    banner.innerHTML = '<b>No analysis has run yet.</b> <span>Signal figures stay blank until the first cycle completes.</span>';
  } else if (aged) {
    banner.hidden = false;
    banner.classList.remove('is-down');
    banner.innerHTML = `<b>Figures are ${ledgerEsc(fmtAge(age))} old.</b> <span>`
      + `The analysis cycle runs every ${ledgerEsc(fmtAge(interval))} and is treated as stale past `
      + `${ledgerEsc(fmtAge(maxAge))}, so a gap here is expected between cycles.</span>`;
  } else {
    banner.hidden = true;
    banner.classList.remove('is-down');
  }
}

function startFreshnessTicker() {
  if (freshnessTicker) return;
  // Age advances whether or not a payload arrives, which is the entire point:
  // a page that has stopped receiving must visibly keep ageing.
  freshnessTicker = setInterval(renderDataStatus, 1000);
  renderDataStatus();
}
