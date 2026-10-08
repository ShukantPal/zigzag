import importlib
import http.server
import json
import os
import pathlib
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch


DEPT_DIR = pathlib.Path(__file__).parent
sys.path.insert(0, str(DEPT_DIR))
gate = importlib.import_module("approval_gate")


class ApprovalGateChecksTest(unittest.TestCase):
    def test_fetch_gate_delegates_the_complete_decision_to_the_daemon(self):
        expected = {"pass": True, "head": "a" * 40, "approvals": {}}
        with patch.object(gate, "relay_call", return_value=expected) as run:
            self.assertEqual(gate.fetch_gate("owner/repo", "17"), expected)
        run.assert_called_once_with(
            "/v1/review-gate?repository=owner%2Frepo&pull_request=17")
        self.assertEqual(
            gate.relay_proxy_url("http://127.0.0.1:3128"),
            "http://127.0.0.1:3130",
        )
        self.assertEqual(gate.relay_proxy_url(""), "")

    def test_fetch_gate_uses_the_authenticated_relay_transport_end_to_end(self):
        observed = {}

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                observed["path"] = self.path
                observed["authorization"] = self.headers.get("Authorization")
                body = json.dumps({"pass": True, "head": "a" * 40}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, _format, *_args):
                pass

        server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        with tempfile.NamedTemporaryFile("w", delete=False) as token_file:
            token_file.write("relay-secret\n")
            token_path = token_file.name
        try:
            with (
                patch.object(
                    gate,
                    "ZIGZAG_URL",
                    f"http://127.0.0.1:{server.server_port}",
                ),
                patch.object(gate, "ZIGZAG_TOKEN_FILE", token_path),
                patch.dict(os.environ, {"HTTPS_PROXY": ""}),
            ):
                result = gate.fetch_gate("owner/repo", "17")
            self.assertTrue(result["pass"])
            self.assertEqual(
                observed["path"],
                "/v1/review-gate?repository=owner%2Frepo&pull_request=17",
            )
            self.assertEqual(observed["authorization"], "Bearer relay-secret")
        finally:
            server.shutdown()
            server.server_close()
            worker.join(timeout=2)
            os.unlink(token_path)


if __name__ == "__main__":
    unittest.main()
