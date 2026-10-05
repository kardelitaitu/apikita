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
    # The timeout must EXCEED the server-block default of 60s.
    #
    # THIS CHECK USED TO MATCH ON SPELLING RATHER THAN VALUE, and the hole was measurable. It
    # rejected the literal strings `60s|30s|10s|5s` and accepted `[0-9]+[mh]`, so it never converted a
    # unit: MEASURED, `1m` - which IS 60 seconds, the very default it must outlast - PASSED, and
    # `0m` - a timeout of zero - passed too, while `90s`, real headroom, was REJECTED for not being in
    # minutes or hours. A guard that reads the unit and not the amount is the same shape as a
    # citation guard that reads a range's start and not its end.
    #
    # So the value is converted to SECONDS and compared. `s`, `m` and `h` are all accepted spellings;
    # what matters is that the number is greater than the 60s default.
    block=$(location_block "$LOC")
    if [ -n "$block" ]; then
        timeout_secs=$(printf '%s\n' "$block" \
            | sed -n 's/^[[:space:]]*proxy_read_timeout[[:space:]]\+\([0-9]\+\)\([smh]\?\);.*/\1 \2/p' \
            | head -n 1)
        if [ -z "$timeout_secs" ]; then
            fail "location $LOC has no proxy_read_timeout, so a live stream is cut at the 60s server-block default"
            continue
        fi
        amount=${timeout_secs%% *}
        unit=${timeout_secs##* }
        case "$unit" in
            h) seconds=$((amount * 3600)) ;;
            m) seconds=$((amount * 60)) ;;
            *) seconds=$amount ;;
        esac
        if [ "$seconds" -le 60 ]; then
            fail "location $LOC sets proxy_read_timeout to ${amount}${unit:-s} = ${seconds}s, which does not EXCEED the 60s server-block default; a live stream is cut mid-answer"
        fi
    fi
done

# ---------------------------------------------------------------------------
# The RATE-LIMIT structure. Until this existed, the file's flood protection had no guard at all.
# ---------------------------------------------------------------------------
#
# WHY IT IS HERE RATHER THAN LEFT TO THE CONFIG'S OWN COMMENT. `relay.conf` carries a long note
# explaining a bug that shipped and was fixed by hand, and that note is currently the ONLY thing
# preventing its return. A rule nothing checks is a rule that decays, and this defect is one line of
# plausible-looking YAML away: adding a location, or tidying the pairs below into a single directive,
# reintroduces it silently.
#
# THE BUG, measured in a real nginx and recorded in relay.conf:22-45. Every API location named
# `zone=perkey`, which is keyed on `$http_authorization`. Two independent facts compound:
#
#   1. `limit_req` is REPLACE-not-merge. A location naming a zone DISCARDS the `perip` directive it
#      would otherwise inherit from the server block, so the per-IP limit stops applying there.
#   2. nginx SKIPS a `limit_req` whose zone key evaluates to the EMPTY STRING. An unauthenticated
#      caller sends no `Authorization` header, so `perkey` never counted them.
#
# The two together meant the tier a FLOOD would use did not exist: /v1/ and /auth/login answered
# 8/8 requests of an unauthenticated burst while an authenticated caller was limited after 3. The
# fix is that every API location names BOTH zones - a per-IP zone for everyone and a per-key zone
# for callers who identify themselves.
#
# WHAT THIS CHECKS, stated as the falsifiable rule rather than as the fixed shape: any location
# carrying a directive whose zone is keyed on a header an anonymous caller does not send must ALSO
# carry a directive keyed on something they DO have (`$binary_remote_addr`).
#
# THE LOCATIONS ARE DISCOVERED, NOT LISTED. The first draft of this check hardcoded the five doors it
# knew about, and MEASURED: adding a SIXTH location carrying only `zone=perkey` passed it, because the
# loop never looked at that location. That is the same defect one level up, and this repository has
# the lesson written down already - `website/tests/citation-lines.test.ts` says of its own citation
# list that "a hand-kept list of nineteen citations would be the same defect this file is about, one
# level up: the twentieth citation would simply not be in it." So the check walks every `location`
# block in the file.
#
# The key-derived zone names ARE named explicitly, and that is the one thing discovery cannot supply:
# a zone is header-keyed because of how its `limit_req_zone` line is written, and reading that line is
# the check's job rather than its subject. The zone list is derived from the file's own declarations
# below, so a renamed or added zone is picked up rather than silently ignored.

# Every zone DEFINED over a request header. Read out of the file's own `limit_req_zone` lines rather
# than hardcoded: the key is the second whitespace-separated token of the directive.
header_zones=$(sed -n 's/^[[:space:]]*limit_req_zone[[:space:]]\+\(\$[A-Za-z_]*\)[[:space:]]\+zone=\([A-Za-z0-9_]*\).*/\1 \2/p' "$CONF" \
    | awk '$1 != "$binary_remote_addr" { printf "%s ", $2 }')
# And every zone defined over the peer address, which every caller has whether or not it identifies.
address_zones=$(sed -n 's/^[[:space:]]*limit_req_zone[[:space:]]\+\(\$[A-Za-z_]*\)[[:space:]]\+zone=\([A-Za-z0-9_]*\).*/\1 \2/p' "$CONF" \
    | awk '$1 == "$binary_remote_addr" { printf "%s ", $2 }')

if [ -z "$header_zones" ]; then
    fail "no limit_req_zone in $CONF is keyed on a request header, so the two-zone check below would examine nothing"
fi
if [ -z "$address_zones" ]; then
    fail "no limit_req_zone in $CONF is keyed on \$binary_remote_addr, so there is no per-IP tier for an unauthenticated caller to be bounded by"
fi

# Every location block in the file, by its name. The awk prints the `location` token of each opening
# line, so the loop below sees locations added later without anyone updating a list.
every_location() {
    awk '
        match($0, /^[[:space:]]*location[[:space:]]+[^[:space:]]+/) {
            line = $0
            sub(/^[[:space:]]*location[[:space:]]+/, "", line)
            sub(/[[:space:]]*\{.*$/, "", line)
            sub(/[[:space:]]*$/, "", line)
            print line
        }
    ' "$CONF"
}

for LOC in $(every_location); do
    block=$(location_block "$LOC")
    [ -n "$block" ] || continue

    # Does this location name a header-keyed zone?
    names_header_zone=""
    for z in $header_zones; do
        if printf '%s\n' "$block" | grep -qE "^[[:space:]]*limit_req[[:space:]]+zone=$z([[:space:]]|;)"; then
            names_header_zone="$z"
        fi
    done
    [ -n "$names_header_zone" ] || continue

    # It does. Then it MUST also name an address-keyed zone, or an unauthenticated caller is
    # unlimited - the exact defect.
    has_address_zone=""
    for z in $address_zones; do
        if printf '%s\n' "$block" | grep -qE "^[[:space:]]*limit_req[[:space:]]+zone=$z([[:space:]]|;)"; then
            has_address_zone="$z"
        fi
    done
    if [ -z "$has_address_zone" ]; then
        fail "location $LOC names zone=$names_header_zone but no per-IP zone. $names_header_zone is keyed on a request header, which an unauthenticated caller does not send, and nginx SKIPS a limit_req whose key is empty. With no address-keyed zone beside it, an anonymous flood of $LOC is UNLIMITED"
    fi
done

# THE POSITIVE CONTROL. A check that reports nothing because it read nothing looks identical to a
# check that passed. If no location in the whole file names a header-keyed zone, the loop above proved
# nothing - it may have been looking at a file that does not exist, or the zone may have been renamed.
if ! grep -qE "^[[:space:]]*limit_req[[:space:]]+zone=($(printf '%s' "$header_zones" | tr ' ' '|'))([[:space:]]|;)" "$CONF"; then
    fail "no location in $CONF names a header-keyed zone, so the two-zone check above examined nothing. Either the zone was renamed or the directives were removed - in both cases this check is now vacuous"
fi

# AND THE COUNT IS PINNED, because the rule above is conditional: it fires only where a header-keyed
# zone appears. Deleting every `zone=perkey` directive would satisfy it trivially while removing the
# per-key limit entirely. The floor is a TRIPWIRE rather than the current figure - five at the time of
# writing, in /events, /v1/, /auth/, /api/ and /webhooks/ - so adding a location does not fail this
# while removing one does.
header_directives=$(grep -cE "^[[:space:]]*limit_req[[:space:]]+zone=($(printf '%s' "$header_zones" | tr ' ' '|'))([[:space:]]|;)" "$CONF")
if [ "$header_directives" -lt 5 ]; then
    fail "only $header_directives limit_req directive(s) name a header-keyed zone; there were 5 in /events, /v1/, /auth/, /api/ and /webhooks/. A check that asserts 'where a per-key limit exists, an address-keyed one exists beside it' proves nothing once the per-key limits are gone"
fi

if [ "$FAILED" -ne 0 ]; then
    echo "relay-check: the relay contract is BROKEN. A buffering relay looks connected" >&2
    echo "relay-check: while the dashboard silently stops updating (docs/edge-relay.md:110)." >&2
    exit 1
fi

echo "relay-check: OK - /events and /v1/ are unbuffered, ungzipped, and outlast the default read timeout"
exit 0