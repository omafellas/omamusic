use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};
use std::sync::Arc;

/// Samples per analysis window. At 48 kHz this is 85 ms of audio, which
/// resolves the lowest band and still follows beats.
pub const SIZE: usize = 4096;

const FLOOR_DB: f32 = -72.0;
const CEILING_DB: f32 = -12.0;
const LOW_HZ: f64 = 40.0;
const HIGH_HZ: f64 = 16_000.0;
const RISE_SECONDS: f64 = 0.03;
const FALL_SECONDS: f64 = 0.22;
const MAX_STEP_SECONDS: f64 = 0.1;

struct Band {
    low: f64,
    high: f64,
    tilt: f32,
}

/// Turns the most recent audio into per-bar levels in 0.0..=1.0.
///
/// Bars are spaced logarithmically from 40 Hz to 16 kHz, read in decibels
/// with a gentle treble tilt so a flat mix lights every bar about evenly,
/// and smoothed with a fast attack and a slow release so beats land at once
/// and sink back gently.
pub struct Analyzer {
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    input: Vec<f32>,
    output: Vec<Complex<f32>>,
    magnitudes: Vec<f32>,
    bands: Vec<Band>,
    shape: (usize, u32),
    levels: Vec<f32>,
    last: Option<f64>,
}

impl Default for Analyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer {
    pub fn new() -> Self {
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(SIZE);
        let window = (0..SIZE)
            .map(|n| 0.5 - 0.5 * (std::f32::consts::TAU * n as f32 / SIZE as f32).cos())
            .collect();
        Self {
            input: fft.make_input_vec(),
            output: fft.make_output_vec(),
            fft,
            window,
            magnitudes: vec![0.0; SIZE / 2],
            bands: Vec::new(),
            shape: (0, 0),
            levels: Vec::new(),
            last: None,
        }
    }

    pub fn levels(&self) -> &[f32] {
        &self.levels
    }

    /// True once every bar has sunk back to the baseline.
    pub fn idle(&self) -> bool {
        self.levels.iter().all(|level| *level < 0.001)
    }

    /// Advance to `time` (seconds, any monotonic origin) and return the bar
    /// levels. `samples` is the latest `SIZE` mono samples, oldest first.
    /// While not `active` every bar falls towards zero.
    pub fn update(
        &mut self,
        samples: &[f32],
        rate: u32,
        count: usize,
        active: bool,
        time: f64,
    ) -> &[f32] {
        if self.shape != (count, rate) {
            self.layout(count, rate);
        }
        if active {
            self.measure(samples);
        }
        let elapsed = self
            .last
            .map(|last| (time - last).clamp(0.0, MAX_STEP_SECONDS));
        self.last = Some(time);
        let (rise, fall) = match elapsed {
            Some(elapsed) => (
                (1.0 - (-elapsed / RISE_SECONDS).exp()) as f32,
                (1.0 - (-elapsed / FALL_SECONDS).exp()) as f32,
            ),
            None => (1.0, 1.0),
        };
        for index in 0..self.levels.len() {
            let target = if active {
                band_level(&self.magnitudes, &self.bands[index])
            } else {
                0.0
            };
            let current = self.levels[index];
            let factor = if target > current { rise } else { fall };
            self.levels[index] = current + (target - current) * factor;
        }
        &self.levels
    }

    fn measure(&mut self, samples: &[f32]) {
        let n = samples.len().min(SIZE);
        self.input.fill(0.0);
        for (out, (sample, window)) in self.input[SIZE - n..].iter_mut().zip(
            samples[samples.len() - n..]
                .iter()
                .zip(&self.window[SIZE - n..]),
        ) {
            *out = sample * window;
        }
        if self.fft.process(&mut self.input, &mut self.output).is_err() {
            return;
        }
        let scale = 2.0 / SIZE as f32;
        for (magnitude, bin) in self.magnitudes.iter_mut().zip(&self.output) {
            *magnitude = bin.norm() * scale;
        }
    }

    fn layout(&mut self, count: usize, rate: u32) {
        self.shape = (count, rate);
        self.levels = vec![0.0; count];
        self.bands.clear();
        if count == 0 || rate == 0 {
            return;
        }
        let resolution = rate as f64 / SIZE as f64;
        let high = HIGH_HZ.min(rate as f64 / 2.0);
        let ratio = high / LOW_HZ;
        self.bands = (0..count)
            .map(|index| {
                let low = LOW_HZ * ratio.powf(index as f64 / count as f64);
                let high = LOW_HZ * ratio.powf((index + 1) as f64 / count as f64);
                Band {
                    low: low / resolution,
                    high: high / resolution,
                    tilt: (3.0 * ((low * high).sqrt() / 1000.0).log2()) as f32,
                }
            })
            .collect();
    }
}

fn band_level(magnitudes: &[f32], band: &Band) -> f32 {
    let last = magnitudes.len() - 1;
    let magnitude = if band.high - band.low < 2.0 {
        let center = ((band.low + band.high) / 2.0).min(last as f64);
        let index = center as usize;
        let next = (index + 1).min(last);
        let fraction = (center - index as f64) as f32;
        magnitudes[index] * (1.0 - fraction) + magnitudes[next] * fraction
    } else {
        let from = band.low.ceil() as usize;
        let to = (band.high as usize).min(last);
        magnitudes[from..=to.max(from)]
            .iter()
            .copied()
            .fold(0.0, f32::max)
    };
    let decibels = 20.0 * magnitude.max(1e-9).log10() + band.tilt;
    ((decibels - FLOOR_DB) / (CEILING_DB - FLOOR_DB)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    const BARS: usize = 24;

    fn tone(hz: f32, amplitude: f32) -> Vec<f32> {
        (0..SIZE)
            .map(|n| (std::f32::consts::TAU * hz * n as f32 / RATE as f32).sin() * amplitude)
            .collect()
    }

    fn band_of(hz: f64) -> usize {
        let ratio = HIGH_HZ / LOW_HZ;
        ((hz / LOW_HZ).ln() / ratio.ln() * BARS as f64).floor() as usize
    }

    #[test]
    fn a_tone_lights_its_own_bar_the_most() {
        let mut analyzer = Analyzer::new();
        let levels = analyzer.update(&tone(1000.0, 0.5), RATE, BARS, true, 0.0);
        assert_eq!(levels.len(), BARS);
        let (loudest, peak) = levels
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap();
        assert_eq!(loudest, band_of(1000.0), "levels: {levels:?}");
        assert!(*peak > 0.8, "a half-scale tone reads near the top: {peak}");
        assert!(
            levels[band_of(100.0)] < 0.3,
            "bars two octaves away stay low: {}",
            levels[band_of(100.0)]
        );
        assert!(levels.iter().all(|l| (0.0..=1.0).contains(l)));
    }

    #[test]
    fn the_first_frame_lands_without_easing() {
        let mut analyzer = Analyzer::new();
        let levels = analyzer.update(&tone(1000.0, 0.5), RATE, BARS, true, 5.0);
        assert!(levels[band_of(1000.0)] > 0.8, "{levels:?}");
    }

    #[test]
    fn bars_rise_fast_and_fall_slowly() {
        let mut analyzer = Analyzer::new();
        analyzer.update(&vec![0.0; SIZE], RATE, BARS, true, 0.0);
        let risen = analyzer.update(&tone(1000.0, 0.5), RATE, BARS, true, 0.016)[band_of(1000.0)];
        assert!(risen > 0.35, "one frame of attack: {risen}");
        let before = analyzer.update(&tone(1000.0, 0.5), RATE, BARS, true, 0.5)[band_of(1000.0)];
        let after = analyzer.update(&vec![0.0; SIZE], RATE, BARS, true, 0.516)[band_of(1000.0)];
        let fallen = before - after;
        assert!(
            fallen > 0.0 && fallen < risen / 2.0,
            "release is slower than attack: fell {fallen}, rose {risen}"
        );
    }

    #[test]
    fn inactive_bars_sink_to_idle() {
        let mut analyzer = Analyzer::new();
        analyzer.update(&tone(1000.0, 0.5), RATE, BARS, true, 0.0);
        assert!(!analyzer.idle());
        let mut time = 0.0;
        for _ in 0..40 {
            time += 0.05;
            analyzer.update(&[], RATE, BARS, false, time);
        }
        assert!(analyzer.idle(), "{:?}", analyzer.levels());
    }

    #[test]
    fn silence_reads_zero() {
        let mut analyzer = Analyzer::new();
        let levels = analyzer.update(&vec![0.0; SIZE], RATE, BARS, true, 0.0);
        assert!(levels.iter().all(|l| *l == 0.0), "{levels:?}");
    }

    #[test]
    fn a_new_bar_count_or_rate_relays_out_the_bands() {
        let mut analyzer = Analyzer::new();
        assert_eq!(
            analyzer
                .update(&tone(440.0, 0.5), RATE, 10, true, 0.0)
                .len(),
            10
        );
        assert_eq!(
            analyzer
                .update(&tone(440.0, 0.5), RATE, 33, true, 0.1)
                .len(),
            33
        );
        let levels = analyzer.update(&tone(440.0, 0.5), 44_100, 33, true, 0.2);
        assert_eq!(levels.len(), 33);
        assert!(levels.iter().any(|l| *l > 0.5));
    }
}
