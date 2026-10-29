//! Validate only pairs observed in one SDK frameset; never match streams by index.
use crate::{FrameEncoding, FramePacket, PacketError, PacketMetadata};
use std::{error::Error, fmt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairClock {
    Device,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairPolicy {
    pub clock: PairClock,
    /// Explicit positive caller-selected skew budget; no business default.
    pub max_delta_us: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepthUnitPolicy {
    RequireMillimetres,
    PreserveSdkCounts,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairAcceptance {
    pub pairing: PairPolicy,
    pub provenance: PairProvenance,
    pub depth_unit: DepthUnitPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairProvenance {
    /// Caller obtained both child frames from the same SDK frameset object.
    /// This is a caller assertion, not hardware exposure synchronization proof.
    CallerObservedSdkFrameset,
    Unverified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairEvidence {
    pub pair_sequence: u64,
    pub color_sequence: u64,
    pub depth_sequence: u64,
    pub color_timestamp_us: u64,
    pub depth_timestamp_us: u64,
    pub delta_us: u64,
    pub clock: PairClock,
    /// Both child handles were observed by the caller in one SDK frameset.
    pub same_frameset: bool,
    pub hardware_sync_verified: bool,
}

#[derive(Debug)]
pub struct RgbdPair<T, U> {
    pub color: FramePacket<T>,
    pub depth: FramePacket<U>,
    pub evidence: PairEvidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairError {
    InvalidPolicy,
    MissingColor,
    MissingDepth,
    UnverifiedFrameset,
    ColorMetadata(PacketError),
    DepthMetadata(PacketError),
    StreamEncoding,
    DepthNotMillimetres,
    MissingTimestamp,
    ZeroTimestamp,
    SkewExceeded,
    PairSequenceNotAdvancing,
    ColorSequenceNotAdvancing,
    DepthSequenceNotAdvancing,
    ColorTimestampNotAdvancing,
    DepthTimestampNotAdvancing,
}
impl fmt::Display for PairError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ColorMetadata(e) => write!(f, "invalid color metadata: {e}"),
            Self::DepthMetadata(e) => write!(f, "invalid depth metadata: {e}"),
            other => f.write_str(match other {
                Self::InvalidPolicy => "RGBD skew budget must be positive",
                Self::MissingColor => "SDK frameset is missing color",
                Self::MissingDepth => "SDK frameset is missing depth",
                Self::UnverifiedFrameset => "same SDK frameset provenance is required",
                Self::StreamEncoding => "RGBD pair requires YUYV color and Depth16 depth",
                Self::DepthNotMillimetres => "RGBD depth output must be millimetres",
                Self::MissingTimestamp => "selected RGBD clock timestamp is missing",
                Self::ZeroTimestamp => "selected RGBD clock timestamp is zero",
                Self::SkewExceeded => "RGBD timestamp difference exceeds explicit budget",
                Self::PairSequenceNotAdvancing => "RGBD pair sequence did not advance",
                Self::ColorSequenceNotAdvancing => "RGBD color sequence did not advance",
                Self::DepthSequenceNotAdvancing => "RGBD depth sequence did not advance",
                Self::ColorTimestampNotAdvancing => "RGBD color timestamp did not advance",
                Self::DepthTimestampNotAdvancing => "RGBD depth timestamp did not advance",
                Self::ColorMetadata(_) | Self::DepthMetadata(_) => unreachable!(),
            }),
        }
    }
}
impl Error for PairError {}

fn selected_stamp(metadata: &PacketMetadata, clock: PairClock) -> Result<u64, PairError> {
    let stamp = match clock {
        PairClock::Device => metadata.device_timestamp_us,
        PairClock::System => metadata.system_timestamp_us,
    }
    .ok_or(PairError::MissingTimestamp)?;
    if stamp == 0 {
        return Err(PairError::ZeroTimestamp);
    }
    Ok(stamp)
}

/// State for one pair publisher. An invalid candidate never advances either stream.
#[derive(Debug, Default)]
pub struct PairGuard {
    last: Option<PairEvidence>,
    depth_unit: Option<DepthUnitPolicy>,
}
impl PairGuard {
    pub fn last(&self) -> Option<PairEvidence> {
        self.last
    }

    /// Consume both owned packets together. Failure drops both instead of
    /// silently publishing a single stream. State advances only on full success.
    pub fn accept<T, U>(
        &mut self,
        pair_sequence: u64,
        color: Option<FramePacket<T>>,
        depth: Option<FramePacket<U>>,
        payload_lengths: (usize, usize),
        policy: PairPolicy,
        provenance: PairProvenance,
    ) -> Result<RgbdPair<T, U>, PairError> {
        self.accept_with_policy(
            pair_sequence,
            color,
            depth,
            payload_lengths,
            PairAcceptance {
                pairing: policy,
                provenance,
                depth_unit: DepthUnitPolicy::RequireMillimetres,
            },
        )
    }
    /// Native-count callers must preserve both pixels and original SDK scale.
    pub fn accept_with_policy<T, U>(
        &mut self,
        pair_sequence: u64,
        color: Option<FramePacket<T>>,
        depth: Option<FramePacket<U>>,
        payload_lengths: (usize, usize),
        acceptance: PairAcceptance,
    ) -> Result<RgbdPair<T, U>, PairError> {
        let policy = acceptance.pairing;
        let provenance = acceptance.provenance;
        if self
            .depth_unit
            .is_some_and(|unit| unit != acceptance.depth_unit)
        {
            return Err(PairError::InvalidPolicy);
        }
        if policy.max_delta_us == 0 {
            return Err(PairError::InvalidPolicy);
        }
        if provenance != PairProvenance::CallerObservedSdkFrameset {
            return Err(PairError::UnverifiedFrameset);
        }
        let color = color.ok_or(PairError::MissingColor)?;
        let depth = depth.ok_or(PairError::MissingDepth)?;
        color
            .metadata
            .validate(payload_lengths.0)
            .map_err(PairError::ColorMetadata)?;
        depth
            .metadata
            .validate(payload_lengths.1)
            .map_err(PairError::DepthMetadata)?;
        if color.metadata.encoding != FrameEncoding::Yuyv
            || depth.metadata.encoding != FrameEncoding::Depth16
        {
            return Err(PairError::StreamEncoding);
        }
        if match acceptance.depth_unit {
            DepthUnitPolicy::RequireMillimetres => {
                depth.metadata.output_depth_scale_mm != Some(1.0)
            }
            DepthUnitPolicy::PreserveSdkCounts => {
                depth.metadata.normalized
                    || depth.metadata.source_depth_scale_mm != depth.metadata.output_depth_scale_mm
            }
        } {
            return Err(PairError::DepthNotMillimetres);
        }
        let color_timestamp_us = selected_stamp(&color.metadata, policy.clock)?;
        let depth_timestamp_us = selected_stamp(&depth.metadata, policy.clock)?;
        let delta_us = color_timestamp_us.abs_diff(depth_timestamp_us);
        if delta_us > policy.max_delta_us {
            return Err(PairError::SkewExceeded);
        }
        if let Some(last) = self.last {
            if pair_sequence <= last.pair_sequence {
                return Err(PairError::PairSequenceNotAdvancing);
            }
            if color.metadata.sequence <= last.color_sequence {
                return Err(PairError::ColorSequenceNotAdvancing);
            }
            if depth.metadata.sequence <= last.depth_sequence {
                return Err(PairError::DepthSequenceNotAdvancing);
            }
            // Changing clock would compare unrelated domains. A new guard is
            // required whenever the publisher's selected clock changes.
            if policy.clock != last.clock {
                return Err(PairError::InvalidPolicy);
            }
            if color_timestamp_us <= last.color_timestamp_us {
                return Err(PairError::ColorTimestampNotAdvancing);
            }
            if depth_timestamp_us <= last.depth_timestamp_us {
                return Err(PairError::DepthTimestampNotAdvancing);
            }
        }
        let evidence = PairEvidence {
            pair_sequence,
            color_sequence: color.metadata.sequence,
            depth_sequence: depth.metadata.sequence,
            color_timestamp_us,
            depth_timestamp_us,
            delta_us,
            clock: policy.clock,
            same_frameset: true,
            hardware_sync_verified: false,
        };
        self.last = Some(evidence);
        self.depth_unit = Some(acceptance.depth_unit);
        Ok(RgbdPair {
            color,
            depth,
            evidence,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FrameLayout, LatestQueue, PixelFormat};
    use std::{cell::Cell, rc::Rc, time::Instant};
    const POLICY: PairPolicy = PairPolicy {
        clock: PairClock::Device,
        max_delta_us: 10,
    };
    const PROVENANCE: PairProvenance = PairProvenance::CallerObservedSdkFrameset;
    fn packet(depth: bool, sequence: u64, stamp: Option<u64>) -> FramePacket<Vec<u8>> {
        FramePacket {
            metadata: PacketMetadata {
                layout: FrameLayout {
                    width: 2,
                    height: 1,
                    stride: 4,
                    format: if depth {
                        PixelFormat::Depth16
                    } else {
                        PixelFormat::Yuyv
                    },
                },
                encoding: if depth {
                    FrameEncoding::Depth16
                } else {
                    FrameEncoding::Yuyv
                },
                sequence,
                device_timestamp_us: stamp,
                system_timestamp_us: Some(1000),
                global_timestamp_us: None,
                sdk_return_unix_ns: None,
                sdk_system_clock_verified: false,

                source_depth_scale_mm: if depth { Some(1.0) } else { None },
                output_depth_scale_mm: if depth { Some(1.0) } else { None },
                normalized: false,
                received_at: Instant::now(),
            },
            payload: vec![0; 4],
        }
    }
    fn accept(
        guard: &mut PairGuard,
        pair_sequence: u64,
        color_sequence: u64,
        depth_sequence: u64,
        color_stamp: u64,
        depth_stamp: u64,
    ) -> Result<RgbdPair<Vec<u8>, Vec<u8>>, PairError> {
        guard.accept(
            pair_sequence,
            Some(packet(false, color_sequence, Some(color_stamp))),
            Some(packet(true, depth_sequence, Some(depth_stamp))),
            (4, 4),
            POLICY,
            PROVENANCE,
        )
    }
    #[test]
    fn unequal_indices_and_skew_at_budget_are_valid() {
        let pair = accept(&mut PairGuard::default(), 1, 100, 80, 1000, 1010).unwrap();
        assert_eq!(pair.color.metadata.sequence, 100);
        assert_eq!(pair.depth.metadata.sequence, 80);
        assert_eq!(pair.evidence.delta_us, 10);
        assert!(pair.evidence.same_frameset);
        assert!(!pair.evidence.hardware_sync_verified);
    }
    #[test]
    fn missing_zero_skew_and_unverified_candidates_do_not_commit() {
        let mut guard = PairGuard::default();
        assert_eq!(
            guard
                .accept::<Vec<u8>, Vec<u8>>(
                    1,
                    None,
                    Some(packet(true, 2, Some(1))),
                    (4, 4),
                    POLICY,
                    PROVENANCE
                )
                .unwrap_err(),
            PairError::MissingColor
        );
        assert_eq!(
            guard
                .accept::<Vec<u8>, Vec<u8>>(
                    1,
                    Some(packet(false, 1, Some(1))),
                    None,
                    (4, 4),
                    POLICY,
                    PROVENANCE
                )
                .unwrap_err(),
            PairError::MissingDepth
        );
        for (stamp, error) in [
            (None, PairError::MissingTimestamp),
            (Some(0), PairError::ZeroTimestamp),
            (Some(100), PairError::SkewExceeded),
        ] {
            assert_eq!(
                guard
                    .accept(
                        1,
                        Some(packet(false, 1, stamp)),
                        Some(packet(true, 2, Some(1))),
                        (4, 4),
                        POLICY,
                        PROVENANCE
                    )
                    .unwrap_err(),
                error
            );
        }
        assert_eq!(
            guard
                .accept(
                    1,
                    Some(packet(false, 1, Some(1))),
                    Some(packet(true, 2, Some(1))),
                    (4, 4),
                    POLICY,
                    PairProvenance::Unverified
                )
                .unwrap_err(),
            PairError::UnverifiedFrameset
        );
        assert!(guard.last().is_none());
        accept(&mut guard, 1, 1, 2, 1, 1).unwrap();
    }
    #[test]
    fn each_stream_progresses_and_rejection_is_atomic() {
        let mut guard = PairGuard::default();
        accept(&mut guard, 1, 10, 20, 100, 100).unwrap();
        let last = guard.last();
        for (pair, color, depth, cstamp, dstamp, error) in [
            (1, 11, 21, 101, 101, PairError::PairSequenceNotAdvancing),
            (2, 10, 21, 101, 101, PairError::ColorSequenceNotAdvancing),
            (2, 11, 19, 101, 101, PairError::DepthSequenceNotAdvancing),
            (2, 11, 21, 100, 101, PairError::ColorTimestampNotAdvancing),
            (2, 11, 21, 101, 99, PairError::DepthTimestampNotAdvancing),
        ] {
            assert_eq!(
                accept(&mut guard, pair, color, depth, cstamp, dstamp).unwrap_err(),
                error
            );
            assert_eq!(guard.last(), last);
        }
        accept(&mut guard, 2, 11, 21, 101, 101).unwrap();
    }
    #[test]
    fn metadata_mm_clock_and_large_deltas_are_checked() {
        let mut guard = PairGuard::default();
        assert_eq!(
            accept(&mut guard, 1, 1, 2, 1, u64::MAX).unwrap_err(),
            PairError::SkewExceeded
        );
        let mut depth = packet(true, 2, Some(1));
        depth.metadata.source_depth_scale_mm = Some(0.1);
        depth.metadata.output_depth_scale_mm = Some(0.1);
        assert_eq!(
            guard
                .accept(
                    1,
                    Some(packet(false, 1, Some(1))),
                    Some(depth),
                    (4, 4),
                    POLICY,
                    PROVENANCE
                )
                .unwrap_err(),
            PairError::DepthNotMillimetres
        );
        assert_eq!(
            guard
                .accept(
                    1,
                    Some(packet(false, 1, Some(1))),
                    Some(packet(true, 2, Some(1))),
                    (3, 4),
                    POLICY,
                    PROVENANCE
                )
                .unwrap_err(),
            PairError::ColorMetadata(PacketError::Layout(crate::LayoutError::PayloadLength))
        );
        let policy = PairPolicy {
            clock: PairClock::System,
            max_delta_us: 1,
        };
        let pair = guard
            .accept(
                1,
                Some(packet(false, 1, None)),
                Some(packet(true, 2, None)),
                (4, 4),
                policy,
                PROVENANCE,
            )
            .unwrap();
        assert_eq!(pair.evidence.color_timestamp_us, 1000);
        assert_eq!(
            guard
                .accept(
                    2,
                    Some(packet(false, 2, Some(1001))),
                    Some(packet(true, 3, Some(1001))),
                    (4, 4),
                    POLICY,
                    PROVENANCE
                )
                .unwrap_err(),
            PairError::InvalidPolicy
        );
    }
    #[test]
    fn pair_queue_eviction_releases_both_owners() {
        #[derive(Debug)]
        struct Handle(Rc<Cell<u32>>);
        impl Drop for Handle {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let count = Rc::new(Cell::new(0));
        let mut guard = PairGuard::default();
        let mut queue = LatestQueue::new(1).unwrap();
        for sequence in 1..=2 {
            let c = packet(false, sequence, Some(sequence));
            let d = packet(true, sequence + 10, Some(sequence));
            let pair = guard
                .accept(
                    sequence,
                    Some(FramePacket {
                        metadata: c.metadata,
                        payload: Handle(count.clone()),
                    }),
                    Some(FramePacket {
                        metadata: d.metadata,
                        payload: Handle(count.clone()),
                    }),
                    (4, 4),
                    POLICY,
                    PROVENANCE,
                )
                .unwrap();
            drop(queue.push(pair));
        }
        assert_eq!(count.get(), 2);
        drop(queue);
        assert_eq!(count.get(), 4);
    }

    #[test]
    fn rejected_pair_releases_both_owners_and_zero_budget_is_invalid() {
        #[derive(Debug)]
        struct Handle(Rc<Cell<u32>>);
        impl Drop for Handle {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let count = Rc::new(Cell::new(0));
        let mut guard = PairGuard::default();
        let color = packet(false, 1, Some(1));
        let depth = packet(true, 2, Some(1));
        let result = guard.accept(
            1,
            Some(FramePacket {
                metadata: color.metadata,
                payload: Handle(count.clone()),
            }),
            Some(FramePacket {
                metadata: depth.metadata,
                payload: Handle(count.clone()),
            }),
            (4, 4),
            PairPolicy {
                max_delta_us: 0,
                ..POLICY
            },
            PROVENANCE,
        );
        assert_eq!(result.unwrap_err(), PairError::InvalidPolicy);
        assert_eq!(count.get(), 2);
        assert_eq!(guard.last(), None);
        // Raw device timestamps can occupy all u64; delta computation must not
        // cast to signed integers or wrap at the boundary.
        let pair = accept(&mut guard, 1, 1, 2, u64::MAX - 1, u64::MAX).unwrap();
        assert_eq!(pair.evidence.delta_us, 1);
    }
}
