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
# =============================================================================
# Stage 1: the credit-expiry runner
# =============================================================================
# WHY A BINARY IS BUILT HERE AT ALL, in an image whose header argues that every
# entry earns its place. `run_credit_expiry` is the one wired job that is not
# inline SQL: `db::expire_credit` is a guarded debit plus a ledger row carrying
# `balance_after` - the Gate 2 invariant - so the shell runs the tested Rust
# function rather than restating that arithmetic. The binary therefore has to be
# reachable from THIS container, and until this stage existed it was not: it was
# built and copied into the SERVER image, which is a different image, and
# docker-compose.yml gave the scheduler no way to see it. The job's own guard
# would have reported "NO CREDIT EXPIRES ... $USAGE_PURGE_BIN is missing" every
# night in production, which is the honest failure but not a working deployment.
#
# A TOOLCHAIN IN THE BUILD STAGE ONLY. The final image below is unchanged - still
# Alpine with sqlite3 and curl and nothing else - so the "short list, every entry
# an argument" rule is kept where it applies: what runs money-adjacent jobs. The
# compiler is discarded with this stage.
#
# The build context is the REPO ROOT (docker-compose.yml sets `context: .`), so
# server/ is reachable here. The two-stage shape - dependency cache, then real
# source - is copied from server/Dockerfile so the two builds share a pattern
# rather than inventing a second one.
#
# 1.88-alpine, not the 1.83 I first wrote: `server/Cargo.toml` declares
# `rust-version = "1.88"`, and a builder below that fails on the first crate that
# uses a newer feature. The version is matched to the manifest deliberately -
# `rust-version` is a promise the toolchain in this file has to keep. Alpine
# rather than the server's bookworm because the RUNTIME here is Alpine, and a
# glibc binary would need a compatibility layer this image does not carry.
FROM rust:1.88-alpine AS purgebuild

RUN apk add --no-cache musl-dev

WORKDIR /build

# The dependency cache. A dummy main lets `cargo build` resolve and compile every
# dependency once, so a source-only change does not rebuild the world.
COPY server/Cargo.toml server/Cargo.lock ./
RUN mkdir -p src/bin \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY server/src ./src
COPY server/migrations ./migrations

# ONE binary, and `--locked` for the same reason the server build uses it: the
# lockfile is the supply-chain pin, and resolving during a production build would
# make the image unreproducible from a commit.
RUN touch src/main.rs src/lib.rs \
    && cargo build --release --locked --bin usage-purge

# =============================================================================
# Stage 2: runtime
# =============================================================================
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

# The credit-expiry runner, at the path the entrypoint defaults to. It is the
# ONLY thing copied from the build stage, so the compiler does not ship.
COPY --from=purgebuild /build/target/release/usage-purge /usr/local/bin/apikita-usage-purge
