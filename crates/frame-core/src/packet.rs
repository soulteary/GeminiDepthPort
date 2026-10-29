//! Owned pixels and their per-frame interpretation travel through one queue item.
use crate::{FrameLayout, LayoutError, PixelFormat};
use std::{error::Error, fmt, time::Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameEncoding {
    Yuyv,
    Depth16,
}

impl FrameEncoding {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Yuyv => "yuyv",
            Self::Depth16 => "16UC1",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PacketMetadata {
    pub layout: FrameLayout,
    pub encoding: FrameEncoding,
    pub sequence: u64,
    /// Raw SDK clock value; this module never assumes it is Unix time.
    pub device_timestamp_us: Option<u64>,
    /// SDK system timestamp, eligible for explicit Unix mapping only.
    pub system_timestamp_us: Option<u64>,
    /// Capture timestamp mapped into the host clock domain; zero is unavailable.
    pub global_timestamp_us: Option<u64>,
    /// Host realtime sampled immediately after the SDK wait returned this frameset.
    pub sdk_return_unix_ns: Option<u64>,
    /// True only after SDK context realtime clock selection was checked.
    pub sdk_system_clock_verified: bool,
    pub source_depth_scale_mm: Option<f32>,
    pub output_depth_scale_mm: Option<f32>,
    pub normalized: bool,
    pub received_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketError {
    Layout(LayoutError),
    EncodingMismatch,
    InvalidDepthScale,
    InvalidNormalization,
}

impl fmt::Display for PacketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Layout(error) => write!(f, "invalid packet layout: {error}"),
            Self::EncodingMismatch => f.write_str("packet encoding does not match pixel format"),
            Self::InvalidDepthScale => {
                f.write_str("depth packet requires finite positive source and output scales")
            }
            Self::InvalidNormalization => {
                f.write_str("packet normalization metadata is inconsistent")
            }
        }
    }
}
impl Error for PacketError {}

impl PacketMetadata {
    pub fn validate(&self, payload_len: usize) -> Result<(), PacketError> {
        self.layout
            .validate(payload_len)
            .map_err(PacketError::Layout)?;
        match (self.layout.format, self.encoding) {
            (PixelFormat::Yuyv, FrameEncoding::Yuyv) => {
                if self.source_depth_scale_mm.is_some()
                    || self.output_depth_scale_mm.is_some()
                    || self.normalized
                {
                    return Err(PacketError::InvalidNormalization);
                }
            }
            (PixelFormat::Depth16, FrameEncoding::Depth16) => {
                let valid = |scale: Option<f32>| scale.is_some_and(|s| s.is_finite() && s > 0.0);
                if !valid(self.source_depth_scale_mm) || !valid(self.output_depth_scale_mm) {
                    return Err(PacketError::InvalidDepthScale);
                }
                if self.normalized {
                    // Normalizer emits tightly packed millimetres.
                    if self.output_depth_scale_mm != Some(1.0)
                        || self.layout.stride != self.layout.width as usize * 2
                    {
                        return Err(PacketError::InvalidNormalization);
                    }
                } else if self.source_depth_scale_mm != self.output_depth_scale_mm {
                    return Err(PacketError::InvalidNormalization);
                }
            }
            _ => return Err(PacketError::EncodingMismatch),
        }
        Ok(())
    }
}

/// Owns a pixel buffer or SDK frame handle together with its exact interpretation.
/// No cloning, dereferencing or thread-safety requirements are imposed on `T`.
#[derive(Debug)]
pub struct FramePacket<T> {
    pub metadata: PacketMetadata,
    pub payload: T,
}

/// ROS-compatible nonnegative Unix stamp. Construction/mutation remains public
/// for adapters; every guard entry point validates all fields again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnixStamp {
    pub sec: i32,
    pub nanosec: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StampError {
    MissingSystemTimestamp,
    Zero,
    OutOfRange,
    InvalidNanoseconds,
    Duplicate,
    Regression,
}

impl fmt::Display for StampError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::MissingSystemTimestamp => "SDK system Unix timestamp is missing",
            Self::Zero => "zero timestamp is invalid",
            Self::OutOfRange => "timestamp exceeds nonnegative ROS i32 seconds",
            Self::InvalidNanoseconds => "timestamp nanoseconds must be below one billion",
            Self::Duplicate => "duplicate publisher timestamp",
            Self::Regression => "publisher timestamp regressed",
        })
    }
}
impl Error for StampError {}

impl UnixStamp {
    pub fn validate(self) -> Result<(), StampError> {
        if self.sec < 0 {
            return Err(StampError::OutOfRange);
        }
        if self.nanosec >= 1_000_000_000 {
            return Err(StampError::InvalidNanoseconds);
        }
        if self.sec == 0 && self.nanosec == 0 {
            return Err(StampError::Zero);
        }
        Ok(())
    }
}

/// Explicit SDK-system-Unix microseconds to ROS stamp mapping.
///
/// Callers must select SDK *system* time under a known Unix-clock contract.
/// No device-clock mapping or host-now replacement is attempted on failure.
pub fn sdk_system_unix_stamp(system_timestamp_us: Option<u64>) -> Result<UnixStamp, StampError> {
    let micros = system_timestamp_us.ok_or(StampError::MissingSystemTimestamp)?;
    let sec = i32::try_from(micros / 1_000_000).map_err(|_| StampError::OutOfRange)?;
    let stamp = UnixStamp {
        sec,
        nanosec: ((micros % 1_000_000) * 1_000) as u32,
    };
    stamp.validate()?;
    Ok(stamp)
}

/// One guard per publisher/stream. Never repairs timestamps or merges clocks.
#[derive(Debug, Default)]
pub struct StampGuard {
    last: Option<UnixStamp>,
}
impl StampGuard {
    pub fn last(&self) -> Option<UnixStamp> {
        self.last
    }
    /// Nonmutating preflight; allows metadata/payload/publication checks first.
    pub fn check(&self, stamp: UnixStamp) -> Result<(), StampError> {
        stamp.validate()?;
        if let Some(last) = self.last {
            if stamp == last {
                return Err(StampError::Duplicate);
            }
            if stamp < last {
                return Err(StampError::Regression);
            }
        }
        Ok(())
    }
    /// Commit only after all packet validation and publication succeeds.
    pub fn accept(&mut self, stamp: UnixStamp) -> Result<(), StampError> {
        self.check(stamp)?;
        self.last = Some(stamp);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LatestQueue;
    use std::{cell::Cell, rc::Rc};
    fn metadata() -> PacketMetadata {
        PacketMetadata {
            layout: FrameLayout {
                width: 2,
                height: 1,
                stride: 4,
                format: PixelFormat::Depth16,
            },
            encoding: FrameEncoding::Depth16,
            sequence: 1,
            device_timestamp_us: Some(10),
            system_timestamp_us: Some(1_700_000_000_000_001),
            global_timestamp_us: None,
            sdk_return_unix_ns: None,
            sdk_system_clock_verified: false,

            source_depth_scale_mm: Some(0.1),
            output_depth_scale_mm: Some(1.0),
            normalized: true,
            received_at: Instant::now(),
        }
    }

    #[test]
    fn metadata_rejects_encoding_span_and_scale_mismatches() {
        let good = metadata();
        assert_eq!(good.validate(4), Ok(()));
        assert_eq!(
            good.validate(3),
            Err(PacketError::Layout(LayoutError::PayloadLength))
        );
        assert_eq!(
            PacketMetadata {
                encoding: FrameEncoding::Yuyv,
                ..good.clone()
            }
            .validate(4),
            Err(PacketError::EncodingMismatch)
        );
        for scale in [
            None,
            Some(f32::NAN),
            Some(f32::INFINITY),
            Some(0.0),
            Some(-1.0),
        ] {
            assert_eq!(
                PacketMetadata {
                    source_depth_scale_mm: scale,
                    ..good.clone()
                }
                .validate(4),
                Err(PacketError::InvalidDepthScale)
            );
        }
        assert_eq!(
            PacketMetadata {
                output_depth_scale_mm: Some(0.1),
                ..good.clone()
            }
            .validate(4),
            Err(PacketError::InvalidNormalization)
        );
        assert_eq!(
            PacketMetadata {
                normalized: false,
                ..good.clone()
            }
            .validate(4),
            Err(PacketError::InvalidNormalization)
        );
        assert_eq!(
            PacketMetadata {
                normalized: false,
                output_depth_scale_mm: Some(0.1),
                ..good.clone()
            }
            .validate(4),
            Ok(())
        );
        let padded = PacketMetadata {
            layout: FrameLayout {
                stride: 6,
                ..good.layout
            },
            ..good
        };
        assert_eq!(padded.validate(6), Err(PacketError::InvalidNormalization));
    }

    #[test]
    fn color_metadata_rejects_depth_interpretation() {
        let mut color = metadata();
        color.layout.format = PixelFormat::Yuyv;
        color.encoding = FrameEncoding::Yuyv;
        assert_eq!(color.validate(4), Err(PacketError::InvalidNormalization));
        color.source_depth_scale_mm = None;
        color.output_depth_scale_mm = None;
        color.normalized = false;
        assert_eq!(color.validate(4), Ok(()));
        assert_eq!(color.encoding.as_str(), "yuyv");
        assert_eq!(metadata().encoding.as_str(), "16UC1");
    }

    #[test]
    fn explicit_system_mapping_has_no_fallback_and_checks_ros_boundary() {
        assert_eq!(
            sdk_system_unix_stamp(None),
            Err(StampError::MissingSystemTimestamp)
        );
        assert_eq!(sdk_system_unix_stamp(Some(0)), Err(StampError::Zero));
        assert_eq!(
            sdk_system_unix_stamp(Some(1)),
            Ok(UnixStamp {
                sec: 0,
                nanosec: 1000
            })
        );
        let maximum = i32::MAX as u64 * 1_000_000 + 999_999;
        assert_eq!(
            sdk_system_unix_stamp(Some(maximum)),
            Ok(UnixStamp {
                sec: i32::MAX,
                nanosec: 999_999_000
            })
        );
        assert_eq!(
            sdk_system_unix_stamp(Some(maximum + 1)),
            Err(StampError::OutOfRange)
        );
        assert_eq!(
            sdk_system_unix_stamp(Some(u64::MAX)),
            Err(StampError::OutOfRange)
        );
    }

    #[test]
    fn guard_rejects_bad_stamps_without_advancing_and_is_per_stream() {
        let mut guard = StampGuard::default();
        let stamp = UnixStamp {
            sec: 100,
            nanosec: 1000,
        };
        guard.check(stamp).unwrap();
        assert_eq!(guard.last(), None);
        guard.accept(stamp).unwrap();
        for (bad, error) in [
            (stamp, StampError::Duplicate),
            (
                UnixStamp {
                    sec: 99,
                    nanosec: 0,
                },
                StampError::Regression,
            ),
            (UnixStamp { sec: 0, nanosec: 0 }, StampError::Zero),
            (
                UnixStamp {
                    sec: 101,
                    nanosec: 1_000_000_000,
                },
                StampError::InvalidNanoseconds,
            ),
            (
                UnixStamp {
                    sec: -1,
                    nanosec: 0,
                },
                StampError::OutOfRange,
            ),
        ] {
            assert_eq!(guard.accept(bad), Err(error));
            assert_eq!(guard.last(), Some(stamp));
        }
        let mut independent = StampGuard::default();
        independent.accept(stamp).unwrap();
        guard
            .accept(UnixStamp {
                sec: 100,
                nanosec: 2000,
            })
            .unwrap();
    }

    #[test]
    fn queued_packet_eviction_preserves_metadata_and_releases_owned_handle() {
        struct Handle(Rc<Cell<u32>>);
        impl Drop for Handle {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let releases = Rc::new(Cell::new(0));
        let mut queue = LatestQueue::new(1).unwrap();
        let first = FramePacket {
            metadata: metadata(),
            payload: Handle(releases.clone()),
        };
        queue.push(first);
        let second = FramePacket {
            metadata: PacketMetadata {
                sequence: 2,
                ..metadata()
            },
            payload: Handle(releases.clone()),
        };
        let evicted = queue.push(second).unwrap();
        assert_eq!(evicted.metadata.sequence, 1);
        drop(evicted);
        assert_eq!(releases.get(), 1);
        let remaining = queue.pop().unwrap();
        assert_eq!(remaining.metadata.sequence, 2);
        drop(remaining);
        assert_eq!(releases.get(), 2);
    }
}
