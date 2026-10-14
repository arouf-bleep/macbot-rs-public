"""Oversized actual urllib bodies stop before JSON parsing, including errors."""
from email.message import Message
import io
import unittest
from unittest.mock import patch
from urllib import request, response

import api_auth
from http_limits import MAX_RESPONSE_BYTES
import reconcile_dashboard as reconcile
import verify_trade_truth as truth


class TrackedBody(io.BytesIO):
    def __init__(self, data):
        super().__init__(data)
        self.read_sizes = []
        self.line_sizes = []

    def read(self, size=-1):
        self.read_sizes.append(size)
        return super().read(size)

    def readline(self, size=-1):
        self.line_sizes.append(size)
        return super().readline(size)


class HTTPLimitTests(unittest.TestCase):
    def test_api_success_and_error_are_bounded_before_json(self):
        for code in (200, 503):
            for method in ('get', 'post'):
                body = TrackedBody(b'x' * (MAX_RESPONSE_BYTES + 1))

                class Transport(request.HTTPHandler):
                    def http_open(self, req):
                        result = response.addinfourl(body, Message(), req.full_url, code)
                        result.msg = 'OK' if code == 200 else 'Unavailable'
                        return result

                opener = request.build_opener(request.ProxyHandler({}), Transport(), api_auth.NoAPIRedirects())
                with self.subTest(status=code, method=method), \
                     patch('api_auth.request.build_opener', return_value=opener), \
                     patch('api_auth.json.loads') as parse:
                    with self.assertRaisesRegex(RuntimeError, 'response limit'):
                        if method == 'get':
                            api_auth.get_json('http://test.invalid')
                        else:
                            api_auth.post_json('http://test.invalid', {})
                    parse.assert_not_called()
                self.assertEqual(body.read_sizes, [MAX_RESPONSE_BYTES + 1])
                self.assertTrue(body.closed)

    def test_public_evidence_body_is_bounded(self):
        body = TrackedBody(b'x' * (MAX_RESPONSE_BYTES + 1))
        with patch.object(truth, 'open_api_request', return_value=body), \
             patch.object(truth, 'require_safe_tls'):
            with self.assertRaisesRegex(RuntimeError, 'response limit'):
                truth.http_json('https://test.invalid')
        self.assertEqual(body.read_sizes, [MAX_RESPONSE_BYTES + 1])

    def test_stream_frame_and_cumulative_noise_are_bounded(self):
        for data in (b'x' * (MAX_RESPONSE_BYTES + 1), b': noise\n' * ((MAX_RESPONSE_BYTES // 8) + 1)):
            body = TrackedBody(data)
            with self.subTest(kind=data[:8]), patch.object(reconcile, 'open_api_request', return_value=body):
                with self.assertRaisesRegex(RuntimeError, 'frame/read limit'):
                    reconcile.one_stream_payload('http://test.invalid', 'test-key')
            self.assertTrue(body.line_sizes)
            self.assertTrue(all(size == MAX_RESPONSE_BYTES + 1 for size in body.line_sizes))

    def test_valid_small_stream_frame_preserves_zero(self):
        body = TrackedBody(b': connected\ndata: {"kind":"full","value":0}\n\n')
        with patch.object(reconcile, 'open_api_request', return_value=body):
            self.assertEqual(reconcile.one_stream_payload('http://test.invalid', 'test-key')['value'], 0)
