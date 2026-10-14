#!/usr/bin/env python3
"""Shared signed API request helpers for local tooling."""

from __future__ import annotations

import argparse
import hashlib
import hmac
import json
import os
import secrets
import time
from typing import Any
from urllib import error, parse, request
from native_tls_runtime import require_safe_tls
from http_limits import read_response


class NoAPIRedirects(request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        # Custom API keys and signed request headers must remain on the exact
        # configured endpoint; urllib otherwise forwards them on redirects.
        return None


def open_api_request(req, *, timeout):
    require_safe_tls(req.full_url)
    return request.build_opener(NoAPIRedirects()).open(req, timeout=timeout)


def add_api_auth_args(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--api-key",
        default=None,
        help="API key for protected endpoints. Defaults to MACBOT_API_KEY.",
    )
    parser.add_argument(
        "--api-secret",
        default=None,
        help="API HMAC secret. Defaults to MACBOT_API_HMAC_SECRET or MACBOT_API_SECRET.",
    )


def _first_nonempty(*values: str | None) -> str:
    return next((value.strip() for value in values if value and value.strip()), "")


def resolve_api_key(explicit: str | None) -> str:
    return _first_nonempty(explicit, os.getenv("MACBOT_API_KEY"))


def resolve_api_secret(explicit: str | None) -> str:
    # Secret bytes are significant: the server preserves a nonempty configured
    # secret, including its outer whitespace. Only blank aliases fall back.
    return next((value for value in (
        explicit, os.getenv("MACBOT_API_HMAC_SECRET"), os.getenv("MACBOT_API_SECRET"),
    ) if value and value.strip()), "")


def path_and_query(url: str) -> str:
    parts = parse.urlsplit(url)
    path = parts.path or "/"
    return f"{path}?{parts.query}" if parts.query else path


def signed_headers(
    method: str,
    url: str,
    body_text: str,
    api_key: str,
    api_secret: str,
    idempotency_key: str = "",
) -> dict[str, str]:
    ts = str(int(time.time()))
    nonce = secrets.token_hex(32)
    canonical = json.dumps(
        [ts, nonce, method.upper(), path_and_query(url), idempotency_key, body_text],
        ensure_ascii=False, separators=(",", ":"),
    )
    signature = hmac.new(
        api_secret.encode("utf-8"),
        canonical.encode("utf-8"),
        hashlib.sha256,
    ).hexdigest()
    headers = {
        "x-api-key": api_key,
        "x-api-ts": ts,
        "x-api-nonce": nonce,
        "x-api-signature": signature,
    }
    if idempotency_key:
        headers["x-idempotency-key"] = idempotency_key
    return headers


def post_json(
    url: str,
    payload: dict[str, Any],
    *,
    timeout: int = 60,
    api_key: str | None = None,
    api_secret: str | None = None,
    require_auth: bool = False,
    idempotency_key: str = "",
) -> dict[str, Any]:
    body_text = json.dumps(payload, separators=(",", ":"))
    body = body_text.encode("utf-8")
    headers = {"Content-Type": "application/json", "Accept": "application/json"}

    resolved_key = resolve_api_key(api_key)
    resolved_secret = resolve_api_secret(api_secret)
    if require_auth:
        if not resolved_key or not resolved_secret:
            raise RuntimeError(
                "protected endpoint requires --api-key/--api-secret or "
                "MACBOT_API_KEY/MACBOT_API_SECRET"
            )
        headers.update(
            signed_headers("POST", url, body_text, resolved_key, resolved_secret, idempotency_key)
        )

    req = request.Request(url, data=body, headers=headers, method="POST")
    try:
        with open_api_request(req, timeout=timeout) as resp:
            raw = read_response(resp, label="API response").decode("utf-8")
    except error.HTTPError as exc:
        with exc:
            detail = read_response(exc, label=f"API HTTP{exc.code} error response").decode("utf-8", errors="replace").strip()
        suffix = f": {detail}" if detail else ""
        raise RuntimeError(f"request failed: HTTP {exc.code}{suffix}") from exc
    except error.URLError as exc:
        raise RuntimeError(f"request failed: {exc}") from exc

    try:
        out = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"invalid JSON response: {exc}") from exc

    if not isinstance(out, dict):
        raise RuntimeError("unexpected response shape")
    if "error" in out:
        raise RuntimeError(str(out["error"]))
    return out


def get_json(
    url: str,
    *,
    timeout: int = 60,
    api_key: str | None = None,
    api_secret: str | None = None,
    require_auth: bool = False,
    read_auth: bool = False,
) -> dict[str, Any]:
    headers = {"Accept": "application/json"}
    resolved_key = resolve_api_key(api_key)
    resolved_secret = resolve_api_secret(api_secret)

    if require_auth:
        if not resolved_key or not resolved_secret:
            raise RuntimeError(
                "protected endpoint requires --api-key/--api-secret or "
                "MACBOT_API_KEY/MACBOT_API_SECRET"
            )
        headers.update(signed_headers("GET", url, "", resolved_key, resolved_secret))
    elif read_auth:
        if not resolved_key:
            raise RuntimeError(
                "read-protected endpoint requires --api-key or MACBOT_API_KEY"
            )
        headers["x-api-key"] = resolved_key

    req = request.Request(url, headers=headers, method="GET")
    try:
        with open_api_request(req, timeout=timeout) as resp:
            raw = read_response(resp, label="API response").decode("utf-8")
    except error.HTTPError as exc:
        with exc:
            detail = read_response(exc, label=f"API HTTP{exc.code} error response").decode("utf-8", errors="replace").strip()
        suffix = f": {detail}" if detail else ""
        raise RuntimeError(f"request failed: HTTP {exc.code}{suffix}") from exc
    except error.URLError as exc:
        raise RuntimeError(f"request failed: {exc}") from exc

    try:
        out = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"invalid JSON response: {exc}") from exc

    if not isinstance(out, dict):
        raise RuntimeError("unexpected response shape")
    if "error" in out:
        raise RuntimeError(str(out["error"]))
    return out
