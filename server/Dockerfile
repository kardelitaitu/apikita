# =============================================================================
# The API + proxy image. This is the artifact docs/deployment.md step [3]
# ("deploy Rust server -- Northflank") ships, and until it existed that step had
# no definition anywhere in the repository.
#
# WHY MULTI-STAGE. The runtime stage carries the binary, the config and the CA
# roots. Nothing else. The service holds a database file and provider
# credentials, so a compiler, a package manager and a shell in the final image
# are pure attack surface with no operational benefit - and the image is also
# ~40x smaller, which is what makes a rollback fast.
#
# WHICH BASE, AND WHY BOTH ENDS ARE PINNED. The builder is the official
# `rust` image at an explicit minor version; the runtime is
# `debian:bookworm-slim`. Both are pinned rather than `:latest` for the reason
# .docker/maintenance/Dockerfile already states: a base bump changes what runs
# money-adjacent code, and that must be a commit rather than a Tuesday. Debian
# slim over alpine because the binary links glibc, not musl - switching to
# alpine would require a musl target and a static build, which is a real change
# with real risk and no benefit this deployment needs.
# =============================================================================

# -----------------------------------------------------------------------------
# Stage 1: build
# -----------------------------------------------------------------------------
# NOTE ON THE BUILD CONTEXT: build from the REPOSITORY ROOT, not from server/,
# because the file needs `config/` as well:
#
#     docker build -f server/Dockerfile -t apikita-server .
#
# Every COPY is therefore prefixed `server/` except the config. Using
# `context: server` would leave the config unreachable and ship an image with no
# prices in it — a failure that only appears at runtime.
# PINNED TO 1.88, and the reason is MEASURED rather than chosen: at 1.82 the build
# dies on `feature `edition2024` is required` (idna_adapter, home), and at 1.85 it
# dies with "rustc 1.85.1 is not supported by the following packages:
# icu_collections@2.3.0 requires rustc 1.88" (the icu_* family, pulled in by `idna`
# for URL handling). 1.88 is therefore the floor the lockfile itself imposes, and
# it is also what `rust-version` in Cargo.toml now claims. Both were established
# by running this build, not by reading a changelog.
FROM rust:1.88-bookworm AS builder

WORKDIR /build

# Compile dependencies BEFORE the source is copied, so a source-only edit
# reuses the cached dependency layer instead of rebuilding the world. This is
# the difference between a ~30s and a ~6min image build on every code change.
#
# The dummy main is the standard trick: `cargo build` needs a target to exist,
# and building the real source here would defeat the cache.
COPY server/Cargo.toml server/Cargo.lock ./
RUN mkdir -p src/bin \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

# Now the real source. `--locked` in every build below: the lockfile is the
# supply-chain pin, and letting cargo resolve during a production build would
# make the image unreproducible from a given commit.
COPY server/src ./src
COPY server/migrations ./migrations

# Touch so cargo cannot consider the stale dummy artefact fresh.
#
# TWO binaries, not one. `apikita-server` is the service; `migrate` is the schema
# tool, and it ships in the same image because deployment step [2] ("run
# migrations") otherwise has NO artifact to run — the pipeline would have a step
# with no executable, which is the same defect as the missing Dockerfile itself.
# docs/local-development.md:117 is explicit that migration is applied by
# `bin/migrate.rs` and NEVER on server boot, so this cannot be folded into the
# server's startup: the operator must be able to migrate the schema BEFORE the
# new binary starts, and against a database the old binary is still writing.
RUN touch src/main.rs src/lib.rs \
    && cargo build --release --locked --bin apikita-server --bin migrate

# -----------------------------------------------------------------------------
# Stage 2: runtime
# -----------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

# ca-certificates: the proxy and the auth exchange make outbound TLS calls to
# upstream providers and to PocketBase, so the CA bundle is a hard requirement,
# not a nicety. tini is the PID 1 init: the server is a tokio process that
# spawns work, and without a real init a container stop signals only PID 1
# instead of the process group - which turns a graceful shutdown into a timeout
# and a SIGKILL. wget comes from busybox: a HEALTHCHECK needs an HTTP client,
# debian-slim has none, and installing a full curl for one probe is a larger
# dependency than the probe is worth.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tini busybox \
    && rm -rf /var/lib/apt/lists/*

# A NON-ROOT USER, and this is the single most important line in the file. The
# process opens a SQLite file it must write and holds provider credentials in
# its environment; running as root means a compromise in the proxy is already
# root in the container. The uid/gid are fixed (not auto-assigned) so a volume
# written by an earlier container is still writable by the next one.
RUN groupadd --system --gid 10001 apikita \
    && useradd --system --uid 10001 --gid apikita --create-home apikita

# /srv/apikita is the same root .docker/maintenance uses (APP_DIR), so the two
# images agree about where the repo and the data directory live:
#   /srv/apikita/config     the routing/pricing config
#   /srv/apikita/server/data  the SQLite file (a mounted volume in production)
WORKDIR /srv/apikita/server

COPY --from=builder /build/target/release/apikita-server /usr/local/bin/apikita-server
COPY --from=builder /build/target/release/migrate /usr/local/bin/apikita-migrate

# The config is baked in because it is source, not a secret: it is versioned in
# the repository and a deployment that mounts a different one is a deployment
# running different PRICES. A ConfigMap-style override is still possible through
# APIKITA_CONFIG_PATH, which main.rs honours first.
COPY config /srv/apikita/config

# The data directory must EXIST and be owned by the runtime user. Without this
# the first container start as uid 10001 fails on a fresh volume with "unable to
# open database file" - the same class of failure the maintenance service hit
# when its data mount was missing. An existing volume keeps its own ownership.
RUN mkdir -p /srv/apikita/server/data \
    && chown -R apikita:apikita /srv/apikita

USER apikita

# 8080 is main.rs's default (PORT overrides it). Stated here so the port is
# discoverable from the image rather than only from the source.
ENV PORT=8080 \
    DATABASE_URL=sqlite://data/server.db \
    APIKITA_CONFIG_PATH=/srv/apikita/config/apikita.toml \
    RUST_LOG=info,apikita_server=info

EXPOSE 8080

# A CONTAINER-LEVEL HEALTHCHECK, distinct from the deploy gate.
#
# docs/deployment.md step [4] gates the release on `/health` from OUTSIDE, and
# that remains the release decision. This is the platform's liveness signal: it
# is what makes an orchestrator replace a wedged container, and what stops a
# rolling deploy from shifting traffic onto a process whose database handle died.
#
# It probes `/health`, which docs/deployment.md:127-132 requires to report the
# process AND the database and to NOT touch upstream providers - so a provider
# outage cannot make a healthy gateway look dead and trigger a restart loop.
# That contract is exactly why this probe is safe to run every 30s.
#
# --start-period covers the migration-free startup (pool, config, listener);
# three failures are tolerated so a slow first boot is not a restart storm.
# `busybox wget`, NOT bare `wget`: the Debian busybox package installs the
# multi-call binary at /usr/bin/busybox without applet symlinks, so a bare `wget`
# is "not found" and the probe reports unhealthy on EVERY container. Measured —
# the first version of this line did exactly that, which is the failure mode a
# healthcheck must never have: an always-red probe turns a healthy deploy into a
# restart loop, and it looks like an application fault rather than a typo.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD busybox wget -qO- http://127.0.0.1:${PORT}/health || exit 1

# `exec` form, so the binary is PID 1 under tini and receives signals directly.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/apikita-server"]
