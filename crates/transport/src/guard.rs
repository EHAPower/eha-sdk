// Copyright The eha_controller Contributors

// Shared solely because both bindings use the same bounded transfer-number guard.
#[derive(Debug)]
pub(crate) struct Guard {
    expires: [u32; 128],
    epoch_ms: u64,
}

impl Guard {
    pub(crate) const fn new() -> Self {
        Self {
            expires: [0; 128],
            epoch_ms: 0,
        }
    }

    // Callers supply nondecreasing time. Rebase at most once per 60 seconds;
    // an expired deadline cannot become live again after a low-32-bit wrap.
    fn elapsed(&mut self, now_ms: u64) -> u32 {
        let elapsed = now_ms.saturating_sub(self.epoch_ms);
        if elapsed > 60_000 {
            for expires in &mut self.expires {
                *expires = u64::from(*expires).saturating_sub(elapsed) as u32;
            }
            self.epoch_ms = now_ms;
            0
        } else {
            elapsed as u32
        }
    }

    pub(crate) fn is_protected(&mut self, id: u8, now_ms: u64) -> bool {
        let elapsed = self.elapsed(now_ms);
        self.expires
            .get(usize::from(id))
            .is_some_and(|end| *end > elapsed)
    }

    // Both bindings use only 40 ms and 60,000 ms durations, so the stored
    // deadline is at most 120,000 even when the supplied u64 time is near MAX.
    pub(crate) fn protect(&mut self, id: u8, now_ms: u64, duration_ms: u32) {
        let elapsed = self.elapsed(now_ms);
        if let Some(expires) = self.expires.get_mut(usize::from(id)) {
            *expires = elapsed.saturating_add(duration_ms);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Guard;

    #[test]
    fn expired_numbers_can_be_reused_at_the_exact_boundary() {
        let mut guard = Guard::new();
        guard.protect(0, 0, 40);
        guard.protect(127, 1, 60_000);
        assert!(guard.is_protected(0, 39));
        assert!(!guard.is_protected(0, 40));
        assert!(guard.is_protected(127, 60_000));
        assert!(!guard.is_protected(127, 60_001));
    }

    #[test]
    fn rebasing_preserves_active_guards_without_reviving_old_numbers() {
        let mut guard = Guard::new();
        guard.protect(1, 1, 40);
        guard.protect(2, 59_999, 60_000);
        assert!(guard.is_protected(2, 60_001));
        assert!(!guard.is_protected(1, 60_001));
        assert!(guard.is_protected(2, 119_998));
        assert!(!guard.is_protected(2, 119_999));
        assert!(!guard.is_protected(1, (1_u64 << 32) + 2));
        guard.protect(1, (1_u64 << 32) + 2, 40);
        assert!(guard.is_protected(1, (1_u64 << 32) + 41));
        assert!(!guard.is_protected(1, (1_u64 << 32) + 42));
    }

    #[test]
    fn large_monotonic_timestamps_do_not_overflow_deadlines() {
        let mut guard = Guard::new();
        guard.protect(7, u64::MAX - 10, 40);
        assert!(guard.is_protected(7, u64::MAX));
    }
}
