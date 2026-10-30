//! Independent raw IMU streams. No interpolation, orientation or extrinsic claim.
use crate::{ContractError, ErrorClass, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
fn require(ok: bool) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(ContractError::new(ErrorClass::InvalidInput, "IMU_INVALID"))
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ImuConfig {
    pub enabled: bool,
    pub accel_rate_hz: u32,
    pub gyro_rate_hz: u32,
    pub accel_range_g: u32,
    pub gyro_range_dps: u32,
    pub queue_capacity: usize,
    pub stale_timeout_ms: u64,
    pub accel_frame_id: String,
    pub gyro_frame_id: String,
}
impl ImuConfig {
    pub fn validate(&self) -> Result<()> {
        require(
            [25, 50, 100, 200, 400, 500, 800, 1000].contains(&self.accel_rate_hz)
                && [25, 50, 100, 200, 400, 500, 800, 1000].contains(&self.gyro_rate_hz)
                && [2, 3, 4, 6, 8, 12, 16, 24].contains(&self.accel_range_g)
                && [16, 31, 62, 125, 250, 400, 500, 800, 1000, 2000].contains(&self.gyro_range_dps)
                && (16..=128).contains(&self.queue_capacity)
                && (100..=5000).contains(&self.stale_timeout_ms)
                && crate::token(&self.accel_frame_id)
                && self
                    .accel_frame_id
                    .as_bytes()
                    .first()
                    .is_some_and(|v| v.is_ascii_alphabetic() || *v == b'_')
                && crate::token(&self.gyro_frame_id)
                && self
                    .gyro_frame_id
                    .as_bytes()
                    .first()
                    .is_some_and(|v| v.is_ascii_alphabetic() || *v == b'_')
                && self.accel_frame_id != self.gyro_frame_id,
        )
    }
}
/// Immutable configuration validated at a trust boundary. The owned copy cannot
/// be changed after validation; sample data must still be checked independently.
#[derive(Debug, Clone)]
pub struct ValidatedImuConfig {
    config: ImuConfig,
    frames: [Arc<str>; 2],
}
impl ValidatedImuConfig {
    pub fn new(config: ImuConfig) -> Result<Self> {
        config.validate()?;
        let frames = [
            Arc::from(config.accel_frame_id.as_str()),
            Arc::from(config.gyro_frame_id.as_str()),
        ];
        Ok(Self { config, frames })
    }
    pub fn config(&self) -> &ImuConfig {
        &self.config
    }
    pub fn frame_id(&self, kind: ImuKind) -> &Arc<str> {
        &self.frames[kind.slot()]
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ImuKind {
    Accel,
    Gyro,
}
impl ImuKind {
    pub fn slot(self) -> usize {
        match self {
            Self::Accel => 0,
            Self::Gyro => 1,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ImuSampleFields<F> {
    pub kind: ImuKind,
    pub sequence: u64,
    pub device_timestamp_us: u64,
    pub system_timestamp_us: u64,
    pub global_timestamp_us: Option<u64>,
    /// SDK SI units: accel m/s²; gyro rad/s. Axes remain the sensor's own axes.
    pub xyz: [f64; 3],
    pub frame_id: F,
}
/// Existing owned wire/read API. Shared live samples serialize identically.
pub type ImuSample = ImuSampleFields<String>;
pub type SharedImuSample = ImuSampleFields<Arc<str>>;
impl<F: AsRef<str>> ImuSampleFields<F> {
    pub fn into_owned(self) -> ImuSample {
        ImuSample {
            kind: self.kind,
            sequence: self.sequence,
            device_timestamp_us: self.device_timestamp_us,
            system_timestamp_us: self.system_timestamp_us,
            global_timestamp_us: self.global_timestamp_us,
            xyz: self.xyz,
            frame_id: self.frame_id.as_ref().to_owned(),
        }
    }
}
type SourceProgress = (u64, u64, u64, Option<u64>);
#[derive(Debug, Clone, Default)]
pub struct ImuProgress {
    last: [Option<SourceProgress>; 2],
}
impl ImuProgress {
    pub fn has_both(&self) -> bool {
        self.last.iter().all(Option::is_some)
    }
    pub fn accept<F: AsRef<str>>(&mut self, s: &ImuSampleFields<F>, c: &ImuConfig) -> Result<()> {
        c.validate()?;
        self.accept_sample(s, c)
    }
    /// Validate every sample against an immutable, previously checked binding.
    pub fn accept_validated<F: AsRef<str>>(
        &mut self,
        s: &ImuSampleFields<F>,
        binding: &ValidatedImuConfig,
    ) -> Result<()> {
        self.accept_sample(s, binding.config())
    }
    fn accept_sample<F: AsRef<str>>(
        &mut self,
        s: &ImuSampleFields<F>,
        c: &ImuConfig,
    ) -> Result<()> {
        let slot = s.kind.slot();
        require(
            c.enabled
                && s.xyz.iter().all(|v| v.is_finite())
                && s.device_timestamp_us > 0
                && s.system_timestamp_us > 0
                && s.system_timestamp_us <= i32::MAX as u64 * 1_000_000 + 999999
                && s.global_timestamp_us
                    .is_none_or(|v| v > 0 && v <= i32::MAX as u64 * 1_000_000 + 999999)
                && s.frame_id.as_ref()
                    == if slot == 0 {
                        &c.accel_frame_id
                    } else {
                        &c.gyro_frame_id
                    },
        )?;
        if let Some((index, device, system, global)) = self.last[slot] {
            require(
                s.sequence > index
                    && s.device_timestamp_us > device
                    // Consecutive samples can share an SDK host receipt timestamp.
                    && s.system_timestamp_us >= system
                    && match (global, s.global_timestamp_us) {
                        (Some(a), Some(b)) => b > a,
                        (None, _) => true,
                        _ => false,
                    },
            )?;
        }
        self.last[slot] = Some((
            s.sequence,
            s.device_timestamp_us,
            s.system_timestamp_us,
            s.global_timestamp_us,
        ));
        Ok(())
    }
}
