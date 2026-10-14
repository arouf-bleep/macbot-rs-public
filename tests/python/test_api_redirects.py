"""Real urllib redirect processing must not forward protected API headers."""
from email.message import Message
import io
import unittest
from unittest.mock import patch
from urllib import request, response

from api_auth import get_json, post_json, NoAPIRedirects


class APIRedirectTests(unittest.TestCase):
    def test_read_and_signed_post_do_not_forward_headers(self):
        received = []
        initial = []

        class RedirectTransport(request.HTTPHandler):
            def http_open(self, req):
                headers = Message()
                if req.host == 'initial.invalid':
                    initial.append(dict(req.header_items()))
                    headers['Location'] = 'http://destination.invalid/other-origin'
                    result = response.addinfourl(io.BytesIO(b''), headers, req.full_url, 302)
                    result.msg = 'Found'
                else:
                    received.append(dict(req.header_items()))
                    result = response.addinfourl(io.BytesIO(b'{}'), headers, req.full_url, 200)
                    result.msg = 'OK'
                return result

        # Execute urllib's real HTTPErrorProcessor and redirect handler with an
        # in-memory transport, avoiding both network access and socket binding.
        opener = request.build_opener(request.ProxyHandler({}), RedirectTransport(), NoAPIRedirects())
        with patch('api_auth.request.build_opener', return_value=opener):
            with self.assertRaisesRegex(RuntimeError, 'HTTP 302'):
                get_json('http://initial.invalid', api_key='read-test-key', read_auth=True)
            with self.assertRaisesRegex(RuntimeError, 'HTTP 302'):
                post_json('http://initial.invalid', {'value': 1}, api_key='write-test-key',
                          api_secret='write-test-secret', require_auth=True)
        self.assertEqual(len(initial), 2)
        self.assertEqual(initial[0]['X-api-key'], 'read-test-key')
        self.assertIn('X-api-signature', initial[1])
        self.assertEqual(received, [], 'redirect destination must see no request or credentials')


if __name__ == '__main__':
    unittest.main()
