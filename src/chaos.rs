//! Client-side fault injection (`--chaos-*`): delayed, dropped and corrupted
//! data sends, connections closed by the client, and a client that reads
//! slowly.
//!
//! Every decision is drawn from a small seeded generator owned by each
//! worker, so a run with `--chaos-seed` makes the same decisions in the same
//! order on every connection slot. What each injected event does to every
//! metric is defined in METHODOLOGY.md, section 13.

use crate::error::{ConfigError, Result};
use crate::worker::SendPlan;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio_tungstenite::tungstenite::protocol::Message;

/// How a chaos close ends a connection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ChaosCloseMode {
    /// Closes the TCP connection without a WebSocket Close frame.
    #[default]
    Abrupt,
    /// Sends a Close frame with code 1001 (going away), then closes the
    /// TCP connection without waiting for the server's Close frame.
    Graceful,
}

/// Faults a run injects on the client side, built with
/// [`ChaosConfig::new`] and the `with_*` setters and attached with
/// [`LoadTestConfigBuilder::chaos`](crate::LoadTestConfigBuilder::chaos).
///
/// Percentages are of data sends (drop, corrupt) or of replies, or of sends
/// in stream mode (close), from 0 to 100. Injected events are counted in
/// [`AggregatedMetrics::chaos`](crate::AggregatedMetrics::chaos) and never
/// counted as errors.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct ChaosConfig {
    /// Pause before every data send.
    pub delay: Duration,
    /// Extra pause drawn uniformly from 0 to this, added to `delay`.
    pub jitter: Duration,
    /// Percentage of data sends skipped.
    pub drop_percent: f64,
    /// Percentage of data sends whose payload is corrupted.
    pub corrupt_percent: f64,
    /// Percentage of replies (sends in stream mode) after which the client
    /// closes the connection.
    pub close_percent: f64,
    /// How a chaos close ends the connection.
    pub close_mode: ChaosCloseMode,
    /// Pause after reading every data frame, before reading the next: a
    /// slow consumer.
    pub read_pause: Duration,
    /// Extra read pause drawn uniformly from 0 to this, added to
    /// `read_pause`.
    pub read_jitter: Duration,
    /// Seed of the generator behind every decision. Without one, the run
    /// picks a seed and reports it.
    pub seed: Option<u64>,
}

impl ChaosConfig {
    /// No faults: every knob is off.
    pub fn new() -> Self {
        Self::default()
    }

    /// Pauses `delay`, plus up to `jitter` drawn uniformly, before every
    /// data send.
    #[must_use]
    pub fn with_delay(mut self, delay: Duration, jitter: Duration) -> Self {
        self.delay = delay;
        self.jitter = jitter;
        self
    }

    /// Skips `percent` of data sends.
    #[must_use]
    pub fn with_drop(mut self, percent: f64) -> Self {
        self.drop_percent = percent;
        self
    }

    /// Corrupts the payload of `percent` of data sends.
    #[must_use]
    pub fn with_corrupt(mut self, percent: f64) -> Self {
        self.corrupt_percent = percent;
        self
    }

    /// Closes the connection after `percent` of replies (of sends in stream
    /// mode), the way `mode` says.
    #[must_use]
    pub fn with_close(mut self, percent: f64, mode: ChaosCloseMode) -> Self {
        self.close_percent = percent;
        self.close_mode = mode;
        self
    }

    /// Pauses `pause`, plus up to `jitter` drawn uniformly, after reading
    /// every data frame and before reading the next, so frames the server
    /// sends meanwhile wait in the socket buffers: a slow consumer.
    #[must_use]
    pub fn with_read_pause(mut self, pause: Duration, jitter: Duration) -> Self {
        self.read_pause = pause;
        self.read_jitter = jitter;
        self
    }

    /// Seeds the generator, for a run that repeats the same decisions.
    #[must_use]
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = Some(seed);
        self
    }

    /// Whether any knob injects something.
    pub(crate) fn is_active(&self) -> bool {
        self.acts_on_sends() || self.pauses_reads()
    }

    /// Whether a knob acts on data sends, which listen and GraphQL mode do
    /// not make.
    pub(crate) fn acts_on_sends(&self) -> bool {
        !self.delay.is_zero()
            || !self.jitter.is_zero()
            || self.drop_percent > 0.0
            || self.corrupt_percent > 0.0
            || self.close_percent > 0.0
    }

    /// Whether reads are paused, which works in every mode.
    pub(crate) fn pauses_reads(&self) -> bool {
        !self.read_pause.is_zero() || !self.read_jitter.is_zero()
    }

    /// Checks the percentages.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidOption`] for a percentage outside 0 to
    /// 100 and for drop and corrupt percentages that add up to more than
    /// 100: a send is either dropped or corrupted, never both.
    pub(crate) fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("drop", self.drop_percent),
            ("corrupt", self.corrupt_percent),
            ("close", self.close_percent),
        ] {
            if !(0.0..=100.0).contains(&value) {
                return Err(ConfigError::InvalidOption(format!(
                    "chaos {name} percentage must be between 0 and 100, received {value}"
                ))
                .into());
            }
        }
        if self.drop_percent + self.corrupt_percent > 100.0 {
            return Err(ConfigError::InvalidOption(format!(
                "chaos drop and corrupt percentages add up to {}; a send is either dropped or corrupted, so they can add up to at most 100",
                self.drop_percent + self.corrupt_percent
            ))
            .into());
        }
        Ok(())
    }

    /// This configuration with a seed: the given one, or one picked now.
    pub(crate) fn seeded(mut self) -> Self {
        self.seed = Some(self.seed.unwrap_or_else(time_seed));
        self
    }
}

/// Parses `DURATION[:JITTER]`, the value of `--chaos-delay` and
/// `--chaos-read-pause`.
///
/// # Errors
///
/// Returns the duration parser's error for either part.
pub(crate) fn parse_delay(raw: &str) -> Result<(Duration, Duration)> {
    let (delay, jitter) = match raw.split_once(':') {
        Some((delay, jitter)) => (delay, Some(jitter)),
        None => (raw, None),
    };
    let delay = crate::config::parse_human_duration(delay)?;
    let jitter = jitter
        .map(crate::config::parse_human_duration)
        .transpose()?
        .unwrap_or_default();
    Ok((delay, jitter))
}

/// Mixed into the seed of the read-pause generator, so it draws a sequence
/// apart from the send decisions'.
const READ_STREAM: u64 = 0x7265_6164_7061_7573;

/// One worker's chaos settings and generator.
#[derive(Debug, Clone)]
pub(crate) struct ChaosState {
    config: ChaosConfig,
    rng: XorShift64,
    /// Draws the read-pause jitter. Reads interleave with sends in an order
    /// that depends on timing, so they draw from their own generator and
    /// the send decisions of a seeded run stay the same with or without
    /// read pauses.
    read_rng: XorShift64,
}

impl ChaosState {
    /// The state of connection slot `worker_id`. Each slot draws its own
    /// sequence from the run's seed, so a seeded run repeats every slot's
    /// decisions whatever the scheduling.
    pub(crate) fn new(config: &ChaosConfig, worker_id: usize) -> Self {
        let seed = config.seed.unwrap_or_else(time_seed);
        Self {
            config: config.clone(),
            rng: XorShift64::new(seed ^ splitmix64(worker_id as u64)),
            read_rng: XorShift64::new(
                splitmix64(seed ^ READ_STREAM) ^ splitmix64(worker_id as u64),
            ),
        }
    }

    /// Draws the plan of the next data send. Every send draws in the same
    /// order (jitter, then drop or corrupt), and only for knobs that are
    /// set.
    pub(crate) fn plan_send(&mut self) -> SendPlan {
        let mut plan = SendPlan {
            delay: self.config.delay,
            ..SendPlan::default()
        };
        if !self.config.jitter.is_zero() {
            let jitter = self.config.jitter.as_nanos().min(u128::from(u64::MAX)) as u64;
            plan.delay += Duration::from_nanos(self.rng.at_most(jitter));
        }
        let (drop, corrupt) = (self.config.drop_percent, self.config.corrupt_percent);
        if drop > 0.0 || corrupt > 0.0 {
            let draw = self.rng.next_f64() * 100.0;
            plan.drop = draw < drop;
            plan.corrupt = !plan.drop && draw < drop + corrupt;
        }
        plan
    }

    /// How long to pause after reading a data frame; `None` without
    /// `--chaos-read-pause`.
    pub(crate) fn read_pause(&mut self) -> Option<Duration> {
        if !self.config.pauses_reads() {
            return None;
        }
        let mut pause = self.config.read_pause;
        if !self.config.read_jitter.is_zero() {
            let jitter = self.config.read_jitter.as_nanos().min(u128::from(u64::MAX)) as u64;
            pause += Duration::from_nanos(self.read_rng.at_most(jitter));
        }
        Some(pause)
    }

    /// Whether to close the connection now, after a reply (after a send in
    /// stream mode); `Some(graceful)` when it closes.
    pub(crate) fn close_now(&mut self) -> Option<bool> {
        if self.config.close_percent <= 0.0 {
            return None;
        }
        (self.rng.next_f64() * 100.0 < self.config.close_percent)
            .then_some(self.config.close_mode == ChaosCloseMode::Graceful)
    }

    /// Corrupts one byte of `msg`. Returns the frame to send and whether it
    /// was changed; an empty payload cannot be corrupted and is sent as is.
    ///
    /// A text frame keeps its length and stays valid UTF-8: a randomly
    /// chosen ASCII character is replaced by another printable ASCII
    /// character. A text frame without any ASCII character is sent as a
    /// binary frame of its bytes with one bit flipped. In a binary frame one
    /// bit of a randomly chosen byte is flipped.
    pub(crate) fn corrupt(&mut self, msg: Message) -> (Message, bool) {
        match msg {
            Message::Text(text) => {
                let bytes = text.as_bytes();
                let ascii = bytes.iter().filter(|b| b.is_ascii()).count();
                if ascii == 0 {
                    if bytes.is_empty() {
                        return (Message::Text(text), false);
                    }
                    let flipped = self.flip_bit(bytes);
                    return (Message::Binary(flipped), true);
                }
                let nth = self.rng.at_most(ascii as u64 - 1) as usize;
                let mut out = bytes.to_vec();
                let Some(pos) = out
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| b.is_ascii())
                    .map(|(i, _)| i)
                    .nth(nth)
                else {
                    return (Message::Text(text), false);
                };
                out[pos] = other_printable(out[pos], self.rng.at_most(93) as u8);
                match String::from_utf8(out) {
                    Ok(corrupted) => (Message::Text(corrupted.into()), true),
                    Err(_) => (Message::Text(text), false),
                }
            }
            Message::Binary(bytes) if !bytes.is_empty() => {
                let flipped = self.flip_bit(&bytes);
                (Message::Binary(flipped), true)
            }
            other => (other, false),
        }
    }

    fn flip_bit(&mut self, bytes: &[u8]) -> Bytes {
        let mut out = bytes.to_vec();
        let pos = self.rng.at_most(out.len() as u64 - 1) as usize;
        out[pos] ^= 1 << self.rng.at_most(7);
        Bytes::from(out)
    }
}

/// A printable ASCII character (0x20 to 0x7E) other than `original`;
/// `offset` (0 to 93) picks which of the 94 others.
fn other_printable(original: u8, offset: u8) -> u8 {
    let base = original.clamp(0x20, 0x7e) - 0x20;
    0x20 + (base + 1 + offset % 94) % 95
}

/// Marsaglia's xorshift64, as in the mock server: fast, small and good
/// enough for fault draws. Not for anything that needs real randomness.
#[derive(Debug, Clone)]
struct XorShift64(u64);

impl XorShift64 {
    fn new(seed: u64) -> Self {
        // A zero state would stay zero; any other value works.
        Self(splitmix64(seed).max(1))
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Uniform in `[0, 1)`.
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in `0..=max`, up to a modulo bias below 2^-64 * max.
    fn at_most(&mut self, max: u64) -> u64 {
        match max.checked_add(1) {
            Some(bound) => self.next_u64() % bound,
            None => self.next_u64(),
        }
    }
}

/// The seed agent `agent` (its position in `--agents`, from 0) of a
/// distributed run draws its decisions from, derived from the run's seed.
///
/// Every agent numbers its connection slots from 0, so with the run's seed
/// itself every agent would make the same decisions on its slot 0, its slot
/// 1 and so on. Mixing in the agent's position gives each agent sequences of
/// its own, and a seeded run still repeats every agent's decisions.
#[cfg(feature = "distributed")]
pub(crate) fn agent_seed(seed: u64, agent: usize) -> u64 {
    splitmix64(seed ^ splitmix64((agent as u64).wrapping_add(1)).rotate_left(17))
}

/// Spreads a seed's bits so nearby seeds give unrelated sequences.
fn splitmix64(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn time_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    splitmix64(nanos ^ u64::from(std::process::id()).rotate_left(32))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(config: ChaosConfig) -> ChaosState {
        ChaosState::new(&config.with_seed(7), 0)
    }

    #[test]
    fn percentages_are_checked() {
        assert!(ChaosConfig::new().with_drop(100.0).validate().is_ok());
        for bad in [-1.0, 100.5, f64::NAN, f64::INFINITY] {
            assert!(
                ChaosConfig::new().with_drop(bad).validate().is_err(),
                "{bad}"
            );
            assert!(ChaosConfig::new().with_corrupt(bad).validate().is_err());
            assert!(
                ChaosConfig::new()
                    .with_close(bad, ChaosCloseMode::Abrupt)
                    .validate()
                    .is_err()
            );
        }
        let err = ChaosConfig::new()
            .with_drop(60.0)
            .with_corrupt(50.0)
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("add up to 110"), "{err}");
    }

    #[test]
    fn delays_parse_with_an_optional_jitter() {
        assert_eq!(
            parse_delay("5ms").unwrap(),
            (Duration::from_millis(5), Duration::ZERO)
        );
        assert_eq!(
            parse_delay("5ms:2ms").unwrap(),
            (Duration::from_millis(5), Duration::from_millis(2))
        );
        assert!(parse_delay("5ms:").is_err());
        assert!(parse_delay("fast").is_err());
    }

    #[test]
    fn drop_and_corrupt_shares_follow_the_percentages() {
        let mut chaos = state(ChaosConfig::new().with_drop(20.0).with_corrupt(30.0));
        let plans: Vec<SendPlan> = (0..20_000).map(|_| chaos.plan_send()).collect();
        let dropped = plans.iter().filter(|p| p.drop).count();
        let corrupted = plans.iter().filter(|p| p.corrupt).count();
        assert!(plans.iter().all(|p| !(p.drop && p.corrupt)));
        assert!((3_600..4_400).contains(&dropped), "{dropped}");
        assert!((5_500..6_500).contains(&corrupted), "{corrupted}");
    }

    #[test]
    fn a_seed_repeats_every_decision_and_slots_differ() {
        let config = ChaosConfig::new()
            .with_delay(Duration::from_millis(1), Duration::from_millis(4))
            .with_drop(10.0)
            .with_corrupt(10.0)
            .with_close(5.0, ChaosCloseMode::Graceful)
            .with_seed(42);
        let draws = |slot| {
            let mut chaos = ChaosState::new(&config, slot);
            (0..200)
                .map(|_| (chaos.plan_send(), chaos.close_now()))
                .collect::<Vec<_>>()
        };
        assert_eq!(draws(3), draws(3));
        assert_ne!(draws(3), draws(4));
        let delays = draws(0);
        assert!(delays.iter().all(|(p, _)| {
            (Duration::from_millis(1)..=Duration::from_millis(5)).contains(&p.delay)
        }));
        assert!(delays.iter().any(|(_, close)| *close == Some(true)));
    }

    #[cfg(feature = "distributed")]
    #[test]
    fn agents_derive_distinct_repeatable_seeds() {
        let seeds: Vec<u64> = (0..64).map(|agent| agent_seed(42, agent)).collect();
        assert_eq!(
            seeds,
            (0..64)
                .map(|agent| agent_seed(42, agent))
                .collect::<Vec<_>>()
        );
        let distinct: std::collections::HashSet<u64> = seeds.iter().copied().collect();
        assert_eq!(distinct.len(), seeds.len());
        assert!(!seeds.contains(&42));
        assert_ne!(agent_seed(42, 0), agent_seed(43, 0));

        // Slot 0 of two agents draws different sequences.
        let config = ChaosConfig::new().with_drop(50.0);
        let draws = |agent| {
            let mut chaos = ChaosState::new(&config.clone().with_seed(agent_seed(7, agent)), 0);
            (0..64).map(|_| chaos.plan_send().drop).collect::<Vec<_>>()
        };
        assert_ne!(draws(0), draws(1));
    }

    #[test]
    fn read_pauses_stay_in_bounds_and_leave_send_decisions_unchanged() {
        let sends = ChaosConfig::new()
            .with_delay(Duration::ZERO, Duration::from_millis(4))
            .with_drop(30.0)
            .with_seed(9);
        let with_reads = sends
            .clone()
            .with_read_pause(Duration::from_millis(10), Duration::from_millis(5));
        let mut plain = ChaosState::new(&sends, 2);
        let mut slow = ChaosState::new(&with_reads, 2);
        assert_eq!(plain.read_pause(), None);
        for _ in 0..200 {
            // Reads interleave with sends; the send decisions must not move.
            let pause = slow.read_pause().unwrap();
            assert!((Duration::from_millis(10)..=Duration::from_millis(15)).contains(&pause));
            assert_eq!(slow.plan_send(), plain.plan_send());
        }
        assert!(with_reads.is_active() && with_reads.pauses_reads());
        assert!(
            !ChaosConfig::new()
                .with_read_pause(Duration::from_millis(1), Duration::ZERO)
                .acts_on_sends()
        );
    }

    #[test]
    fn knobs_that_are_off_draw_nothing() {
        let mut chaos =
            state(ChaosConfig::new().with_delay(Duration::from_millis(3), Duration::ZERO));
        let before = chaos.rng.0;
        assert_eq!(
            chaos.plan_send(),
            SendPlan {
                delay: Duration::from_millis(3),
                drop: false,
                corrupt: false
            }
        );
        assert_eq!(chaos.close_now(), None);
        assert_eq!(chaos.rng.0, before);
    }

    #[test]
    fn corrupted_text_stays_valid_utf8_of_the_same_length() {
        let mut chaos = state(ChaosConfig::new());
        for _ in 0..500 {
            let (msg, changed) = chaos.corrupt(Message::Text("héllo wörld {}".into()));
            assert!(changed);
            let Message::Text(text) = msg else {
                panic!("text stays text: {msg:?}");
            };
            assert_eq!(text.len(), "héllo wörld {}".len());
            assert_ne!(text.as_str(), "héllo wörld {}");
            assert!(text.contains('é') && text.contains('ö'));
            assert!(text.bytes().all(|b| !b.is_ascii_control()));
        }
    }

    #[test]
    fn frames_without_ascii_become_binary_and_empty_frames_are_left_alone() {
        let mut chaos = state(ChaosConfig::new());
        let (msg, changed) = chaos.corrupt(Message::Text("éé".into()));
        assert!(changed);
        let Message::Binary(bytes) = msg else {
            panic!("{msg:?}");
        };
        assert_eq!(bytes.len(), 4);
        assert_ne!(&bytes[..], "éé".as_bytes());

        let (msg, changed) = chaos.corrupt(Message::Binary(Bytes::from_static(b"\x00\x01")));
        assert!(changed);
        let Message::Binary(bytes) = msg else {
            panic!("{msg:?}");
        };
        let differing: u32 = bytes
            .iter()
            .zip([0u8, 1])
            .map(|(a, b)| (a ^ b).count_ones())
            .sum();
        assert_eq!(differing, 1);

        for empty in [Message::Text("".into()), Message::Binary(Bytes::new())] {
            let (msg, changed) = chaos.corrupt(empty.clone());
            assert!(!changed);
            assert_eq!(msg, empty);
        }
    }

    #[test]
    fn replacement_characters_are_printable_and_different() {
        for original in 0x20..=0x7eu8 {
            for offset in 0..94 {
                let replaced = other_printable(original, offset);
                assert_ne!(replaced, original);
                assert!((0x20..=0x7e).contains(&replaced));
            }
        }
    }
}
