"""Use the same native TLS policy when a training tool is run directly."""
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from native_tls_runtime import require_safe_tls  # noqa: E402,F401
from http_limits import MAX_RESPONSE_BYTES, read_response  # noqa: E402,F401
from api_auth import open_api_request  # noqa: E402,F401
