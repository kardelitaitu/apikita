# Maintenance scheduler image.
#
# It exists for the nightly jobs, and it carries EXACTLY TWO tools - each one
# because a job in the schedule cannot run without it:
#
#   sqlite3  the retention, reconciliation and hold-sweep jobs, plus the three
#            database-backed alert checks. The money database is embedded
#            SQLite - a FILE the API opens, named by DATABASE_URL.
#   curl     tools/alert/probe.sh, the two HTTP alert checks (api_down,
#            relay_down). It is NOT a general-purpose convenience: without it
#            those checks exit 3 and the scheduler cannot report the API being
#            down, which is most of the reason a scheduler exists. 260 KiB.
#
# The list is short on purpose and every entry is an ARGUMENT, not a habit. A
# tool added here is a tool that runs money-adjacent jobs, so it earns its place
# by being required by one - the same standard the retention jobs were held to.
#
# DO NOT "simplify" this back to `image: postgres:16` for its psql client. There
# is no Postgres in this stack and no database service in docker-compose.yml;
# psql is not the client these jobs use and it cannot open a SQLite file at all.
# That substitution is what left the scheduler permanently unable to succeed.
#
# Alpine, deliberately: `sqlite` is an apk package, and nginx:1.27-alpine is
# already the base of the other service in the compose file. The base tag is
# pinned explicitly - never :latest, which would let a base bump change the
# sqlite3 that runs money-adjacent SQL without a commit.
FROM alpine:3.21

# `sqlite` is the sqlite3 command-line shell (its libsqlite3 is pulled in as a
# dependency). No Postgres client is installed here, on purpose: there is
# nothing for one to talk to.
#
# curl is here for tools/alert/probe.sh and nothing else - see the header. The
# BusyBox wget this base already ships CANNOT replace it: probe.sh needs status
# codes (-w), connect and read timeouts, request headers and response bodies
# across a dozen call sites, and rewriting that transport to a BusyBox subset
# would put a second HTTP implementation in the tree to avoid 260 KiB - more
# risk than the package, and two things that can drift.
RUN apk add --no-cache sqlite curl
