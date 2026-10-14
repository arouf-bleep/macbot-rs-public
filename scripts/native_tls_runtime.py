"""Check the interpreter's native TLS library before HTTPS requests.

Package pins do not replace CPython's OpenSSL. These maintained branch minima
include the 29 September 2026 fixes, including CVE-2026-35189 certificate-memory
DoS: https://openssl-library.org/news/vulnerabilities-3.5/index.html
"""
from __future__ import annotations

import json
import os
from pathlib import Path
import re
import ssl
from urllib.parse import urlsplit

PATCHED_BRANCHES = {(3, 4): 8, (3, 5): 9, (3, 6): 5, (4, 0): 3}
BACKPORT_ENV = 'MACBOT_OPENSSL_BACKPORT_EVIDENCE'


def tls_requirement_error() -> str | None:
    # For OpenSSL3+, CPython exposes (major,minor,fix,patch,status), e.g.
    # (3,5,0,9,0) for3.5.9. The first three tuple fields are not semver.
    match = re.match(r'^OpenSSL (\d+)\.(\d+)\.(\d+)(?:\s|$)', ssl.OPENSSL_VERSION)
    version = tuple(map(int, match.groups())) if match else (0, 0, 0)
    minimum = PATCHED_BRANCHES.get(version[:2])
    if ssl.OPENSSL_VERSION.startswith('OpenSSL ') and minimum is not None and version[2] >= minimum:
        return None
    evidence_path = os.environ.get(BACKPORT_ENV)
    if evidence_path:
        try:
            path = Path(evidence_path).expanduser()
            if not path.is_file() or path.stat().st_size > 64 * 1024:
                raise ValueError('evidence must be a small JSON file')
            evidence = json.loads(path.read_text())
            if (not isinstance(evidence, dict)
                    or evidence.get('openssl_version') != ssl.OPENSSL_VERSION
                    or evidence.get('cve') != 'CVE-2026-35189'
                    or not isinstance(evidence.get('source'), str)
                    or urlsplit(evidence['source']).scheme != 'https'
                    or not urlsplit(evidence['source']).hostname
                    or not isinstance(evidence.get('reason'), str)
                    or not evidence['reason'].strip()):
                raise ValueError('evidence must identify this exact runtime, CVE, HTTPS distributor source and reason')
            # This is an explicit operator assertion, not automatic proof that
            # an arbitrary distributor has backported the fixes.
            return None
        except (OSError, ValueError, TypeError) as error:
            return f'Invalid {BACKPORT_ENV}: {error}'
    return (
        f'Python uses {ssl.OPENSSL_VERSION}; HTTPS requires a reviewed patched '
        'OpenSSL branch (3.4.8+, 3.5.9+, 3.6.5+ or 4.0.3+). Upgrade the native '
        'TLS runtime, or provide exact distributor backport evidence using '
        f'{BACKPORT_ENV}. No HTTPS request was opened.'
    )


def require_safe_tls(url: str) -> None:
    if urlsplit(url).scheme.lower() == 'https':
        error = tls_requirement_error()
        if error:
            raise RuntimeError(error)
