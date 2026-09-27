# The edge relay (local)

`relay.conf` is mounted as `/etc/nginx/conf.d/default.conf` in the `nginx`
service. It is the **local** shape of the relay described in
[`docs/edge-relay.md`](../../docs/edge-relay.md); the contract is the same, the
topology is not.

## The one decision in this file: the relay serves ONE origin

Production splits the site and the API across two hosts — Cloudflare Pages serves
the static build, Northflank runs the Rust API
([`docs/architecture.md`](../../docs/architecture.md)). Locally that split is a
trap: the browser would see the site on one origin and the API on another, and the
Rust session cookie (set for the API's origin) would never be sent back. The
dashboard would 401 forever while every individual piece looked healthy —
[`docs/local-development.md`](../../docs/local-development.md), "Cookie domains
are the trap".

So locally the relay answers on **:8000 for both halves**:

```
browser -> :8000 (one origin, one cookie jar)
              |-- /events, /v1/            -> backend, UNBUFFERED (SSE, token stream)
              |-- /auth/, /api/, /webhooks/, = /health -> backend, buffered
              |-- = /healthz               -> the relay itself (compose healthcheck)
              \-- everything else          -> static file under /srv/www, else 404
```

Production still has to answer its own question (same subdomain? `api.`? CORS?).
Nothing here pre-empts that answer; it only removes the question from the local
loop.

## Where the static files come from

`website/dist` is **bind-mounted read-only** at `/srv/www`
(`docker-compose.yml`). Not copied into an image, not baked into a layer.

| Option | Why not |
| --- | --- |
| `COPY website/dist` in a Dockerfile | **The stale-content trap.** A rebuild that forgot the image step silently serves yesterday's JS, and the symptom is a frontend bug that isn't one. |
| A separate static-server container | A second service, a second port, and a second origin unless nginx proxies it anyway — more moving parts for nothing. |
| Bind-mount `website/dist` **(chosen)** | `docker compose up` works from a clean checkout with no build step; a missing `dist` is an honest 404, not a stale 200; a rebuild is visible immediately. |

**Cost accepted:** the host must build first (`cd website && PUBLIC_API_BASE_URL= npm run build`),
and on Docker Desktop the bind mount is a shared-filesystem read. Both are fine
for a development loop.

**A missing `dist` 404s.** If `website/dist` does not exist, Compose creates it as
an empty directory and nginx serves 404 for every page. That is deliberate: the
alternative (an empty directory rendering as a 200) is the failure mode above.

## Why `PUBLIC_API_BASE_URL` must be set at build time

`website/src/lib/api.ts` reads `import.meta.env.PUBLIC_API_BASE_URL ?? 'http://localhost:8080'`.
`PUBLIC_*` variables are **inlined into the JS at build time** — there is no runtime
env in a static bundle. Unset, the bundle hardcodes `http://localhost:8080`, a
*different origin* from the relay, and the cookie is never sent. Built with the
variable empty, every call is relative (`fetch('' + path)`, `new EventSource('/events')`)
and therefore same-origin by construction.

```sh
cd website && PUBLIC_API_BASE_URL= npm run build
grep -rl 'localhost:8080' dist/_astro/ || echo 'clean: bundle is same-origin'
```

`website/src/pages/docs/quickstart.astro` still *prints* `http://localhost:8080` as
documentation prose (it is literal text in the page, not a bundled call). That is
expected and is not the trap.

## What must never be "simplified"

These are load-bearing; see `docs/edge-relay.md`:

- `proxy_buffering off` + `proxy_read_timeout 1h` + `chunked_transfer_encoding off` +
  `gzip off` on `/events` and `/v1/`. A buffered SSE stream **looks connected while
  the dashboard silently stops updating** — the single most expensive bug in this
  file.
- `access_log off` — the relay sees API keys and bodies and must never persist
  them (`docs/data-retention.md`).
- The two `limit_req_zone`s, the `limit_conn_zone`, `client_max_body_size 25m`,
  and `location = /healthz`.
- **No `try_files $uri /index.html`.** This build is multi-page; an SPA fallback
  turns every 404 into a 200 of the landing page and hides broken links. `=404`
  is the correct ending.

## The one nginx rule that bites: header inheritance is all-or-nothing

The proxy headers (`Host`, `X-Real-IP`, `X-Forwarded-For`, `X-Forwarded-Proto`,
`Connection ''`, `proxy_http_version`) live at **server** level and the proxy
locations inherit them. But inheritance is **per directive, not per set**: the
moment a location sets *one* `proxy_set_header`, nginx discards **all** inherited
ones. That is why `/events` and `/v1/` below carry no `proxy_set_header` at all.

Verified, not assumed — a location with no `proxy_set_header` forwarded
`xff=172.17.0.1 xfp=http`; the same location with a single `proxy_set_header`
forwarded `xff= xfp=`. If you ever need to override one header in a location,
repeat the **whole** set there.

## Verifying it still works

```sh
docker compose config >/dev/null && echo 'compose ok'
docker compose up -d nginx
curl -s http://127.0.0.1:8000/ | grep -o 'One OpenAI-compatible endpoint'  # landing page, not 404
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8000/login/      # 200
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8000/nope        # 404, not 200
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8000/api/me      # 401 (proxied)
curl -s http://127.0.0.1:8000/health                                        # backend health
docker exec apikita-nginx nginx -T | grep -A9 'location /events'            # buffering still off
```
