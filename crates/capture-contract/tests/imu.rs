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
            7 => s.system_timestamp_us -= 1,
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
