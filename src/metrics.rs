//! Lightweight, dependency-free request metrics (task_1380): per-endpoint latency
//! percentiles, an in-flight gauge, a throughput basis, and status-class counts, exposed at
//! `GET /api/metrics` so the fleet can see and optimize board performance (cameron reported
//! requests hanging and wanted instrumentation to optimize the fleet itself).
//!
//! Design: cheap hot path. Recording is lock-free atomics on a per-endpoint `Arc<Endpoint>`;
//! the only lock is a brief `Mutex<HashMap>` get-or-insert to find that Arc the first time an
//! endpoint key is seen (then released before the atomic math). Latency is kept as coarse,
//! fixed, log-spaced millisecond buckets from which p50/p90/p99 are derived by cumulative count,
//! so percentiles are APPROXIMATE -- good enough to spot a slow or hung endpoint without the
//! cost/complexity of exact histograms, and with no new dependency.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use serde_json::{json, Map, Value};

/// Coarse, fixed, log-spaced latency bucket UPPER EDGES in milliseconds. A sample lands in the
/// first bucket whose edge it is `<=`; anything slower than the last edge lands in the implicit
/// trailing +inf bucket. p50/p90/p99 are the upper edge of the bucket the cumulative count
/// crosses (saturating at the last concrete edge for the +inf bucket), so a reported pNN means
/// "about this many ms or faster" -- approximate by construction.
const BUCKET_EDGES_MS: [u64; 12] = [1, 2, 5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000];
/// One counter per edge, plus the trailing +inf overflow bucket.
const NBUCKETS: usize = BUCKET_EDGES_MS.len() + 1;

/// Per-endpoint counters. All atomics so the record path is lock-free once the `Arc` is in hand.
struct Endpoint {
    total: AtomicU64,
    status_2xx: AtomicU64,
    status_4xx: AtomicU64,
    status_5xx: AtomicU64,
    /// 1xx/3xx and anything not 2/4/5 -- kept so the class counts always sum to `total`.
    status_other: AtomicU64,
    /// Current concurrent requests in this endpoint's handler; incremented on entry and
    /// decremented by a drop-guard so it never leaks on panic / early return / cancellation.
    in_flight: AtomicI64,
    /// High-water mark of `in_flight` observed.
    max_in_flight: AtomicI64,
    buckets: [AtomicU64; NBUCKETS],
}

impl Endpoint {
    fn new() -> Self {
        Endpoint {
            total: AtomicU64::new(0),
            status_2xx: AtomicU64::new(0),
            status_4xx: AtomicU64::new(0),
            status_5xx: AtomicU64::new(0),
            status_other: AtomicU64::new(0),
            in_flight: AtomicI64::new(0),
            max_in_flight: AtomicI64::new(0),
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// Record one completed request: bump total, its status class, and its latency bucket.
    fn observe(&self, elapsed_ms: u64, status: u16) {
        self.total.fetch_add(1, Ordering::Relaxed);
        let class = match status / 100 {
            2 => &self.status_2xx,
            4 => &self.status_4xx,
            5 => &self.status_5xx,
            _ => &self.status_other,
        };
        class.fetch_add(1, Ordering::Relaxed);
        let idx = BUCKET_EDGES_MS
            .iter()
            .position(|&edge| elapsed_ms <= edge)
            .unwrap_or(NBUCKETS - 1);
        self.buckets[idx].fetch_add(1, Ordering::Relaxed);
    }
}

/// Decrements an endpoint's in-flight gauge on drop, so a panic or cancellation mid-request can
/// never leak the gauge upward (which would otherwise read as a permanent phantom hang).
struct InFlightGuard {
    ep: Arc<Endpoint>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.ep.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The process-wide metrics registry, held behind an `Arc` shared by the recording middleware and
/// the `GET /api/metrics` handler.
pub struct Metrics {
    started: Instant,
    started_unix: u64,
    endpoints: Mutex<HashMap<String, Arc<Endpoint>>>,
}

impl Metrics {
    pub fn new() -> Self {
        Metrics {
            started: Instant::now(),
            started_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            endpoints: Mutex::new(HashMap::new()),
        }
    }

    /// Get-or-create the per-endpoint counters for `key`. Holds the map lock only for the lookup +
    /// `Arc` clone, never across the recording atomics.
    fn endpoint(&self, key: &str) -> Arc<Endpoint> {
        let mut map = self.endpoints.lock().unwrap();
        if let Some(e) = map.get(key) {
            return e.clone();
        }
        let e = Arc::new(Endpoint::new());
        map.insert(key.to_string(), e.clone());
        e
    }

    /// A JSON snapshot: a per-endpoint object plus an overall rollup, keyed by "METHOD /route"
    /// (route templates relative to the /api mount, e.g. "GET /documents/{document_id}", so ids do
    /// not explode cardinality). Mutates nothing.
    pub fn snapshot(&self) -> Value {
        let map = self.endpoints.lock().unwrap();
        let mut per = Map::new();
        let mut roll_total = 0u64;
        let mut roll_2xx = 0u64;
        let mut roll_4xx = 0u64;
        let mut roll_5xx = 0u64;
        let mut roll_other = 0u64;
        let mut roll_in_flight = 0i64;
        let mut roll_max_in_flight = 0i64;
        let mut roll_buckets = [0u64; NBUCKETS];

        for (key, ep) in map.iter() {
            let total = ep.total.load(Ordering::Relaxed);
            let s2 = ep.status_2xx.load(Ordering::Relaxed);
            let s4 = ep.status_4xx.load(Ordering::Relaxed);
            let s5 = ep.status_5xx.load(Ordering::Relaxed);
            let so = ep.status_other.load(Ordering::Relaxed);
            let in_flight = ep.in_flight.load(Ordering::Relaxed);
            let max_in_flight = ep.max_in_flight.load(Ordering::Relaxed);
            let mut buckets = [0u64; NBUCKETS];
            for (i, b) in ep.buckets.iter().enumerate() {
                buckets[i] = b.load(Ordering::Relaxed);
                roll_buckets[i] += buckets[i];
            }

            per.insert(
                key.clone(),
                json!({
                    "count": total,
                    "p50_ms": percentile(&buckets, total, 0.50),
                    "p90_ms": percentile(&buckets, total, 0.90),
                    "p99_ms": percentile(&buckets, total, 0.99),
                    "in_flight": in_flight,
                    "max_in_flight": max_in_flight,
                    "status_2xx": s2,
                    "status_4xx": s4,
                    "status_5xx": s5,
                    "status_other": so,
                }),
            );

            roll_total += total;
            roll_2xx += s2;
            roll_4xx += s4;
            roll_5xx += s5;
            roll_other += so;
            roll_in_flight += in_flight;
            roll_max_in_flight = roll_max_in_flight.max(max_in_flight);
        }

        json!({
            "started_unix": self.started_unix,
            "uptime_secs": self.started.elapsed().as_secs(),
            "latency_bucket_edges_ms": BUCKET_EDGES_MS,
            "overall": {
                "count": roll_total,
                "p50_ms": percentile(&roll_buckets, roll_total, 0.50),
                "p90_ms": percentile(&roll_buckets, roll_total, 0.90),
                "p99_ms": percentile(&roll_buckets, roll_total, 0.99),
                "in_flight": roll_in_flight,
                "max_in_flight": roll_max_in_flight,
                "status_2xx": roll_2xx,
                "status_4xx": roll_4xx,
                "status_5xx": roll_5xx,
                "status_other": roll_other,
            },
            "endpoints": per,
        })
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Approximate percentile (ms) from the cumulative bucket counts: the upper edge of the bucket in
/// which the nearest-rank target count falls, saturating at the last concrete edge for the +inf
/// overflow bucket. 0 when there are no samples.
fn percentile(buckets: &[u64; NBUCKETS], total: u64, p: f64) -> u64 {
    if total == 0 {
        return 0;
    }
    let target = (p * total as f64).ceil() as u64;
    let mut cum = 0u64;
    for (i, &c) in buckets.iter().enumerate() {
        cum += c;
        if cum >= target {
            return BUCKET_EDGES_MS
                .get(i)
                .copied()
                .unwrap_or_else(|| *BUCKET_EDGES_MS.last().unwrap());
        }
    }
    *BUCKET_EDGES_MS.last().unwrap()
}

/// Axum middleware that times each request and records it against its matched route template.
/// Added as a `route_layer` so [`MatchedPath`] is populated; unmatched paths (which have none)
/// are bucketed under a single sentinel key so random 404 URLs cannot explode cardinality.
pub async fn record_request_metrics(metrics: Arc<Metrics>, req: Request, next: Next) -> Response {
    let method = req.method().as_str().to_owned();
    let key = match req.extensions().get::<MatchedPath>() {
        Some(mp) => format!("{method} {}", mp.as_str()),
        None => format!("{method} <unmatched>"),
    };
    let ep = metrics.endpoint(&key);
    let now = ep.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
    ep.max_in_flight.fetch_max(now, Ordering::Relaxed);
    let _guard = InFlightGuard { ep: ep.clone() };
    let start = Instant::now();
    let resp = next.run(req).await;
    ep.observe(start.elapsed().as_millis() as u64, resp.status().as_u16());
    resp
}
