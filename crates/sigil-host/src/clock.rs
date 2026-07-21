use std::time::{Duration, Instant};

/// One monotonic epoch shared by every media source in an active session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SessionClock {
    epoch: Instant,
}

impl SessionClock {
    pub(crate) fn start() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }

    pub(crate) fn now_micros(self) -> u64 {
        self.micros_at(Instant::now())
    }

    pub(crate) fn micros_at(self, observed_at: Instant) -> u64 {
        duration_micros(
            observed_at
                .checked_duration_since(self.epoch)
                .unwrap_or(Duration::ZERO),
        )
    }
}

fn duration_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

/// Add ticks from a fixed-rate media clock to a common-clock anchor.
pub(crate) fn anchored_timestamp_micros(
    anchor_micros: u64,
    elapsed_ticks: u64,
    ticks_per_second: u32,
) -> i64 {
    let elapsed_micros = elapsed_ticks
        .saturating_mul(1_000_000)
        .checked_div(u64::from(ticks_per_second))
        .unwrap_or(u64::MAX);
    i64::try_from(anchor_micros.saturating_add(elapsed_micros)).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_clock_is_nondecreasing() {
        let clock = SessionClock::start();
        let first = clock.now_micros();
        let second = clock.now_micros();
        assert!(second >= first);
    }

    #[test]
    fn session_clock_preserves_real_observation_gaps() {
        let epoch = Instant::now();
        let clock = SessionClock { epoch };
        assert_eq!(clock.micros_at(epoch), 0);
        assert_eq!(clock.micros_at(epoch + Duration::from_millis(750)), 750_000);
    }

    #[test]
    fn anchored_timestamps_preserve_epoch_and_fixed_rate_cadence() {
        assert_eq!(anchored_timestamp_micros(2_000_000, 0, 60), 2_000_000);
        assert_eq!(anchored_timestamp_micros(2_000_000, 1, 60), 2_016_666);
        assert_eq!(anchored_timestamp_micros(2_000_000, 3, 60), 2_050_000);
        assert_eq!(anchored_timestamp_micros(2_000_000, 960, 48_000), 2_020_000);
    }

    #[test]
    fn anchored_timestamp_conversion_saturates() {
        assert_eq!(anchored_timestamp_micros(u64::MAX, u64::MAX, 1), i64::MAX);
        assert_eq!(anchored_timestamp_micros(1, 1, 0), i64::MAX);
    }
}
