#![forbid(unsafe_code)]

// SDL timestamps are nanoseconds. A single click is deliberately not a mode
// switch: a side button may be mapped to other actions by a tablet driver.
const DOUBLE_CLICK_NS: u64 = 350_000_000;

#[derive(Default)]
pub(super) struct PenDoubleTap {
    last: Option<(u32, u8, u64)>,
}

impl PenDoubleTap {
    pub(super) fn pressed(&mut self, pen: u32, button: u8, timestamp: u64) -> bool {
        if pen == 0 || button == 0 {
            self.last = None;
            return false;
        }
        let previous = self.last.take();
        if previous.is_some_and(|(old_pen, old_button, old_timestamp)| {
            pen == old_pen
                && button == old_button
                && timestamp >= old_timestamp
                && timestamp - old_timestamp <= DOUBLE_CLICK_NS
        }) {
            return true;
        }
        self.last = Some((pen, button, timestamp));
        false
    }

    pub(super) fn clear(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_only_after_two_matching_presses() {
        let mut detector = PenDoubleTap::default();
        assert!(!detector.pressed(1, 1, 1_000_000_000));
        assert!(detector.pressed(1, 1, 1_300_000_000));
        assert!(!detector.pressed(1, 1, 1_320_000_000));
        assert!(detector.pressed(1, 1, 1_500_000_000));
    }

    #[test]
    fn ignores_different_pens_buttons_and_old_timestamps() {
        let mut detector = PenDoubleTap::default();
        assert!(!detector.pressed(1, 1, 1_000_000_000));
        assert!(!detector.pressed(2, 1, 1_100_000_000));
        assert!(!detector.pressed(2, 2, 1_200_000_000));
        assert!(!detector.pressed(2, 2, 900_000_000));
        assert!(!detector.pressed(2, 2, 1_500_000_000));
        detector.clear();
        assert!(!detector.pressed(2, 2, 1_600_000_000));
    }
}
