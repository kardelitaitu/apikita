#!/usr/bin/env python3
"""A tiny SSE origin for the relay behavioural check.

WHY A REAL ORIGIN AND A REAL RELAY, rather than grepping the config: the config check
proves the DIRECTIVES are present. This proves they DO WHAT THE DOC CLAIMS. The two
are different failures - a typo in a directive name passes a grep and still buffers -
and docs/edge-relay.md:110 makes the stakes explicit:

    "A relay that buffers SSE is worse than no relay - the UI silently stops
     updating."

An event every INTERVAL seconds, so the arrival TIMES distinguish the two cases:
streamed events arrive one per interval; buffered ones arrive together at the end.
"""
import http.server
import os
import time

INTERVAL = float(os.environ.get("SSE_INTERVAL_SECONDS", "1"))
COUNT = int(os.environ.get("SSE_EVENT_COUNT", "3"))
PORT = int(os.environ.get("SSE_PORT", "9099"))


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):  # noqa: N802 - the stdlib name
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        for i in range(COUNT):
            self.wfile.write(("data: event%d\n\n" % i).encode())
            self.wfile.flush()
            time.sleep(INTERVAL)

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    http.server.HTTPServer(("0.0.0.0", PORT), Handler).serve_forever()