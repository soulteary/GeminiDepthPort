//! Capture contracts contain no SDK, ROS, Docker or wall-clock acquisition.
//! Monotonic receive times are offsets from a declared session origin. They
//! must never be compared between sessions or interpreted as Unix timestamps.
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

pub mod acquisition;
pub mod content;
pub use acquisition::{
    AcquisitionOverrides, AcquisitionPolicy, AcquisitionPreset, FrameAggregate, MessageTimestamp,
    ResolvedAcquisition, UnpairedPolicy,
};
pub mod geometry;
pub mod recording;
pub mod snapshot;
#[cfg(unix)]
pub mod store;
pub mod timing;
pub use content::{ContentKind, content_digest, parse_strict};
pub use geometry::{CameraModel, DistortionModel, RigidTransform};
pub const VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorClass {
    InvalidInput,
    Integrity,
    Binding,
    UnsupportedVersion,
    Geometry,
    Protection,
    Io,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractError {
    pub version: u32,
    pub class: ErrorClass,
    pub code: String,
}
impl ContractError {
    pub fn new(class: ErrorClass, code: &str) -> Self {
        Self {
            version: VERSION,
            class,
            code: code.into(),
        }
    }
}
impl fmt::Display for ContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.code)
    }
}
impl std::error::Error for ContractError {}
pub type Result<T> = std::result::Result<T, ContractError>;
pub(crate) fn check(ok: bool, class: ErrorClass, code: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(ContractError::new(class, code))
    }
}
macro_rules! id {
    ($name:ident, $len:expr) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);
        impl $name {
            pub fn parse(value: impl Into<String>) -> Result<Self> {
                let value = value.into();
                check(
                    value.len() == $len
                        && value
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                    ErrorClass::InvalidInput,
                    concat!(stringify!($name), "_INVALID"),
                )?;
                Ok(Self(value))
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl FromStr for $name {
            type Err = ContractError;
            fn from_str(s: &str) -> Result<Self> {
                Self::parse(s)
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(
                deserializer: D,
            ) -> std::result::Result<Self, D::Error> {
                Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
    };
}
id!(SessionId, 32);
// Legacy generation IDs retain their existing 32-hex format and hash algorithm.
id!(GenerationId, 32);
id!(ConfigurationId, 64);
id!(CalibrationId, 64);
id!(SnapshotId, 64);
id!(InstanceId, 64);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonoTime {
    pub session_id: SessionId,
    pub offset_ns: u64,
}
impl MonoTime {
    pub fn elapsed_since(&self, earlier: &Self) -> Result<u64> {
        check(
            self.session_id == earlier.session_id,
            ErrorClass::Binding,
            "MONOTONIC_SESSION_MISMATCH",
        )?;
        self.offset_ns
            .checked_sub(earlier.offset_ns)
            .ok_or_else(|| ContractError::new(ErrorClass::Binding, "MONOTONIC_REGRESSION"))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClockDomain {
    DeviceMicroseconds,
    SdkSystemUnixMicroseconds,
    SessionMonotonicNanoseconds,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameContext {
    pub version: u32,
    pub session_id: SessionId,
    pub generation_id: GenerationId,
    pub configuration_id: ConfigurationId,
    pub calibration_id: CalibrationId,
    pub snapshot_id: SnapshotId,
    pub stream_id: String,
    /// None means unobserved; zero is an observed valid source origin.
    pub sequence: Option<u64>,
    pub device_timestamp_us: Option<u64>,
    pub system_timestamp_us: Option<u64>,
    /// Publisher-reported capture time mapped into the SDK host clock domain.
    /// Missing values preserve the serialization of existing sealed records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_timestamp_us: Option<u64>,
    pub received_mono_offset_ns: u64,
}
impl FrameContext {
    pub fn validate(&self) -> Result<()> {
        check(
            self.version == VERSION,
            ErrorClass::UnsupportedVersion,
            "FRAME_CONTEXT_VERSION",
        )?;
        check(
            token(&self.stream_id),
            ErrorClass::InvalidInput,
            "STREAM_ID_INVALID",
        )
    }
    pub fn received_at(&self) -> MonoTime {
        MonoTime {
            session_id: self.session_id.clone(),
            offset_ns: self.received_mono_offset_ns,
        }
    }
}
pub fn token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}
/// Same integer conversion as frame-core: round nearest with halves upwards,
/// preserve invalid raw zero, clamp positive sub-mm to 1, reject frame overflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QuantizationPolicy {
    NearestHalfUpPreserveZeroClampPositiveRejectOverflow,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "representation", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DepthRepresentation {
    SdkCounts { scale_mm: f64 },
    Millimetres { quantization: QuantizationPolicy },
}
impl DepthRepresentation {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::SdkCounts { scale_mm } => check(
                scale_mm.is_finite() && *scale_mm > 0.,
                ErrorClass::InvalidInput,
                "DEPTH_SCALE_INVALID",
            ),
            Self::Millimetres { .. } => Ok(()),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QualityAction {
    ReportOnly,
    FailClosed,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityPolicy {
    pub version: u32,
    pub max_source_age_ns: u64,
    pub max_future_ns: u64,
    pub max_pair_delta_us: u64,
    pub min_valid_fraction: f64,
    pub max_zero_fraction: f64,
    pub action: QualityAction,
}
impl QualityPolicy {
    pub fn validate(&self) -> Result<()> {
        check(
            self.version == VERSION,
            ErrorClass::UnsupportedVersion,
            "QUALITY_POLICY_VERSION",
        )?;
        check(
            self.max_source_age_ns > 0
                && self.max_pair_delta_us > 0
                && [self.min_valid_fraction, self.max_zero_fraction]
                    .iter()
                    .all(|x| x.is_finite() && (0.0..=1.0).contains(x)),
            ErrorClass::InvalidInput,
            "QUALITY_POLICY_INVALID",
        )
    }
}
pub(crate) fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|b| {
            [
                HEX[(b >> 4) as usize] as char,
                HEX[(b & 15) as usize] as char,
            ]
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lowercase_hex_keeps_every_byte_and_standard_sha256() {
        assert_eq!(lower_hex(&[0, 1, 15, 16, 127, 128, 255]), "00010f107f80ff");
        let bytes: Vec<u8> = (0..=255).collect();
        let mut reference = String::new();
        for byte in &bytes {
            use std::fmt::Write;
            write!(&mut reference, "{byte:02x}").unwrap();
        }
        assert_eq!(lower_hex(&bytes), reference);
        assert_eq!(
            crate::snapshot::raw_sha(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn ids_reject_case_length_and_nonhex_on_deserialize() {
        assert!(SessionId::parse("a".repeat(32)).is_ok());
        for bad in ["A".repeat(32), "g".repeat(32), "a".repeat(31)] {
            assert!(SessionId::parse(bad.clone()).is_err());
            assert!(serde_json::from_str::<SessionId>(&format!("\"{bad}\"")).is_err());
        }
    }
    #[test]
    fn receive_offsets_require_same_session_and_advance() {
        let a = MonoTime {
            session_id: SessionId::parse("a".repeat(32)).unwrap(),
            offset_ns: 1,
        };
        let mut b = a.clone();
        b.offset_ns = 5;
        assert_eq!(b.elapsed_since(&a).unwrap(), 4);
        assert!(a.elapsed_since(&b).is_err());
        b.session_id = SessionId::parse("b".repeat(32)).unwrap();
        assert!(b.elapsed_since(&a).is_err());
    }
    #[test]
    fn invalid_scale_and_nonfinite_policy_are_not_serialization_nulls() {
        for x in [0., -1., f64::NAN, f64::INFINITY] {
            assert!(
                DepthRepresentation::SdkCounts { scale_mm: x }
                    .validate()
                    .is_err()
            );
        }
        let p = QualityPolicy {
            version: 1,
            max_source_age_ns: 1,
            max_future_ns: 0,
            max_pair_delta_us: 1,
            min_valid_fraction: f64::NAN,
            max_zero_fraction: 1.,
            action: QualityAction::FailClosed,
        };
        assert!(p.validate().is_err());
    }
}

pub mod capabilities;
