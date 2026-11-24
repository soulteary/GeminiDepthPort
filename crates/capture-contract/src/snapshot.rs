//! Immutable capture evidence. Legacy imports remain offline provenance and
//! cannot be used as evidence of a live session by the recording gate.
use crate::{
    CalibrationId, CameraModel, ConfigurationId, ContentKind, ContractError, DepthRepresentation,
    ErrorClass, GenerationId, InstanceId, QualityPolicy, Result, RigidTransform, SessionId,
    SnapshotId, VERSION, check, content_digest, parse_strict,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CalibrationSource {
    SdkActiveProfiles,
    RosCameraInfoTf,
    LegacyImport,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationRecord {
    pub version: u32,
    pub source: CalibrationSource,
    pub actual_runtime_observed: bool,
    pub source_session_id: Option<SessionId>,
    pub source_instance_id: Option<InstanceId>,
    pub observed_unix_ns: Option<u64>,
    pub models: BTreeMap<String, CameraModel>,
    pub transforms: Vec<RigidTransform>,
    pub physical_accuracy_verified: bool,
    pub legacy_source_sha256: Option<String>,
}
impl CalibrationRecord {
    pub fn validate(&self) -> Result<()> {
        check(
            self.version == VERSION,
            ErrorClass::UnsupportedVersion,
            "CALIBRATION_VERSION",
        )?;
        check(
            !self.physical_accuracy_verified,
            ErrorClass::InvalidInput,
            "PHYSICAL_VERIFICATION_NOT_CERTIFIED",
        )?;
        check(
            !self.models.is_empty() && self.models.len() <= 16 && self.transforms.len() <= 32,
            ErrorClass::Geometry,
            "CALIBRATION_MODEL_COUNT",
        )?;
        for (name, model) in &self.models {
            check(
                crate::token(name),
                ErrorClass::Geometry,
                "MODEL_NAME_INVALID",
            )?;
            model.validate()?;
        }
        for t in &self.transforms {
            t.validate()?;
            check(
                self.models.values().any(|m| m.frame_id == t.source_frame)
                    && self.models.values().any(|m| m.frame_id == t.target_frame),
                ErrorClass::Binding,
                "CALIBRATION_TRANSFORM_UNBOUND",
            )?;
        }
        let mut frames = std::collections::BTreeSet::new();
        check(
            self.models.values().all(|m| frames.insert(&m.frame_id)),
            ErrorClass::Binding,
            "CALIBRATION_DUPLICATE_FRAME",
        )?;
        if self.source == CalibrationSource::LegacyImport {
            check(
                !self.actual_runtime_observed
                    && self.source_session_id.is_none()
                    && self.source_instance_id.is_none()
                    && self.observed_unix_ns.is_none()
                    && self
                        .legacy_source_sha256
                        .as_ref()
                        .is_some_and(|s| crate::CalibrationId::parse(s.clone()).is_ok()),
                ErrorClass::Binding,
                "LEGACY_PROVENANCE_INVALID",
            )?;
        } else {
            check(
                self.actual_runtime_observed
                    && self.source_session_id.is_some()
                    && self.source_instance_id.is_some()
                    && self.observed_unix_ns.is_some_and(|v| v > 0)
                    && self.legacy_source_sha256.is_none(),
                ErrorClass::Binding,
                "RUNTIME_CALIBRATION_PROVENANCE_REQUIRED",
            )?;
        }
        Ok(())
    }
    pub fn id(&self) -> Result<CalibrationId> {
        self.validate()?;
        CalibrationId::parse(content_digest(ContentKind::Calibration, &encode(self)?)?)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotBinding {
    pub instance_id: InstanceId,
    pub session_id: SessionId,
    pub generation_id: GenerationId,
    pub configuration_id: ConfigurationId,
    pub calibration_id: CalibrationId,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureSnapshot {
    pub version: u32,
    pub binding: SnapshotBinding,
    /// Capture configuration content, excluding any self-referential ID/proof.
    pub configuration: Value,
    pub calibration: CalibrationRecord,
    pub depth: DepthRepresentation,
    pub quality_policy: QualityPolicy,
}
impl CaptureSnapshot {
    pub fn validate(&self) -> Result<()> {
        check(
            self.version == VERSION,
            ErrorClass::UnsupportedVersion,
            "SNAPSHOT_VERSION",
        )?;
        self.calibration.validate()?;
        self.depth.validate()?;
        self.quality_policy.validate()?;
        check(
            self.binding.configuration_id
                == ConfigurationId::parse(content_digest(
                    ContentKind::Configuration,
                    &encode(&self.configuration)?,
                )?)?,
            ErrorClass::Integrity,
            "CONFIGURATION_ID_MISMATCH",
        )?;
        check(
            self.binding.calibration_id == self.calibration.id()?,
            ErrorClass::Integrity,
            "CALIBRATION_ID_MISMATCH",
        )?;
        if self.calibration.actual_runtime_observed {
            check(
                self.calibration.source_session_id.as_ref() == Some(&self.binding.session_id)
                    && self.calibration.source_instance_id.as_ref()
                        == Some(&self.binding.instance_id),
                ErrorClass::Binding,
                "CALIBRATION_SESSION_MISMATCH",
            )?;
        }
        Ok(())
    }
    pub fn seal(self) -> Result<SnapshotEnvelope> {
        self.validate()?;
        let snapshot_id =
            SnapshotId::parse(content_digest(ContentKind::Snapshot, &encode(&self)?)?)?;
        Ok(SnapshotEnvelope {
            version: VERSION,
            snapshot_id,
            content: self,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotEnvelope {
    pub version: u32,
    pub snapshot_id: SnapshotId,
    pub content: CaptureSnapshot,
}
impl SnapshotEnvelope {
    pub fn parse(bytes: &[u8], expected: &SnapshotBinding) -> Result<Self> {
        let value = parse_strict(bytes)?;
        let envelope: Self = serde_json::from_value(value)
            .map_err(|_| ContractError::new(ErrorClass::InvalidInput, "SNAPSHOT_SCHEMA"))?;
        envelope.verify(expected)?;
        Ok(envelope)
    }
    pub fn verify(&self, expected: &SnapshotBinding) -> Result<()> {
        check(
            self.version == VERSION,
            ErrorClass::UnsupportedVersion,
            "SNAPSHOT_ENVELOPE_VERSION",
        )?;
        self.content.validate()?;
        check(
            &self.content.binding == expected,
            ErrorClass::Binding,
            "SNAPSHOT_BINDING_MISMATCH",
        )?;
        check(
            self.snapshot_id == self.content.clone().seal()?.snapshot_id,
            ErrorClass::Integrity,
            "SNAPSHOT_ID_MISMATCH",
        )
    }
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.verify(&self.content.binding)?;
        encode(self)
    }
    /// Only a live source can authorize a recording gate; offline import is read-only evidence.
    pub fn require_runtime(&self) -> Result<()> {
        self.verify(&self.content.binding)?;
        check(
            self.content.calibration.actual_runtime_observed,
            ErrorClass::Binding,
            "OFFLINE_CALIBRATION_NOT_RUNTIME_EVIDENCE",
        )
    }
}
pub(crate) fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value)
        .map_err(|_| ContractError::new(ErrorClass::InvalidInput, "CONTRACT_SERIALIZATION"))
}
/// Read-only conversion of the previous calibration-v1 format. Original bytes
/// and the legacy digest remain unchanged; no session/instance is invented.
pub fn import_legacy(bytes: &[u8], camera_name: &str) -> Result<CalibrationRecord> {
    let mut value = parse_strict(bytes)?;
    check(
        value["kind"] == "geminidepthport-calibration-v1"
            && value["structural_validation"] == true
            && value["physical_accuracy_verified"] == false,
        ErrorClass::InvalidInput,
        "LEGACY_CALIBRATION_SCHEMA",
    )?;
    let claimed = value
        .as_object_mut()
        .unwrap()
        .remove("snapshot_sha256")
        .and_then(|v| v.as_str().map(str::to_owned))
        .ok_or_else(|| ContractError::new(ErrorClass::Integrity, "LEGACY_DIGEST_MISSING"))?;
    let legacy_hash = raw_sha(legacy_canonical(&value).as_bytes());
    check(
        legacy_hash == claimed,
        ErrorClass::Integrity,
        "LEGACY_DIGEST_MISMATCH",
    )?;
    let cameras = value["cameras"]
        .as_array()
        .ok_or_else(|| ContractError::new(ErrorClass::Binding, "LEGACY_CAMERA_MISSING"))?;
    let selected: Vec<_> = cameras
        .iter()
        .filter(|c| c["name"] == camera_name)
        .collect();
    check(
        selected.len() == 1,
        ErrorClass::Binding,
        "LEGACY_CAMERA_AMBIGUOUS_OR_MISSING",
    )?;
    let camera = selected[0];
    let mut models = BTreeMap::new();
    let intrinsics = camera["intrinsics"]
        .as_object()
        .ok_or_else(|| ContractError::new(ErrorClass::Geometry, "LEGACY_INTRINSICS_MISSING"))?;
    for (name, model) in intrinsics {
        let distortion_model = match model["distortion_model"].as_str() {
            Some("plumb_bob") => crate::DistortionModel::PlumbBob,
            Some("rational_polynomial") => crate::DistortionModel::RationalPolynomial,
            Some("equidistant") => crate::DistortionModel::Equidistant,
            _ => {
                return Err(ContractError::new(
                    ErrorClass::Geometry,
                    "LEGACY_DISTORTION_UNSUPPORTED",
                ));
            }
        };
        let converted = CameraModel {
            frame_id: field(model, "frame_id")?,
            width: serde_json::from_value(model["width"].clone())
                .map_err(|_| ContractError::new(ErrorClass::Geometry, "LEGACY_DIMENSIONS"))?,
            height: serde_json::from_value(model["height"].clone())
                .map_err(|_| ContractError::new(ErrorClass::Geometry, "LEGACY_DIMENSIONS"))?,
            k: array(&model["k"])?,
            distortion_model,
            distortion_coefficients: serde_json::from_value(model["d"].clone())
                .map_err(|_| ContractError::new(ErrorClass::Geometry, "LEGACY_DISTORTION"))?,
        };
        converted.validate()?;
        models.insert(name.clone(), converted);
    }
    let mut transforms = vec![];
    if let Some(t) = camera.get("depth_to_color") {
        check(
            t["translation_unit"] == "m" && t["rotation_representation"] == "quaternion_xyzw",
            ErrorClass::Geometry,
            "LEGACY_TRANSFORM_UNIT",
        )?;
        let [x, y, z, w]: [f64; 4] = array(&t["rotation"])?;
        check(
            [x, y, z, w].iter().all(|v| v.is_finite())
                && (x * x + y * y + z * z + w * w - 1.).abs() < 1e-6,
            ErrorClass::Geometry,
            "LEGACY_ROTATION_INVALID",
        )?;
        transforms.push(RigidTransform {
            source_frame: field(t, "source_frame")?,
            target_frame: field(t, "target_frame")?,
            translation_m: array(&t["translation"])?,
            rotation_row_major: [
                1. - 2. * (y * y + z * z),
                2. * (x * y - z * w),
                2. * (x * z + y * w),
                2. * (x * y + z * w),
                1. - 2. * (x * x + z * z),
                2. * (y * z - x * w),
                2. * (x * z - y * w),
                2. * (y * z + x * w),
                1. - 2. * (x * x + y * y),
            ],
        });
    }
    let out = CalibrationRecord {
        version: VERSION,
        source: CalibrationSource::LegacyImport,
        actual_runtime_observed: false,
        source_session_id: None,
        source_instance_id: None,
        observed_unix_ns: None,
        models,
        transforms,
        physical_accuracy_verified: false,
        legacy_source_sha256: Some(raw_sha(bytes)),
    };
    out.validate()?;
    Ok(out)
}
fn field(v: &Value, name: &str) -> Result<String> {
    v[name]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| ContractError::new(ErrorClass::InvalidInput, "LEGACY_FIELD_INVALID"))
}
fn array<const N: usize>(v: &Value) -> Result<[f64; N]> {
    let values: Vec<f64> = serde_json::from_value(v.clone())
        .map_err(|_| ContractError::new(ErrorClass::Geometry, "LEGACY_ARRAY_INVALID"))?;
    values
        .try_into()
        .map_err(|_| ContractError::new(ErrorClass::Geometry, "LEGACY_ARRAY_INVALID"))
}
pub(crate) fn raw_sha(bytes: &[u8]) -> String {
    crate::lower_hex(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
}

// Exact compatibility with the pre-contract Python canonical calibration digest.
fn legacy_canonical(v: &Value) -> String {
    match v {
        Value::String(s) => {
            let encoded = serde_json::to_string(s).unwrap();
            let mut out = String::new();
            for c in encoded.chars() {
                if c <= '~' {
                    out.push(c);
                } else {
                    let mut units = [0; 2];
                    for u in c.encode_utf16(&mut units) {
                        out.push_str(&format!("\\u{u:04x}"));
                    }
                }
            }
            out
        }
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(legacy_canonical).collect::<Vec<_>>().join(",")
        ),
        Value::Object(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!(
                    "{}:{}",
                    legacy_canonical(&Value::String(k.clone())),
                    legacy_canonical(v)
                ))
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::Number(n) if n.is_f64() => python_float(&n.to_string()),
        _ => v.to_string(),
    }
}
fn python_float(s: &str) -> String {
    let (sign, unsigned) = if let Some(v) = s.strip_prefix('-') {
        ("-", v)
    } else {
        ("", s)
    };
    let (mantissa, power) = unsigned
        .split_once('e')
        .map(|(m, e)| (m, e.parse::<i32>().unwrap()))
        .unwrap_or((unsigned, 0));
    let decimal = mantissa.find('.').unwrap_or(mantissa.len()) as i32;
    let raw: String = mantissa.chars().filter(|c| *c != '.').collect();
    let Some(first) = raw.find(|c| c != '0') else {
        return format!("{sign}0.0");
    };
    let exponent = decimal - first as i32 - 1 + power;
    let digits = raw[first..].trim_end_matches('0');
    if !(-4..16).contains(&exponent) {
        let m = if digits.len() == 1 {
            digits.to_string()
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        format!(
            "{sign}{m}e{}{:02}",
            if exponent < 0 { "-" } else { "+" },
            exponent.abs()
        )
    } else {
        let point = exponent + 1;
        if point <= 0 {
            format!("{sign}0.{}{digits}", "0".repeat((-point) as usize))
        } else if point as usize >= digits.len() {
            format!(
                "{sign}{digits}{}.0",
                "0".repeat(point as usize - digits.len())
            )
        } else {
            format!(
                "{sign}{}.{}",
                &digits[..point as usize],
                &digits[point as usize..]
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn legacy_fixture() -> Value {
        json!({
            "kind": "geminidepthport-calibration-v1",
            "structural_validation": true,
            "physical_accuracy_verified": false,
            "cameras": [{"name": "front", "intrinsics": {"depth": {
                "frame_id": "depth_optical_frame", "width": 640, "height": 480,
                "k": [500.0, 0.0, 320.0, 0.0, 500.0, 240.0, 0.0, 0.0, 1.0],
                "distortion_model": "plumb_bob", "d": [0.0, 0.0, 0.0, 0.0, 0.0]
            }}}]
        })
    }

    fn legacy_bytes(mut value: Value) -> Vec<u8> {
        value["snapshot_sha256"] = json!(raw_sha(legacy_canonical(&value).as_bytes()));
        serde_json::to_vec(&value).unwrap()
    }

    #[test]
    fn legacy_import_reports_missing_or_non_string_digest() {
        for digest in [None, Some(json!(17))] {
            let mut value = legacy_fixture();
            if let Some(digest) = digest {
                value["snapshot_sha256"] = digest;
            }
            let err = import_legacy(&serde_json::to_vec(&value).unwrap(), "front").unwrap_err();
            assert_eq!(err.class, ErrorClass::Integrity);
            assert_eq!(err.code, "LEGACY_DIGEST_MISSING");
        }
    }

    #[test]
    fn legacy_import_preserves_field_error_classes_after_digest_validation() {
        import_legacy(&legacy_bytes(legacy_fixture()), "front").unwrap();
        for (pointer, replacement, class, code) in [
            (
                "/cameras",
                json!(null),
                ErrorClass::Binding,
                "LEGACY_CAMERA_MISSING",
            ),
            (
                "/cameras/0/intrinsics",
                json!(null),
                ErrorClass::Geometry,
                "LEGACY_INTRINSICS_MISSING",
            ),
            (
                "/cameras/0/intrinsics/depth/frame_id",
                json!(42),
                ErrorClass::InvalidInput,
                "LEGACY_FIELD_INVALID",
            ),
            (
                "/cameras/0/intrinsics/depth/width",
                json!("640"),
                ErrorClass::Geometry,
                "LEGACY_DIMENSIONS",
            ),
            (
                "/cameras/0/intrinsics/depth/height",
                json!(-1),
                ErrorClass::Geometry,
                "LEGACY_DIMENSIONS",
            ),
            (
                "/cameras/0/intrinsics/depth/d",
                json!({}),
                ErrorClass::Geometry,
                "LEGACY_DISTORTION",
            ),
            (
                "/cameras/0/intrinsics/depth/k",
                json!({}),
                ErrorClass::Geometry,
                "LEGACY_ARRAY_INVALID",
            ),
            (
                "/cameras/0/intrinsics/depth/k",
                json!([1.0, 2.0]),
                ErrorClass::Geometry,
                "LEGACY_ARRAY_INVALID",
            ),
        ] {
            let mut value = legacy_fixture();
            *value.pointer_mut(pointer).unwrap() = replacement;
            let err = import_legacy(&legacy_bytes(value), "front").unwrap_err();
            assert_eq!(err.class, class, "{pointer}");
            assert_eq!(err.code, code, "{pointer}");
        }
    }

    #[test]
    fn encoding_failure_is_reported_as_a_contract_error() {
        struct RefusesSerialization;
        impl Serialize for RefusesSerialization {
            fn serialize<S: serde::Serializer>(
                &self,
                _serializer: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("value cannot be serialized"))
            }
        }
        let err = encode(&RefusesSerialization).unwrap_err();
        assert_eq!(err.class, ErrorClass::InvalidInput);
        assert_eq!(err.code, "CONTRACT_SERIALIZATION");
    }
}
