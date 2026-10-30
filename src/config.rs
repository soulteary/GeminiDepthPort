//! Inventory validation, migration plans and confirmation invalidation.
//!
//! Draft acceptance permits incomplete configuration, not capture readiness.
//! Confirmation digests bind settings to recorded checks; they are drift
//! detectors, not signatures or physical calibration certificates.
use crate::util::*;
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::BTreeSet;
pub const COMMUNICATION: &str = "humble-fastdds-local-udp-v1";
pub fn catalog() -> Value {
    serde_json::from_str(include_str!("../driver_profiles.json")).unwrap()
}
pub fn active(data: &Value) -> Vec<Value> {
    data["cameras"]
        .as_array()
        .map(|a| a.iter().filter(|c| c["enabled"] == true).cloned().collect())
        .unwrap_or_default()
}
pub fn camera_digest(data: &Value, camera: &Value) -> String {
    let mut shared = data.clone();
    shared.as_object_mut().unwrap().remove("cameras");
    let mut c = camera.clone();
    c.as_object_mut().unwrap().remove("verification");
    c.as_object_mut().unwrap().remove("profiles_verified");
    hash(canonical(&json!({"shared":shared,"camera":c})))
}
pub fn signature(camera: &Value) -> String {
    let mut c = camera.clone();
    if let Some(m) = c.as_object_mut() {
        m.remove("verification");
        m.remove("profiles_verified");
    } // Legacy preview signatures used spaced Python JSON.
    hash(python_spaced(&c))
}
fn python_spaced(v: &Value) -> String {
    match v {
        Value::Object(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!("{}: {}", canonical(&json!(k)), python_spaced(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(python_spaced).collect::<Vec<_>>().join(", ")
        ),
        _ => canonical(v),
    }
}
pub fn qos(params: &Value) -> Result<Value> {
    let mut result = json!({});
    for stream in ["color", "depth"] {
        for suffix in ["_qos", "_camera_info_qos"] {
            let k = format!("{stream}{suffix}");
            let v = params.get(&k).unwrap_or(&Value::Null);
            require(
                v.is_null() && params.get(&k).is_none()
                    || v.as_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case("DEFAULT")),
                "QOS_CONTRACT",
                "Reliable-compatible driver QoS required",
            )?;
            result[k] = "DEFAULT".into();
        }
    }
    Ok(result)
}
fn keys(v: &Value) -> BTreeSet<&str> {
    v.as_object()
        .map(|m| m.keys().map(String::as_str).collect())
        .unwrap_or_default()
}
fn fields(v: &Value, s: &str) -> bool {
    v.is_object() && keys(v) == s.split_whitespace().collect()
}
pub fn validate(data: &Value, draft: bool) -> Result<()> {
    require(
        data.is_object(),
        "CONFIG_SCHEMA",
        "Expected versioned inventory",
    )?;
    crate::capture_backend::validate(data)?;
    match data["schema_version"].as_u64() {
        Some(1) => legacy(data, draft),
        Some(2) => v2(data, draft),
        _ => require(false, "CONFIG_SCHEMA", "Unsupported inventory schema"),
    }
}
fn v2(d: &Value, draft: bool) -> Result<()> {
    let mut root = d.clone();
    root.as_object_mut().unwrap().remove("communication");
    root.as_object_mut().unwrap().remove("capture_backend");
    root.as_object_mut().unwrap().remove("rust_capture");
    root.as_object_mut().unwrap().remove("usbfs_minimum_mb");
    require(
        d.get("usbfs_minimum_mb")
            .is_none_or(|v| v.as_u64().is_some_and(|n| (1..=65536).contains(&n))),
        "USBFS_POLICY",
        "Explicit USBFS minimum must be 1..65536 MiB",
    )?;
    require(
        fields(
            &root,
            "schema_version image upstream_commit ros_domain_id startup_timeout_seconds cameras",
        ),
        "CONFIG_FIELDS",
        "Unexpected or missing inventory fields",
    )?;
    require(
        matches(r"^[a-zA-Z0-9][a-zA-Z0-9._/:-]*$", text(&d["image"])),
        "CONFIG_IMAGE",
        "Invalid image reference",
    )?;
    let catalog = catalog();
    require(
        d["upstream_commit"] == catalog["upstream_commit"],
        "PROFILE_BASELINE",
        "Driver baseline differs from catalog",
    )?;
    require(
        d["ros_domain_id"].as_u64().is_some_and(|v| v <= 101),
        "CONFIG_DOMAIN",
        "Invalid domain",
    )?;
    require(
        d["startup_timeout_seconds"]
            .as_u64()
            .is_some_and(|v| (30..=600).contains(&v)),
        "CONFIG_TIMEOUT",
        "Invalid startup timeout",
    )?;
    require(
        d.get("communication")
            .is_none_or(|v| *v == json!({"profile":COMMUNICATION})),
        "COMMUNICATION_PROFILE",
        "Unsupported communication profile",
    )?;
    require(
        d["cameras"].as_array().is_some_and(|a| a.len() <= 3),
        "CAMERA_COUNT",
        "At most three cameras supported",
    )?;
    let (mut names, mut namespaces, mut frames, mut serials) = (
        BTreeSet::new(),
        BTreeSet::<String>::new(),
        BTreeSet::new(),
        BTreeSet::new(),
    );
    for c in d["cameras"].as_array().unwrap() {
        require(
            fields(
                &{
                    let mut camera = c.clone();
                    if let Some(fields) = camera.as_object_mut() {
                        fields.remove("imu");
                    }
                    camera
                },
                "name profile namespace frame_prefix serial_number enabled profiles_verified verification parameters health timestamp_source calibration_revision depth_unit",
            ),
            "CAMERA_FIELDS",
            "Unexpected or missing camera fields",
        )?;
        if let Some(imu) = c.get("imu") {
            let imu: capture_contract::imu::ImuConfig = serde_json::from_value(imu.clone())?;
            imu.validate()?;
            require(
                !imu.enabled || crate::capture_backend::selected(d) == "rust",
                "IMU_BACKEND",
                "Native IMU requires explicit Rust backend",
            )?;
        }
        let name = text(&c["name"]);
        require(
            matches(r"^[a-z][a-z0-9_]{0,31}$", name) && names.insert(name),
            "CAMERA_ID",
            "Invalid or duplicate identity",
        )?;
        let ns = text(&c["namespace"]);
        require(
            ns.starts_with('/')
                && ns[1..]
                    .split('/')
                    .all(|s| matches(r"^[A-Za-z][A-Za-z0-9_]*$", s)),
            "CAMERA_NAMESPACE",
            "Canonical absolute ROS namespace required",
        )?;
        require(
            !namespaces.iter().any(|v| {
                v == ns || ns.starts_with(&format!("{v}/")) || v.starts_with(&format!("{ns}/"))
            }),
            "CAMERA_NAMESPACE",
            "Overlapping namespaces",
        )?;
        namespaces.insert(ns.to_string());
        let frame = text(&c["frame_prefix"]);
        require(
            matches(r"^[A-Za-z][A-Za-z0-9_]*$", frame) && frames.insert(frame),
            "CAMERA_FRAME",
            "Invalid or duplicate frame",
        )?;
        require(
            c["enabled"].is_boolean() && c["profiles_verified"].is_boolean(),
            "CAMERA_BOOLEAN",
            "Expected booleans",
        )?;
        let profile = text(&c["profile"]);
        require(
            catalog["profiles"].get(profile).is_some(),
            "CAMERA_PROFILE",
            "Unknown profile",
        )?;
        require(
            catalog["profiles"][profile]
                .get("capture_backend")
                .is_none_or(|v| v == crate::capture_backend::selected(d)),
            "CAPTURE_BACKEND_PROFILE",
            "Selected model profile requires the Rust backend",
        )?;
        let serial = text(&c["serial_number"]);
        require(
            c["serial_number"].is_string()
                && (serial.is_empty() || matches(r"^[A-Za-z0-9_-]+$", serial)),
            "CAMERA_BINDING",
            "Invalid binding",
        )?;
        if c["enabled"] == true {
            require(
                !placeholder(serial),
                "CAMERA_UNBOUND",
                "Enabled cameras require bindings",
            )?;
            require(
                serials.insert(serial),
                "CAMERA_BINDING",
                "Duplicate enabled binding",
            )?;
        }
        require(
            c["timestamp_source"].is_string() && c["calibration_revision"].is_string(),
            "CAMERA_METADATA",
            "Expected metadata strings",
        )?;
        require(
            c["depth_unit"] == "mm",
            "DEPTH_UNIT",
            "Raw depth uses millimetres",
        )?;
        let p = &c["parameters"];
        require(
            p.is_object(),
            "PARAMETER_FIELDS",
            "Expected parameter object",
        )?;
        for (k, v) in p.as_object().unwrap() {
            require(
                !reserved(k)
                    && catalog["profiles"][profile]["parameters"]
                        .as_array()
                        .is_some_and(|a| a.iter().any(|v| v == k)),
                "PARAMETER_FIELDS",
                "Unsupported or managed parameter",
            )?;
            require(
                v.is_string() || v.is_boolean() || v.is_number(),
                "PARAMETER_TYPE",
                "Expected finite scalar",
            )?;
        }
        require(
            p["enable_color"].is_boolean() && p["enable_depth"].is_boolean(),
            "STREAM_SWITCH",
            "Explicit stream switches required",
        )?;
        qos(p)?;
        require(
            p["color_format"] == "YUYV",
            "COLOR_FORMAT",
            "Native YUYV required",
        )?;
        require(
            ![
                "enable_color_undistortion",
                "color_flip",
                "color_mirror",
                "color_rotation",
                "enable_enhanced_depth",
            ]
            .iter()
            .any(|k| truthy(&p[k])),
            "COLOR_PROCESSING_UNSUPPORTED",
            "Unvalidated YUYV processing",
        )?;
        require(
            p["depth_format"] == "Y16" && p["depth_registration"] == false,
            "DEPTH_FORMAT",
            "Raw unregistered depth required",
        )?;
        require(
            ![
                "enable_left_ir",
                "enable_right_ir",
                "enable_ir",
                "enable_accel",
                "enable_gyro",
                "enable_point_cloud",
                "enable_colored_point_cloud",
            ]
            .iter()
            .any(|k| truthy(&p[k])),
            "STREAM_UNSUPPORTED",
            "Only contracted streams supported",
        )?;
        if c["enabled"] == true {
            require(
                p["enable_color"] == true || p["enable_depth"] == true,
                "STREAM_EMPTY",
                "Enable an image stream",
            )?;
            require(
                !text(&c["timestamp_source"]).is_empty(),
                "CAMERA_TIME",
                "Declare timestamp source",
            )?;
            for stream in ["color", "depth"] {
                if p[format!("enable_{stream}")] == true {
                    for suffix in ["width", "height", "fps"] {
                        require(
                            p[format!("{stream}_{suffix}")]
                                .as_u64()
                                .is_some_and(|v| v > 0),
                            "STREAM_PROFILE",
                            "Explicit positive stream profiles required",
                        )?;
                    }
                }
            }
            require(
                p["enable_color"] != true || p["color_width"].as_u64().unwrap_or(1) % 2 == 0,
                "COLOR_WIDTH",
                "YUYV width must be even",
            )?;
        }
        health(&c["health"], c["enabled"] == true)?;
        if c["profiles_verified"] == true {
            require(
                fields(
                    &c["verification"],
                    if crate::capture_backend::selected(d) == "rust" {
                        "method configuration_digest runtime_digest"
                    } else {
                        "method configuration_digest"
                    },
                ) && (crate::capture_backend::selected(d) != "rust"
                    || matches(
                        r"^[a-f0-9]{64}$",
                        text(&c["verification"]["runtime_digest"]),
                    ))
                    && c["verification"]["method"] == "runtime-probe"
                    && c["verification"]["configuration_digest"] == camera_digest(d, c),
                "CONFIRMATION_STALE",
                "Confirmation must match configuration and probe",
            )?;
        } else {
            require(
                c["verification"].is_null(),
                "CONFIRMATION_STALE",
                "Unconfirmed camera cannot retain evidence",
            )?;
        }
        require(
            draft || c["enabled"] != true || c["profiles_verified"] == true,
            "PROFILE_UNVERIFIED",
            "Actual profile verification required",
        )?;
    }
    require(
        draft || !active(d).is_empty(),
        "NO_ACTIVE_CAMERAS",
        "No enabled cameras",
    )
}
pub fn placeholder(s: &str) -> bool {
    let s = s.to_uppercase();
    s.is_empty()
        || ["REPLACE_", "UNBOUND", "PLACEHOLDER"]
            .iter()
            .any(|p| s.starts_with(p))
}
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}
fn reserved(k: &str) -> bool {
    "camera_name serial_number device_num usb_port config_file_path load_config_json_file_path export_config_json_file_path net_device_ip net_device_port bag_filename tf_prefix".split_whitespace().any(|v|v==k)
}
fn health(h: &Value, enabled: bool) -> Result<()> {
    require(
        h.is_object()
            && keys(h).iter().all(|k| {
                "discovery_seconds sample_seconds freshness_seconds min_fps_ratio"
                    .split_whitespace()
                    .any(|v| v == *k)
            }),
        "HEALTH_FIELDS",
        "Invalid health settings",
    )?;
    if enabled || !h.as_object().unwrap().is_empty() {
        require(
            fields(
                h,
                "discovery_seconds sample_seconds freshness_seconds min_fps_ratio",
            ) && h.as_object().unwrap().values().all(Value::is_number),
            "HEALTH_FIELDS",
            "Explicit numeric health settings required",
        )?;
        require(
            (1.0..=10.0).contains(&h["discovery_seconds"].as_f64().unwrap())
                && (2.0..=10.0).contains(&h["sample_seconds"].as_f64().unwrap())
                && (0.1..=1.).contains(&h["min_fps_ratio"].as_f64().unwrap())
                && h["freshness_seconds"]
                    .as_f64()
                    .is_some_and(|v| v > 0. && v <= 5.),
            "HEALTH_RANGE",
            "Probe settings outside limits",
        )?;
    }
    Ok(())
}
fn legacy(d: &Value, draft: bool) -> Result<()> {
    require(
        matches(r"^[0-9a-f]{40}$", text(&d["upstream_commit"]))
            && matches(r"^[a-zA-Z0-9][a-zA-Z0-9._/:-]*$", text(&d["image"]))
            && d["ros_domain_id"].as_u64().is_some_and(|v| v <= 101)
            && d["startup_timeout_seconds"]
                .as_f64()
                .is_some_and(|v| (30.0..=600.).contains(&v)),
        "CONFIG_INVALID",
        "Invalid legacy settings",
    )?;
    let a = d["cameras"].as_array();
    require(
        a.is_some_and(|a| a.len() == 2) && active(d).len() == 2,
        "CONFIG_INVALID",
        "Both head and wrist required",
    )?;
    let mut names = BTreeSet::new();
    let mut serials = BTreeSet::new();
    for c in a.unwrap() {
        let name = text(&c["name"]);
        require(
            ["head", "wrist"].contains(&name) && names.insert(name),
            "CONFIG_INVALID",
            "Maintain head and wrist",
        )?;
        let (model, launch) = if name == "head" {
            ("Gemini 335L", "gemini_330_series.launch.py")
        } else {
            ("Gemini 2", "gemini2.launch.py")
        };
        require(
            c["model"] == model
                && c["launch_file"] == launch
                && c["profiles_verified"].is_boolean(),
            "CONFIG_INVALID",
            "Invalid legacy camera role",
        )?;
        let p = &c["parameters"];
        require(p.is_object(), "CONFIG_INVALID", "Expected parameters")?;
        for (k, v) in p.as_object().unwrap() {
            require(
                !reserved(k) && (v.is_string() || v.is_number() || v.is_boolean()),
                "CONFIG_INVALID",
                "Invalid parameter",
            )?;
        }
        require(
            p["uvc_backend"] == "libuvc"
                && p["sync_mode"] == "standalone"
                && p["depth_registration"] == false,
            "CONFIG_INVALID",
            "Unsupported legacy source",
        )?;
        require(
            if name == "head" {
                p["enumerate_net_device"] == false
            } else {
                p.get("enumerate_net_device").is_none() && p["enable_ir"] == false
            },
            "CONFIG_INVALID",
            "Unsupported stream/source",
        )?;
        require(
            ![
                "enable_left_ir",
                "enable_right_ir",
                "enable_accel",
                "enable_gyro",
            ]
            .iter()
            .any(|k| truthy(&p[k])),
            "CONFIG_INVALID",
            "Unsupported extra streams",
        )?;
        require(
            p["enable_color"] == true || p["enable_depth"] == true,
            "CONFIG_INVALID",
            "Enable image stream",
        )?;
        for stream in ["color", "depth"] {
            require(
                p[format!("enable_{stream}")].is_boolean(),
                "CONFIG_INVALID",
                "Boolean streams required",
            )?;
            if p[format!("enable_{stream}")] == true {
                for suffix in ["width", "height", "fps"] {
                    require(
                        p[format!("{stream}_{suffix}")]
                            .as_u64()
                            .is_some_and(|v| v > 0),
                        "CONFIG_INVALID",
                        "Positive profile required",
                    )?;
                }
            }
        }
        health(&c["health"], true)?;
        let sn = text(&c["serial_number"]);
        require(
            !sn.is_empty() && serials.insert(sn),
            "CONFIG_INVALID",
            "Empty or duplicate binding",
        )?;
        require(
            draft || (!sn.starts_with("REPLACE_") && c["profiles_verified"] == true),
            "CONFIG_INVALID",
            "Verify actual profile",
        )?;
    }
    Ok(())
}
pub fn invalidate(previous: &Value, candidate: &Value) -> Value {
    let mut out = candidate.clone();
    let before = previous["cameras"].as_array().cloned().unwrap_or_default();
    for c in out["cameras"].as_array_mut().unwrap() {
        if before
            .iter()
            .find(|b| b["name"] == c["name"])
            .is_none_or(|b| camera_digest(previous, b) != camera_digest(candidate, c))
        {
            c["profiles_verified"] = false.into();
            if candidate["schema_version"] == 2 {
                c["verification"] = Value::Null;
            }
        }
    }
    out
}
pub fn resolved(data: &Value, c: &Value) -> Value {
    let mut c = c.clone();
    if data["schema_version"] == 1 {
        let prefix = format!("{}_camera", text(&c["name"]));
        c["namespace"] = format!("/{prefix}").into();
        c["frame_prefix"] = prefix.into();
    } else {
        let p = &catalog()["profiles"][text(&c["profile"])];
        c["model"] = p["model"].clone();
        c["launch_file"] = p["launch_file"].clone();
        let q = qos(&c["parameters"]).unwrap();
        c["parameters"]
            .as_object_mut()
            .unwrap()
            .extend(q.as_object().unwrap().clone());
    }
    c
}
pub fn migrate(data: &Value, bindings: &Value) -> Result<Value> {
    validate(data, true)?;
    require(
        data["schema_version"] == 1,
        "MIGRATION_SCHEMA",
        "Expected legacy inventory",
    )?;
    require(
        bindings.is_null() || bindings.is_object(),
        "MIGRATION_BINDING",
        "Expected binding map",
    )?;
    if let Some(b) = bindings.as_object() {
        require(
            b.keys().all(|k| {
                data["cameras"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|c| c["name"] == *k)
            }),
            "MIGRATION_BINDING",
            "Unknown legacy identity",
        )?;
    }
    let mut out = data.clone();
    out["schema_version"] = 2.into();
    let catalog = catalog();
    for c in out["cameras"].as_array_mut().unwrap() {
        let name = text(&c["name"]);
        let default = json!({"name":name,"namespace":format!("/{name}_camera"),"frame_prefix":format!("{name}_camera")});
        let b = bindings.get(name).unwrap_or(&default);
        require(
            fields(b, "name namespace frame_prefix"),
            "MIGRATION_BINDING",
            "Complete explicit binding required",
        )?;
        let profile = catalog["profiles"]
            .as_object()
            .unwrap()
            .iter()
            .find(|(_, p)| p["model"] == c["model"])
            .unwrap()
            .0
            .clone();
        let m = c.as_object_mut().unwrap();
        m.remove("model");
        m.remove("launch_file");
        m.extend(b.as_object().unwrap().clone());
        c["profile"] = profile.into();
        c["profiles_verified"] = false.into();
        c["verification"] = Value::Null;
        c["depth_unit"] = "mm".into();
        c["timestamp_source"] = c["parameters"]
            .get("time_domain")
            .cloned()
            .unwrap_or(json!(""));
        c["calibration_revision"] = "".into();
        c["parameters"]["color_format"] = "YUYV".into();
        c["parameters"]
            .as_object_mut()
            .unwrap()
            .extend(qos(&json!({}))?.as_object().unwrap().clone());
        if placeholder(text(&c["serial_number"])) {
            c["serial_number"] = "".into();
            c["enabled"] = false.into();
        }
    }
    validate(&out, true)?;
    Ok(out)
}
