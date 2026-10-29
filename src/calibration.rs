use crate::{config, control::Controller, util::*};
use anyhow::Result;
use serde_json::{Value, json};
pub const KIND: &str = "geminidepthport-calibration-v1";
pub fn digest(v: &Value) -> String {
    hash(canonical(v))
}
pub fn request(d: &Value, generation: &str) -> Result<Value> {
    require(
        d["schema_version"] == 2,
        "CALIBRATION_SCHEMA",
        "Explicit bindings required",
    )?;
    config::validate(d, true)?;
    require(
        matches(r"^[0-9a-f]{32}$", generation),
        "CALIBRATION_GENERATION",
        "Valid generation required",
    )?;
    let mut cameras = vec![];
    for c in config::active(d) {
        let mut streams = json!({});
        let p = &c["parameters"];
        for stream in ["color", "depth"] {
            if p[format!("enable_{stream}")] == true {
                streams[stream] = json!({"topic":format!("{}/{stream}/camera_info",text(&c["namespace"])),"frame_id":format!("{}_{stream}_optical_frame",text(&c["frame_prefix"])),"width":p[format!("{stream}_width")],"height":p[format!("{stream}_height")]});
            }
        }
        cameras.push(json!({"name":c["name"],"configuration_sha256":config::camera_digest(d,&c),"calibration_revision":c["calibration_revision"],"streams":streams}));
    }
    require(
        !cameras.is_empty(),
        "CALIBRATION_MISSING",
        "No enabled cameras",
    )?;
    Ok(json!({"kind":KIND,"generation":generation,"cameras":cameras}))
}
fn numbers(v: &Value, size: Option<usize>) -> Result<Vec<f64>> {
    require(
        v.as_array().is_some_and(|a| {
            size.is_none_or(|s| a.len() == s)
                && a.iter().all(|v| v.as_f64().is_some_and(f64::is_finite))
        }),
        "CALIBRATION_INVALID",
        "Invalid numeric array",
    )?;
    Ok(v.as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect())
}
fn intrinsics(info: &Value, expected: &Value) -> Result<Value> {
    require(
        info.is_object(),
        "CALIBRATION_MISSING",
        "CameraInfo required",
    )?;
    require(
        ["frame_id", "width", "height"]
            .iter()
            .all(|k| info[k] == expected[k]),
        "CALIBRATION_BINDING",
        "CameraInfo geometry differs",
    )?;
    let k = numbers(&info["k"], Some(9))?;
    let r = numbers(&info["r"], Some(9))?;
    let p = numbers(&info["p"], Some(12))?;
    let d = numbers(&info["d"], None)?;
    let identity = [1., 0., 0., 0., 1., 0., 0., 0., 1.];
    let gram: Vec<f64> = (0..3)
        .flat_map(|row| (0..3).map(move |col| (row, col)))
        .map(|(row, col)| (0..3).map(|j| r[row * 3 + j] * r[col * 3 + j]).sum())
        .collect();
    let det = r[0] * (r[4] * r[8] - r[5] * r[7]) - r[1] * (r[3] * r[8] - r[5] * r[6])
        + r[2] * (r[3] * r[7] - r[4] * r[6]);
    require(
        gram.iter().zip(identity).all(|(a, b)| (a - b).abs() < 1e-5)
            && (det - 1.).abs() < 1e-5
            && p[0] > 0.
            && p[5] > 0.
            && (p[10] - 1.).abs() < 1e-6
            && [8, 9, 11].iter().all(|i| p[*i].abs() < 1e-6),
        "CALIBRATION_GEOMETRY",
        "Invalid rectification or projection",
    )?;
    require(
        k[0] > 0.
            && k[4] > 0.
            && (k[8] - 1.).abs() < 1e-6
            && k[6].abs() < 1e-6
            && k[7].abs() < 1e-6,
        "CALIBRATION_UNCALIBRATED",
        "Valid focal lengths and homogeneous intrinsics required",
    )?;
    let len = match text(&info["distortion_model"]) {
        "plumb_bob" => 5,
        "rational_polynomial" => 8,
        "equidistant" => 4,
        _ => 0,
    };
    require(
        len > 0 && d.len() == len,
        "CALIBRATION_DISTORTION",
        "Invalid distortion model or coefficients",
    )?;
    require(
        info.get("roi").is_none_or(|v| v.is_object()),
        "CALIBRATION_INVALID",
        "ROI evidence must be an object",
    )?;
    require(
        ["binning_x", "binning_y"]
            .iter()
            .all(|k| info.get(k).is_none_or(|v| v == 0 || v == 1))
            && ["x_offset", "y_offset", "width", "height"]
                .iter()
                .all(|k| info["roi"].get(k).is_none_or(|v| v.as_u64() == Some(0))),
        "CALIBRATION_GEOMETRY",
        "Binned or cropped geometry unsupported",
    )?;
    let mut out = json!({});
    for key in [
        "frame_id",
        "width",
        "height",
        "distortion_model",
        "k",
        "d",
        "r",
        "p",
    ] {
        out[key] = info[key].clone();
    }
    Ok(out)
}
fn extrinsics(v: &Value, streams: &Value) -> Result<Value> {
    require(
        v.is_object(),
        "CALIBRATION_MISSING",
        "Depth-to-color transform required",
    )?;
    require(
        v["source_frame"] == streams["depth"]["frame_id"]
            && v["target_frame"] == streams["color"]["frame_id"]
            && v["translation_unit"] == "m"
            && v["rotation_representation"] == "quaternion_xyzw",
        "CALIBRATION_TRANSFORM",
        "Explicit direction, meters and xyzw required",
    )?;
    numbers(&v["translation"], Some(3))?;
    let r = numbers(&v["rotation"], Some(4))?;
    require(
        (r.iter().map(|v| v * v).sum::<f64>() - 1.).abs() <= 1e-5,
        "CALIBRATION_TRANSFORM",
        "Unit quaternion required",
    )?;
    let mut out = json!({});
    for k in [
        "source_frame",
        "target_frame",
        "translation_unit",
        "rotation_representation",
        "translation",
        "rotation",
    ] {
        out[k] = v[k].clone();
    }
    out["equation"] = "p_target_m = R_source_to_target * p_source_m + translation_m".into();
    Ok(out)
}
pub fn snapshot(d: &Value, generation: &str, observations: &Value) -> Result<Value> {
    let req = request(d, generation)?;
    require(
        observations["request_sha256"] == digest(&req),
        "CALIBRATION_BINDING",
        "Observation request differs",
    )?;
    require(
        ["ros-camera-info-tf", "offline-evidence"].contains(&text(&observations["source"])),
        "CALIBRATION_SOURCE",
        "Explicit source required",
    )?;
    let observed = &observations["cameras"];
    require(
        observed.as_object().is_some_and(|m| {
            m.len() == req["cameras"].as_array().unwrap().len()
                && req["cameras"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|c| m.contains_key(text(&c["name"])))
        }),
        "CALIBRATION_MISSING",
        "All enabled cameras require evidence",
    )?;
    let mut cameras = vec![];
    for c in req["cameras"].as_array().unwrap() {
        let e = &observed[text(&c["name"])];
        let mut i = json!({});
        for (stream, expected) in c["streams"].as_object().unwrap() {
            i[stream] = intrinsics(&e[stream], expected)?;
        }
        let mut item = c.clone();
        item["intrinsics"] = i;
        if c["streams"].get("color").is_some() && c["streams"].get("depth").is_some() {
            item["depth_to_color"] = extrinsics(&e["depth_to_color"], &c["streams"])?;
        }
        cameras.push(item);
    }
    let mut out = json!({"kind":KIND,"generation":generation,"request_sha256":digest(&req),"cameras":cameras,"evidence_source":observations["source"],"structural_validation":true,"physical_accuracy_verified":false,"robot_hand_eye_included":false});
    out["snapshot_sha256"] = digest(&out).into();
    Ok(out)
}
pub fn export(ctl: &Controller) -> Result<Value> {
    let d = ctl.load()?;
    let s = ctl.check_render(&d)?;
    let existing = ctl.existing(&s)?;
    require(
        existing.len() == s["cameras"].as_array().unwrap().len(),
        "GROUP_INCOMPLETE",
        "Calibration requires every active camera",
    )?;
    for c in s["cameras"].as_array().unwrap() {
        ctl.probe(text(&c["service"]))?;
    }
    let req = request(&d, text(&s["generation"]))?;
    let first = &s["cameras"][0]["name"];
    let container = existing
        .iter()
        .find(|e| e["Config"]["Labels"]["orbbec.camera"] == *first)
        .unwrap();
    let id = text(&container["Id"]);
    let remote = format!("/tmp/calibration-{}.json", random(16));
    let operation = (|| {
        ctl.run_with_input(
            &args(&[
                "docker",
                "exec",
                "-i",
                id,
                "/app/entrypoint.sh",
                "python3",
                "/app/calibration_export.py",
                "--request",
                "-",
                "--output",
                &remote,
            ]),
            140,
            &serde_json::to_vec(&req)?,
        )
        .map_err(|_| {
            anyhow::anyhow!(
                "CALIBRATION_COLLECTION_FAILED: Published evidence could not be collected"
            )
        })?;
        let evidence: Value =
            serde_json::from_str(&ctl.run(&["docker", "exec", id, "cat", &remote])?)
                .map_err(|_| anyhow::anyhow!("CALIBRATION_INVALID: Invalid evidence"))?;
        let current = ctl.check_render(&ctl.load()?)?;
        require(
            current["generation"] == s["generation"] && ctl.load()? == d,
            "CALIBRATION_BINDING",
            "Configuration changed during collection",
        )?;
        let after = ctl.existing(&current)?;
        let identities = |values: &[Value]| {
            values
                .iter()
                .map(|v| {
                    (
                        text(&v["Config"]["Labels"]["orbbec.camera"]).to_string(),
                        text(&v["Id"]).to_string(),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        require(
            after.len() == current["cameras"].as_array().unwrap().len(),
            "GROUP_INCOMPLETE",
            "Camera group changed",
        )?;
        require(
            identities(&after) == identities(&existing),
            "CALIBRATION_BINDING",
            "Driver instances changed during calibration collection",
        )?;
        let record = snapshot(&d, text(&s["generation"]), &evidence)?;
        atomic_json(
            &ctl.root
                .join("state/calibration")
                .join(format!("{}.json", text(&record["snapshot_sha256"]))),
            &record,
        )?;
        Ok(
            json!({"kind":KIND,"camera_count":record["cameras"].as_array().unwrap().len(),"structural_validation":true,"physical_accuracy_verified":false,"snapshot_sha256":record["snapshot_sha256"]}),
        )
    })();
    let _ = ctl.run(&["docker", "exec", id, "rm", "-f", &remote]);
    operation
}
