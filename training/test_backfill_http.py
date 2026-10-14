"""Exercise real Requests redirect handling without any network transport."""
import contextlib
import io
import unittest
from unittest.mock import patch

try:
    import requests
    import backfill_historical as backfill
except ModuleNotFoundError as error:
    if error.name != "requests":
        raise
    backfill = None


@unittest.skipIf(backfill is None, "install requests to check authenticated backfill HTTP")
class BackfillHTTPTests(unittest.TestCase):
    def test_decoded_body_limit_and_response_cleanup(self):
        class TrackedResponse(requests.Response):
            def __init__(self, payload):
                super().__init__()
                self.status_code = 200
                self.raw = io.BytesIO(payload)
                self.was_closed = False

            def close(self):
                self.was_closed = True
                return super().close()

        for payload, oversized in ((b'{"value":0}', False), (b'x' * 33, True)):
            response = TrackedResponse(payload)
            with self.subTest(oversized=oversized), \
                 patch.object(backfill, 'MAX_RESPONSE_BYTES', 32), \
                 patch.object(backfill, 'require_safe_tls'), \
                 patch.object(backfill.requests, 'get', return_value=response) as transport:
                if oversized:
                    with self.assertRaisesRegex(RuntimeError, 'response limit'):
                        backfill._https_get(backfill.BINANCE_API)
                    self.assertTrue(response.raw.closed)
                else:
                    self.assertEqual(backfill._https_get(backfill.BINANCE_API).json(), {'value': 0})
                self.assertTrue(response.was_closed)
                self.assertTrue(transport.call_args.kwargs['stream'])
                self.assertFalse(transport.call_args.kwargs['allow_redirects'])

    def test_repeated_full_pages_stop_before_duplicate_page_is_retained(self):
        # Real collectors process two full pages. The second has the same final
        # timestamp and must stop rather than issue a third request forever.
        cases = [
            (lambda: backfill.fetch_binance_futures_klines(10, 20), [[10, 0, 0, 0, 0, 0, 12]] * 1000, 1000, 2),
            (lambda: backfill.fetch_binance_funding_rates(10, 20), [{'fundingTime': 12}] * 1000, 1000, 2),
            (lambda: backfill.fetch_binance_open_interest_hist(10, 20), [{'timestamp': 12}] * 500, 1, 2),
            (lambda: backfill.fetch_binance_futures_data_series('openInterestHist', 10, 20), [{'timestamp': 12}] * 500, 1, 2),
            # Liquidation tries its second fixed endpoint after rejecting the first.
            (lambda: backfill.fetch_binance_force_orders(10, 20), [{'time': 12}] * 1000, 0, 4),
        ]
        for run, page, kept, calls in cases:
            response = unittest.mock.Mock(status_code=200)
            response.json.return_value = page
            with self.subTest(collector=run), \
                 patch.object(backfill, '_https_get', side_effect=[response] * calls + [AssertionError('unexpected repeated request')]) as transport, \
                 patch.object(backfill.time, 'sleep'), \
                 patch.object(backfill, '_sign_params', side_effect=lambda params: params), \
                 contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(len(run()), kept)
                self.assertEqual(transport.call_count, calls)
                starts = [call.kwargs['params']['startTime'] for call in transport.call_args_list]
                self.assertEqual(starts, [10, 13] * (calls // 2))

    def test_missing_or_backwards_page_is_not_retained(self):
        for page in ([{}] * 500, [{'timestamp': 0}] * 500):
            response = unittest.mock.Mock(status_code=200)
            response.json.return_value = page
            with self.subTest(page=page[0]), \
                 patch.object(backfill, '_https_get', return_value=response) as transport, \
                 contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(backfill.fetch_binance_open_interest_hist(10, 20), [])
                self.assertEqual(transport.call_count, 1)

    def test_unsafe_tls_refused_before_requests_transport(self):
        import native_tls_runtime as native
        with patch.object(native.ssl, "OPENSSL_VERSION_INFO", (3, 6, 1, 0, 0)), \
             patch.object(native.ssl, "OPENSSL_VERSION", "OpenSSL 3.6.1"), \
             patch.dict(native.os.environ, {native.BACKPORT_ENV: ""}), \
             patch.object(backfill.requests, "get") as get:
            with self.assertRaisesRegex(RuntimeError, "No HTTPS request"):
                backfill._https_get(backfill.BINANCE_API)
            get.assert_not_called()

    def test_main_refuses_unsafe_tls_before_creating_database(self):
        import native_tls_runtime as native
        import sys
        with patch.object(native.ssl, "OPENSSL_VERSION_INFO", (3, 6, 1, 0, 0)), \
             patch.object(native.ssl, "OPENSSL_VERSION", "OpenSSL 3.6.1"), \
             patch.dict(native.os.environ, {native.BACKPORT_ENV: ""}), \
             patch.object(sys, "argv", ["backfill_historical.py", "--db", "must-not-exist.sqlite"]), \
             patch.object(backfill, "init_db") as database:
            with self.assertRaisesRegex(RuntimeError, "No HTTPS request"):
                backfill.main()
            database.assert_not_called()

    def test_authenticated_futures_endpoints_do_not_forward_cross_host_redirects(self):
        recorded = []

        class RedirectTransport(requests.adapters.BaseAdapter):
            def send(self, prepared, **kwargs):
                recorded.append(prepared)
                response = requests.Response()
                response.status_code = 302
                response.headers["Location"] = "https://other-host.invalid/collect"
                response.request = prepared
                response.url = prepared.url
                response._content = b""
                return response

            def close(self):
                pass

        calls = [
            lambda: backfill.fetch_binance_futures_klines(0, 1),
            lambda: backfill.fetch_binance_funding_rates(0, 1),
            lambda: backfill.fetch_binance_open_interest_hist(0, 1),
            lambda: backfill.fetch_binance_futures_data_series("globalLongShortAccountRatio", 0, 1),
            lambda: backfill.fetch_binance_force_orders(0, 1),
        ]
        for run in calls:
            recorded.clear()
            with self.subTest(endpoint=run), \
                 patch.object(backfill, "require_safe_tls"), \
                 patch("requests.sessions.HTTPAdapter", side_effect=lambda **kwargs: RedirectTransport()), \
                 patch.object(backfill, "_binance_headers", return_value={"X-MBX-APIKEY": "test-only-key"}), \
                 patch.object(backfill, "_sign_params", side_effect=lambda params: params), \
                 contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(run(), [])
            # The liquidation helper tries two fixed venue URLs; the others one.
            self.assertIn(len(recorded), (1, 2))
            for prepared in recorded:
                self.assertTrue(prepared.url.startswith(backfill.BINANCE_FUTURES_API + "/"))
                self.assertEqual(prepared.headers["X-MBX-APIKEY"], "test-only-key")


if __name__ == "__main__":
    unittest.main()
