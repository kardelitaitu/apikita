#!/bin/sh
# apikita relay BEHAVIOURAL check - does the relay actually STREAM SSE?
#
# WHY THIS EXISTS BESIDE check.sh. That one proves the DIRECTIVES are present; this
# proves they DO WHAT THE DOC CLAIMS. A typo in a directive name passes a grep and
# still buffers, and docs/edge-relay.md:110 is explicit that the failure is silent:
#
#   "A relay that buffers SSE is worse than no relay - the UI silently stops
#    updating."
#
# It runs a REAL nginx with the SHIPPED config, a REAL origin emitting an event every
# second, and times arrival. Streamed events arrive one per second; buffered ones
# arrive together at the end. The CLIENT fails on a batched arrival, which is the
# distinction a "did they arrive" test would miss.
#
# NEEDS DOCKER. Skips loudly - exit 3, not 0 - when it is unavailable, because a
# silent pass on a machine that ran nothing is the failure mode this whole directory
# exists to avoid.
#
# Usage: sh tools/relay-check/behaviour.sh [path/to/relay.conf]
# Exit: 0 streamed (contract holds), 1 buffered or unreachable, 3 docker unavailable.

# MSYS_NO_PATHCONV stops Git Bash rewriting /c/... into C:/Program Files/Git/... on
# the way to the daemon. Without it the -v mounts silently point somewhere else, and
# python fails with a path under the Git installation - which is how this script first
# failed while every Docker step itself reported success.
MSYS_NO_PATHCONV=1
export MSYS_NO_PATHCONV

set -u

REPO=$(cd -- "$(dirname -- "$0")/../.." && pwd)
CONF="${1:-$REPO/.docker/nginx/relay.conf}"
NET="apikita-relaycheck-$$"
ORIGIN="apikita-relaycheck-origin-$$"
RELAY="apikita-relaycheck-relay-$$"
PORT=18099

command -v docker >/dev/null 2>&1 || {
    echo "relay-check: SKIPPED - docker is not on PATH, so the behaviour was NOT verified" >&2
    exit 3
}
docker info >/dev/null 2>&1 || {
    echo "relay-check: SKIPPED - the docker daemon is not reachable, so the behaviour was NOT verified" >&2
    exit 3
}

[ -f "$CONF" ] || { echo "relay-check: no such config: $CONF" >&2; exit 2; }

cleanup() {
    docker rm -f "$RELAY" "$ORIGIN" >/dev/null 2>&1 || true
    docker network rm "$NET" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

# The relay proxies to an upstream name. In the shipped config that is
# host.docker.internal:8080 (the local backend); here it must be the origin
# container. Only the upstream and the listen port are rewritten - EVERY directive
# under test is the shipped one, which is the point.
PATCHED=$(mktemp)
sed "s|host.docker.internal:8080|$ORIGIN:9099|; s|listen 8000;|listen 8099;|; s|/srv/www|/tmp|" \
    "$CONF" > "$PATCHED"

if ! grep -q "$ORIGIN:9099" "$PATCHED"; then
    echo "relay-check: could not point the config at the origin; the upstream line changed" >&2
    exit 2
fi

docker network create "$NET" >/dev/null 2>&1 || { echo "relay-check: could not create a network" >&2; exit 2; }

docker run -d --rm --name "$ORIGIN" --network "$NET" \
    -v "$REPO/tools/relay-check/sse-origin.py:/origin.py:ro" \
    python:3-alpine python /origin.py >/dev/null || { echo "relay-check: origin failed to start" >&2; exit 2; }
sleep 3

# The config is mounted from a WINDOWS-ABSOLUTE path. A relative path or an MSYS
# /c/... path is treated as a DIRECTORY by the daemon and nginx then fails with
# "pread() ... Is a directory" - found the hard way.
# A DRIVE-LETTER path, because the daemon on Windows rejects the MSYS /c/... form
# even with MSYS_NO_PATHCONV set at this point in the shell: it sees a relative path
# and creates an empty DIRECTORY at the mount point, so nginx then fails with
# "pread() ... Is a directory". cygpath gives the native form when it exists; on
# Linux and macOS pwd is already correct.
if command -v cygpath >/dev/null 2>&1; then
    CONF_MOUNT=$(cygpath -m "$PATCHED")
else
    CONF_MOUNT=$PATCHED
fi

if ! docker run -d --rm --name "$RELAY" --network "$NET" -p "$PORT:8099" \
    -v "$CONF_MOUNT:/etc/nginx/conf.d/relay.conf:ro" nginx:alpine >/dev/null 2>&1; then
    echo "relay-check: the relay container could not start with the config at $CONF_MOUNT" >&2
    exit 2
fi
sleep 3

if ! docker ps --format "{{.Names}}" | grep -q "^$RELAY$"; then
    echo "relay-check: the relay container exited; its nginx config was rejected:" >&2
    docker logs "$RELAY" >&2 2>&1 | tail -5
    exit 2
fi

# The client runs INSIDE the network so no host port mapping is needed for the
# origin, and it reports the SPREAD rather than just the arrival.
docker run --rm --network "$NET" -v "$REPO/tools/relay-check/sse-client.py:/client.py:ro" \
    python:3-alpine python /client.py "$RELAY" 8099 3
RC=$?

if [ "$RC" -ne 0 ]; then
    echo "relay-check: FAIL - the relay did not stream SSE (docs/edge-relay.md:110)" >&2
    exit 1
fi

echo "relay-check: OK - the relay streams SSE through the shipped config"
exit 0