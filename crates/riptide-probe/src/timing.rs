//! Arithmetic shared by the timed checks.

use std::time::Duration;

/// Timed runs kept per measurement, after one warm-up run that is thrown away.
pub const RUNS: usize = 5;

pub fn gib_per_s(bytes: u64, elapsed: Duration) -> f64 {
    bytes as f64 / (1u64 << 30) as f64 / elapsed.as_secs_f64()
}

pub fn best(samples: &[Duration]) -> Duration {
    *samples.iter().min().expect("at least one sample")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_gib_in_half_a_second_is_two_gib_per_second() {
        assert_eq!(gib_per_s(1 << 30, Duration::from_millis(500)), 2.0);
    }

    #[test]
    fn best_is_the_shortest_sample() {
        let s = [
            Duration::from_millis(9),
            Duration::from_millis(4),
            Duration::from_millis(7),
        ];
        assert_eq!(best(&s), Duration::from_millis(4));
    }
}
