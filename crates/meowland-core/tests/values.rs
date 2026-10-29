//! Validated value types and latency summaries.

use std::time::Duration;

use meowland_core::{
    latency::Latency,
    pixels::{FrameSize, MAX_SURFACE_SIDE},
    scale::UiScale,
    timing::RefreshRate,
};

#[test]
fn frame_size_rejects_empty_and_oversized_dimensions() {
    assert_eq!(FrameSize::new(0, 10), None);
    assert_eq!(FrameSize::new(10, 0), None);
    assert!(FrameSize::new(MAX_SURFACE_SIDE, 1).is_some());
    assert_eq!(FrameSize::new(MAX_SURFACE_SIDE + 1, 1), None);
    assert_eq!(FrameSize::new(1, MAX_SURFACE_SIDE + 1), None);
}

#[test]
fn frame_size_enforces_the_pixel_budget() {
    assert!(FrameSize::new(4000, 4000).is_some());
    assert_eq!(FrameSize::new(4001, 4000), None);
    assert_eq!(FrameSize::new(4000, 4001), None);
    assert_eq!(FrameSize::new(MAX_SURFACE_SIDE, MAX_SURFACE_SIDE), None);
}

#[test]
fn frame_size_strides_follow_the_channel_count() {
    let size = FrameSize::new(3, 2).unwrap();

    assert_eq!(size.rgb_stride(), 9);
    assert_eq!(size.rgb_len(), 18);
    assert_eq!(size.rgba_stride(), 12);
    assert_eq!(size.rgba_len(), 24);
}

#[test]
fn frame_size_fits_optional_bounds() {
    let size = FrameSize::new(3, 2).unwrap();

    assert!(size.fits(None));
    assert!(size.fits(Some((3, 2))));
    assert!(size.fits(Some((4, 3))));
    assert!(!size.fits(Some((2, 2))));
    assert!(!size.fits(Some((3, 1))));
}

#[test]
fn ui_scale_validates_the_supported_range() {
    for valid in [0.5, 1.0, 1.5, 2.0, 4.0] {
        assert!(UiScale::try_from(valid).is_ok(), "{valid}");
    }
    for invalid in [0.49, 4.01, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(UiScale::try_from(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn ui_scale_quantizes_and_rounds_physical_pixels() {
    let scale = UiScale::try_from(1.5).unwrap();
    assert!((scale.factor() - 1.5).abs() < f64::EPSILON);
    assert_eq!(scale.logical(3), 2);
    assert_eq!(scale.logical(4), 3);

    let scale = UiScale::try_from(1.004).unwrap();
    assert!((scale.factor() - 1.0).abs() < f64::EPSILON);

    let scale = UiScale::try_from(1.005).unwrap();
    assert!((scale.factor() - 121.0 / 120.0).abs() < f64::EPSILON);

    let half = UiScale::try_from(0.5).unwrap();
    assert_eq!(half.logical(0), 1);
    assert_eq!(half.logical(2), 4);
    assert_eq!(half.logical(3), 6);
    assert_eq!(half.logical(u32::MAX), i32::MAX);
}

#[test]
fn ui_scale_parses_the_same_way() {
    assert!(("1.5".parse::<UiScale>().unwrap().factor() - 1.5).abs() < f64::EPSILON);
    assert!("not a number".parse::<UiScale>().is_err());
    assert!("0.4".parse::<UiScale>().is_err());
    assert!((UiScale::default().factor() - 1.0).abs() < f64::EPSILON);
}

#[test]
fn refresh_rate_validates_and_converts_to_time() {
    assert!(RefreshRate::try_from(0).is_err());
    assert!(RefreshRate::try_from(1001).is_err());
    assert!(RefreshRate::try_from(1).is_ok());
    assert!(RefreshRate::try_from(1000).is_ok());

    let rate = RefreshRate::try_from(120).unwrap();
    assert_eq!(rate.millihertz(), 120_000);
    assert_eq!(rate.interval(), Duration::from_nanos(8_333_333));

    assert_eq!(
        RefreshRate::try_from(60).unwrap().interval(),
        Duration::from_nanos(16_666_666)
    );
    assert_eq!(
        RefreshRate::try_from(3).unwrap().interval(),
        Duration::from_nanos(333_333_333)
    );
    assert_eq!(
        RefreshRate::try_from(1000).unwrap().interval(),
        Duration::from_millis(1)
    );
    assert_eq!(RefreshRate::default().millihertz(), 120_000);
}

#[test]
fn latency_uses_nearest_rank_percentiles() {
    let mut latency = Latency::default();
    assert_eq!(latency.summarize().p50, 0);

    for micros in 1..=100 {
        latency.record(Duration::from_micros(micros));
    }
    let summary = latency.summarize();
    assert_eq!((summary.p50, summary.p95, summary.p99), (50, 95, 99));

    for micros in [100, 1, 50, 2] {
        latency.record(Duration::from_micros(micros));
    }
    let summary = latency.summarize();
    assert_eq!((summary.p50, summary.p95, summary.p99), (2, 100, 100));

    latency.record(Duration::from_micros(7));
    let summary = latency.summarize();
    assert_eq!((summary.p50, summary.p95, summary.p99), (7, 7, 7));
}

#[test]
fn latency_summaries_reset_the_window() {
    let mut latency = Latency::default();
    latency.record(Duration::from_micros(10));
    assert_eq!(latency.summarize().p50, 10);

    let summary = latency.summarize();
    assert_eq!((summary.p50, summary.p95, summary.p99), (0, 0, 0));
}

#[test]
fn latency_keeps_a_bounded_rolling_window() {
    let mut latency = Latency::default();
    for micros in 1..=2049 {
        latency.record(Duration::from_micros(micros));
    }
    let summary = latency.summarize();
    assert_eq!(summary.p50, 1025);
    assert_eq!(summary.p95, 1947);
    assert_eq!(summary.p99, 2029);
}

#[test]
fn latency_saturates_long_durations() {
    let mut latency = Latency::default();
    latency.record(Duration::MAX);

    let summary = latency.summarize();
    assert_eq!(
        (summary.p50, summary.p95, summary.p99),
        (u64::MAX, u64::MAX, u64::MAX)
    );
}
