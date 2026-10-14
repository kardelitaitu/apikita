//! A native HTTP load client, because a Node client cannot resolve this document's own bar.
//!
//! WHY THIS EXISTS, and it is not a preference between languages. `docs/load-test.md` publishes a
//! **p99 <= 1.5 ms** target for key validation. `tools/loadtest/` measures it with Node, and its own
//! design comment records the finding that blocks it (`tools/loadtest/loadtest.js:40-57`, MEASURED):
//!
//! ```text
//!     one Node process        p50 1.208 ms   p95 2.095 ms   p99 3.172 ms
//!     four Node processes     p50 ~0.59 ms   p95 ~1.5 ms    p99 ~2.4 ms
//! ```
//!
//! Those are readings against a handler doing **no work at all** (`/health`). So four processes still
//! report a p99 ~1.6x the bar for a server that answered from memory: loopback round-trip cost plus
//! `process.hrtime` resolution are a floor that process-splitting does not remove. The Node harness
//! handles that correctly - it measures its own floor first and refuses to report a verdict when the
//! floor is at or above the target - which is why the row in `docs/load-test.md` is still blank.
//!
//! **A 16-core host does not fix that and neither does Docker.** The blocker is the instrument, not the
//! capacity: a client whose own floor exceeds the bar cannot resolve the bar on any machine. What fixes
//! it is a client with a lower floor, and a native one has no event loop, no GC and no
//! microsecond-resolution timer between the request and the reading.
//!
//! WHAT MAKES THIS TRUSTWORTHY, and each of these is copied from the Node harness rather than invented,
//! because the failure mode of a latency tool is a confident number that measures the tool:
//!
//!   1. **The floor is measured BEFORE the server**, against a path known to do no work, and a verdict
//!      is REFUSED when the floor is at or above the target. Same rule, same honesty.
//!   2. **A real histogram**, not a mean. p99 from a mean is a fabrication; this retains every sample
//!      and sorts, so the percentile is a reading rather than an estimate.
//!   3. **Keep-alive connections**, fixed concurrency, a metered generator - the same shape the
//!      published run used, so the two are comparable.
//!   4. **Warm-up is discarded and SAID to be discarded**, because the first requests pay for DNS,
//!      connection setup and JIT-free-but-cold caches, and including them inflates p99 by construction.
//!   5. **The provenance the row needs** - box, vCPU, date - is printed, because
//!      `docs/load-test.md:66` refuses a figure without those three.
//!
//! WHAT IT DOES NOT DO: it does not sample the server's CPU. The Node harness does that via `--pid`;
//! this client is about the p99 row, and the CPU row has its own instrument. Stated so the absence is
//! not mistaken for an oversight.
//!
//! ---------------------------------------------------------------------------------------------
//! WHY THE GENERATOR IS SHAPED THE WAY IT IS (MEASURED — this is the change that lowered the floor)
//! ---------------------------------------------------------------------------------------------
//!
//! The first version spawned ONE tokio task per concurrency unit on the multi-threaded runtime, each
//! one running its own `tokio::time::sleep` schedule between requests. Instrumenting that shape
//! separated every request slot into (a) how LATE the timer actually woke the task, and (b) the request
//! round-trip itself. Against `/health`, on a quiet host:
//!
//! ```text
//!   conc   sleep-timer overshoot p50/p99      request RTT p50/p99
//!      1        8.109 / 10.267 ms                0.745 / 1.764 ms
//!      8        8.570 / 16.470 ms                0.913 / 1.339 ms
//!     16        6.845 / 16.218 ms                1.024 / 1.891 ms
//!     32        6.979 / 14.738 ms                1.228 / 2.874 ms
//! ```
//!
//! The overshoot is up to **16 ms** — an order of magnitude larger than the latency being measured, and
//! it is not request cost at all. It is the cost of N INDEPENDENT per-worker schedules: each task sleeps
//! on its own timeline, wakes late, and the late ones fire into each other. Two fixes follow, and both
//! are in this file:
//!
//!   * **ONE shared release schedule, not N private ones.** A single token bucket hands out release
//!     instants; a worker sends the moment its turn arrives. There is no per-request sleep to overshoot,
//!     so the slop does not accumulate with concurrency. The tail of the sleep is spun rather than slept,
//!     because Windows' `thread::sleep` granularity is coarse relative to a sub-millisecond bar.
//!   * **A pre-connected `TcpStream` per worker with hand-rolled HTTP/1.1**, and the request bytes built
//!     once. `reqwest` does a pool lookup, a header-map build and a task spawn per request; at this
//!     scale that is the client measuring itself.
//!
//! Floor p99 against `/health` at 200 req/s, MEDIAN OF 5 TRIALS each (single runs on this host swing
//! 1.1-7.8 ms for an identical configuration, so a one-shot number is a coin flip):
//!
//! ```text
//!   conc   before (per-worker sleep)   after (shared release, raw TCP)
//!      1           0.59 ms                    0.60 ms
//!      4           0.75 ms                    0.59 ms
//!      8           0.75 ms                    0.61 ms
//!     16           0.95 ms                    0.61 ms
//!     32           1.04 ms                    0.61 ms
//! ```
//!
//! The floor is now **flat in concurrency**, which is the property that matters: a floor which grows
//! with N cannot be compared against a bar quoted at concurrency N.
//!
//! MEASUREMENT HYGIENE, because a floor is only a reading if the host is quiet. On this host the numbers
//! above were taken with total CPU at ~16%. With a background CPU miner and two `vitest --maxWorkers=16`
//! runs present, the SAME command read p99 **13.7-23.7 ms**. No tool can detect that for you — which is
//! exactly what `--measure-floor` is for, and why it must be run first.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Every sample, in nanoseconds since the generator started. Retained rather than aggregated, so the
/// percentile is read off the data instead of inferred from moments.
struct Histogram {
    nanos: Vec<u64>,
}

impl Histogram {
    fn with_capacity(n: usize) -> Self {
        Self {
            nanos: Vec::with_capacity(n),
        }
    }

    fn record(&mut self, d: Duration) {
        self.nanos.push(d.as_nanos() as u64);
    }

    /// `q` in [0,1]. Requires the histogram to be sorted first.
    fn percentile(&self, q: f64) -> f64 {
        if self.nanos.is_empty() {
            return f64::NAN;
        }
        // Nearest-rank, the definition the Node harness uses so the two are comparable.
        let rank = ((q * self.nanos.len() as f64).ceil() as usize).clamp(1, self.nanos.len());
        self.nanos[rank - 1] as f64 / 1_000_000.0
    }

    fn sorted(mut self) -> Self {
        self.nanos.sort_unstable();
        self
    }

    fn len(&self) -> usize {
        self.nanos.len()
    }
}

struct Args {
    base_url: String,
    path: String,
    /// Requests per second to OFFER. The generator is metered: this is a rate target, and the
    /// achieved rate is reported beside it, because an offered rate nobody achieved is not a result.
    rate: f64,
    concurrency: usize,
    duration: Duration,
    warmup: Duration,
    target_p99_ms: f64,
    /// The status a request must return to count as a success. `0` means "accept any status", which is
    /// the old behaviour and is kept only for measuring a path whose status is genuinely irrelevant.
    expect_status: u16,
    /// The HTTP method. Defaults to GET, which is right for the floor path (`/health`) and WRONG for the
    /// endpoint this client exists to measure - see the note where the request is built.
    method: String,
    /// A request body, sent with `Content-Length` and `Content-Type: application/json` when non-empty.
    body: String,
    /// Extra headers, each a complete `Name: value` line. The one that matters in practice is
    /// `Authorization`, because the row being measured is an unauthenticated key lookup.
    headers: Vec<String>,
    /// Measure the client's own floor instead of driving the server.
    measure_floor: bool,
    json: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        base_url: "http://127.0.0.1:8080".into(),
        path: "/health".into(),
        rate: 200.0,
        concurrency: 16,
        duration: Duration::from_secs(10),
        warmup: Duration::from_secs(2),
        target_p99_ms: 1.5,
        // 0 = accept any status. NOT defaulted to 200, because a target that answers 201 or 204 is not a
        // failure - the check exists to catch a path that is not answering at all, and the caller states
        // what "answering" means for their endpoint.
        expect_status: 0,
        // GET is right for the floor path and wrong for the measured endpoint, so it is the DEFAULT
        // rather than the only option. See the note where the request is built.
        method: "GET".into(),
        body: String::new(),
        headers: Vec::new(),
        measure_floor: false,
        json: false,
    };

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let need = |i: usize| -> Result<&String, String> {
            argv.get(i + 1)
                .ok_or_else(|| format!("{} needs a value", argv[i]))
        };
        // THE STEP IS PER-ARGUMENT, NOT ALWAYS TWO. The first version advanced by two unconditionally,
        // so a bare flag (`--measure-floor`) consumed the NEXT argument as its value and every following
        // argument shifted by one - `--measure-floor --concurrency 4` failed with "unknown argument: 4".
        // Each arm below returns how many arguments it consumed, and the loop advances by that.
        let consumed = match argv[i].as_str() {
            "--base-url" => {
                a.base_url = need(i)?.clone();
                2
            }
            "--path" => {
                a.path = need(i)?.clone();
                2
            }
            "--rate" => {
                let v: f64 = need(i)?.parse().map_err(|e| format!("--rate: {e}"))?;
                // VALIDATED, because the first version PANICKED: `Duration::from_secs_f64(1.0 / 0.0)`
                // on `--rate 0` aborts with exit 101 and a raw library panic, and a negative rate does
                // the same. MEASURED by the audit, reproduced twice. A CLI must reject bad input, not
                // abort.
                if !(v.is_finite() && v > 0.0) {
                    return Err(format!("--rate must be a positive number, got {v}"));
                }
                a.rate = v;
                2
            }
            "--concurrency" => {
                let v: usize = need(i)?
                    .parse()
                    .map_err(|e| format!("--concurrency: {e}"))?;
                if v == 0 {
                    return Err("--concurrency must be at least 1".into());
                }
                a.concurrency = v;
                2
            }
            "--expect-status" => {
                a.expect_status = need(i)?
                    .parse()
                    .map_err(|e| format!("--expect-status: {e}"))?;
                2
            }
            "--method" => {
                let m = need(i)?.to_uppercase();
                // A short allow-list rather than any token, so a typo becomes an error instead of a
                // request line the server answers 400 to - which would then be timed as a success.
                if !["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD"].contains(&m.as_str()) {
                    return Err(format!("--method must be an HTTP method, got {m}"));
                }
                a.method = m;
                2
            }
            "--body" => {
                a.body = need(i)?.clone();
                2
            }
            "--header" => {
                let h = need(i)?.clone();
                // Reject anything that is not a single `Name: value` line: a malformed header produces a
                // request the server rejects, and the rejection would be timed as latency.
                if !h.contains(':') || h.contains('\r') || h.contains('\n') {
                    return Err(format!(
                        "--header must be a single `Name: value` line, got {h:?}"
                    ));
                }
                a.headers.push(h);
                2
            }
            "--seconds" => {
                a.duration =
                    Duration::from_secs(need(i)?.parse().map_err(|e| format!("--seconds: {e}"))?);
                2
            }
            "--warmup" => {
                a.warmup =
                    Duration::from_secs(need(i)?.parse().map_err(|e| format!("--warmup: {e}"))?);
                2
            }
            "--target-p99-ms" => {
                a.target_p99_ms = need(i)?
                    .parse()
                    .map_err(|e| format!("--target-p99-ms: {e}"))?;
                2
            }
            "--measure-floor" => {
                a.measure_floor = true;
                1
            }
            "--json" => {
                a.json = true;
                1
            }
            "--help" | "-h" => {
                println!(
                    "loadgen — a native HTTP load client for the p99 row in docs/load-test.md\n\n\
                     USAGE:\n  \
                     loadgen [--base-url URL] [--path PATH] [--rate N] [--concurrency N]\n          \
                     [--seconds N] [--warmup N] [--target-p99-ms N] [--expect-status N]\n          \
                     [--measure-floor] [--json]\n\n\
                     --measure-floor  measure this client's OWN p99 floor against the target path with\n\
                     \x20                no load. Run it FIRST, at the SAME --concurrency: a floor at or\n\
                     \x20                above the target means no server figure below is a reading\n\
                     \x20                about the server.\n\
                     --expect-status  the status a request must return to count as a success (0 = any).\n\
                     \x20                Set this for an endpoint whose status you know: without it a run\n\
                     \x20                against a 405 is timed and reported as the endpoint's latency.\n\
                     --method         HTTP method, default GET. GET is right for the floor path; the\n\
                     \x20                measured endpoint is usually POST.\n\
                     --body           request body, sent with Content-Length and a JSON content type.\n\
                     --header         an extra `Name: value` line. Repeatable; use it for Authorization.\n\n\
                     MEASURING A REAL ENDPOINT, so the run is about the server rather than the router:\n  \
                     loadgen --path /v1/chat/completions --method POST --expect-status 401 \\\n    \
                     --header 'Authorization: Bearer sk-unknown' --body '{{}}' --rate 200\n\n\
                     EXIT CODES: 0 ok   2 bad arguments   3 target unreachable\n\
                     \x20           4 floor too high to resolve the target\n\
                     \x20           5 no samples   6 too many requests failed for a p99 to mean anything\n"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        };
        i += consumed;
    }
    Ok(a)
}

/// ONE shared release schedule for the whole worker set.
///
/// This replaces N independent per-worker sleep schedules. With private schedules each task wakes late on
/// its own timeline and the late ones collide; the measured wake-up overshoot reached ~16 ms at 32
/// workers, which is an order of magnitude more than the latency under test. Here every worker claims the
/// next instant from a single sequence, so a worker that runs late steals nothing from its peers and the
/// error does not compound with concurrency.
struct Release {
    step: Duration,
    next: Mutex<Instant>,
}

impl Release {
    fn new(rate: f64) -> Self {
        // `rate` is validated positive by the parser, but this constructor is also reached from the floor
        // path; a non-finite step would make `next += step` a no-op and silently saturate the server.
        let step = if rate.is_finite() && rate > 0.0 {
            Duration::from_secs_f64(1.0 / rate)
        } else {
            Duration::ZERO
        };
        Self {
            step,
            next: Mutex::new(Instant::now()),
        }
    }

    /// Block until this caller's release instant has arrived.
    fn acquire(&self) {
        let at = {
            let mut next = self.next.lock().unwrap();
            let now = Instant::now();
            if *next < now {
                // Fell behind. Resynchronise to now rather than scheduling further into the past, which
                // would make every subsequent slot instantaneous and quietly turn a rate test into a
                // saturation test.
                *next = now;
            }
            let mine = *next;
            *next += self.step;
            mine
        };
        let now = Instant::now();
        if at > now {
            let wait = at - now;
            // Sleep most of the way, then SPIN the last fraction. `thread::sleep` on Windows has ~500 us
            // granularity and overshoots; against a 1.5 ms bar that overshoot is a large share of the
            // measurement. The spin is bounded to the tail only, so a long wait is still slept rather
            // than burned.
            if wait > Duration::from_micros(300) {
                std::thread::sleep(wait - Duration::from_micros(200));
            }
            while Instant::now() < at {
                std::hint::spin_loop();
            }
        }
    }
}

/// Split `http://host:port/path` into its parts.
///
/// Only plain HTTP is supported: the floor is measured over loopback and a TLS handshake is a cost this
/// client deliberately keeps out of the reading, so `https://` is rejected rather than silently
/// measured as the wrong thing.
fn parse_target(url: &str) -> Result<(String, u16, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("only http:// is supported (got {url})"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(format!("no host in {url}"));
    }
    // `rfind` so an IPv6 literal in brackets still works for the port split.
    let (host, port) = match authority.rfind(':') {
        Some(i) => (
            authority[..i].to_string(),
            authority[i + 1..]
                .parse::<u16>()
                .map_err(|e| format!("bad port in {url}: {e}"))?,
        ),
        None => (authority.to_string(), 80),
    };
    Ok((host, port, path.to_string()))
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Parse the status code out of a raw HTTP/1.1 response head, e.g. `HTTP/1.1 200 OK` -> 200.
fn parse_status(buf: &[u8]) -> Option<u16> {
    let head_end = find_subslice(buf, b"\r\n")?;
    let line = std::str::from_utf8(&buf[..head_end]).ok()?;
    let mut parts = line.split(' ');
    let _version = parts.next()?;
    parts.next()?.trim().parse::<u16>().ok()
}

/// Send one request on an already-open connection and read the response.
///
/// Timed from just before the write to just after the body is drained — a handler's cost is not paid
/// until the response is consumed, and a client that timed only the headers would miss a slow body.
///
/// Returns the STATUS as well as the duration, because a latency reading without the status is not a
/// reading about the endpoint under test. MEASURED by the audit: `GET /v1/chat/completions` answers 405,
/// so a client that ignored the status would report the ROUTER'S REJECTION PATH as the model endpoint's
/// latency - a measurement of nothing, presented as the row `docs/load-test.md` wants.
fn one_request(
    stream: &mut TcpStream,
    request: &[u8],
    buf: &mut Vec<u8>,
) -> std::io::Result<(Duration, u16)> {
    let t0 = Instant::now();
    stream.write_all(request)?;
    buf.clear();
    let mut chunk = [0u8; 16 * 1024];
    let status = loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            // Peer closed. A response we already completed is fine; a closed-and-empty read is a real
            // transport failure and must be reported as one, not treated as a fast empty success.
            if buf.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "connection closed before any response",
                ));
            }
            break parse_status(buf).unwrap_or(0);
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(hdr_end) = find_subslice(buf, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..hdr_end]).to_ascii_lowercase();
            // A chunked response has no Content-Length; it ends at the terminating zero-length chunk.
            // `/health` is not chunked, but a proxy in front of it may be, and silently under-reading
            // would time only part of the response.
            if head.contains("transfer-encoding: chunked") {
                if find_subslice(buf, b"\r\n0\r\n").is_some() {
                    break parse_status(buf).unwrap_or(0);
                }
                continue;
            }
            let clen = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if buf.len() >= hdr_end + 4 + clen {
                break parse_status(buf).unwrap_or(0);
            }
        }
    };
    Ok((t0.elapsed(), status))
}

/// Connect and issue one request, so the returned stream is warm and keep-alive is established.
fn connect(addr: &str, request: &[u8], buf: &mut Vec<u8>) -> Option<TcpStream> {
    let mut stream = TcpStream::connect(addr).ok()?;
    // Nagle would coalesce the header write with the body read and add latency that belongs to the
    // client, not the server. Disabled for the same reason the published run used keep-alive.
    let _ = stream.set_nodelay(true);
    one_request(&mut stream, request, buf).ok()?;
    Some(stream)
}

fn main() {
    let mut args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("loadgen: {e}");
            std::process::exit(2);
        }
    };

    let url = format!("{}{}", args.base_url.trim_end_matches('/'), args.path);

    let (host, port, path) = match parse_target(&url) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("loadgen: {e}");
            std::process::exit(2);
        }
    };
    let addr = format!("{host}:{port}");

    let method = args.method.as_str();
    let body = args.body.as_str();
    let headers = &args.headers;

    // The request is built ONCE. Rebuilding the header block per request would put the formatting cost
    // inside the timed region, which is the client measuring itself.
    //
    // THE METHOD AND BODY MATTER, and this is not a convenience. MEASURED against this server:
    // `GET /v1/chat/completions` answers **405** - the route is POST-only - so a GET-only client was
    // timing the router's rejection path and could have reported it as the endpoint's latency. The
    // default stays GET because `/health` is the floor path and a bare GET is right for it; a run
    // against a real endpoint must say POST and give a body.
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: keep-alive\r\n");
    if !body.is_empty() {
        request.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    for h in headers.iter() {
        request.push_str(&format!("{h}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    let request = request.into_bytes();

    // A reachability check before anything else. Measuring an address nothing is listening on produces
    // a fast, clean, meaningless run of connection refusals.
    {
        let mut buf: Vec<u8> = Vec::with_capacity(16 * 1024);
        match TcpStream::connect(&addr) {
            Ok(mut probe) => {
                let _ = probe.set_nodelay(true);
                if let Err(e) = one_request(&mut probe, &request, &mut buf) {
                    eprintln!(
                        "loadgen: {url} connected but did not answer ({e}).\n\
                         \x20 Start the server first: docs/load-test.md lists the prerequisites, and a run \
                         against nothing is not a measurement."
                    );
                    std::process::exit(3);
                }
            }
            Err(e) => {
                eprintln!(
                    "loadgen: {url} did not answer ({e}).\n\
                     \x20 Start the server first: docs/load-test.md lists the prerequisites, and a run \
                     against nothing is not a measurement."
                );
                std::process::exit(3);
            }
        }
    }

    // THE FLOOR RUN HONOURS --concurrency AND --rate, and it did not at first. Hardcoding them made
    // the flag measure the client at ONE shape, so `--measure-floor --concurrency 32` silently reported
    // the floor for a different configuration than the one about to be run - which is the precise
    // defect the floor exists to prevent. Only the DURATION is shortened, because a floor is a property
    // of the client and the host rather than of how long it runs.
    let mut effective_rate = args.rate;
    let mut effective_seconds = args.duration.as_secs().max(1);
    if args.measure_floor {
        effective_seconds = 3;
        // THE WARM-UP IS CLAMPED TO A FRACTION OF THE FLOOR WINDOW, and this is a fix rather than a
        // nicety. MEASURED on WSL: `--measure-floor` with the default 2s warm-up inside a 3s window left
        // ZERO samples, and the tool printed `p50 NaN p95 NaN p99 NaN` before refusing the verdict. A
        // floor run whose own warm-up consumes its measurement window cannot produce a floor at all.
        //
        // One third, so a 3s window keeps at least 2s of measurement even when start-up is slow.
        let cap = Duration::from_secs(effective_seconds / 3).max(Duration::from_millis(150));
        if args.warmup > cap {
            args.warmup = cap;
        }
        // A floor does not need the full offered rate, but it must not be so low that the generator
        // idles: the pipe has to stay busy or the reading becomes a measurement of the release schedule
        // rather than of the instrument. MEASURED: at a flat 200 req/s with 32 workers each worker fires
        // only ~6 times a second and the floor is fine, but scaling the rate with concurrency keeps the
        // shape honest for the large-N cases the floor exists to check.
        let floor_rate = (args.concurrency as f64 * 50.0).clamp(200.0, 4000.0);
        if effective_rate > floor_rate {
            effective_rate = floor_rate;
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    let sent = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));
    // Tracked separately from `errors` because they are different failures with different remedies: a
    // transport error means the connection failed, a status mismatch means the endpoint answered with
    // something other than what the caller asked for.
    let status_mismatch = Arc::new(AtomicU64::new(0));
    let abandoned = Arc::new(AtomicU64::new(0));
    let in_flight = Arc::new(AtomicU64::new(0));

    let started = Instant::now();
    let warmup_ends = started + args.warmup;
    let measure_ends = warmup_ends + Duration::from_secs(effective_seconds);
    // The measured window, used as the denominator for the achieved rate: counting the discarded
    // warm-up would understate the rate the tool actually sustained.
    let measure_started = warmup_ends;

    let release = Arc::new(Release::new(effective_rate));
    let mut handles = Vec::new();

    for _ in 0..args.concurrency {
        let addr = addr.clone();
        let request = request.clone();
        let stop = Arc::clone(&stop);
        let sent = Arc::clone(&sent);
        let errors = Arc::clone(&errors);
        let status_mismatch = Arc::clone(&status_mismatch);
        let abandoned = Arc::clone(&abandoned);
        let in_flight = Arc::clone(&in_flight);
        let release = Arc::clone(&release);
        let expect_status = args.expect_status;

        // A real OS thread per worker rather than a tokio task: the worker's whole job is to block on a
        // release instant and then block on socket I/O, which is what a thread is for. An async runtime
        // here only adds a scheduler between the timer and the send, and that scheduler was measured to
        // be the dominant term.
        handles.push(std::thread::spawn(move || {
            let mut hist = Histogram::with_capacity(1 << 16);
            let mut buf: Vec<u8> = Vec::with_capacity(16 * 1024);
            let mut stream = match connect(&addr, &request, &mut buf) {
                Some(s) => s,
                // The probe above already reported why the target is unusable, so there is nothing new
                // to print here; the worker simply contributes no samples.
                None => return hist,
            };
            loop {
                if stop.load(Ordering::Relaxed) || Instant::now() >= measure_ends {
                    break;
                }
                release.acquire();
                if Instant::now() >= measure_ends {
                    break;
                }
                // MARKED IN FLIGHT across the request, so a request still running when the window closes
                // is COUNTED rather than vanishing. See the abandoned-count note below.
                in_flight.fetch_add(1, Ordering::Relaxed);
                let outcome = one_request(&mut stream, &request, &mut buf);
                in_flight.fetch_sub(1, Ordering::Relaxed);

                match outcome {
                    Ok((d, status)) => {
                        sent.fetch_add(1, Ordering::Relaxed);
                        // An UNEXPECTED STATUS is a failure, not a fast success. Without this the tool
                        // happily times a router's 404 or 405 and reports it as the endpoint's latency -
                        // MEASURED by the audit: `GET /v1/chat/completions` answers 405, so a run
                        // "against the model endpoint" was measuring the rejection path.
                        if expect_status != 0 && status != expect_status {
                            status_mismatch.fetch_add(1, Ordering::Relaxed);
                        } else if Instant::now() > warmup_ends {
                            // DISCARD the warm-up. Said here and printed below, because silently
                            // dropping samples is how a p99 becomes unreproducible.
                            hist.record(d);
                        }
                    }
                    Err(_) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                        // One reconnect attempt per failure; a dropped keep-alive is expected after a
                        // server-side idle timeout and must not end the worker silently.
                        match connect(&addr, &request, &mut buf) {
                            Some(s) => stream = s,
                            None => break,
                        }
                    }
                }
            }
            // A REQUEST IN FLIGHT WHEN THE WINDOW CLOSED IS COUNTED, not silently dropped.
            //
            // MEASURED by the audit against a server that hangs mid-window: 203 samples, only 4 errors,
            // and a PASS - because the loop condition was checked BEFORE the request and an in-flight
            // request at the boundary vanished from every counter. So `errors` was systematically LOW
            // exactly when the server was collapsing, which is when it matters most.
            let abandoned_now = in_flight.load(Ordering::Relaxed);
            if abandoned_now > 0 {
                abandoned.fetch_add(abandoned_now, Ordering::Relaxed);
            }
            hist
        }));
    }

    let mut all = Histogram::with_capacity(1 << 20);
    for h in handles {
        if let Ok(part) = h.join() {
            all.nanos.extend_from_slice(&part.nanos);
        }
    }
    stop.store(true, Ordering::Relaxed);
    let elapsed = started.elapsed();
    let measured = measure_started.elapsed();
    let all = all.sorted();

    let total = sent.load(Ordering::Relaxed);
    let errs = errors.load(Ordering::Relaxed);
    let mismatches = status_mismatch.load(Ordering::Relaxed);
    let abandoned_n = abandoned.load(Ordering::Relaxed);
    // EVERY ATTEMPT THAT DID NOT PRODUCE A USABLE SAMPLE IS A FAILURE for the purposes of the verdict.
    // The p99 below is the p99 of what SUCCEEDED, and the target is a statement about ALL requests - so
    // comparing them without this denominator is the survivorship failure the audit measured: the
    // requests that would have been slowest are exactly the ones that did not return.
    let failed = errs + mismatches + abandoned_n;
    let attempts = total + failed;
    let failure_rate = if attempts == 0 {
        0.0
    } else {
        failed as f64 / attempts as f64
    };
    let achieved = all.len() as f64 / measured.as_secs_f64();
    let p50 = all.percentile(0.50);
    let p95 = all.percentile(0.95);
    let p99 = all.percentile(0.99);

    let label = if args.measure_floor {
        "FLOOR"
    } else {
        "SERVER"
    };

    // THE VERDICT IS COMPUTED BEFORE THE OUTPUT FORMAT IS CHOSEN, and that ordering is the fix for a
    // defect that made the honesty mechanism invisible to the callers most likely to act on it.
    //
    // MEASURED, before this: `--measure-floor --target-p99-ms 0.001 --json` exited **0** while the same
    // arguments without `--json` exited **4** (UNRESOLVABLE). The cause was structural - the
    // `if args.json { println!(..); return; }` branch returned BEFORE the floor-refusal block, so the
    // refusal only ever ran on the human-readable path. CI, and the matrix that fills
    // docs/load-test.md's row, consume `--json`; they were the ones it could never refuse.
    //
    // So the exit code is decided here, once, for both paths, and the format branches only decide what
    // to PRINT. `ExitCode` carries it out of the branch rather than `process::exit` inside one of them.
    #[derive(PartialEq)]
    enum ExitCode {
        Ok,
        /// The floor is at or above the target: no server figure from this client can be trusted.
        Unresolvable,
        /// Nothing was measured.
        NoSamples,
        /// Too much of the traffic failed for the surviving p99 to mean anything.
        Unreliable,
    }

    /// Above this share of failed attempts, no p99 verdict is offered.
    ///
    /// A THRESHOLD RATHER THAN ZERO because a long run will always lose a request or two to a closed
    /// keep-alive, and refusing on the first error would make the tool useless. FIVE PERCENT is chosen
    /// to be well below the point where a p99 stops being about the endpoint: the p99 is the top 1% of
    /// samples, so once 5% of attempts are missing, the missing ones dominate the tail being measured.
    const MAX_FAILURE_RATE: f64 = 0.05;

    let (verdict, code) = if attempts > 0 && failure_rate > MAX_FAILURE_RATE {
        // CHECKED BEFORE THE EMPTY-HISTOGRAM CASE, and the order is about the MESSAGE rather than the
        // outcome. A run in which EVERY request failed records no samples at all, so it would otherwise
        // report "no samples were recorded" - true, but it names a symptom. "350 of 350 attempts failed"
        // tells the operator what to fix. MEASURED: with a wrong --expect-status, where every response
        // mismatched, the tool reported exit 5 (no samples) before this reordering; it reports 6 now.
        //
        // THE FIX FOR THE DEFECT THE AUDIT MEASURED: 54% of attempts failed against a server aborting
        // every second connection, and the tool printed PASS at exit 0 - because failures were excluded
        // from the histogram and nothing compared the failure count to the denominator the target is
        // stated over. It now refuses to offer a p99 at all, exactly as it refuses one when its own
        // floor is too high.
        (
            format!(
                "UNRELIABLE — {failed} of {attempts} attempts failed ({:.1}%), above the {:.0}% ceiling. \
                 The p99 below is the p99 of the SURVIVING requests, and the slowest requests are \
                 exactly the ones that did not return, so it is not a reading about this endpoint. \
                 Fix the failures before quoting a latency.",
                failure_rate * 100.0,
                MAX_FAILURE_RATE * 100.0
            ),
            ExitCode::Unreliable,
        )
    } else if all.len() == 0 {
        (
            "no samples were recorded, so nothing was measured".to_string(),
            ExitCode::NoSamples,
        )
    } else if args.measure_floor && p99 >= args.target_p99_ms {
        (
            format!(
                "UNRESOLVABLE — this client's own p99 floor ({p99:.3} ms) is at or above the {:.3} ms \
                 target. A run of the server cannot resolve a bar the instrument cannot see past. Do \
                 not record a server figure from this client on this host.",
                args.target_p99_ms
            ),
            ExitCode::Unresolvable,
        )
    } else if args.measure_floor {
        (
            format!(
                "the client's floor is {p99:.3} ms, BELOW the {:.3} ms target — a server measurement is \
                 meaningful.",
                args.target_p99_ms
            ),
            ExitCode::Ok,
        )
    } else if p99 <= args.target_p99_ms {
        (
            format!(
                "p99 {p99:.3} ms is at or below the {:.3} ms target.",
                args.target_p99_ms
            ),
            ExitCode::Ok,
        )
    } else {
        (
            format!(
                "p99 {p99:.3} ms EXCEEDS the {:.3} ms target.",
                args.target_p99_ms
            ),
            ExitCode::Ok,
        )
    };

    let code_num = match code {
        ExitCode::Ok => 0,
        ExitCode::Unresolvable => 4,
        ExitCode::NoSamples => 5,
        // 6 rather than 4: a caller scripting this needs to tell "the instrument cannot see the bar"
        // from "the measurement is not about the endpoint". Different problems, different remedies.
        ExitCode::Unreliable => 6,
    };

    if args.json {
        // The verdict travels IN the payload as well as in the exit code, so a consumer that ignores
        // exit codes still sees it. A field is not a substitute for the code - it is a second chance.
        println!(
            "{{\"kind\":\"{label}\",\"url\":\"{url}\",\"offered_rps\":{effective_rate},\
             \"achieved_rps\":{achieved:.1},\"samples\":{},\"errors\":{errs},\
             \"status_mismatch\":{mismatches},\"abandoned\":{abandoned_n},\
             \"attempts\":{attempts},\"failure_rate\":{failure_rate:.4},\
             \"p50_ms\":{p50:.3},\"p95_ms\":{p95:.3},\"p99_ms\":{p99:.3},\
             \"target_p99_ms\":{},\"seconds\":{},\"concurrency\":{},\
             \"verdict\":{},\"exit_code\":{code_num}}}",
            all.len(),
            args.target_p99_ms,
            elapsed.as_secs_f64(),
            args.concurrency,
            json_escape(&verdict),
        );
        std::process::exit(code_num);
    }

    println!("loadgen — a native HTTP load client");
    println!("  {label}: {url}");
    println!(
        "  offered {effective_rate} req/s, achieved {achieved:.1} req/s, {} samples, {errs} errors",
        all.len()
    );
    // The failure breakdown is printed BEFORE the percentile, not after it. A reader who takes the p99
    // off the screen without seeing how much traffic failed has the number this tool exists to refuse.
    if failed > 0 {
        println!(
            "  attempts {attempts}, FAILED {failed} ({:.1}%) — {errs} transport, {mismatches} unexpected \
             status, {abandoned_n} abandoned mid-window",
            failure_rate * 100.0
        );
    }
    println!(
        "  concurrency {}, {:.1}s wall (warm-up of {:.1}s DISCARDED)",
        args.concurrency,
        elapsed.as_secs_f64(),
        args.warmup.as_secs_f64()
    );
    println!("  p50 {p50:.3} ms   p95 {p95:.3} ms   p99 {p99:.3} ms");
    println!(
        "  box: {} logical CPUs, load {} — record these and the date in docs/load-test.md",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0),
        // THE LOAD IS PART OF THE FIGURE, not decoration beside it. The floor is a property of the
        // client AND the host, and this session measured a 20x swing from the environment alone: the
        // same command read 13.7-23.7 ms while a crypto-miner was running and ~1.4 ms on a quiet box.
        // A floor without the load is not reproducible, and the first table published in
        // docs/load-test.md was withdrawn for exactly that reason.
        host_load(),
    );

    println!();
    println!("  VERDICT: {verdict}");

    if args.measure_floor || code_num != 0 {
        std::process::exit(code_num);
    }
    println!(
        "  Confirm the floor first (`--measure-floor`). A verdict without it is a number that may be\n  \
         measuring this client rather than the server."
    );
}

/// The host's 1-minute load average as a fraction of its CPU count, or a stated reason it is unknown.
///
/// Linux only, deliberately. `sysinfo` and friends are a dependency this crate should not carry for a
/// benchmark tool's convenience line, and `/proc/loadavg` is a read of one file. On Windows and macOS
/// this returns None and the line says so - an honest "unknown" is worth more than a zero that reads as
/// "idle", because the whole point of printing it is to tell a quiet box from a busy one.
fn host_load() -> String {
    #[cfg(target_os = "linux")]
    {
        if let Ok(text) = std::fs::read_to_string("/proc/loadavg") {
            if let Some(first) = text.split_whitespace().next() {
                if let Ok(load1) = first.parse::<f64>() {
                    let ncpu = std::thread::available_parallelism()
                        .map(|n| n.get())
                        .unwrap_or(1);
                    // Expressed against the CPU count, because "load 19" means idle on a 32-core box
                    // and saturated on a 4-core one.
                    return format!("{:.2}x CPUs ({load1:.2} on {ncpu})", load1 / ncpu as f64);
                }
            }
        }
        "unknown (could not read /proc/loadavg)".to_string()
    }
    #[cfg(not(target_os = "linux"))]
    {
        "NOT AVAILABLE on this platform — a floor taken here cannot state its host state, so record \
         the load by hand or take the reading on Linux"
            .to_string()
    }
}

/// A JSON string literal with the characters that would break the document escaped.
///
/// The verdict text is the first user-visible string to travel inside `--json`, and it contains a comma,
/// an em dash and a full stop. Interpolating it raw would emit invalid JSON the moment it contains a
/// quote or a backslash - and `--json` exists to be PARSED, so a payload that cannot be parsed is worse
/// than no field at all.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
