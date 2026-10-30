//! Data-plane primitives independent of SDK, ROS and image codecs.
//!
//! Queue ownership is explicit: pushing into a full queue returns the oldest
//! item to the caller, which can release it immediately or account for it first.
//! This queue does not impose thread-safety on frame handles.

use std::{collections::VecDeque, error::Error, fmt, time::Instant};

mod depth;
pub use capture_contract::imu;
pub use depth::{
    DepthNormalizationError, DepthSampleStats, DepthStats, NormalizedDepth, measure_depth_samples,
    normalize_depth_mm,
};
mod packet;
mod pair;
pub use packet::{
    FrameEncoding, FramePacket, PacketError, PacketMetadata, StampError, StampGuard, UnixStamp,
    sdk_system_unix_stamp,
};
pub use pair::{
    PairClock, PairError, PairEvidence, PairGuard, PairPolicy, PairProvenance, RgbdPair,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Yuyv,
    Depth16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLayout {
    pub width: u32,
    pub height: u32,
    /// Trusted bytes per row, including padding. YUYV must be tightly packed.
    pub stride: usize,
    pub format: PixelFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutError {
    EmptyDimensions,
    OddYuyvWidth,
    ArithmeticOverflow,
    InvalidStride,
    PayloadLength,
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EmptyDimensions => "frame dimensions must be positive",
            Self::OddYuyvWidth => "YUYV width must be even",
            Self::ArithmeticOverflow => "frame layout exceeds addressable size",
            Self::InvalidStride => "frame stride does not match the pixel format",
            Self::PayloadLength => "payload length does not match declared frame span",
        })
    }
}

impl Error for LayoutError {}

impl FrameLayout {
    pub fn validate(self, payload_len: usize) -> Result<(), LayoutError> {
        if self.width == 0 || self.height == 0 {
            return Err(LayoutError::EmptyDimensions);
        }
        if self.format == PixelFormat::Yuyv && self.width % 2 != 0 {
            return Err(LayoutError::OddYuyvWidth);
        }
        let row = usize::try_from(self.width)
            .map_err(|_| LayoutError::ArithmeticOverflow)?
            .checked_mul(2)
            .ok_or(LayoutError::ArithmeticOverflow)?;
        match self.format {
            PixelFormat::Yuyv if self.stride != row => return Err(LayoutError::InvalidStride),
            PixelFormat::Depth16 if self.stride < row || self.stride % 2 != 0 => {
                return Err(LayoutError::InvalidStride);
            }
            _ => {}
        }
        let span = self
            .stride
            .checked_mul(usize::try_from(self.height).map_err(|_| LayoutError::ArithmeticOverflow)?)
            .ok_or(LayoutError::ArithmeticOverflow)?;
        // Rust slices cannot span more than isize::MAX bytes.
        if span > isize::MAX as usize {
            return Err(LayoutError::ArithmeticOverflow);
        }
        if payload_len != span {
            return Err(LayoutError::PayloadLength);
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct FrameMetadata {
    pub layout: FrameLayout,
    pub sequence: u64,
    /// Raw SDK clock values. No cross-clock mapping is inferred here.
    pub device_timestamp_us: Option<u64>,
    pub system_timestamp_us: Option<u64>,
    /// Host monotonic reception time, suitable for local queue-age measurement.
    pub received_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueStats {
    pub pushed: u64,
    pub popped: u64,
    pub dropped: u64,
    pub high_water: usize,
    pub len: usize,
    pub capacity: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZeroCapacity;

impl fmt::Display for ZeroCapacity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("queue capacity must be positive")
    }
}
impl Error for ZeroCapacity {}

/// Bounded FIFO which evicts its oldest item on overflow.
///
/// This policy is intended for preview/probe paths, not lossless recording.
/// Counters saturate rather than wrapping during long-running sessions.
#[derive(Debug)]
pub struct LatestQueue<T> {
    items: VecDeque<T>,
    capacity: usize,
    pushed: u64,
    popped: u64,
    dropped: u64,
    high_water: usize,
}

impl<T> LatestQueue<T> {
    pub fn new(capacity: usize) -> Result<Self, ZeroCapacity> {
        if capacity == 0 {
            return Err(ZeroCapacity);
        }
        Ok(Self {
            items: VecDeque::new(),
            capacity,
            pushed: 0,
            popped: 0,
            dropped: 0,
            high_water: 0,
        })
    }

    /// Returns the evicted oldest item, if the queue was already full.
    pub fn push(&mut self, item: T) -> Option<T> {
        let evicted = if self.items.len() == self.capacity {
            self.dropped = self.dropped.saturating_add(1);
            self.items.pop_front()
        } else {
            None
        };
        self.items.push_back(item);
        self.pushed = self.pushed.saturating_add(1);
        self.high_water = self.high_water.max(self.items.len());
        evicted
    }

    pub fn pop(&mut self) -> Option<T> {
        let item = self.items.pop_front();
        if item.is_some() {
            self.popped = self.popped.saturating_add(1);
        }
        item
    }

    pub fn stats(&self) -> QueueStats {
        QueueStats {
            pushed: self.pushed,
            popped: self.popped,
            dropped: self.dropped,
            high_water: self.high_water,
            len: self.items.len(),
            capacity: self.capacity,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};

    #[test]
    fn validates_packed_yuyv_and_explicit_depth_padding() {
        let yuyv = FrameLayout {
            width: 4,
            height: 2,
            stride: 8,
            format: PixelFormat::Yuyv,
        };
        assert_eq!(yuyv.validate(16), Ok(()));
        assert_eq!(yuyv.validate(15), Err(LayoutError::PayloadLength));
        assert_eq!(
            FrameLayout { stride: 10, ..yuyv }.validate(20),
            Err(LayoutError::InvalidStride)
        );
        assert_eq!(
            FrameLayout { width: 3, ..yuyv }.validate(16),
            Err(LayoutError::OddYuyvWidth)
        );
        let depth = FrameLayout {
            width: 3,
            height: 2,
            stride: 8,
            format: PixelFormat::Depth16,
        };
        assert_eq!(depth.validate(16), Ok(()));
        assert_eq!(
            FrameLayout { stride: 7, ..depth }.validate(14),
            Err(LayoutError::InvalidStride)
        );
        assert_eq!(
            FrameLayout { stride: 4, ..depth }.validate(8),
            Err(LayoutError::InvalidStride)
        );
        assert_eq!(
            FrameLayout { width: 0, ..depth }.validate(16),
            Err(LayoutError::EmptyDimensions)
        );
    }

    #[test]
    fn rejects_overflow_before_any_payload_access() {
        let layout = FrameLayout {
            width: 2,
            height: 2,
            stride: usize::MAX - 1,
            format: PixelFormat::Depth16,
        };
        assert_eq!(layout.validate(0), Err(LayoutError::ArithmeticOverflow));
    }

    #[test]
    fn queue_is_bounded_and_counts_eviction_separately_from_pop() {
        assert!(LatestQueue::<u8>::new(0).is_err());
        let mut queue = LatestQueue::new(2).unwrap();
        assert_eq!(queue.pop(), None);
        assert_eq!(queue.push(1), None);
        assert_eq!(queue.push(2), None);
        assert_eq!(queue.push(3), Some(1));
        assert_eq!(
            queue.stats(),
            QueueStats {
                pushed: 3,
                popped: 0,
                dropped: 1,
                high_water: 2,
                len: 2,
                capacity: 2
            }
        );
        assert_eq!(queue.pop(), Some(2));
        assert_eq!(queue.pop(), Some(3));
        assert_eq!(queue.pop(), None);
        assert_eq!(queue.stats().popped, 2);
    }

    #[test]
    fn handles_are_released_exactly_once_including_shutdown() {
        struct Handle(Rc<Cell<u32>>);
        impl Drop for Handle {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let released = Rc::new(Cell::new(0));
        let mut queue = LatestQueue::new(1).unwrap();
        drop(queue.push(Handle(released.clone())));
        assert_eq!(released.get(), 0);
        let evicted = queue.push(Handle(released.clone()));
        assert_eq!(released.get(), 0);
        drop(evicted);
        assert_eq!(released.get(), 1);
        drop(queue);
        assert_eq!(released.get(), 2);
    }
}

pub mod bound_pair;
pub use bound_pair::{BoundPair, BoundPairError, BoundPairEvidence, BoundPairGuard, FrameEvidence};

pub use pair::{DepthUnitPolicy, PairAcceptance};
