// Shared dashboard credential handling and request signing.
(function (global) {
  const AUTH_STORAGE_KEY = 'macbot.apiAuth';

  function storageCandidates() {
    const stores = [];
    try { if (global.sessionStorage) stores.push(global.sessionStorage); } catch {}
    try { if (global.localStorage) stores.push(global.localStorage); } catch {}
    return stores;
  }

  function setApiAuth(apiKey, apiSecret = '', remember = false) {
    const payload = JSON.stringify({
      apiKey: String(apiKey || '').trim(),
      apiSecret: String(apiSecret || ''),
    });
    try {
      const store = remember ? global.localStorage : global.sessionStorage;
      if (!store || typeof store.setItem !== 'function') return false;
      store.setItem(AUTH_STORAGE_KEY, payload);
      for (const other of storageCandidates()) {
        if (other !== store) {
          try { other.removeItem(AUTH_STORAGE_KEY); } catch {}
        }
      }
      return true;
    } catch {
      return false;
    }
  }

  function clearApiAuth() {
    for (const store of storageCandidates()) {
      try { store.removeItem(AUTH_STORAGE_KEY); } catch {}
    }
  }

  function persistApiAuthFromUrl() {
    const url = new URL(window.location.href);
    const hashParams = new URLSearchParams(url.hash.startsWith('#') ? url.hash.slice(1) : url.hash);
    const apiKey = hashParams.get('api_key');
    const apiSecret = hashParams.get('api_secret');
    if (!apiKey && !apiSecret) return;
    setApiAuth(apiKey || '', apiSecret || '', false);
    hashParams.delete('api_key');
    hashParams.delete('api_secret');
    const nextHash = hashParams.toString();
    url.hash = nextHash ? `#${nextHash}` : '';
    history.replaceState({}, document.title, url.toString());
  }

  function getApiAuth() {
    for (const store of storageCandidates()) {
      try {
        const raw = store.getItem(AUTH_STORAGE_KEY);
        if (!raw) continue;
        const parsed = JSON.parse(raw);
        return {
          apiKey: String(parsed.apiKey || ''),
          apiSecret: String(parsed.apiSecret || ''),
        };
      } catch {}
    }
    return { apiKey: '', apiSecret: '' };
  }

  async function buildAuthHeaders(method, path, bodyText, idempotencyKey = '') {
    const auth = getApiAuth();
    const headers = {};
    if (!auth.apiKey) return headers;
    headers['x-api-key'] = auth.apiKey;
    const idempotency = String(idempotencyKey || '');
    if (idempotency) headers['x-idempotency-key'] = idempotency;
    if (!auth.apiSecret || !window.crypto?.subtle) return headers;

    const ts = String(Math.floor(Date.now() / 1000));
    const nonce = [...crypto.getRandomValues(new Uint8Array(32))]
      .map(b => b.toString(16).padStart(2, '0')).join('');
    const canonical = JSON.stringify([ts, nonce, String(method).toUpperCase(), path, idempotency, bodyText || '']);
    const enc = new TextEncoder();
    const key = await crypto.subtle.importKey(
      'raw',
      enc.encode(auth.apiSecret),
      { name: 'HMAC', hash: 'SHA-256' },
      false,
      ['sign'],
    );
    const sigBuf = await crypto.subtle.sign('HMAC', key, enc.encode(canonical));
    const sig = [...new Uint8Array(sigBuf)]
      .map(b => b.toString(16).padStart(2, '0'))
      .join('');
    headers['x-api-ts'] = ts;
    headers['x-api-nonce'] = nonce;
    headers['x-api-signature'] = sig;
    return headers;
  }

  global.macbotApiAuth = {
    persistApiAuthFromUrl,
    getApiAuth,
    setApiAuth,
    clearApiAuth,
    buildAuthHeaders,
  };
})(globalThis);
