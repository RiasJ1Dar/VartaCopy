//! Bounded observations of newly written application bytes, independent of UI frames.
use std::{collections::VecDeque, time::Duration};
use vartacopy_core::recovery::{Phase, Progress};

pub const HISTORY_LIMIT: usize = 121;
pub const RATE_WINDOW: Duration = Duration::from_secs(2);
const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
const MIN_MEASUREMENT: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct Sample {
    pub at: Duration,
    pub written: u64,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub run_id: u64,
    pub written: u64,
    pub samples: VecDeque<Sample>,
    pub overflowed: bool,
    last_write: Option<Duration>,
}
impl Snapshot {
    pub fn rate_at(&self, now: Duration) -> Option<f64> {
        if self.overflowed || now < self.observed_at() {
            return None;
        }
        let last = self.last_write?;
        if now.saturating_sub(last) >= RATE_WINDOW {
            return Some(0.0);
        }
        let cutoff = now.saturating_sub(RATE_WINDOW);
        let anchor = self
            .samples
            .iter()
            .take_while(|s| s.at <= cutoff)
            .last()
            .or_else(|| self.samples.front())?;
        let elapsed = now.saturating_sub(anchor.at);
        if elapsed < MIN_MEASUREMENT {
            return None;
        }
        Some(self.written.saturating_sub(anchor.written) as f64 / elapsed.as_secs_f64())
    }
    pub fn observed_at(&self) -> Duration {
        self.samples.back().map(|s| s.at).unwrap_or_default()
    }

    /// ETA is for the current writing stream only, never the whole job or cleanup.
    pub fn stream_eta(&self, now: Duration, progress: &Progress) -> Option<Duration> {
        if progress.phase != Phase::BlockWritten || progress.total == 0 {
            return None;
        }
        let remaining = progress.total.checked_sub(progress.bytes)?;
        let rate = self.rate_at(now)?;
        if remaining == 0 || rate <= 0.0 || !rate.is_finite() {
            return None;
        }
        Duration::try_from_secs_f64(remaining as f64 / rate).ok()
    }

    /// A 30-second view of recorded counter deltas, plus an age-aware current rate.
    pub fn graph_at(&self, now: Duration) -> Vec<(f64, f64)> {
        if self.overflowed || now < self.observed_at() {
            return Vec::new();
        }
        let cutoff = now.saturating_sub(Duration::from_secs(30));
        let mut points = Vec::with_capacity(HISTORY_LIMIT + 1);
        for (index, sample) in self.samples.iter().enumerate().skip(1) {
            if sample.at < cutoff {
                continue;
            }
            let previous = &self.samples[index - 1];
            let elapsed = sample.at.saturating_sub(previous.at).as_secs_f64();
            if elapsed > 0.0 {
                points.push((
                    sample.at.as_secs_f64(),
                    sample.written.saturating_sub(previous.written) as f64 / elapsed,
                ));
            }
        }
        points.push((now.as_secs_f64(), self.rate_at(now).unwrap_or(0.0)));
        points
    }
}

pub struct Meter {
    snapshot: Snapshot,
}
impl Meter {
    pub fn new(run_id: u64) -> Self {
        Self {
            snapshot: Snapshot {
                run_id,
                written: 0,
                overflowed: false,
                last_write: None,
                samples: VecDeque::from([Sample {
                    at: Duration::ZERO,
                    written: 0,
                }]),
            },
        }
    }
    pub fn observe(&mut self, now: Duration, progress: &Progress) {
        let now = now.max(self.snapshot.samples.back().unwrap().at);
        if progress.phase == Phase::BlockWritten && progress.fresh_written_delta != 0 {
            match self
                .snapshot
                .written
                .checked_add(progress.fresh_written_delta)
            {
                Some(written) => self.snapshot.written = written,
                None => self.snapshot.overflowed = true,
            }
            self.snapshot.last_write = Some(now);
        }
        let sample = Sample {
            at: now,
            written: self.snapshot.written,
        };
        let len = self.snapshot.samples.len();
        if len > 1 && now.saturating_sub(self.snapshot.samples[len - 2].at) < SAMPLE_INTERVAL {
            *self.snapshot.samples.back_mut().unwrap() = sample;
        } else {
            self.snapshot.samples.push_back(sample);
            if self.snapshot.samples.len() > HISTORY_LIMIT {
                self.snapshot.samples.pop_front();
            }
        }
    }
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(phase: Phase, delta: u64, stream: usize, bytes: u64) -> Progress {
        Progress {
            job_id: 1,
            entry: stream,
            phase,
            bytes,
            total: 1000,
            fresh_written_delta: delta,
            stream_index: Some(stream),
        }
    }
    #[test]
    fn counts_only_real_writes_across_files_ads_resume_and_resets() {
        let mut meter = Meter::new(7);
        meter.observe(Duration::from_secs(1), &event(Phase::Checking, 0, 0, 900));
        assert_eq!(meter.snapshot().written, 0);
        meter.observe(
            Duration::from_secs(2),
            &event(Phase::BlockWritten, 100, 0, 1000),
        );
        meter.observe(
            Duration::from_millis(2500),
            &event(Phase::CheckpointCommitted, 0, 0, 1000),
        );
        meter.observe(
            Duration::from_secs(3),
            &event(Phase::BlockWritten, 50, 1, 50),
        );
        meter.observe(Duration::from_secs(4), &event(Phase::Verified, 0, 1, 1000));
        assert_eq!(meter.snapshot().written, 150);
        assert_eq!(meter.snapshot().run_id, 7);
        assert_eq!(meter.snapshot().rate_at(Duration::from_secs(6)), Some(0.0));
        let reset = Meter::new(8).snapshot();
        assert_eq!(reset.written, 0);
        assert_eq!(reset.run_id, 8);
        assert_eq!(reset.rate_at(Duration::from_secs(1)), None);
    }
    #[test]
    fn measured_rate_eta_stalls_and_phase_changes_use_injected_monotonic_time() {
        let mut meter = Meter::new(1);
        let writing = event(Phase::BlockWritten, 100, 0, 100);
        meter.observe(Duration::from_secs(1), &writing);
        let snapshot = meter.snapshot();
        assert_eq!(snapshot.rate_at(Duration::from_secs(1)), Some(100.0));
        assert_eq!(
            snapshot.stream_eta(Duration::from_secs(1), &writing),
            Some(Duration::from_secs(9))
        );
        assert_eq!(snapshot.rate_at(Duration::from_secs(4)), Some(0.0));
        assert_eq!(snapshot.stream_eta(Duration::from_secs(4), &writing), None);
        assert_eq!(
            snapshot.stream_eta(Duration::from_secs(1), &event(Phase::Checking, 0, 0, 100)),
            None
        );
        assert_eq!(
            snapshot.graph_at(Duration::from_secs(4)).last().unwrap().1,
            0.0
        );
    }
    #[test]
    fn history_and_sampling_are_bounded_and_repeated_ui_reads_never_count_bytes() {
        let mut meter = Meter::new(1);
        for tick in 1..1000 {
            meter.observe(
                Duration::from_millis(tick * 250),
                &event(Phase::BlockWritten, 1, 0, tick),
            );
        }
        let snapshot = meter.snapshot();
        assert!(snapshot.samples.len() <= HISTORY_LIMIT);
        for _ in 0..100 {
            snapshot.graph_at(Duration::from_secs(250));
        }
        assert_eq!(snapshot.written, 999);
        assert_eq!(meter.snapshot().written, 999);
    }

    #[test]
    fn future_observations_and_counter_overflow_never_produce_a_rate_or_eta() {
        let mut meter = Meter::new(1);
        let writing = event(Phase::BlockWritten, 100, 0, 100);
        meter.observe(Duration::from_secs(1), &writing);
        assert_eq!(meter.snapshot().rate_at(Duration::from_millis(500)), None);
        assert!(
            meter
                .snapshot()
                .graph_at(Duration::from_millis(500))
                .is_empty()
        );
        meter.observe(
            Duration::from_secs(2),
            &event(Phase::BlockWritten, u64::MAX, 0, 100),
        );
        let snapshot = meter.snapshot();
        assert!(snapshot.overflowed);
        assert_eq!(snapshot.rate_at(Duration::from_secs(2)), None);
        assert_eq!(snapshot.stream_eta(Duration::from_secs(2), &writing), None);
        assert!(snapshot.graph_at(Duration::from_secs(2)).is_empty());
    }
}
