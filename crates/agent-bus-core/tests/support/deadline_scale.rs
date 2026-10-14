//! Wall-clock scaling for deadline-bound unit tests.
//!
//! Included by path (`#[path = ...] mod deadline_scale;`) from test modules whose
//! fixtures wait on real time. The Windows cross job runs the unit tests under Wine
//! on a shared host whose load average was measured at 31 to 39; process start-up
//! and the first loopback connection there took longer than the fixtures' unloaded
//! deadlines, so a correct implementation failed one different timing test per run.
//!
//! `AGENT_BUS_TEST_DEADLINE_SCALE` (an integer from 1 to 20, default 1) multiplies
//! those deadlines. The scale only widens how long a test waits for something that
//! must happen; it never changes what a test asserts. The Wine job sets it in
//! `scripts/ci/wine_job.py` (the variable keeps the `AGENT_BUS_` prefix because the
//! Wine guest environment forwards only that prefix). Native runs leave it unset.

use std::time::Duration;

pub const SCALE_VAR: &str = "AGENT_BUS_TEST_DEADLINE_SCALE";
const MAX_SCALE: u32 = 20;

/// The multiplier for this process; 1 when unset, unparsable or out of range.
pub fn scale() -> u32 {
    parse(std::env::var(SCALE_VAR).ok().as_deref())
}

fn parse(value: Option<&str>) -> u32 {
    value
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .filter(|scale| (1..=MAX_SCALE).contains(scale))
        .unwrap_or(1)
}

/// `base` stretched by the configured scale.
pub fn scaled(base: Duration) -> Duration {
    base * scale()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_defaults_to_one_and_rejects_out_of_range_values() {
        assert_eq!(parse(None), 1);
        assert_eq!(parse(Some("")), 1);
        assert_eq!(parse(Some("abc")), 1);
        assert_eq!(parse(Some("0")), 1);
        assert_eq!(parse(Some("21")), 1);
        assert_eq!(parse(Some("-3")), 1);
        assert_eq!(parse(Some("5")), 5);
        assert_eq!(parse(Some(" 20 ")), 20);
    }

    #[test]
    fn scaled_multiplies_the_base_duration() {
        assert_eq!(Duration::from_secs(2) * 5, Duration::from_secs(10));
        assert_eq!(
            scaled(Duration::from_millis(100)),
            Duration::from_millis(100) * scale()
        );
    }
}
