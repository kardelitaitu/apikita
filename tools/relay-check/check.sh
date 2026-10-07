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

# ---------------------------------------------------------------------------
# The TIER TABLE in docs/edge-relay.md, against the zones the config declares.
# ---------------------------------------------------------------------------
#
# WHY. The doc's tier table is what a reader consults to know the relay's limits, and it is the table
# the last commit EDITED - adding the webhook row. Nothing compared it to the config, and MEASURED:
# changing the doc's `60 burst` to `99`, or the config's `rate=30r/s` to `rate=3r/s`, failed NO gate.
# A tier table that disagrees with the file it describes is worse than no table, because it is the
# thing a reader trusts - `docs/testing.md` makes the same argument about a guard that overstates its
# own scope.
#
# WHAT IT COMPARES, narrowly: for each zone the config declares, the doc must state that zone's rate
# and burst. The comparison is on the NUMBERS, not the prose - a reader may reword the row freely, but
# a row whose number no longer matches the config fails here.
#
# The zone-to-row mapping is by the zone's own rate, which is what makes each row identifiable: the
# doc writes `30 req/s`, `100 req/s` and `200 req/s` and the config declares exactly those rates.
DOC_CONF="$(dirname -- "$CONF")/../../docs/edge-relay.md"
if [ -f "$DOC_CONF" ]; then
    # THE POSITIVE CONTROL COMES FIRST, and it is the lesson from this block's own first draft.
    #
    # `grep -qE` exits 2 on a MALFORMED pattern, and `! grep -qE` reads any non-zero exit - including
    # 2 - as "no match". A pattern with a syntax error therefore made this block report a violation
    # or, in the burst case, silently skip: the first draft used `(\*\*)?`, which is invalid POSIX
    # ERE, and MEASURED the block exited 0 while comparing nothing at all. So the pattern is proven
    # valid before it is trusted, and a comparison that cannot run is an ERROR rather than a pass.
    if ! printf '%s\n' 'probe' | grep -qE '[[:space:]]*probe' 2>/dev/null; then
        fail "grep -E is not usable, so every comparison in this block would fail open"
    fi

    # `burst=` lives on the limit_req DIRECTIVE, not on the limit_req_zone DECLARATION. The first
    # draft read it from the declaration line, where it never appears, so every zone was skipped by
    # the `[ -n "$burst" ] || continue` guard below and the comparison ran zero times. MEASURED: the
    # whole block exited 0 on a doc with every burst figure removed.
    #
    # WHAT THIS CANNOT SEE, stated because a reader who believes otherwise will not look for it. The
    # comparison asserts each config figure APPEARS somewhere in the doc. It does not assert the TABLE
    # ROW is the only place, and it could not: this doc states `30 req/s` three times by design - the
    # tier row, the sentence contrasting the webhook's 200 req/s with it, and the rationale bullet. A
    # row corrupted to `3 req/s` while the prose still says `30 req/s` therefore passes. MEASURED.
    #
    # That is the boundary `server/src/doc_claims.rs` describes about its own citation guard: it
    # catches the drift that actually happens, not a number that was wrong when written. Deleting a
    # figure, or letting the config and the doc disagree about which number exists at all, is caught;
    # editing one mention of a number that is repeated is not.
    #
    # So each zone's figures are read from the directives that name it, and a zone used with TWO
    # DIFFERENT figures is reported rather than silently taking the first.
    #
    # MEASURED, and the paragraph above used to claim this was already true: it is not. The first
    # version read `... | sort -u | head -n 1`, and `sort -u` does not help - two DIFFERENT values
    # survive it and `head -n 1` then discards one. MEASURED: a second `limit_req zone=perwh
    # burst=7` alongside the real `burst=1000` left this gate at exit 0, and so did a second
    # `limit_req_zone ... zone=perwh ... rate=5r/s` alongside `rate=200r/s`.
    #
    # The floor below could not have caught either: it counts COMPARISONS, and a duplicate ADDS one
    # rather than removing any. A count of how many times a loop ran cannot see a wrong value flowing
    # through it.
    #
    # Both readers now return EVERY distinct value, and the caller fails if there is more than one.
    # That is the check the paragraph promised and the code did not do.
    zone_burst() { # zone_burst <zone> -> every distinct burst, one per line
        sed -n "s/^[[:space:]]*limit_req[[:space:]]\+zone=$1[[:space:]]\+burst=\([0-9]\+\).*/\1/p" "$CONF" | sort -u
    }
    zone_rate() { # zone_rate <zone> -> every distinct declared rate, one per line
        sed -n "s/.*zone=$1:[0-9a-z]*[[:space:]]\+rate=\([0-9]\+\)r\/s.*/\1/p" "$CONF" | sort -u
    }

    # A zone whose figures are stated more than once, differently, is a config that disagrees with
    # itself - and the doc comparison below could only ever check one of the two.
    ambiguous() { # ambiguous <zone> <label> <values...>
        z=$1; label=$2; shift 2
        n=$(printf '%s\n' "$@" | grep -c . || true)
        if [ "$n" -gt 1 ]; then
            fail "the relay config gives zone=$z more than one $label ($(printf '%s ' "$@" | sed 's/ $//')). The doc comparison below can check only one of them, so a config that states two different figures is a config this gate cannot verify - settle on one."
        fi
    }

    compared=0
    for z in $header_zones $address_zones; do
        # EVERY distinct value, then a single one is what the comparison may use.
        rates=$(zone_rate "$z")
        ambiguous "$z" "rate" $rates
        rate=$(printf '%s\n' "$rates" | grep . | head -n 1 || true)
        if [ -n "$rate" ]; then
            compared=$((compared + 1))
            if ! grep -qE "(^|[^0-9])${rate}[[:space:]]*req/s" "$DOC_CONF"; then
                fail "the config declares zone=$z at rate=${rate}r/s, and $DOC_CONF states no '${rate} req/s' tier. The doc's tier table is the thing a reader consults, so a rate that is in one and not the other is a limit the reader will get wrong"
            fi
        fi

        # AND THE BURST, paired with the word `burst`. That pairing is what makes `<n> burst` a burst
        # figure rather than an incidental number: the tier table's cells are bold, and the first
        # draft's alternative `\*\*[^*]*<n>[^*]*\*\*` matched ANY bold run containing the number, so
        # `**30 req/s sustained, 60 burst**` satisfied it for 60 whatever the row said. MEASURED:
        # rewriting every burst figure in the doc to the words "many bursts" left that version at
        # exit 0.
        bursts=$(zone_burst "$z")
        ambiguous "$z" "burst" $bursts
        burst=$(printf '%s\n' "$bursts" | grep . | head -n 1 || true)
        if [ -n "$burst" ]; then
            compared=$((compared + 1))
            if ! grep -qE "(^|[^0-9])${burst}[[:space:]]*\*{0,2}[[:space:]]*burst" "$DOC_CONF"; then
                fail "the config uses zone=$z with burst=$burst, and $DOC_CONF states no '${burst} burst' figure. A doc that names the rate but not the burst describes a limit half the size of the real one"
            fi
        fi
    done

    # THE OTHER HALF OF THE POSITIVE CONTROL: the loop above must have compared something. If the zone
    # lists came back empty - a renamed key variable, a reformatted declaration - it would run zero
    # times and report nothing while printing the same OK as a real pass.
    #
    # A FLOOR WITH SLACK, not one AT the measured value. This read `-lt 6` and justified itself with
    # "there are three zones with a rate and a burst" - arithmetically true, and a TRIPWIRE, which is
    # the failure `doc_claims.rs` names about its own floors: an exact floor "passes today, fails the
    # moment one file is deleted, and cannot tell a whole-crate walk from a narrowed one that happens
    # to reach the same number". MEASURED: removing the ONE `burst=` this config gives `perwh` - a
    # legitimate edit that takes the comparison to 5 - made this gate FAIL on an otherwise-correct
    # tree, printing the sentence above it as if the config contradicted itself. The message then
    # asserts "there are three zones", which is false in exactly the case that trips it.
    #
    # The floor's JOB is to catch a loop that ran almost nothing, and that failure looks like 0 or 1,
    # not 5. So the bound is set below what the config could plausibly declare: two zones would give
    # four comparisons, one gives two, and a parser that broke gives zero - all still caught - while a
    # config that drops a zone no longer fails a check about the DOC.
    if [ "$compared" -lt 4 ]; then
        fail "the tier-table comparison ran $compared time(s), which is too few to be a comparison of the whole table. A working parse of this config compares a rate and a burst per declared zone; zero or one comparison means the loop found no zones at all, which is how this block fails - and it fails while printing the same OK as a real pass"
    fi

    # And the doc must still contain the table. A doc that lost its rate figures would make every
    # `grep` above fail - which is correct - but a doc that lost the TABLE while keeping three rates
    # in prose would pass vacuously, so the heading is required too.
    if ! grep -qE '^\|[[:space:]]*Tier' "$DOC_CONF"; then
        fail "$DOC_CONF has no '| Tier' table header, so the tier table this block compares against is gone"
    fi
fi

if [ "$FAILED" -ne 0 ]; then
    echo "relay-check: the relay contract is BROKEN. A buffering relay looks connected" >&2
    echo "relay-check: while the dashboard silently stops updating (docs/edge-relay.md:110)." >&2
    exit 1
fi

echo "relay-check: OK - /events and /v1/ are unbuffered, ungzipped, and outlast the default read timeout"
exit 0