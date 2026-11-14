use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameRate {
    #[default]
    Auto,
    Fps120,
    Fps90,
    Fps60,
    Fps30,
}
impl FrameRate {
    pub const ALL: [Self; 5] = [
        Self::Auto,
        Self::Fps120,
        Self::Fps90,
        Self::Fps60,
        Self::Fps30,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Fps120 => "120 FPS",
            Self::Fps90 => "90 FPS",
            Self::Fps60 => "60 FPS",
            Self::Fps30 => "30 FPS",
        }
    }
    pub fn hz(self, refresh: f64) -> f64 {
        match self {
            Self::Auto if refresh.is_finite() && (10.0..=1000.0).contains(&refresh) => refresh,
            Self::Auto | Self::Fps60 => 60.0,
            Self::Fps120 => 120.0,
            Self::Fps90 => 90.0,
            Self::Fps30 => 30.0,
        }
    }
    pub fn load(path: &Path) -> Self {
        if std::fs::metadata(path).is_ok_and(|m| m.len() <= 4096) {
            std::fs::read(path)
                .ok()
                .and_then(|v| serde_json::from_slice(&v).ok())
                .unwrap_or_default()
        } else {
            Self::default()
        }
    }
    pub fn save(self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        let data = serde_json::to_vec(&self).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, data)
            .and_then(|()| std::fs::rename(&tmp, path))
            .inspect_err(|_| {
                let _ = std::fs::remove_file(&tmp);
            })
            .map_err(|e| e.to_string())
    }
}

pub struct FramePacer {
    pub period: Duration,
    next: Instant,
}
impl FramePacer {
    pub fn new(rate: FrameRate, refresh: f64, now: Instant) -> Self {
        let period = Duration::from_secs_f64(1.0 / rate.hz(refresh));
        Self {
            period,
            next: now + period,
        }
    }
    pub fn configure(&mut self, rate: FrameRate, refresh: f64, now: Instant) {
        let period = Duration::from_secs_f64(1.0 / rate.hz(refresh));
        if period != self.period {
            self.period = period;
            self.next = now + period;
        }
    }
    pub fn delay(&mut self, now: Instant) -> Duration {
        let delay = self.next.saturating_duration_since(now);
        self.next = if now > self.next + self.period {
            now + self.period
        } else {
            self.next + self.period
        };
        delay
    }
    pub fn wait(&mut self) {
        let delay = self.delay(Instant::now());
        if !delay.is_zero() {
            unsafe {
                crate::sdl::SDL_DelayPrecise(delay.as_nanos() as u64);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn auto_tracks_fractional_refresh_and_fixed_rates_stay_fixed() {
        assert_eq!(FrameRate::Auto.hz(60000.0 / 1001.0), 60000.0 / 1001.0);
        for rate in FrameRate::ALL {
            for bad in [0.0, f64::NAN, f64::INFINITY, -60.0] {
                assert!(rate.hz(bad).is_finite());
            }
            assert_eq!(
                serde_json::from_str::<FrameRate>(&serde_json::to_string(&rate).unwrap()).unwrap(),
                rate
            );
        }
        assert_eq!(FrameRate::Fps120.hz(60.0), 120.0);
        assert_eq!(FrameRate::Fps30.hz(144.0), 30.0);
        assert!(serde_json::from_str::<FrameRate>("\"Fps999\"").is_err());
    }
    #[test]
    fn deadlines_do_not_add_render_time_or_catch_up_after_a_long_stall() {
        let now = Instant::now();
        let mut pacer = FramePacer::new(FrameRate::Fps60, 144.0, now);
        let render = Duration::from_millis(4);
        assert_eq!(pacer.delay(now + render), pacer.period - render);
        assert_eq!(
            pacer.delay(now + pacer.period + render),
            pacer.period - render
        );
        assert_eq!(pacer.delay(now + Duration::from_secs(3)), Duration::ZERO);
        assert_eq!(
            pacer.delay(now + Duration::from_secs(3) + render),
            pacer.period - render
        );
        pacer.configure(FrameRate::Auto, 90.0, now);
        assert!((pacer.period.as_secs_f64() - 1.0 / 90.0).abs() < 1e-9);
    }
}
