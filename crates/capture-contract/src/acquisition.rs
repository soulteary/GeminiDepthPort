//! Explicit acquisition choices. Clock mapping is not exposure synchronization.
use crate::{ErrorClass, Result, check};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AcquisitionPreset {
    Complete,
    StrictRgbd,
    HardwareSync,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FrameAggregate {
    Any,
    All,
    Disabled,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MessageTimestamp {
    Global,
    System,
}
impl MessageTimestamp {
    pub fn ros_stamp_source(self) -> &'static str {
        match self {
            Self::Global => "sdk-global-unix",
            Self::System => "sdk-system-unix",
        }
    }
    pub fn camera_timestamp_source(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::System => "host",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnpairedPolicy {
    Preserve,
    Strict,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcquisitionOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdk_frame_sync: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregate: Option<FrameAggregate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_timestamp: Option<MessageTimestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unpaired: Option<UnpairedPolicy>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcquisitionPolicy {
    pub preset: AcquisitionPreset,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advanced: Option<AcquisitionOverrides>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedAcquisition {
    pub sdk_frame_sync: bool,
    pub aggregate: FrameAggregate,
    pub message_timestamp: MessageTimestamp,
    pub unpaired: UnpairedPolicy,
}
impl AcquisitionPolicy {
    pub fn resolve(&self) -> Result<ResolvedAcquisition> {
        check(
            self.preset != AcquisitionPreset::HardwareSync,
            ErrorClass::Protection,
            "HARDWARE_SYNC_UNAVAILABLE",
        )?;
        let strict = self.preset == AcquisitionPreset::StrictRgbd;
        let mut resolved = ResolvedAcquisition {
            sdk_frame_sync: strict,
            aggregate: FrameAggregate::Any,
            message_timestamp: MessageTimestamp::Global,
            unpaired: if strict {
                UnpairedPolicy::Strict
            } else {
                UnpairedPolicy::Preserve
            },
        };
        if let Some(advanced) = &self.advanced {
            resolved.sdk_frame_sync = advanced.sdk_frame_sync.unwrap_or(resolved.sdk_frame_sync);
            resolved.aggregate = advanced.aggregate.unwrap_or(resolved.aggregate);
            resolved.message_timestamp = advanced
                .message_timestamp
                .unwrap_or(resolved.message_timestamp);
            resolved.unpaired = advanced.unpaired.unwrap_or(resolved.unpaired);
        }
        check(
            if strict {
                resolved.sdk_frame_sync
                    && resolved.unpaired == UnpairedPolicy::Strict
                    && resolved.aggregate != FrameAggregate::Disabled
            } else {
                resolved.aggregate != FrameAggregate::All
                    && resolved.unpaired == UnpairedPolicy::Preserve
            },
            ErrorClass::InvalidInput,
            "ACQUISITION_PRESET_CONFLICT",
        )?;
        Ok(resolved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn policy(value: Value) -> AcquisitionPolicy {
        serde_json::from_value(value).unwrap()
    }
    #[test]
    fn presets_have_explicit_distinct_effective_behavior() {
        let complete = policy(json!({"preset":"complete"})).resolve().unwrap();
        assert_eq!(
            complete,
            ResolvedAcquisition {
                sdk_frame_sync: false,
                aggregate: FrameAggregate::Any,
                message_timestamp: MessageTimestamp::Global,
                unpaired: UnpairedPolicy::Preserve
            }
        );
        let strict = policy(json!({"preset":"strict-rgbd"})).resolve().unwrap();
        assert!(strict.sdk_frame_sync);
        assert_eq!(strict.unpaired, UnpairedPolicy::Strict);
        assert_eq!(strict.aggregate, FrameAggregate::Any);
        assert_eq!(
            policy(json!({"preset":"hardware-sync"}))
                .resolve()
                .unwrap_err()
                .code,
            "HARDWARE_SYNC_UNAVAILABLE"
        );
    }
    #[test]
    fn advanced_options_preserve_preset_guarantees_and_serialize_effective_settings() {
        let policy = policy(
            json!({"preset":"complete","advanced":{"sdk_frame_sync":true,"aggregate":"disabled","message_timestamp":"system","unpaired":"preserve"}}),
        );
        let effective = policy.resolve().unwrap();
        assert!(effective.sdk_frame_sync);
        assert_eq!(effective.aggregate, FrameAggregate::Disabled);
        assert_eq!(
            effective.message_timestamp.ros_stamp_source(),
            "sdk-system-unix"
        );
        assert_eq!(
            effective.message_timestamp.camera_timestamp_source(),
            "host"
        );
        assert_eq!(
            MessageTimestamp::Global.ros_stamp_source(),
            "sdk-global-unix"
        );
        assert_eq!(MessageTimestamp::Global.camera_timestamp_source(), "global");
        assert_eq!(
            serde_json::from_value::<AcquisitionPolicy>(serde_json::to_value(&policy).unwrap())
                .unwrap(),
            policy
        );
        assert_eq!(
            serde_json::from_value::<ResolvedAcquisition>(serde_json::to_value(effective).unwrap())
                .unwrap(),
            effective
        );
        for value in [
            json!({"preset":"complete","advanced":{"aggregate":"all"}}),
            json!({"preset":"complete","advanced":{"unpaired":"strict"}}),
            json!({"preset":"strict-rgbd","advanced":{"sdk_frame_sync":false}}),
            json!({"preset":"strict-rgbd","advanced":{"unpaired":"preserve"}}),
            json!({"preset":"strict-rgbd","advanced":{"aggregate":"disabled"}}),
        ] {
            assert_eq!(
                super::tests::policy(value).resolve().unwrap_err().code,
                "ACQUISITION_PRESET_CONFLICT"
            );
        }
        let all =
            super::tests::policy(json!({"preset":"strict-rgbd","advanced":{"aggregate":"all"}}));
        assert_eq!(all.resolve().unwrap().aggregate, FrameAggregate::All);
    }
    #[test]
    fn unknown_fields_and_values_are_rejected() {
        for value in [
            json!({}),
            json!({"preset":"unknown"}),
            json!({"preset":"complete","hardware_sync_verified":true}),
            json!({"preset":"complete","advanced":{"message_timestamp":"device"}}),
            json!({"preset":"complete","advanced":{"extra":true}}),
        ] {
            assert!(serde_json::from_value::<AcquisitionPolicy>(value).is_err());
        }
    }
}
