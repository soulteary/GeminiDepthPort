//! Independent raw IMU streams. No interpolation, orientation or extrinsic claim.
use crate::{ContractError, ErrorClass, Result};
use serde::{Deserialize, Serialize};
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
pub struct ImuSample {
    pub kind: ImuKind,
    pub sequence: u64,
    pub device_timestamp_us: u64,
    pub system_timestamp_us: u64,
    pub global_timestamp_us: Option<u64>,
    /// SDK SI units: accel m/s²; gyro rad/s. Axes remain the sensor's own axes.
    pub xyz: [f64; 3],
    pub frame_id: String,
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
    pub fn accept(&mut self, s: &ImuSample, c: &ImuConfig) -> Result<()> {
        c.validate()?;
        let slot = s.kind.slot();
        require(
            c.enabled
                && s.xyz.iter().all(|v| v.is_finite())
                && s.device_timestamp_us > 0
                && s.system_timestamp_us > 0
                && s.system_timestamp_us <= i32::MAX as u64 * 1_000_000 + 999999
                && s.global_timestamp_us
                    .is_none_or(|v| v > 0 && v <= i32::MAX as u64 * 1_000_000 + 999999)
                && &s.frame_id
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
                    && s.system_timestamp_us > system
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
