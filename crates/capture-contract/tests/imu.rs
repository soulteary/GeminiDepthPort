use capture_contract::imu::*;
use serde_json::{Value, json};
fn config() -> Value {
    json!({"enabled":true,"accel_rate_hz":200,"gyro_rate_hz":200,"accel_range_g":4,"gyro_range_dps":1000,"queue_capacity":128,"stale_timeout_ms":500,"accel_frame_id":"accel_frame","gyro_frame_id":"gyro_frame"})
}
#[test]
fn imu_config_is_explicit_bounded_and_rejects_unrecognized_profiles() {
    let good = config();
    for (k, bads) in [
        ("accel_rate_hz", vec![json!(0), json!(333)]),
        ("gyro_rate_hz", vec![json!(0), json!(1200)]),
        ("accel_range_g", vec![json!(0), json!(32)]),
        ("gyro_range_dps", vec![json!(1), json!(4000)]),
        ("queue_capacity", vec![json!(0), json!(129)]),
        ("stale_timeout_ms", vec![json!(99), json!(5001)]),
        ("accel_frame_id", vec![json!(""), json!("gyro_frame")]),
    ] {
        for bad in bads {
            let mut v = good.clone();
            v[k] = bad;
            assert!(
                serde_json::from_value::<ImuConfig>(v)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
    }
    let mut unknown = good.clone();
    unknown["automatic"] = json!(true);
    assert!(serde_json::from_value::<ImuConfig>(unknown).is_err());
    for rate in [25, 50, 100, 200, 400, 500, 800, 1000] {
        let mut v = good.clone();
        v["accel_rate_hz"] = json!(rate);
        serde_json::from_value::<ImuConfig>(v)
            .unwrap()
            .validate()
            .unwrap();
    }
}
#[test]
fn imu_progress_requires_finite_si_values_and_independent_monotonic_source_clocks() {
    let c: ImuConfig = serde_json::from_value(config()).unwrap();
    let mut progress = ImuProgress::default();
    assert!(!progress.has_both());
    let sample = ImuSample {
        kind: ImuKind::Accel,
        sequence: 1,
        device_timestamp_us: 100,
        system_timestamp_us: 1_700_000_000_000_000,
        global_timestamp_us: Some(1_700_000_000_000_001),
        xyz: [1., 2., 3.],
        frame_id: c.accel_frame_id.clone(),
    };
    progress.accept(&sample, &c).unwrap();
    assert!(!progress.has_both());
    assert!(progress.accept(&sample, &c).is_err());
    let mut gyro = sample.clone();
    gyro.kind = ImuKind::Gyro;
    gyro.frame_id = c.gyro_frame_id.clone();
    progress.accept(&gyro, &c).unwrap();
    assert!(progress.has_both());
    for fault in 0..9 {
        let mut s = sample.clone();
        s.sequence = 2;
        s.device_timestamp_us += 1;
        s.system_timestamp_us += 1;
        s.global_timestamp_us = Some(s.global_timestamp_us.unwrap() + 1);
        match fault {
            0 => s.xyz[0] = f64::NAN,
            1 => s.device_timestamp_us = 0,
            2 => s.system_timestamp_us = u64::MAX,
            3 => s.global_timestamp_us = Some(0),
            4 => s.global_timestamp_us = None,
            5 => s.sequence = 0,
            6 => s.frame_id = "wrong".into(),
            7 => s.system_timestamp_us -= 2,
            _ => s.global_timestamp_us = Some(u64::MAX),
        }
        assert!(progress.clone().accept(&s, &c).is_err());
    }
    let mut next = sample;
    next.sequence = 2;
    next.device_timestamp_us += 1;
    next.system_timestamp_us += 1;
    next.global_timestamp_us = Some(next.global_timestamp_us.unwrap() + 1);
    progress.accept(&next, &c).unwrap();
    let mut disabled = c;
    disabled.enabled = false;
    assert!(ImuProgress::default().accept(&next, &disabled).is_err());
}

#[test]
fn validated_binding_is_immutable_and_preserves_every_sample_check() {
    let mut config: ImuConfig = serde_json::from_value(config()).unwrap();
    let binding = ValidatedImuConfig::new(config.clone()).unwrap();
    config.accel_rate_hz = 333;
    config.accel_frame_id = "mutated".into();
    assert!(ValidatedImuConfig::new(config).is_err());
    assert_eq!(binding.config().accel_rate_hz, 200);
    let sample = ImuSample {
        kind: ImuKind::Accel,
        sequence: 1,
        device_timestamp_us: 100,
        system_timestamp_us: 1_700_000_000_000_000,
        global_timestamp_us: Some(1_700_000_000_000_001),
        xyz: [1., 2., 3.],
        frame_id: binding.config().accel_frame_id.clone(),
    };
    let mut accepted = ImuProgress::default();
    accepted.accept_validated(&sample, &binding).unwrap();
    for fault in 0..12 {
        let mut next = sample.clone();
        next.sequence += 1;
        next.device_timestamp_us += 1;
        next.system_timestamp_us += 1;
        next.global_timestamp_us = Some(sample.global_timestamp_us.unwrap() + 1);
        match fault {
            0 => next.xyz[0] = f64::NAN,
            1 => next.xyz[1] = f64::INFINITY,
            2 => next.device_timestamp_us = 0,
            3 => next.device_timestamp_us -= 1,
            4 => next.system_timestamp_us = u64::MAX,
            5 => next.system_timestamp_us -= 2,
            6 => next.global_timestamp_us = Some(0),
            7 => next.global_timestamp_us = None,
            8 => next.global_timestamp_us = sample.global_timestamp_us,
            9 => next.sequence -= 1,
            10 => next.frame_id = "other".into(),
            _ => next.global_timestamp_us = Some(u64::MAX),
        }
        assert!(accepted.clone().accept_validated(&next, &binding).is_err());
        assert!(accepted.clone().accept(&next, binding.config()).is_err());
    }
    assert!(accepted.accept_validated(&sample, &binding).is_err());
    let mut next = sample;
    next.sequence += 1;
    next.device_timestamp_us += 1;
    next.system_timestamp_us += 1;
    next.global_timestamp_us = Some(next.global_timestamp_us.unwrap() + 1);
    accepted.accept_validated(&next, &binding).unwrap();
    let mut disabled = binding.config().clone();
    disabled.enabled = false;
    assert!(
        ImuProgress::default()
            .accept_validated(&next, &ValidatedImuConfig::new(disabled).unwrap())
            .is_err()
    );
}

#[test]
fn imu_usb_batches_keep_equal_system_timestamps_without_rewriting_source_samples() {
    let c: ImuConfig = serde_json::from_value(config()).unwrap();
    let binding = ValidatedImuConfig::new(c.clone()).unwrap();
    for global in [false, true] {
        let mut owned = ImuProgress::default();
        let mut shared = ImuProgress::default();
        // Model the observed 200 Hz streams: two samples share one host receipt
        // time, while device/global clocks and indices advance for each sample.
        for n in 0..50_u64 {
            for kind in [ImuKind::Accel, ImuKind::Gyro] {
                let sample = SharedImuSample {
                    kind,
                    sequence: n,
                    device_timestamp_us: 1_000_000 + n * 5_000,
                    system_timestamp_us: 1_700_000_000_000_000 + (n / 2) * 10_000,
                    global_timestamp_us: global.then_some(1_700_000_000_000_000 + n * 5_000),
                    xyz: [1., 2., 3.],
                    frame_id: binding.frame_id(kind).clone(),
                };
                let original = serde_json::to_vec(&sample).unwrap();
                owned.accept(&sample.clone().into_owned(), &c).unwrap();
                shared.accept_validated(&sample, &binding).unwrap();
                assert_eq!(serde_json::to_vec(&sample).unwrap(), original);
            }
        }
        assert!(owned.has_both());
        assert!(shared.has_both());
    }
}

#[test]
#[ignore = "explicit release comparison of repeated validation and immutable binding"]
fn imu_validation_cache_benchmark() {
    use std::{hint::black_box, time::Instant};
    let mut rows = Vec::new();
    for length in [20, 240] {
        let mut c: ImuConfig = serde_json::from_value(config()).unwrap();
        c.accel_frame_id = "a".repeat(length);
        c.gyro_frame_id = "g".repeat(length);
        let binding = ValidatedImuConfig::new(c.clone()).unwrap();
        for repeat in 0..7 {
            for cached in if repeat % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                let mut progress = ImuProgress::default();
                let mut sample = ImuSample {
                    kind: ImuKind::Accel,
                    sequence: 1,
                    device_timestamp_us: 1,
                    system_timestamp_us: 1_700_000_000_000_000,
                    global_timestamp_us: None,
                    xyz: [1., 2., 3.],
                    frame_id: c.accel_frame_id.clone(),
                };
                let started = Instant::now();
                for _ in 0..500_000 {
                    sample.sequence += 1;
                    sample.device_timestamp_us += 1;
                    sample.system_timestamp_us += 1;
                    if cached {
                        progress
                            .accept_validated(black_box(&sample), black_box(&binding))
                            .unwrap();
                    } else {
                        progress.accept(black_box(&sample), black_box(&c)).unwrap();
                    }
                }
                rows.push(json!({"frame_id_length":length,"repeat":repeat,"cached":cached,"samples":500_000,"elapsed_ns":started.elapsed().as_nanos() as u64}));
                black_box(progress);
            }
        }
    }
    std::fs::write(
        std::env::var_os("GDP_IMU_CACHE_MICRO_OUTPUT").unwrap(),
        serde_json::to_vec_pretty(&rows).unwrap(),
    )
    .unwrap();
}

#[test]
fn shared_imu_frames_keep_owned_wire_format_and_validation() {
    use std::sync::Arc;
    let c: ImuConfig = serde_json::from_value(config()).unwrap();
    let binding = ValidatedImuConfig::new(c).unwrap();
    let shared = SharedImuSample {
        kind: ImuKind::Accel,
        sequence: 1,
        device_timestamp_us: 100,
        system_timestamp_us: 1_700_000_000_000_000,
        global_timestamp_us: None,
        xyz: [1., 2., 3.],
        frame_id: binding.frame_id(ImuKind::Accel).clone(),
    };
    assert!(Arc::ptr_eq(
        &shared.frame_id,
        binding.frame_id(ImuKind::Accel)
    ));
    let copied = shared.clone();
    assert!(Arc::ptr_eq(&shared.frame_id, &copied.frame_id));
    let owned = copied.into_owned();
    let bytes = serde_json::to_vec(&shared).unwrap();
    assert_eq!(bytes, serde_json::to_vec(&owned).unwrap());
    assert_eq!(serde_json::from_slice::<ImuSample>(&bytes).unwrap(), owned);
    ImuProgress::default()
        .accept_validated(&shared, &binding)
        .unwrap();
    let mut bad = shared.clone();
    bad.frame_id = Arc::from("wrong");
    assert!(
        ImuProgress::default()
            .accept_validated(&bad, &binding)
            .is_err()
    );
    bad = shared;
    bad.xyz[2] = f64::NAN;
    assert!(
        ImuProgress::default()
            .accept_validated(&bad, &binding)
            .is_err()
    );
}
