//! Per-step timing samples + percentile report (benchmark of every meaningful step).

use std::collections::BTreeMap;
use std::sync::Mutex;

/// Microsecond samples for one step (sign / encode / rpc / total …).
#[derive(Default)]
pub struct Samples {
    v: Vec<u128>,
}

impl Samples {
    pub fn push(&mut self, us: u128) {
        self.v.push(us);
    }

    pub fn extend(&mut self, other: Samples) {
        self.v.extend(other.v);
    }

    pub fn len(&self) -> usize {
        self.v.len()
    }

    /// One-line report: count, avg, min, p50, p95, p99, max.
    pub fn report(&self, name: &str) -> String {
        if self.v.is_empty() {
            return format!("{name}: (no samples)");
        }
        let mut s = self.v.clone();
        s.sort_unstable();
        let n = s.len();
        let pct = |p: f64| -> String {
            let idx = ((n as f64 - 1.0) * p).round() as usize;
            fmt_us(s[idx])
        };
        let sum: u128 = s.iter().sum();
        format!(
            "{name}: n={n} avg={} min={} p50={} p95={} p99={} max={}",
            fmt_us(sum / n as u128),
            fmt_us(s[0]),
            pct(0.50),
            pct(0.95),
            pct(0.99),
            fmt_us(s[n - 1]),
        )
    }
}

fn fmt_us(us: u128) -> String {
    if us >= 1_000_000 {
        format!("{:.2}ms", us as f64 / 1_000.0)
    } else if us >= 1_000 {
        format!("{:.2}ms", us as f64 / 1_000.0)
    } else {
        format!("{us}µs")
    }
}

/// All step timings collected by one worker, merged at the end.
#[derive(Default)]
pub struct StepSet {
    pub sign: Samples,
    pub encode: Samples,
    pub rpc: Samples,
    pub total: Samples,
}

/// Outcome tally with bounded error-reason histogram.
#[derive(Default)]
pub struct Tally {
    pub ok: u64,
    pub err: u64,
    reasons: BTreeMap<String, u64>,
}

impl Tally {
    pub fn record_ok(&mut self) {
        self.ok += 1;
    }

    pub fn record_err(&mut self, reason: &str) {
        self.err += 1;
        // keep bounded: first 160 chars, cap distinct reasons
        let key: String = reason.chars().take(160).collect();
        if self.reasons.len() < 32 || self.reasons.contains_key(&key) {
            *self.reasons.entry(key).or_insert(0) += 1;
        }
    }

    pub fn merge(&mut self, other: &Tally) {
        self.ok += other.ok;
        self.err += other.err;
        for (k, v) in &other.reasons {
            if self.reasons.len() < 32 || self.reasons.contains_key(k) {
                *self.reasons.entry(k.clone()).or_insert(0) += v;
            }
        }
    }

    pub fn reasons(&self) -> &BTreeMap<String, u64> {
        &self.reasons
    }
}

/// Cross-thread collector for a full run.
#[derive(Default)]
pub struct Collector {
    pub steps: StepSet,
    pub tally: Tally,
}

impl Collector {
    pub fn push_steps(
        &mut self,
        sign: u128,
        encode: u128,
        rpc: u128,
        total: u128,
    ) {
        self.steps.sign.push(sign);
        self.steps.encode.push(encode);
        self.steps.rpc.push(rpc);
        self.steps.total.push(total);
    }
}

/// Mutex-wrapped collector handle for worker threads.
pub type SharedCollector = Mutex<Collector>;

/// Print the standard benchmark report block.
pub fn print_report(
    title: &str,
    wall_s: f64,
    coll: &Collector,
) {
    println!("--- {title} ---");
    let total = coll.tally.ok + coll.tally.err;
    println!(
        "txs={total} accepted={} rejected={} wall={wall_s:.3}s",
        coll.tally.ok, coll.tally.err
    );
    if wall_s > 0.0 && total > 0 {
        println!(
            "SEND TPS (accepted)={:.0}  offered TPS={:.0}",
            coll.tally.ok as f64 / wall_s,
            total as f64 / wall_s
        );
    }
    println!("{}", coll.steps.sign.report("step sign  "));
    println!("{}", coll.steps.encode.report("step encode"));
    println!("{}", coll.steps.rpc.report("step rpc   "));
    println!("{}", coll.steps.total.report("step total "));
    if !coll.tally.reasons().is_empty() {
        println!("rejection reasons:");
        let mut rows: Vec<_> = coll.tally.reasons().iter().collect();
        rows.sort_by(|a, b| b.1.cmp(a.1));
        for (reason, cnt) in rows.iter().take(8) {
            println!("  {cnt:>8}  {reason}");
        }
    }
}
