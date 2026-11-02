#!/usr/bin/env python3
"""Minimal webhook receiver for tests: appends each delivery to a JSONL file with
the headers that matter and whether the HMAC signature verifies.
Usage: hook_receiver.py <port> <secret> <out.jsonl>"""
import hashlib, hmac, http.server, json, sys

port, secret, out = int(sys.argv[1]), sys.argv[2].encode(), sys.argv[3]

class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length", 0)))
        want = "sha256=" + hmac.new(secret, body, hashlib.sha256).hexdigest()
        rec = {"event": self.headers.get("x-cambium-event"), "delivery": self.headers.get("x-cambium-delivery"),
               "signature_ok": hmac.compare_digest(want, self.headers.get("x-cambium-signature", "")),
               "payload": json.loads(body)}
        with open(out, "a") as f:
            f.write(json.dumps(rec) + "\n")
        self.send_response(204)
        self.end_headers()
    def log_message(self, *a):
        pass

http.server.HTTPServer(("127.0.0.1", port), H).serve_forever()
