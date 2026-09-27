#!/bin/sh
# apikita relay contract check - the SSE directives that keep the dashboard LIVE.
#
# WHY THIS EXISTS. docs/edge-relay.md:110 states the stakes:
#   "A relay that buffers SSE is worse than no relay - the UI silently stops
#    updating."
# The failure is SILENT: the stream looks connected, the HTTP status is 200, and the
# dashboard simply freezes on the last value. Nothing in CI looked at this file at
# all - a syntax error, or a deleted proxy_buffering line, would reach production.
#
# WHAT IT CHECKS, and why each one:
#   proxy_buffering off        THE line. On, nginx accumulates the stream and
#                              delivers it in bursts (or at the end).
#   proxy_read_timeout > 60s   the server block sets 60s as the default; a live SSE
#                              stream must outlast it or nginx cuts it mid-answer.
#   gzip off                   gzip buffers, even when proxy_buffering is off.
#   chunked_transfer_encoding off  nginx would otherwise re-chunk a stream it is
#                              passing through, which can delay the first byte.
#
# It checks BOTH streaming locations. /v1/ streams token deltas and needs the SAME
# treatment as /events - a buffered token stream is a truncated-looking answer.
#
# Usage:
#   sh tools/relay-check/check.sh [path/to/relay.conf]
# Exit: 0 = contract holds, 1 = a directive is missing or wrong, 2 = usage.

set -u

CONF="${1:-$(dirname -- "$0")/../../.docker/nginx/relay.conf}"

if [ ! -f "$CONF" ]; then
    echo "relay-check: no such config: $CONF" >&2
    exit 2
fi

FAILED=0

fail() {
    echo "relay-check: FAIL - $1" >&2
    FAILED=1
}

# Extract one location block by name, from "location NAME {" to the closing brace at
# the same indent. awk over the text is enough: the file is flat and hand-written,
# and a real parser would be a dependency this check deliberately does not have.
location_block() {
    awk -v want="$1" '
        $0 ~ "^[[:space:]]*location[[:space:]]+" want "[[:space:]]*\\{" { inside=1 }
        inside { print }
        inside && /^[[:space:]]*\}/ { exit }
    ' "$CONF"
}

# assert_directive <location> <directive-regex> <human description>
assert_directive() {
    loc="$1"; re="$2"; desc="$3"
    block=$(location_block "$loc")
    if [ -z "$block" ]; then
        fail "location $loc is MISSING from $CONF ($desc)"
        return
    fi
    if ! printf '%s\n' "$block" | grep -qE "$re"; then
        fail "location $loc does not set $desc"
    fi
}

for LOC in "/events" "/v1/"; do
    assert_directive "$LOC" '^[[:space:]]*proxy_buffering[[:space:]]+off;' "proxy_buffering off"
    assert_directive "$LOC" '^[[:space:]]*gzip[[:space:]]+off;' "gzip off"
    assert_directive "$LOC" '^[[:space:]]*chunked_transfer_encoding[[:space:]]+off;' "chunked_transfer_encoding off"
    # The timeout must EXCEED the server-block default of 60s. `1h` is what ships;
    # rather than accept only that spelling, reject the known-bad defaults.
    block=$(location_block "$LOC")
    if [ -n "$block" ]; then
        if printf '%s\n' "$block" | grep -qE '^[[:space:]]*proxy_read_timeout[[:space:]]+(60s|30s|10s|5s);'; then
            fail "location $LOC sets proxy_read_timeout to a short default; a live stream must outlast the 60s server-block default"
        elif ! printf '%s\n' "$block" | grep -qE '^[[:space:]]*proxy_read_timeout[[:space:]]+[0-9]+[mh];'; then
            fail "location $LOC has no proxy_read_timeout in minutes or hours"
        fi
    fi
done

if [ "$FAILED" -ne 0 ]; then
    echo "relay-check: the relay contract is BROKEN. A buffering relay looks connected" >&2
    echo "relay-check: while the dashboard silently stops updating (docs/edge-relay.md:110)." >&2
    exit 1
fi

echo "relay-check: OK - /events and /v1/ are unbuffered, ungzipped, and outlast the default read timeout"
exit 0