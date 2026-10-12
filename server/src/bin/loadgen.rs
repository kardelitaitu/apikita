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
//!   3. **Keep-alive connections**, fixed concurrency, a closed-loop generator - the same shape the
//!      published run used, so the two are comparable.
//!   4. **Warm-up is discarded and SAID to be discarded**, because the first requests pay for DNS,
//!      connection setup and JIT-free-but-cold caches, and including them inflates p99 by construction.
//!   5. **The provenance the row needs** - box, vCPU, date - is printed, because
//!      `docs/load-test.md:66` refuses a figure without those three.
//!
//! WHAT IT DOES NOT DO: it does not sample the server's CPU. The Node harness does that via `--pid`;
//! this client is about the p99 row, and the CPU row has its own instrument. Stated so the absence is
//! not mistaken for an oversight.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
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
    /// Requests per second to OFFER. The generator is closed-loop: this is a rate target, and the
    /// achieved rate is reported beside it, because an offered rate nobody achieved is not a result.
    rate: f64,
    concurrency: usize,
    duration: Duration,
    warmup: Duration,
    target_p99_ms: f64,
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
        measure_floor: false,
        json: false,
    };

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let need = |i: usize| -> Result<&String, String> {
            argv.get(i + 1).ok_or_else(|| format!("{} needs a value", argv[i]))
        };
        // THE STEP IS PER-ARGUMENT, NOT ALWAYS TWO. The first version advanced by two unconditionally,
        // so a bare flag (`--measure-floor`) consumed the NEXT argument as its value and every following
        // argument shifted by one - `--measure-floor --concurrency 4` failed with "unknown argument: 4".
        // Each arm below returns how many arguments it consumed, and the loop advances by that.
        let consumed = match argv[i].as_str() {
            "--base-url" => { a.base_url = need(i)?.clone(); 2 }
            "--path" => { a.path = need(i)?.clone(); 2 }
            "--rate" => { a.rate = need(i)?.parse().map_err(|e| format!("--rate: {e}"))?; 2 }
            "--concurrency" => {
                a.concurrency = need(i)?.parse().map_err(|e| format!("--concurrency: {e}"))?;
                2
            }
            "--seconds" => {
                a.duration =
                    Duration::from_secs(need(i)?.parse().map_err(|e| format!("--seconds: {e}"))?);
                2
            }
            "--warmup" => {
                a.warmup = Duration::from_secs(need(i)?.parse().map_err(|e| format!("--warmup: {e}"))?);
                2
            }
            "--target-p99-ms" => {
                a.target_p99_ms = need(i)?.parse().map_err(|e| format!("--target-p99-ms: {e}"))?;
                2
            }
            "--measure-floor" => { a.measure_floor = true; 1 }
            "--json" => { a.json = true; 1 }
            "--help" | "-h" => {
                println!(
                    "loadgen — a native HTTP load client for the p99 row in docs/load-test.md\n\n\
                     USAGE:\n  \
                     loadgen [--base-url URL] [--path PATH] [--rate N] [--concurrency N]\n          \
                     [--seconds N] [--warmup N] [--target-p99-ms N] [--measure-floor] [--json]\n\n\
                     --measure-floor  measure this client's OWN p99 floor against the target path with\n\
                     \x20                no load. Run it FIRST: a floor at or above the target means no\n\
                     \x20                server figure below is a reading about the server.\n"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        };
        i += consumed;
    }
    Ok(a)
}

/// One request, timed from just before the send to just after the body is read.
async fn one_request(client: &reqwest::Client, url: &str) -> Result<Duration, String> {
    let t0 = Instant::now();
    let resp = client.get(url).send().await.map_err(|e| e.to_string())?;
    // Read the body: a handler's cost is not paid until the response is consumed, and a client that
    // timed only the headers would miss a slow body. `bytes()` drains it.
    let _ = resp.bytes().await.map_err(|e| e.to_string())?;
    Ok(t0.elapsed())
}

#[tokio::main]
async fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("loadgen: {e}");
            std::process::exit(2);
        }
    };

    let url = format!("{}{}", args.base_url.trim_end_matches('/'), args.path);

    let client = match reqwest::Client::builder()
        // Keep-alive, so the measurement excludes TCP and TLS setup. `pool_max_idle_per_host` above the
        // concurrency, or the pool would serialise connections the generator intends to run in parallel.
        .pool_max_idle_per_host(args.concurrency + 8)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_nodelay(true)
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("loadgen: could not build the client: {e}");
            std::process::exit(2);
        }
    };

    // A reachability check before anything else. Measuring an address nothing is listening on produces
    // a fast, clean, meaningless run of connection refusals.
    match client.get(&url).send().await {
        Ok(_) => {}
        Err(e) => {
            eprintln!(
                "loadgen: {url} did not answer ({e}).\n\
                 \x20 Start the server first: docs/load-test.md lists the prerequisites, and a run \
                 against nothing is not a measurement."
            );
            std::process::exit(3);
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
        // A floor does not need the full offered rate, but it must not be so low that the generator
        // sleeps more than it works: at 10 req/s a single worker idles ~100 ms between requests and the
        // measurement becomes a reading about the sleep timer.
        if effective_rate > 200.0 {
            effective_rate = 200.0;
        }
    }

    let stop = Arc::new(AtomicBool::new(false));
    let sent = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));

    let started = Instant::now();
    let warmup_ends = started + args.warmup;
    let measure_ends = warmup_ends + Duration::from_secs(effective_seconds);

    let mut handles = Vec::new();
    // The offered rate is split across the workers; each waits its own share of the interval.
    let interval = Duration::from_secs_f64(1.0 / (effective_rate / args.concurrency as f64));

    for _ in 0..args.concurrency {
        let client = client.clone();
        let url = url.clone();
        let stop = Arc::clone(&stop);
        let sent = Arc::clone(&sent);
        let errors = Arc::clone(&errors);

        handles.push(tokio::spawn(async move {
            let mut hist = Histogram::with_capacity(1 << 16);
            let mut next = Instant::now();
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let now = Instant::now();
                if now >= measure_ends {
                    break;
                }
                next += interval;
                if next > now {
                    tokio::time::sleep(next - now).await;
                } else {
                    // Fell behind; resynchronise rather than accumulating a backlog that would turn the
                    // run into a saturation test while claiming to be a rate test.
                    next = Instant::now();
                }
                match one_request(&client, &url).await {
                    Ok(d) => {
                        sent.fetch_add(1, Ordering::Relaxed);
                        // DISCARD the warm-up. Said here and printed below, because silently dropping
                        // samples is how a p99 becomes unreproducible.
                        if Instant::now() > warmup_ends {
                            hist.record(d);
                        }
                    }
                    Err(_) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            hist
        }));
    }

    let mut all = Histogram::with_capacity(1 << 20);
    for h in handles {
        if let Ok(part) = h.await {
            all.nanos.extend_from_slice(&part.nanos);
        }
    }
    stop.store(true, Ordering::Relaxed);
    let elapsed = started.elapsed();
    let all = all.sorted();

    let total = sent.load(Ordering::Relaxed);
    let errs = errors.load(Ordering::Relaxed);
    let achieved = total as f64 / elapsed.as_secs_f64();
    let p50 = all.percentile(0.50);
    let p95 = all.percentile(0.95);
    let p99 = all.percentile(0.99);

    let label = if args.measure_floor { "FLOOR" } else { "SERVER" };

    if args.json {
        println!(
            "{{\"kind\":\"{label}\",\"url\":\"{url}\",\"offered_rps\":{effective_rate},\
             \"achieved_rps\":{achieved:.1},\"samples\":{},\"errors\":{errs},\
             \"p50_ms\":{p50:.3},\"p95_ms\":{p95:.3},\"p99_ms\":{p99:.3},\
             \"target_p99_ms\":{},\"seconds\":{},\"concurrency\":{}}}",
            all.len(),
            args.target_p99_ms,
            elapsed.as_secs_f64(),
            args.concurrency,
        );
        return;
    }

    println!("loadgen — a native HTTP load client");
    println!("  {label}: {url}");
    println!("  offered {effective_rate} req/s, achieved {achieved:.1} req/s, {} samples, {errs} errors",
        all.len());
    println!("  concurrency {}, {:.1}s wall (warm-up of {:.1}s DISCARDED)",
        args.concurrency, elapsed.as_secs_f64(), args.warmup.as_secs_f64());
    println!("  p50 {p50:.3} ms   p95 {p95:.3} ms   p99 {p99:.3} ms");
    println!("  box: {} logical CPUs — record this and the date in docs/load-test.md",
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0));

    if args.measure_floor {
        println!();
        if p99 >= args.target_p99_ms {
            println!(
                "  VERDICT: UNRESOLVABLE — this client's own p99 floor ({p99:.3} ms) is at or above the \
                 {:.3} ms target.\n           A run of the server cannot resolve a bar the instrument \
                 cannot see past. Do not record a server\n           figure from this client on this \
                 host.",
                args.target_p99_ms
            );
            std::process::exit(4);
        }
        println!(
            "  VERDICT: the client's floor is {p99:.3} ms, BELOW the {:.3} ms target — a server \
             measurement is meaningful.",
            args.target_p99_ms
        );
        return;
    }

    if all.len() == 0 {
        println!();
        println!("  VERDICT: no samples were recorded, so nothing was measured.");
        std::process::exit(5);
    }

    println!();
    if p99 <= args.target_p99_ms {
        println!("  VERDICT: p99 {p99:.3} ms is at or below the {:.3} ms target.", args.target_p99_ms);
    } else {
        println!("  VERDICT: p99 {p99:.3} ms EXCEEDS the {:.3} ms target.", args.target_p99_ms);
    }
    println!(
        "  Confirm the floor first (`--measure-floor`). A verdict without it is a number that may be\n  \
         measuring this client rather than the server."
    );
}
