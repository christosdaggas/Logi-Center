#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Up = 0,
    Down = 1,
    Left = 2,
    Right = 3,
}

#[derive(Default)]
pub struct Stroke {
    config: GestureConfig,
    x: i32,
    y: i32,
    samples: u32,
    moved: bool,
    fired: bool,
}

impl Stroke {
    pub fn new(config: GestureConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    pub fn sample(&mut self, dx: i16, dy: i16, elapsed_ms: u128) -> Option<Direction> {
        self.x = self.x.saturating_add(i32::from(dx));
        self.y = self.y.saturating_add(i32::from(dy));
        self.samples = self.samples.saturating_add(1);
        let x = self.x.unsigned_abs();
        let y = self.y.unsigned_abs();
        self.moved |= x > u32::from(self.config.jitter) || y > u32::from(self.config.jitter);
        if self.fired
            || self.samples < u32::from(self.config.min_samples)
            || elapsed_ms > u128::from(self.config.timeout_ms)
            || x.max(y) < u32::from(self.config.threshold)
        {
            return None;
        }
        let ratio = u64::from(self.config.axis_ratio_percent);
        let direction = if u64::from(x) * 100 > u64::from(y) * ratio {
            if self.x < 0 {
                Direction::Left
            } else {
                Direction::Right
            }
        } else if u64::from(y) * 100 > u64::from(x) * ratio {
            if self.y < 0 {
                Direction::Up
            } else {
                Direction::Down
            }
        } else {
            return None;
        };
        self.fired = true;
        Some(direction)
    }
    pub fn tap(&self) -> bool {
        !self.fired && !self.moved
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Report {
    Buttons(Vec<u16>),
    Motion(i16, i16),
}

pub fn decode(raw: &[u8], number: u8, index: u8) -> Option<Report> {
    if raw.len() < 7
        || !matches!(raw[0], 0x10 | 0x11)
        || (raw[0] == 0x11 && raw.len() < 20)
        || raw[1] != number
        || raw[2] != index
        || raw[3] & 0x0F != 0
    {
        return None;
    }
    let data = &raw[4..];
    match raw[3] >> 4 {
        0 => Some(Report::Buttons(
            data.chunks_exact(2)
                .map(|b| u16::from_be_bytes([b[0], b[1]]))
                .take_while(|cid| *cid != 0)
                .take(8)
                .collect(),
        )),
        1 if data.len() >= 4 => Some(Report::Motion(
            i16::from_be_bytes([data[0], data[1]]),
            i16::from_be_bytes([data[2], data[3]]),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configurable_thresholds_timeout_and_validation() {
        let config = GestureConfig {
            threshold: 100,
            jitter: 8,
            min_samples: 2,
            timeout_ms: 200,
            axis_ratio_percent: 200,
        };
        config.validate().unwrap();
        let mut stroke = Stroke::new(config.clone());
        assert_eq!(stroke.sample(60, 20, 10), None);
        assert_eq!(stroke.sample(60, 20, 20), Some(Direction::Right));
        let mut stroke = Stroke::new(config.clone());
        assert_eq!(stroke.sample(60, 0, 201), None);
        assert_eq!(stroke.sample(60, 0, 202), None);
        assert!(!stroke.tap());
        for bad in [
            GestureConfig {
                threshold: 0,
                ..config.clone()
            },
            GestureConfig {
                jitter: 100,
                ..config.clone()
            },
            GestureConfig {
                min_samples: 0,
                ..config.clone()
            },
            GestureConfig {
                timeout_ms: 0,
                ..config.clone()
            },
            GestureConfig {
                axis_ratio_percent: 100,
                ..config.clone()
            },
        ] {
            assert!(bad.validate().is_err());
        }
        assert_eq!(
            serde_json::from_str::<GestureConfig>("{}").unwrap(),
            GestureConfig::default()
        );
    }
    #[test]
    fn strokes_ignore_jitter_diagonals_slow_drift_and_returns() {
        for (dx, dy, want) in [
            (20, 0, Direction::Right),
            (-20, 0, Direction::Left),
            (0, 20, Direction::Down),
            (0, -20, Direction::Up),
        ] {
            let mut s = Stroke::default();
            assert_eq!(s.sample(dx, dy, 10), None);
            assert_eq!(s.sample(dx, dy, 20), None);
            assert_eq!(s.sample(dx, dy, 30), Some(want));
            assert!(!s.tap());
            assert_eq!(s.sample(-dx * 3, -dy * 3, 40), None);
        }
        let mut s = Stroke::default();
        for _ in 0..5 {
            assert_eq!(s.sample(1, -1, 10), None);
        }
        assert!(s.tap());
        let mut s = Stroke::default();
        for _ in 0..4 {
            assert_eq!(s.sample(20, 20, 10), None);
        }
        assert!(!s.tap());
        let mut s = Stroke::default();
        for _ in 0..4 {
            assert_eq!(s.sample(20, 0, 1500), None);
        }
        assert!(!s.tap());
    }
    #[test]
    fn packets_are_bounded_and_ignore_other_devices_and_replies() {
        let packet = |data: &[u8]| {
            let mut raw = data.to_vec();
            raw.resize(20, 0);
            raw
        };
        assert_eq!(
            decode(&packet(&[0x11, 1, 5, 0, 0, 195, 0, 0]), 1, 5),
            Some(Report::Buttons(vec![195]))
        );
        assert_eq!(
            decode(&packet(&[0x11, 1, 5, 0x10, 0xFF, 0xFE, 0, 20]), 1, 5),
            Some(Report::Motion(-2, 20))
        );
        assert_eq!(decode(&packet(&[0x11, 2, 5, 0, 0, 195]), 1, 5), None);
        assert_eq!(decode(&packet(&[0x11, 1, 5, 0x0B, 0, 195]), 1, 5), None);
        assert_eq!(decode(&[0x11, 1, 5, 0x10, 1], 1, 5), None);
        assert_eq!(
            decode(&[0x10, 1, 5, 0, 0, 195, 0], 1, 5),
            Some(Report::Buttons(vec![195]))
        );
    }
}
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// RawXY units are device reports, not screen pixels. Defaults retain the old feel.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct GestureConfig {
    pub threshold: u16,
    pub jitter: u16,
    pub min_samples: u8,
    pub timeout_ms: u16,
    pub axis_ratio_percent: u16,
}

impl Default for GestureConfig {
    fn default() -> Self {
        Self {
            threshold: 40,
            jitter: 12,
            min_samples: 3,
            timeout_ms: 900,
            axis_ratio_percent: 150,
        }
    }
}

impl GestureConfig {
    pub fn validate(&self) -> Result<()> {
        if !(2..=4096).contains(&self.threshold)
            || self.jitter >= self.threshold
            || !(1..=32).contains(&self.min_samples)
            || !(50..=5000).contains(&self.timeout_ms)
            || !(110..=400).contains(&self.axis_ratio_percent)
        {
            bail!(
                "Gesture threshold must be 2–4096, jitter below threshold, samples 1–32, timeout 50–5000 ms and axis ratio 110–400%"
            );
        }
        Ok(())
    }
}
