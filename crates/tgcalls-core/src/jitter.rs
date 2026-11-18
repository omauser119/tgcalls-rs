//! Bounded, wrap-aware reorder queue and RFC 3550 interarrival jitter estimator.
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const MIN_DELAY_MS: u32 = 20;
const MAX_DELAY_MS: u32 = 200;

pub struct JitterBuffer<T> {
    packets: BTreeMap<i64, T>,
    next: Option<i64>,
    highest: i64,
    previous: Option<(Instant, u32)>,
    jitter: f64,
    target_ms: u32,
    start: Option<Instant>,
    last_pop: Option<Instant>,
    applied_delay_ms: u32,
    frame_ms: u32,
    pub dropped: u64,
}

impl<T> Default for JitterBuffer<T> {
    fn default() -> Self {
        Self {
            packets: BTreeMap::new(),
            next: None,
            highest: 0,
            previous: None,
            jitter: 0.0,
            target_ms: MIN_DELAY_MS,
            start: None,
            last_pop: None,
            applied_delay_ms: MIN_DELAY_MS,
            frame_ms: 20,
            dropped: 0,
        }
    }
}

impl<T> JitterBuffer<T> {
    pub fn push(&mut self, sequence: u16, timestamp: u32, now: Instant, packet: T) {
        let extended = if self.next.is_none() {
            i64::from(sequence)
        } else {
            self.highest + i64::from(sequence.wrapping_sub(self.highest as u16) as i16)
        };
        let duplicate = self.packets.contains_key(&extended);
        if !duplicate {
            // Include late packets: sustained delay must be able to increase the target
            // even when the corresponding packet missed its original playout deadline.
            if let Some((last_arrival, last_ts)) = self.previous {
                let arrival_delta =
                    now.saturating_duration_since(last_arrival).as_secs_f64() * 48_000.0;
                let media_delta = f64::from(timestamp.wrapping_sub(last_ts) as i32);
                self.jitter += ((arrival_delta - media_delta).abs() - self.jitter) / 16.0;
                self.update_target();
            }
            self.previous = Some((now, timestamp));
        }
        if let Some(next) = self.next {
            if extended < next || extended > next + 127 || duplicate {
                self.dropped += 1;
                return;
            }
        } else {
            self.next = Some(extended);
            self.highest = extended;
            self.start = Some(now);
        }
        self.highest = self.highest.max(extended);
        self.packets.insert(extended, packet);
    }

    /// None before prebuffering; Some(None) requests concealment for a missing sequence.
    pub fn pop(&mut self, now: Instant) -> Option<Option<T>> {
        if now < self.ready_at()? {
            return None;
        }
        let mut next = self.next?;
        if self.last_pop.is_some() {
            if self.target_ms > self.applied_delay_ms {
                self.applied_delay_ms = self.target_ms;
            } else if self.applied_delay_ms > self.target_ms {
                // Shrink accumulated delay by bounded frame discard. A full WSOLA/time-
                // stretch implementation is outside this initial jitter controller.
                let skip = ((self.applied_delay_ms - self.target_ms) / self.frame_ms)
                    .min(self.packets.len().saturating_sub(1) as u32);
                next += i64::from(skip);
                let before = self.packets.len();
                self.packets.retain(|sequence, _| *sequence >= next);
                self.dropped += (before - self.packets.len()) as u64;
                self.applied_delay_ms -= skip * self.frame_ms;
            }
        } else {
            self.applied_delay_ms = self.target_ms;
        }
        // Stop concealing after 1s without packets; wait for a fresh stream instead of growing a gap forever.
        if next > self.highest + 50 {
            self.next = None;
            self.start = None;
            self.previous = None;
            self.last_pop = None;
            self.packets.clear();
            return None;
        }
        self.next = Some(next + 1);
        self.last_pop = Some(now);
        Some(self.packets.remove(&next))
    }

    pub(crate) fn ready_at(&self) -> Option<Instant> {
        let prebuffered = self.start? + Duration::from_millis(u64::from(self.target_ms));
        match self.last_pop {
            Some(last) if self.target_ms > self.applied_delay_ms => {
                let needed = self.frame_ms + self.target_ms - self.applied_delay_ms;
                Some(prebuffered.max(last + Duration::from_millis(u64::from(needed))))
            }
            _ => Some(prebuffered),
        }
    }

    pub fn target_delay_ms(&self) -> u32 {
        self.target_ms
    }

    pub fn set_frame_samples(&mut self, samples: usize) {
        let frame_ms = (samples.saturating_mul(1000) / crate::SAMPLE_RATE).clamp(2, 120) as u32;
        if frame_ms != self.frame_ms {
            self.frame_ms = frame_ms;
            self.update_target();
        }
    }

    fn update_target(&mut self) {
        // One audio frame of base buffering plus the observed jitter margin.
        self.target_ms = (f64::from(self.frame_ms) + 4.0 * self.jitter / 48.0)
            .clamp(f64::from(MIN_DELAY_MS), f64::from(MAX_DELAY_MS))
            as u32;
    }
}
