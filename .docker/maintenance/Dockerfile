# Maintenance scheduler image.
#
# It exists for exactly ONE reason: to put the `sqlite3` CLI on PATH for the
# nightly retention + reconciliation jobs. The money database is embedded SQLite
# - a FILE the API opens, named by DATABASE_URL - so the jobs need a SQLite
# client and nothing else.
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
RUN apk add --no-cache sqlite
