use capture_contract::{MonoTime, SessionId};
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WindowEvictions {
    pub expired: u64,
    pub capacity: u64,
}

/// A single explicitly declared monotonic domain. Capacity and age both bound
/// retained data. Source/Unix clocks never participate in expiry.
#[derive(Debug)]
pub struct RollingWindow<T> {
    session: SessionId,
    capacity: usize,
    horizon_ns: u64,
    last_ns: u64,
    entries: VecDeque<(u64, T)>,
}
impl<T> RollingWindow<T> {
    pub fn new(session: SessionId, capacity: usize, horizon_ns: u64) -> Result<Self, &'static str> {
        if !(1..=100_000).contains(&capacity) || horizon_ns == 0 {
            return Err("WINDOW_POLICY_INVALID");
        }
        Ok(Self {
            session,
            capacity,
            horizon_ns,
            last_ns: 0,
            entries: VecDeque::new(),
        })
    }
    fn validate_time(&self, at: &MonoTime) -> Result<(), &'static str> {
        if at.session_id != self.session {
            return Err("MONOTONIC_DOMAIN_MISMATCH");
        }
        if at.offset_ns < self.last_ns {
            return Err("MONOTONIC_REGRESSION");
        }
        Ok(())
    }
    pub fn tick(&mut self, at: &MonoTime) -> Result<WindowEvictions, &'static str> {
        self.validate_time(at)?;
        self.last_ns = at.offset_ns;
        let mut result = WindowEvictions::default();
        while self
            .entries
            .front()
            .is_some_and(|(t, _)| at.offset_ns - t > self.horizon_ns)
        {
            self.entries.pop_front();
            result.expired += 1;
        }
        Ok(result)
    }
    pub fn push(&mut self, at: &MonoTime, value: T) -> Result<WindowEvictions, &'static str> {
        let mut result = self.tick(at)?;
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
            result.capacity += 1;
        }
        self.entries.push_back((at.offset_ns, value));
        Ok(result)
    }
    /// Updates the latest retained receipt without adding a frame or moving the
    /// window clock. Used for a later pixel check of that exact receipt.
    pub(crate) fn latest_mut(&mut self, receipt_ns: u64) -> Result<&mut T, &'static str> {
        match self.entries.back_mut() {
            Some((ns, value)) if *ns == receipt_ns => Ok(value),
            _ => Err("RECEIPT_NOT_RETAINED"),
        }
    }
    pub fn iter(&self) -> impl Iterator<Item = &(u64, T)> {
        self.entries.iter()
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    #[test]
    fn invalid_bounds_and_expired_receipts_cannot_be_updated() {
        let session = SessionId::parse("a".repeat(32)).unwrap();
        for (capacity, horizon) in [(0, 1), (100_001, 1), (1, 0)] {
            assert_eq!(
                RollingWindow::<u8>::new(session.clone(), capacity, horizon).unwrap_err(),
                "WINDOW_POLICY_INVALID"
            );
        }
        let mut window = RollingWindow::new(session.clone(), 1, 10).unwrap();
        assert_eq!(window.latest_mut(0).unwrap_err(), "RECEIPT_NOT_RETAINED");
        let at = |offset_ns| MonoTime {
            session_id: session.clone(),
            offset_ns,
        };
        window.push(&at(1), 7).unwrap();
        window.push(&at(2), 8).unwrap();
        assert_eq!(window.latest_mut(1).unwrap_err(), "RECEIPT_NOT_RETAINED");
        window.tick(&at(13)).unwrap();
        assert_eq!(window.latest_mut(2).unwrap_err(), "RECEIPT_NOT_RETAINED");
        assert!(window.is_empty());
    }
}
