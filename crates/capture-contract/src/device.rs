//! Startup-only exposure controls. Omission and -1 preserve device settings.
use crate::{ContractError, ErrorClass, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceControls {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_color_auto_exposure: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_exposure: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_gain: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_ir_auto_exposure: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ir_exposure: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ir_gain: Option<i32>,
}
impl DeviceControls {
    pub fn validate(&self) -> Result<()> {
        for (auto, exposure, gain) in [
            (
                self.enable_color_auto_exposure,
                self.color_exposure,
                self.color_gain,
            ),
            (self.enable_ir_auto_exposure, self.ir_exposure, self.ir_gain),
        ] {
            if exposure.into_iter().chain(gain).any(|v| v < -1)
                || (exposure.into_iter().chain(gain).any(|v| v >= 0) && auto != Some(false))
            {
                return Err(ContractError::new(
                    ErrorClass::InvalidInput,
                    "DEVICE_CONTROL_CONFLICT",
                ));
            }
        }
        Ok(())
    }
    pub fn requested(&self) -> bool {
        self.enable_color_auto_exposure.is_some()
            || self.enable_ir_auto_exposure.is_some()
            || [
                self.color_exposure,
                self.color_gain,
                self.ir_exposure,
                self.ir_gain,
            ]
            .into_iter()
            .flatten()
            .any(|v| v >= 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manual_controls_require_explicit_auto_off_and_strict_types() {
        for raw in [
            r#"{"color_exposure":123}"#,
            r#"{"enable_color_auto_exposure":true,"color_gain":0}"#,
            r#"{"ir_gain":-2}"#,
        ] {
            assert!(
                serde_json::from_str::<DeviceControls>(raw)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        for raw in [
            r#"{"color_gain":1.5}"#,
            r#"{"enable_color_auto_exposure":1}"#,
            r#"{"firmware":"x"}"#,
        ] {
            assert!(serde_json::from_str::<DeviceControls>(raw).is_err());
        }
        let keep = DeviceControls {
            color_exposure: Some(-1),
            ..Default::default()
        };
        keep.validate().unwrap();
        assert!(!keep.requested());
        DeviceControls {
            enable_ir_auto_exposure: Some(false),
            ir_gain: Some(0),
            ..Default::default()
        }
        .validate()
        .unwrap();
    }
}
