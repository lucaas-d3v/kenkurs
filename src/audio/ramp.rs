//! Sample-accurate linear gain ramps.
//!
//! Linear ramps finish in an exact number of frames, so fades end precisely where they are
//! scheduled, and they settle exactly on their target: gains never decay into denormals.

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Ramp {
    value: f32,
    target: f32,
    step: f32,
    remaining: u32,
}

impl Ramp {
    pub(crate) const fn new(value: f32) -> Self {
        Self { value, target: value, step: 0.0, remaining: 0 }
    }

    /// Moves from the current value to `target` over `frames` frames (`0` jumps).
    pub(crate) fn set_target(&mut self, target: f32, frames: u32) {
        self.target = target;
        if frames == 0 || self.value == target {
            self.value = target;
            self.step = 0.0;
            self.remaining = 0;
        } else {
            self.step = (target - self.value) / frames as f32;
            self.remaining = frames;
        }
    }

    /// Jumps to `value`, cancelling any ramp in progress.
    pub(crate) fn jump(&mut self, value: f32) {
        self.set_target(value, 0);
    }

    /// Advances one frame and returns the gain for that frame.
    #[inline]
    pub(crate) fn tick(&mut self) -> f32 {
        if self.remaining > 0 {
            self.remaining -= 1;
            self.value = if self.remaining == 0 { self.target } else { self.value + self.step };
        }
        self.value
    }

    /// Advances `frames` frames at once.
    pub(crate) fn skip(&mut self, frames: u32) {
        if frames >= self.remaining {
            self.value = self.target;
            self.remaining = 0;
        } else {
            self.remaining -= frames;
            self.value += self.step * frames as f32;
        }
    }

    #[cfg(test)]
    pub(crate) fn value(&self) -> f32 {
        self.value
    }

    pub(crate) fn is_ramping(&self) -> bool {
        self.remaining > 0
    }

    pub(crate) fn remaining(&self) -> u32 {
        self.remaining
    }

    /// True once the ramp has settled at zero.
    pub(crate) fn is_silent(&self) -> bool {
        self.remaining == 0 && self.value == 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramps_linearly_and_lands_exactly() {
        let mut ramp = Ramp::new(0.0);
        ramp.set_target(1.0, 4);
        let values: Vec<f32> = (0..6).map(|_| ramp.tick()).collect();
        assert_eq!(values, [0.25, 0.5, 0.75, 1.0, 1.0, 1.0]);
        assert!(!ramp.is_ramping());
    }

    #[test]
    fn fade_out_reaches_silence() {
        let mut ramp = Ramp::new(1.0);
        ramp.set_target(0.0, 3);
        assert!(!ramp.is_silent());
        for _ in 0..3 {
            ramp.tick();
        }
        assert!(ramp.is_silent());
    }

    #[test]
    fn retargeting_starts_from_the_current_value() {
        let mut ramp = Ramp::new(0.0);
        ramp.set_target(1.0, 10);
        ramp.skip(5);
        assert!((ramp.value() - 0.5).abs() < 1e-6);
        ramp.set_target(0.0, 5);
        assert!((ramp.tick() - 0.4).abs() < 1e-6);
        assert_eq!(ramp.remaining(), 4);
        ramp.skip(100);
        assert!(ramp.is_silent());
        ramp.jump(0.3);
        assert_eq!(ramp.tick(), 0.3);
    }
}
