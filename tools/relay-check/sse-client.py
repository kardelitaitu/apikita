#!/usr/bin/env python3
"""Times the arrival of each SSE event through a host:port, and reports the SPREAD.

The spread is the signal. Streamed events arrive spread apart (one per interval);
buffered events arrive with a spread near ZERO, because nginx delivers them in one
burst at the end. A test that only asked "did the events arrive" would pass on both -
which is exactly the trap this client exists to avoid.

Usage: client.py <host> <port> [expected_count]
Exit: 0 if the events arrived SPREAD (streamed), 1 if they arrived batched (buffered).
"""
import socket
import sys
import time

host = sys.argv[1]
port = int(sys.argv[2])
expected = int(sys.argv[3]) if len(sys.argv) > 3 else 3

sock = socket.create_connection((host, port), timeout=20)
sock.sendall(b"GET /events HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\n\r\n")

start = time.time()
arrivals = {}
buffer = b""
deadline = start + 30

while time.time() < deadline and len(arrivals) < expected:
    try:
        chunk = sock.recv(4096)
    except socket.timeout:
        break
    if not chunk:
        break
    buffer += chunk
    for i in range(expected):
        marker = ("event%d" % i).encode()
        if marker in buffer and i not in arrivals:
            arrivals[i] = time.time() - start

if len(arrivals) < expected:
    print("client: only %d of %d events arrived" % (len(arrivals), expected))
    sys.exit(1)

times = [arrivals[i] for i in range(expected)]
spread = max(times) - min(times)
print("client: arrivals " + ", ".join("+%.2fs" % t for t in times))
print("client: spread %.2fs" % spread)

# A spread well under the origin's interval means everything landed together: the
# relay buffered a stream it was supposed to pass through. Half the interval is a
# generous threshold that no streamed run can trip.
INTERVAL = 1.0
if spread < INTERVAL / 2:
    print("client: BUFFERED - the events arrived in one burst, so the dashboard would")
    print("client:   have shown nothing and then jumped (docs/edge-relay.md:110).")
    sys.exit(1)

print("client: STREAMED - the events arrived spread apart, which is the contract")
sys.exit(0)