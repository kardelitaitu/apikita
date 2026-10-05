#!/bin/sh
# apikita compose contract check.
#
# WHY THIS EXISTS. docker-compose.yml is the DEPLOYMENT DEFINITION - the file a new
# operator reads first and `docker compose up -d` obeys. Nothing in CI validated it: a
# search for "compose" in the workflow found only comments. The nginx README told
# operators to run `docker compose config` BY HAND, which is the manual-checklist
# pattern that lets a machine-breaking edit ship.
#
# WHY IT ASSERTS MORE THAN SYNTAX. `docker compose config` proves the YAML parses. It
# cannot know that a mount is read-only when it must be writable, or that two
# variables the file says MUST stay separate were collapsed. Those are the invariants
# the file documents as load-bearing, and a config that parses can still be wrong -
# in ways that fail at 03:00 in production rather than at review time.
#
# Each assertion below quotes the comment it enforces, so a future editor is arguing
# with the reasoning rather than with this script.
#
# Usage: sh tools/compose-check/check.sh [path/to/docker-compose.yml]
# Exit: 0 = contract holds, 1 = a violation, 2 = usage, 3 = docker unavailable.

MSYS_NO_PATHCONV=1
export MSYS_NO_PATHCONV

set -u

# A DRIVE-LETTER path for the file handed to the daemon. The MSYS /c/... form is
# rewritten to C:\c\... and the file "cannot be found" - the same trap the relay
# check documents, found the same way.
native_path() {
    if command -v cygpath >/dev/null 2>&1; then
        cygpath -m "$1"
    else
        printf '%s' "$1"
    fi
}

REPO=$(cd -- "$(dirname -- "$0")/../.." && pwd)
COMPOSE=$(native_path "${1:-$REPO/docker-compose.yml}")

[ -f "$COMPOSE" ] || { echo "compose-check: no such file: $COMPOSE" >&2; exit 2; }

command -v docker >/dev/null 2>&1 || {
    echo "compose-check: SKIPPED - docker is not on PATH, so compose was NOT validated" >&2
    exit 3
}

RENDERED=$(docker compose -f "$COMPOSE" config 2>&1) || {
    echo "compose-check: FAIL - docker compose config rejected $COMPOSE:" >&2
    printf '%s\n' "$RENDERED" >&2
    exit 1
}

FAILED=0
fail() { echo "compose-check: FAIL - $1" >&2; FAILED=1; }

# 1. THE SCHEDULER'S WORKING DIRECTORY, which its own comment calls load-bearing:
#    "a relative sqlite:// path must resolve to the SAME file for both jobs... any
#     other value makes reconciliation look for /data/server.db, report exit 6, and
#     the Gate 2 money check SILENTLY never runs against the real database."
if ! printf '%s\n' "$RENDERED" | grep -q "working_dir: /srv/apikita/server"; then
    fail "the scheduler working_dir is not /srv/apikita/server; reconcile.sh resolves a relative DSN against its CWD, so the Gate 2 money check would silently run against nothing"
fi

# 2. THE TWO DSNs STAY SEPARATE. The file says: "Deliberately a SEPARATE variable, not
#    a copy of DATABASE_URL: it is what lets a bad DSN disarm the reconciliation job
#    alone without taking the retention sweep down with it." If they were collapsed,
#    one bad value would disarm BOTH money-critical jobs at once.
DB=$(printf '%s\n' "$RENDERED" | grep -c "DATABASE_URL: sqlite://data/server.db" || true)
RDB=$(printf '%s\n' "$RENDERED" | grep -c "RECONCILE_DATABASE_URL: sqlite://data/server.db" || true)
if [ "$DB" -lt 1 ] || [ "$RDB" -lt 1 ]; then
    fail "DATABASE_URL and RECONCILE_DATABASE_URL are not both set to the data file; collapsing them means one bad DSN disarms BOTH the reconciliation and the retention job"
fi

# 3. THE DATA MOUNT IS WRITABLE, and the two REPO mounts are read-only. The file:
#    "NOT :ro, unlike the two mounts above, and that is not an oversight: the
#     retention job is a DELETE and a DELETE against a read-only mount fails on every
#     host... The two repo mounts above stay :ro: what this service must never write
#     is the repo's source, not the data directory."
if ! printf '%s\n' "$RENDERED" | grep -q "target: /srv/apikita/server/data"; then
    fail "the scheduler does not mount /srv/apikita/server/data; without it the container cannot see the database and the service is permanently unable to succeed"
elif printf '%s\n' "$RENDERED" | grep -A2 "target: /srv/apikita/server/data" | grep -q "read_only: true"; then
    # The file is explicit that this mount must NOT be :ro - "the retention job is a
    # DELETE and a DELETE against a read-only mount fails on every host... The promise
    # this service exists to keep would then never be kept." A silent read-only mount
    # is the worst version of this bug: every job reports a failure nobody wired an
    # alert to, and the retention promise is breached on schedule.
    fail "the scheduler's DATA mount is read-only; the retention job is a DELETE and a read-only mount fails on every host, so the retention promise would never be kept"
fi
# The entrypoint and reconcile mounts must BOTH be read-only.
for target in /usr/local/bin/maintenance-entrypoint.sh /usr/local/share/reconcile; do
    if printf '%s\n' "$RENDERED" | grep -q "target: $target"; then
        if ! printf '%s\n' "$RENDERED" | grep -A3 "target: $target" | grep -q "read_only: true"; then
            fail "the mount at $target is not read-only; this service must never write the repo's source"
        fi
    fi
done

# 4. THE RELAY CONF LANDS WHERE NGINX READS IT. nginx loads conf.d/*.conf, so a mount
#    at any other path leaves the relay running its DEFAULT config - which buffers SSE
#    (tools/relay-check/). The composed file is verified by the relay check; this only
#    asserts it is actually mounted into the container.
if ! printf '%s\n' "$RENDERED" | grep -q "target: /etc/nginx/conf.d/default.conf"; then
    fail "the relay config is not mounted at /etc/nginx/conf.d/default.conf; nginx would run its default config, which buffers SSE"
fi

# 5. NO /healthcheck ON THE SCHEDULER - it "serves nothing and listens on nothing, so
#    any probe would be theatre". A probe here would report unhealthy forever.
if printf '%s\n' "$RENDERED" | awk '/apikita-scheduler/{f=1} f&&/healthcheck:/{print;exit}' | grep -q healthcheck; then
    fail "the scheduler has a healthcheck; it serves nothing and listens on nothing, so the probe would be theatre that reports unhealthy forever"
fi

# 6. EVERY `docker compose up` IN THE DOCS NAMES A SERVICE, OR THE DOC SAYS IT MEANS ALL.
#
# WHY. `docker compose up -d` with NO service starts EVERY service in the file. That was a
# one-service file when `docs/local-development.md` told a front-end session to run it and said the
# step "brings up nginx alone". The maintenance `scheduler` service was added later, so the line
# silently started a nightly container too, and the sentence explaining it became false.
#
# FOUND BY RUNNING IT, not by reading: `docker compose up -d` on the shipped file brings up
# `nginx` AND `scheduler`, and the doc's promise is the reason a reader would not look. I caused
# exactly this while verifying it - the bare command took down a running container - so the guard is
# for the person who follows the doc without thinking about it, which is the point of a doc.
#
# THE RULE: a bare `docker compose up` is allowed ONLY where the surrounding text says it starts
# everything. Otherwise the service must be named. This is checked against the SERVICE LIST the
# file actually declares, so adding a service cannot make a naming line wrong - only a bare line
# wrong, which is the direction that matters.
if [ -f "$REPO/docs/local-development.md" ]; then
    SERVICES=$(printf '%s\n' "$RENDERED" | sed -n 's/^  \([a-z][a-z0-9_-]*\):$/\1/p')
    svc_count=$(printf '%s\n' "$SERVICES" | grep -c . || true)
    # Every line that invokes `docker compose up`.
    grep -nE 'docker compose up' "$REPO/docs/local-development.md" > /tmp/composeup.$$ 2>/dev/null || true
    while IFS= read -r hit; do
        [ -z "$hit" ] && continue
        lineno=${hit%%:*}
        text=${hit#*:}
        # A service is named if a token follows `up` that is not a flag and is a declared service.
        named=""
        for s in $SERVICES; do
            case "$text" in
                *"up -d $s"*|*"up $s"*) named="$s" ;;
            esac
        done
        if [ -z "$named" ]; then
            # Bare form: allowed only if the nearby text says it covers all services.
            if ! sed -n "$((lineno > 3 ? lineno - 3 : 1)),$((lineno + 3))p" "$REPO/docs/local-development.md" \
                 | grep -qiE 'every service|all services|all of them'; then
                fail "docs/local-development.md:$lineno runs \`docker compose up\` with no service, and the file declares $svc_count services ($(printf '%s' "$SERVICES" | tr '\n' ' ')). Compose starts ALL of them - so this line does more than the sentence above it says. Name the service, or state that it means every one."
            fi
        fi
    done < /tmp/composeup.$$
    rm -f /tmp/composeup.$$
fi

if [ "$FAILED" -ne 0 ]; then
    echo "compose-check: the compose contract is BROKEN (see above)" >&2
    exit 1
fi

echo "compose-check: OK - the deployment definition parses and holds its documented invariants"
exit 0