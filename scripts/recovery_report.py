#!/usr/bin/env python3
"""Save authenticated, read-only recovery evidence from a running bot."""
from __future__ import annotations
import argparse
import json
import os
from pathlib import Path
from urllib.parse import quote
from api_auth import get_json


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--base-url', default='http://127.0.0.1:8000')
    parser.add_argument('--order-id', help='Include one order, its fills, lots and audit records')
    parser.add_argument('--out', type=Path, required=True, help='New private JSON evidence file')
    args = parser.parse_args()
    route = '/api/recovery' if not args.order_id else '/api/recovery/orders/' + quote(args.order_id, safe='')
    payload = get_json(args.base_url.rstrip('/') + route,
                       api_key=os.environ.get('MACBOT_API_KEY', ''), read_auth=True)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    # Exclusive creation prevents losing earlier evidence; mode 0600 keeps
    # wallet identities and private trading history out of other user accounts.
    fd = os.open(args.out, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, 'w') as stream:
        json.dump(payload, stream, indent=2)
        stream.write('\n')
    print(f'Recovery evidence saved: {args.out}. This is local ledger evidence, not venue verification.')
    return 0


if __name__ == '__main__': raise SystemExit(main())
