//! Latency histograms shared by every worker of a run.
//!
//! A run used to give each connection its own HDR histograms for handshake
//! latency, round-trip time and response time, about 136 KiB each, and add
//! them up when the run ended. Memory then grew with the connection count:
//! 10,000 connections needed about 4 GB before any traffic.
//!
//! [`LatencyRecorder`] keeps one set of histograms per run instead. Each is
//! split into shards so that workers on different CPU cores rarely write to
//! the same cache line, and every count is an atomic incremented with relaxed
//! ordering, so recording takes no lock. The bucket layout is the one
//! `hdrhistogram` uses for the same bounds (1 µs to 60 s, three significant
//! figures), so moving the counts into an [`hdrhistogram::Histogram`] when
//! the run ends gives exactly the histogram per-connection recording would
//! have produced.

use hdrhistogram::Histogram;
use std::fmt;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Smallest recorded latency, in microseconds.
pub(crate) const LOWEST_US: u64 = 1;
/// Largest recorded latency, in microseconds (60 s).
pub(crate) const HIGHEST_US: u64 = 60_000_000;
/// Significant figures of every latency histogram.
pub(crate) const SIGFIG: u8 = 3;

// The layout `hdrhistogram` derives for LOWEST_US = 1 and SIGFIG = 3. Each
// power-of-two range of values is a bucket of 2048 sub-buckets, of which the
// upper 1024 are stored for every bucket after the first.
const SUB_BUCKET_COUNT_MAGNITUDE: u32 = 11;
const SUB_BUCKET_HALF_COUNT_MAGNITUDE: u32 = SUB_BUCKET_COUNT_MAGNITUDE - 1;
const SUB_BUCKET_HALF_COUNT: u64 = 1 << SUB_BUCKET_HALF_COUNT_MAGNITUDE;
const SUB_BUCKET_MASK: u64 = (1 << SUB_BUCKET_COUNT_MAGNITUDE) - 1;
const LEADING_ZERO_COUNT_BASE: u32 = 64 - SUB_BUCKET_COUNT_MAGNITUDE;
/// Buckets needed to reach HIGHEST_US: 2048 << 15 exceeds 60,000,000.
const BUCKET_COUNT: u64 = 16;
/// Number of counts, as in `hdrhistogram` for the same bounds.
const LEN: usize = ((BUCKET_COUNT + 1) << SUB_BUCKET_HALF_COUNT_MAGNITUDE) as usize;

/// The index of the count that `value` (within the bounds) is recorded in.
#[inline]
fn index_of(value: u64) -> usize {
    let bucket = LEADING_ZERO_COUNT_BASE - (value | SUB_BUCKET_MASK).leading_zeros();
    let sub_bucket = value >> bucket;
    (((u64::from(bucket) + 1) << SUB_BUCKET_HALF_COUNT_MAGNITUDE) + sub_bucket
        - SUB_BUCKET_HALF_COUNT) as usize
}

/// The lowest value recorded in the count at `index`.
fn value_at(index: usize) -> u64 {
    let index = index as u64;
    let bucket = (index >> SUB_BUCKET_HALF_COUNT_MAGNITUDE).saturating_sub(1);
    let mut sub_bucket = (index & (SUB_BUCKET_HALF_COUNT - 1)) + SUB_BUCKET_HALF_COUNT;
    if index >> SUB_BUCKET_HALF_COUNT_MAGNITUDE == 0 {
        sub_bucket -= SUB_BUCKET_HALF_COUNT;
    }
    sub_bucket << bucket
}

/// An empty histogram with the bounds every latency histogram uses.
pub(crate) fn latency_histogram() -> Option<Histogram<u64>> {
    Histogram::new_with_bounds(LOWEST_US, HIGHEST_US, SIGFIG).ok()
}

/// One latency distribution, recorded concurrently without locks.
struct AtomicHistogram {
    /// Counts per shard, allocated when the shard records its first value,
    /// so a distribution nobody records (response time in an unpaced run)
    /// costs nothing.
    shards: Box<[OnceLock<Box<[AtomicU64]>>]>,
}

impl AtomicHistogram {
    fn new(shards: usize) -> Self {
        Self {
            shards: (0..shards.max(1)).map(|_| OnceLock::new()).collect(),
        }
    }

    #[inline]
    fn record(&self, shard: usize, micros: u64) {
        let counts = self.shards[shard % self.shards.len()]
            .get_or_init(|| (0..LEN).map(|_| AtomicU64::new(0)).collect());
        counts[index_of(micros.clamp(LOWEST_US, HIGHEST_US))].fetch_add(1, Ordering::Relaxed);
    }

    /// Adds every recorded value to `hist`, which must use the latency
    /// histogram bounds.
    fn add_to(&self, hist: &mut Histogram<u64>) {
        for counts in self.shards.iter().filter_map(OnceLock::get) {
            for (index, count) in counts.iter().enumerate() {
                let count = count.load(Ordering::Relaxed);
                if count > 0 {
                    let _ = hist.record_n(value_at(index), count);
                }
            }
        }
    }
}

/// The handshake, round-trip and response-time distributions of one run,
/// written by all of its workers.
pub(crate) struct LatencyRecorder {
    handshake: AtomicHistogram,
    rtt: AtomicHistogram,
    response: AtomicHistogram,
    /// RTT and response time of each named scenario action, by the
    /// position of its name in the phase's action names.
    actions: Box<[ActionLatency]>,
}

/// The distributions of one action name. Its shards are allocated when a
/// worker first records into them, like the run's.
struct ActionLatency {
    name: String,
    rtt: AtomicHistogram,
    response: AtomicHistogram,
}

impl fmt::Debug for LatencyRecorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LatencyRecorder")
            .field("shards", &self.handshake.shards.len())
            .finish_non_exhaustive()
    }
}

impl LatencyRecorder {
    /// A recorder with one shard per CPU core, at most 64.
    pub(crate) fn new() -> Self {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        Self::with_shards(cores.min(64))
    }

    /// A recorder with `shards` shards, for a single worker or a test.
    pub(crate) fn with_shards(shards: usize) -> Self {
        Self {
            handshake: AtomicHistogram::new(shards),
            rtt: AtomicHistogram::new(shards),
            response: AtomicHistogram::new(shards),
            actions: Box::default(),
        }
    }

    /// Adds a pair of distributions for each of `names`, the named actions
    /// of a scenario phase.
    #[cfg(any(feature = "scenario", all(test, feature = "distributed")))]
    pub(crate) fn with_actions(mut self, names: &[String]) -> Self {
        let shards = self.rtt.shards.len();
        self.actions = names
            .iter()
            .map(|name| ActionLatency {
                name: name.clone(),
                rtt: AtomicHistogram::new(shards),
                response: AtomicHistogram::new(shards),
            })
            .collect();
        self
    }

    /// The names of the actions, by position.
    pub(crate) fn action_names(&self) -> impl Iterator<Item = &str> {
        self.actions.iter().map(|action| action.name.as_str())
    }

    #[inline]
    pub(crate) fn record_action_rtt(&self, action: usize, shard: usize, micros: u64) {
        if let Some(action) = self.actions.get(action) {
            action.rtt.record(shard, micros);
        }
    }

    #[inline]
    pub(crate) fn record_action_response(&self, action: usize, shard: usize, micros: u64) {
        if let Some(action) = self.actions.get(action) {
            action.response.record(shard, micros);
        }
    }

    /// Adds the distributions of action `index` to `rtt` and `response`.
    pub(crate) fn add_action_to(
        &self,
        index: usize,
        rtt: &mut Histogram<u64>,
        response: &mut Histogram<u64>,
    ) {
        if let Some(action) = self.actions.get(index) {
            action.rtt.add_to(rtt);
            action.response.add_to(response);
        }
    }

    #[inline]
    pub(crate) fn record_handshake(&self, shard: usize, micros: u64) {
        self.handshake.record(shard, micros);
    }

    #[inline]
    pub(crate) fn record_rtt(&self, shard: usize, micros: u64) {
        self.rtt.record(shard, micros);
    }

    #[inline]
    pub(crate) fn record_response(&self, shard: usize, micros: u64) {
        self.response.record(shard, micros);
    }

    /// Adds the recorded distributions to the run's totals.
    pub(crate) fn add_to(
        &self,
        handshake: &mut Histogram<u64>,
        rtt: &mut Histogram<u64>,
        response: &mut Histogram<u64>,
    ) {
        self.handshake.add_to(handshake);
        self.rtt.add_to(rtt);
        self.response.add_to(response);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference() -> Histogram<u64> {
        latency_histogram().expect("bounds")
    }

    #[test]
    fn the_layout_matches_hdrhistogram() {
        assert_eq!(reference().distinct_values(), LEN);
        assert!(index_of(HIGHEST_US) < LEN);
        assert_eq!(index_of(LOWEST_US), 1);
    }

    #[test]
    fn every_value_lands_in_the_count_hdrhistogram_uses() {
        let hist = reference();
        let mut check = |value: u64| {
            assert_eq!(
                value_at(index_of(value)),
                hist.lowest_equivalent(value),
                "value {value}"
            );
        };
        (1..=300_000).for_each(&mut check);
        let mut value = 300_000u64;
        while value <= HIGHEST_US {
            check(value);
            check(value - 1);
            check(value + 1);
            value = value * 17 / 16;
        }
        check(HIGHEST_US);
    }

    #[test]
    fn recorded_histograms_equal_direct_recording() {
        let recorder = LatencyRecorder::with_shards(3);
        let mut direct = [reference(), reference(), reference()];
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for i in 0..50_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let micros = 1 + seed % (if i % 10 == 0 { HIGHEST_US } else { 50_000 });
            let shard = i % 5;
            match i % 3 {
                0 => {
                    recorder.record_handshake(shard, micros);
                    direct[0].record(micros).unwrap();
                }
                1 => {
                    recorder.record_rtt(shard, micros);
                    direct[1].record(micros).unwrap();
                }
                _ => {
                    recorder.record_response(shard, micros);
                    direct[2].record(micros).unwrap();
                }
            }
        }
        let [mut h, mut r, mut p] = [reference(), reference(), reference()];
        recorder.add_to(&mut h, &mut r, &mut p);
        assert_eq!(h, direct[0]);
        assert_eq!(r, direct[1]);
        assert_eq!(p, direct[2]);
        assert_eq!(h.value_at_quantile(0.99), direct[0].value_at_quantile(0.99));
    }

    #[test]
    fn values_outside_the_bounds_are_clamped() {
        let recorder = LatencyRecorder::with_shards(1);
        recorder.record_rtt(0, 0);
        recorder.record_rtt(0, u64::MAX);
        let [mut h, mut r, mut p] = [reference(), reference(), reference()];
        recorder.add_to(&mut h, &mut r, &mut p);
        assert_eq!(r.len(), 2);
        assert_eq!(r.min(), LOWEST_US);
        assert_eq!(r.max(), r.highest_equivalent(HIGHEST_US));
        assert!(h.is_empty() && p.is_empty());
    }

    #[test]
    fn concurrent_recording_loses_no_samples() {
        let recorder = std::sync::Arc::new(LatencyRecorder::with_shards(2));
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let recorder = std::sync::Arc::clone(&recorder);
                std::thread::spawn(move || {
                    for i in 0..10_000u64 {
                        recorder.record_rtt(t, 100 + i % 7);
                    }
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        let [mut h, mut r, mut p] = [reference(), reference(), reference()];
        recorder.add_to(&mut h, &mut r, &mut p);
        assert_eq!(r.len(), 80_000);
    }
}
