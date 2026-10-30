//! Explicit opt-in Rust data plane. Official driver remains the compatibility default.
use crate::{control::Controller, util::*};
use anyhow::Result;
use serde_json::{Value, json};
pub const SDK_PATH: &str = "/opt/orbbec-sdk/lib/libOrbbecSDK.so";
pub const BRIDGE_PATH: &str = "/opt/gdp-ros/lib/libgeminidepthport_ros_bridge.so";
pub const SERVICE_PATH: &str = "/usr/local/bin/capture-service";
pub const SOURCE_COMMIT: &str = "c153462518ad674650bafa4464fda72c27ab797a";
pub fn selected(d: &Value) -> &str {
    d.get("capture_backend")
        .and_then(Value::as_str)
        .unwrap_or("official")
}
/// Preview evidence binds shared configuration for every backend, matching the
/// management confirmation contract introduced by the upstream binding fix.
pub fn preview_revision(d: &Value, camera: &Value) -> String {
    crate::config::camera_digest(d, camera)
}
fn exact(v: &Value, fields: &[&str]) -> bool {
    v.as_object()
        .is_some_and(|m| m.len() == fields.len() && fields.iter().all(|k| m.contains_key(*k)))
}
pub fn validate(d: &Value) -> Result<()> {
    require(
        d.get("capture_backend")
            .is_none_or(|v| v == "official" || v == "rust"),
        "CAPTURE_BACKEND",
        "Unknown capture backend",
    )?;
    if selected(d) == "official" {
        return require(
            d.get("rust_capture").is_none(),
            "CAPTURE_BACKEND",
            "Rust policy requires explicit Rust backend",
        );
    }
    require(
        d["schema_version"] == 2,
        "CAPTURE_BACKEND_SCHEMA",
        "Rust capture requires schema v2",
    )?;
    let p = &d["rust_capture"];
    require(
        p.as_object().is_some_and(|object| {
            let required = [
                "depth_unit_policy",
                "pairing",
                "poll_ms",
                "capacity",
                "startup_discovery_seconds",
                "service",
            ];
            required.iter().all(|key| object.contains_key(*key))
                && object
                    .keys()
                    .all(|key| required.contains(&key.as_str()) || key == "acquisition")
        }),
        "RUST_CAPTURE_POLICY",
        "Explicit complete Rust capture policy required",
    )?;
    let mut imu_frames = std::collections::BTreeSet::new();
    let image_frames: std::collections::BTreeSet<String> = d["cameras"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|c| {
            [
                format!("{}_color_optical_frame", text(&c["frame_prefix"])),
                format!("{}_depth_optical_frame", text(&c["frame_prefix"])),
            ]
        })
        .collect();
    for c in d["cameras"].as_array().into_iter().flatten() {
        if let Some(v) = c.get("imu") {
            let imu: capture_contract::imu::ImuConfig = serde_json::from_value(v.clone())?;
            imu.validate()?;
            for frame in [&imu.accel_frame_id, &imu.gyro_frame_id] {
                require(
                    !imu.enabled
                        || (!image_frames.contains(frame) && imu_frames.insert(frame.clone())),
                    "IMU_FRAME",
                    "IMU frames must be unique across cameras and image streams",
                )?;
            }

            require(
                !imu.enabled || p.get("acquisition").is_some(),
                "IMU_CLOCK",
                "IMU requires explicit acquisition clock",
            )?;
            require(
                !imu.enabled
                    || (imu.accel_rate_hz + imu.gyro_rate_hz) as u64
                        * p["poll_ms"].as_u64().unwrap_or(u64::MAX).min(60000)
                        * 2
                        <= imu.queue_capacity as u64 * 1000,
                "IMU_QUEUE",
                "IMU queue needs two poll periods",
            )?;
        }
    }
    let acquisition = p
        .get("acquisition")
        .map(|value| {
            serde_json::from_value::<capture_contract::AcquisitionPolicy>(value.clone())
                .map_err(anyhow::Error::from)?
                .resolve()
                .map_err(anyhow::Error::from)
        })
        .transpose()?;
    require(
        p["depth_unit_policy"] == "require-mm" || p["depth_unit_policy"] == "normalize-mm",
        "RUST_DEPTH_POLICY",
        "Explicit depth unit policy required",
    )?;
    require(
        exact(&p["pairing"], &["clock", "max_delta_us"])
            && (p["pairing"]["clock"] == "device" || p["pairing"]["clock"] == "system")
            && p["pairing"]["max_delta_us"]
                .as_u64()
                .is_some_and(|v| (1..=1_000_000).contains(&v)),
        "RUST_PAIR_POLICY",
        "Explicit pairing clock and tolerance required",
    )?;
    require(
        p["poll_ms"]
            .as_u64()
            .is_some_and(|v| (1..=1000).contains(&v))
            && p["capacity"]
                .as_u64()
                .is_some_and(|v| (1..=1024).contains(&v))
            && p["startup_discovery_seconds"]
                .as_f64()
                .is_some_and(|v| v.is_finite() && (0.0..=30.0).contains(&v)),
        "RUST_CAPTURE_POLICY",
        "Invalid capture limits",
    )?;
    let service = &p["service"];
    let service_keys = [
        "idle_timeout_ms",
        "startup_timeout_ms",
        "heartbeat_ms",
        "heartbeat_timeout_ms",
        "max_restarts",
        "restart_backoff_ms",
        "shutdown_timeout_ms",
        "ready_min_pairs",
    ];
    require(
        exact(service, &service_keys)
            && service_keys.iter().all(|k| service[*k].as_u64().is_some()),
        "RUST_SERVICE_POLICY",
        "Explicit bounded supervisor policy required",
    )?;
    require(
        service["idle_timeout_ms"]
            .as_u64()
            .is_some_and(|v| (100..=60_000).contains(&v))
            && service["startup_timeout_ms"]
                .as_u64()
                .is_some_and(|v| (1000..=120_000).contains(&v))
            && service["heartbeat_ms"]
                .as_u64()
                .is_some_and(|v| (50..=5000).contains(&v))
            && service["heartbeat_timeout_ms"]
                .as_u64()
                .is_some_and(|v| (500..=120_000).contains(&v))
            && service["max_restarts"].as_u64().is_some_and(|v| v <= 20)
            && service["restart_backoff_ms"]
                .as_u64()
                .is_some_and(|v| (100..=30_000).contains(&v))
            && service["shutdown_timeout_ms"]
                .as_u64()
                .is_some_and(|v| (100..=5000).contains(&v))
            && service["ready_min_pairs"]
                .as_u64()
                .is_some_and(|v| (1..=10_000).contains(&v)),
        "RUST_SERVICE_POLICY",
        "Supervisor limits outside supported range",
    )?;
    require(
        service["idle_timeout_ms"].as_u64().unwrap() >= p["poll_ms"].as_u64().unwrap()
            && service["startup_timeout_ms"].as_u64().unwrap()
                > service["idle_timeout_ms"].as_u64().unwrap()
            && service["heartbeat_timeout_ms"].as_u64().unwrap()
                >= service["heartbeat_ms"].as_u64().unwrap().saturating_mul(3)
            && p["startup_discovery_seconds"].as_f64().unwrap() * 1000.0
                < (service["startup_timeout_ms"].as_u64().unwrap() as f64),
        "RUST_SERVICE_POLICY",
        "Supervisor deadlines inconsistent",
    )?;
    let allowed = [
        "enable_color",
        "enable_depth",
        "color_format",
        "depth_format",
        "depth_registration",
        "color_width",
        "color_height",
        "color_fps",
        "depth_width",
        "depth_height",
        "depth_fps",
        "color_qos",
        "color_camera_info_qos",
        "depth_qos",
        "depth_camera_info_qos",
    ];
    for c in d["cameras"].as_array().into_iter().flatten() {
        let parameters = &c["parameters"];
        require(
            parameters
                .as_object()
                .is_some_and(|m| m.keys().all(|k| allowed.contains(&k.as_str()))),
            "RUST_PARAMETER_UNSUPPORTED",
            "Rust backend cannot apply this official-driver parameter",
        )?;
        require(
            parameters["enable_color"] == true && parameters["enable_depth"] == true,
            "RUST_STREAMS",
            "Rust service currently requires both RGB-D streams",
        )?;
        if c["enabled"] == true {
            require(
                c["timestamp_source"]
                    == acquisition.map_or("host", |policy| {
                        policy.message_timestamp.camera_timestamp_source()
                    }),
                "RUST_TIME_SOURCE",
                "Camera timestamp source must match the effective acquisition policy",
            )?;
        }
    }
    Ok(())
}
pub fn runtime(ctl: &Controller, d: &Value) -> Result<Value> {
    let inspect: Value =
        serde_json::from_str(&ctl.run(&["docker", "image", "inspect", text(&d["image"])])?)?;
    let image = &inspect[0];
    require(
        image["Config"]["Labels"]["org.geminidepthport.capture-backend"] == "rust-v1"
            && image["Config"]["Labels"]["org.opencontainers.image.revision"]
                == d["upstream_commit"],
        "RUST_IMAGE_CONTRACT",
        "Rust capture image source/backend differs",
    )?;
    let raw = ctl.run(&[
        "docker",
        "run",
        "--rm",
        "--network",
        "none",
        "--entrypoint",
        "cat",
        text(&d["image"]),
        "/opt/gdp-capture/runtime.json",
    ])?;
    let manifest: Value = serde_json::from_str(&raw)?;
    require(
        manifest["version"] == 1
            && manifest["upstream_commit"] == d["upstream_commit"]
            && manifest["sdk_version"] == "2.10.6"
            && manifest["communication_profile"] == crate::config::COMMUNICATION,
        "RUST_RUNTIME_CONTRACT",
        "Unsupported capture runtime manifest",
    )?;
    for key in ["sdk_sha256", "bridge_sha256", "service_sha256"] {
        require(
            matches(r"^[a-f0-9]{64}$", text(&manifest[key])),
            "RUST_RUNTIME_CONTRACT",
            "Runtime binary hash missing",
        )?;
    }
    require(
        manifest["sdk_source"]["repository"] == "https://github.com/orbbec/OrbbecSDK_ROS2"
            && manifest["sdk_source"]["commit"] == d["upstream_commit"]
            && manifest["sdk_source"]["license_files"]
                .as_object()
                .is_some_and(|m| !m.is_empty())
            && manifest["files"].as_object().is_some_and(|m| !m.is_empty()),
        "RUST_RUNTIME_CONTRACT",
        "Runtime source/license inventory missing",
    )?;
    for (path, key) in [
        (SDK_PATH, "sdk_sha256"),
        (BRIDGE_PATH, "bridge_sha256"),
        (SERVICE_PATH, "service_sha256"),
    ] {
        require(
            manifest["files"][path] == manifest[key],
            "RUST_RUNTIME_CONTRACT",
            "Runtime binary hash does not match file inventory",
        )?;
    }
    if let Some(recovery) = manifest.get("usb_recovery") {
        require(
            exact(recovery, &["helper", "sha256"])
                && recovery["helper"] == "/opt/gdp-ros/bin/gdp-usb-recover"
                && matches(r"^[a-f0-9]{64}$", text(&recovery["sha256"]))
                && manifest["files"]["/opt/gdp-ros/bin/gdp-usb-recover"] == recovery["sha256"],
            "RUST_RUNTIME_CONTRACT",
            "USB recovery artifact is not bound to the runtime inventory",
        )?;
    }
    require(
        matches(r"^sha256:[a-f0-9]{64}$", text(&image["Id"])),
        "RUST_RUNTIME_CONTRACT",
        "Immutable image identity missing",
    )?;
    Ok(json!({"image_id":image["Id"],"manifest_sha256":hash(raw.as_bytes()),"manifest":manifest}))
}
pub fn runtime_digest(value: &Value) -> String {
    hash(canonical(value))
}
pub fn service_config(d: &Value, c: &Value, generation: &str, runtime: &Value) -> Value {
    let p = &c["parameters"];
    let policy = &d["rust_capture"];
    let stream = |name: &str| json!({"width":p[format!("{name}_width")],"height":p[format!("{name}_height")],"fps":p[format!("{name}_fps")],"frame_id":format!("{}_{name}_optical_frame",text(&c["frame_prefix"]))});
    let mut depth = stream("depth");
    depth["unit_policy"] = policy["depth_unit_policy"].clone();
    let mut service = policy["service"].clone();
    service["status_path"] = "/tmp/capture/status.json".into();
    service["generation"] = generation.into();
    let m = &runtime["manifest"];
    let mut value = json!({"version":1,"sdk_library":SDK_PATH,"serial":c["serial_number"],"color":stream("color"),"depth":depth,"pairing":policy["pairing"],"poll_ms":policy["poll_ms"],"capacity":policy["capacity"],"startup_discovery_seconds":policy["startup_discovery_seconds"],"ros":{"bridge":BRIDGE_PATH,"namespace":c["namespace"],"stamp_source":"sdk-system-unix"},"service":service,"runtime_contract":{"sdk_sha256":m["sdk_sha256"],"bridge_sha256":m["bridge_sha256"],"service_sha256":m["service_sha256"]}});
    if let Some(acquisition) = policy.get("acquisition") {
        value["acquisition"] = acquisition.clone();
        // The service validates this policy again before loading the SDK. Do not
        // turn malformed input into a different, apparently valid clock policy.
        if let Some(resolved) =
            serde_json::from_value::<capture_contract::AcquisitionPolicy>(acquisition.clone())
                .ok()
                .and_then(|policy| policy.resolve().ok())
        {
            value["ros"]["stamp_source"] = resolved.message_timestamp.ros_stamp_source().into();
        }
    }
    if let Some(recovery) = m.get("usb_recovery") {
        value["usb_recovery"] = recovery.clone();
    }
    value["quality_policy"] = json!({
        "version":1,
        "max_source_age_ns":service["idle_timeout_ms"].as_u64().unwrap_or(0).saturating_mul(1_000_000),
        "max_future_ns":100_000_000,
        "max_pair_delta_us":policy["pairing"]["max_delta_us"],
        "min_valid_fraction":0.0,
        "max_zero_fraction":1.0,
        "action":"report-only"
    });
    if let Some(imu) = c.get("imu") {
        value["imu"] = imu.clone();
    }
    let mut rules = serde_json::Map::new();
    for code in [
        "STREAM_STALE",
        "TIMESTAMP_REGRESSION",
        "PAIR_SKEW_EXCEEDED",
        "FRAME_CONTRACT_INVALID",
        "QUEUE_OVERFLOW",
        "DEVICE_DISCONNECTED",
        "SESSION_CHANGED",
        "PUBLISH_PARTIAL_FAILURE",
        "TIME_MAPPING_INVALID",
    ] {
        rules.insert(
            code.into(),
            json!({"open_after":1,"recover_after":3,"severity":"degraded"}),
        );
    }
    value["quality_runtime"] = json!({"version":1,"quality":value["quality_policy"],"stale_after_ns":value["quality_policy"]["max_source_age_ns"],"event_capacity":256,"window_capacity":1024,"window_horizon_ns":10_000_000_000_u64,"summary_capacity":1,"max_sessions":1,"rules":rules});
    value["service"]["recording_control_path"] = "/recording/control.json".into();
    value
}
#[cfg(test)]
mod quality_generation_tests {
    use super::*;
    fn acquisition_inventory() -> Value {
        json!({"schema_version":2,"capture_backend":"rust","rust_capture":{"depth_unit_policy":"normalize-mm","pairing":{"clock":"device","max_delta_us":20000},"poll_ms":100,"capacity":4,"startup_discovery_seconds":0,"service":{"idle_timeout_ms":3456,"startup_timeout_ms":15000,"heartbeat_ms":500,"heartbeat_timeout_ms":10000,"max_restarts":2,"restart_backoff_ms":1000,"shutdown_timeout_ms":1000,"ready_min_pairs":5}},"cameras":[{"enabled":true,"timestamp_source":"host","parameters":{"enable_color":true,"enable_depth":true}}]})
    }
    #[test]
    fn acquisition_validation_preserves_legacy_and_rejects_inconsistent_clocks() {
        let mut d = acquisition_inventory();
        validate(&d).unwrap();
        d["rust_capture"]["acquisition"] = json!({"preset":"complete"});
        assert!(
            validate(&d)
                .unwrap_err()
                .to_string()
                .starts_with("RUST_TIME_SOURCE")
        );
        d["cameras"][0]["timestamp_source"] = "global".into();
        validate(&d).unwrap();
        d["rust_capture"]["acquisition"]["preset"] = "strict-rgbd".into();
        validate(&d).unwrap();
        d["rust_capture"]["acquisition"]["advanced"] = json!({"message_timestamp":"system"});
        assert!(validate(&d).is_err());
        d["cameras"][0]["timestamp_source"] = "host".into();
        validate(&d).unwrap();
        d["rust_capture"]["acquisition"]["preset"] = "hardware-sync".into();
        assert_eq!(
            validate(&d).unwrap_err().to_string(),
            "HARDWARE_SYNC_UNAVAILABLE"
        );
        d["rust_capture"]["acquisition"] =
            json!({"preset":"complete","advanced":{"aggregate":"all"}});
        assert_eq!(
            validate(&d).unwrap_err().to_string(),
            "ACQUISITION_PRESET_CONFLICT"
        );
        d["rust_capture"]["acquisition"] = json!({"preset":"complete","unknown":true});
        assert!(validate(&d).is_err());
        d["rust_capture"]["unknown"] = true.into();
        assert!(
            validate(&d)
                .unwrap_err()
                .to_string()
                .starts_with("RUST_CAPTURE_POLICY")
        );
    }
    #[test]
    fn service_config_binds_requested_acquisition_and_effective_message_clock() {
        let mut d = acquisition_inventory();
        let camera = json!({"parameters":{"color_width":640,"color_height":480,"color_fps":30,"depth_width":640,"depth_height":480,"depth_fps":30},"serial_number":"synthetic-private","namespace":"/test","frame_prefix":"test"});
        let runtime = json!({"manifest":{"sdk_sha256":"a".repeat(64),"bridge_sha256":"b".repeat(64),"service_sha256":"c".repeat(64)}});
        let legacy = service_config(&d, &camera, &"a".repeat(32), &runtime);
        assert!(legacy.get("acquisition").is_none());
        assert_eq!(legacy["ros"]["stamp_source"], "sdk-system-unix");
        for (acquisition, stamp) in [
            (json!({"preset":"complete"}), "sdk-global-unix"),
            (
                json!({"preset":"strict-rgbd","advanced":{"message_timestamp":"system"}}),
                "sdk-system-unix",
            ),
        ] {
            d["rust_capture"]["acquisition"] = acquisition.clone();
            let value = service_config(&d, &camera, &"a".repeat(32), &runtime);
            assert_eq!(value["acquisition"], acquisition);
            assert_eq!(value["ros"]["stamp_source"], stamp);
        }
        // Invalid input remains invalid for the service parser; it must not
        // disappear into a legacy policy during generation.
        d["rust_capture"]["acquisition"] = json!({"preset":"hardware-sync"});
        assert_eq!(
            service_config(&d, &camera, &"a".repeat(32), &runtime)["acquisition"],
            d["rust_capture"]["acquisition"]
        );
        d["rust_capture"]["acquisition"] = json!({"preset":"unknown"});
        assert_eq!(
            service_config(&d, &camera, &"a".repeat(32), &runtime)["acquisition"],
            d["rust_capture"]["acquisition"]
        );
    }
    #[test]
    fn acquisition_is_accepted_by_full_inventory_validation_and_binds_every_camera() {
        let mut d: Value =
            serde_json::from_str(include_str!("../inventory.v2.example.json")).unwrap();
        d["capture_backend"] = "rust".into();
        d["rust_capture"] = acquisition_inventory()["rust_capture"].clone();
        crate::config::validate(&d, true).unwrap();
        let before: Vec<_> = d["cameras"]
            .as_array()
            .unwrap()
            .iter()
            .map(|camera| preview_revision(&d, camera))
            .collect();
        d["rust_capture"]["acquisition"] = json!({"preset":"complete"});
        crate::config::validate(&d, true).unwrap();
        for (camera, previous) in d["cameras"].as_array().unwrap().iter().zip(before) {
            assert_ne!(previous, preview_revision(&d, camera));
        }
    }
    #[test]
    fn generated_quality_runtime_keeps_explicit_existing_idle_and_pair_budgets() {
        let d = json!({"rust_capture":{"depth_unit_policy":"normalize-mm","pairing":{"clock":"device","max_delta_us":20000},"poll_ms":100,"capacity":4,"startup_discovery_seconds":0,"service":{"idle_timeout_ms":3456,"startup_timeout_ms":15000,"heartbeat_ms":500,"heartbeat_timeout_ms":10000,"max_restarts":2,"restart_backoff_ms":1000,"shutdown_timeout_ms":1000,"ready_min_pairs":5}}});
        let camera = json!({"parameters":{"color_width":640,"color_height":480,"color_fps":30,"depth_width":640,"depth_height":480,"depth_fps":30},"serial_number":"synthetic-private","namespace":"/test","frame_prefix":"test"});
        let config = service_config(
            &d,
            &camera,
            &"a".repeat(32),
            &json!({"manifest":{"sdk_sha256":"a".repeat(64),"bridge_sha256":"b".repeat(64),"service_sha256":"c".repeat(64),"usb_recovery":{"helper":"/opt/gdp-ros/bin/gdp-usb-recover","sha256":"d".repeat(64)}}}),
        );
        assert_eq!(config["usb_recovery"]["sha256"], "d".repeat(64));
        assert_eq!(config["service"]["max_restarts"], 2);
        assert_eq!(
            config["quality_policy"]["max_source_age_ns"],
            3_456_000_000_u64
        );
        assert_eq!(
            config["quality_runtime"]["stale_after_ns"],
            3_456_000_000_u64
        );
        assert_eq!(config["quality_policy"]["max_pair_delta_us"], 20000);
        assert_eq!(
            config["quality_runtime"]["quality"],
            config["quality_policy"]
        );
        assert_eq!(config["quality_policy"]["action"], "report-only");
        assert_eq!(config["quality_policy"]["min_valid_fraction"], 0.0);
        assert_eq!(config["quality_policy"]["max_zero_fraction"], 1.0);
        assert_eq!(
            config["quality_runtime"]["rules"]
                .as_object()
                .unwrap()
                .len(),
            9
        );
        for rule in config["quality_runtime"]["rules"]
            .as_object()
            .unwrap()
            .values()
        {
            assert_eq!(rule["open_after"], 1);
            assert_eq!(rule["recover_after"], 3);
        }
        assert_eq!(config["service"]["max_restarts"], 2);
    }
}

#[cfg(test)]
mod imu_config_tests {
    use super::*;
    fn policy() -> Value {
        json!({"enabled":true,"accel_rate_hz":200,"gyro_rate_hz":200,"accel_range_g":4,"gyro_range_dps":1000,"queue_capacity":128,"stale_timeout_ms":500,"accel_frame_id":"camera_a_accel_frame","gyro_frame_id":"camera_a_gyro_frame"})
    }
    #[test]
    fn camera_imu_is_validated_rendered_and_invalidates_preview_evidence() {
        let mut official: Value =
            serde_json::from_str(include_str!("../inventory.v2.example.json")).unwrap();
        for invalid in [Value::Null, json!("camera"), json!(1), json!([])] {
            official["cameras"][0] = invalid;
            assert!(crate::config::validate(&official, true).is_err());
        }
        let mut inventory: Value = serde_json::from_str(include_str!(
            "../examples/rust-single-camera/inventory.json"
        ))
        .unwrap();
        let previous = preview_revision(&inventory, &inventory["cameras"][0]);
        inventory["cameras"][0]["imu"] = policy();
        crate::config::validate(&inventory, true).unwrap();
        assert_ne!(
            preview_revision(&inventory, &inventory["cameras"][0]),
            previous
        );
        let rendered = service_config(
            &inventory,
            &inventory["cameras"][0],
            &"a".repeat(32),
            &json!({"manifest":{}}),
        );
        assert_eq!(rendered["imu"], policy());
        let mut other = inventory["cameras"][0].clone();
        other["name"] = json!("camera_b");
        other["namespace"] = json!("/camera_b");
        other["frame_prefix"] = json!("camera_b");
        inventory["cameras"].as_array_mut().unwrap().push(other);
        assert!(validate(&inventory).is_err());
        inventory["cameras"].as_array_mut().unwrap().pop();
        inventory["cameras"][0]["imu"]["accel_frame_id"] = json!("camera_a_color_optical_frame");
        assert!(validate(&inventory).is_err());
        inventory["cameras"][0]["imu"] = policy();
        inventory["rust_capture"]["poll_ms"] = json!(1000);
        assert!(validate(&inventory).is_err());
        inventory["rust_capture"]["poll_ms"] = json!(100);
        inventory["rust_capture"]
            .as_object_mut()
            .unwrap()
            .remove("acquisition");
        assert!(validate(&inventory).is_err());
    }
}
