//! Stereo-linked sample-peak protection after device-rate resampling.
//!
//! Input history provides 5 ms of lookahead. A monotonic peak queue bounds work
//! amortized O(1) per frame. Gain reaches any lower target within the lookahead,
//! rather than truncating individual waveform peaks or shifting stereo balance.
use std::collections::VecDeque;

pub(crate) const LOOKAHEAD_MS: usize = 5;
const CEILING: f32 = 0.98;
const RELEASE_SECONDS: f32 = 0.080;

pub(crate) struct OutputLimiter {
    delay: Vec<[f32; 2]>,
    peaks: VecDeque<(u64, f32)>,
    cursor: usize,
    frame: u64,
    gain: f32,
    attack_step: f32,
    release: f32,
}

impl OutputLimiter {
    pub(crate) fn new(sample_rate: u32) -> Self {
        let lookahead = (sample_rate as usize * LOOKAHEAD_MS / 1000).max(1);
        Self {
            delay: vec![[0.0; 2]; lookahead],
            peaks: VecDeque::with_capacity(lookahead + 1),
            cursor: 0,
            frame: 0,
            gain: 1.0,
            attack_step: 1.0 / lookahead as f32,
            release: 1.0 - (-1.0 / (sample_rate.max(1) as f32 * RELEASE_SECONDS)).exp(),
        }
    }

    pub(crate) fn process(&mut self, buffer: &mut [f32], frames: usize, master_gain: f32) {
        for stereo in buffer.chunks_exact_mut(2).take(frames) {
            let input = [stereo[0], stereo[1]].map(|sample| {
                let value = sample * master_gain;
                if value.is_finite() { value } else { 0.0 }
            });
            let peak = input[0].abs().max(input[1].abs());
            let lookahead = self.delay.len() as u64;
            while self
                .peaks
                .front()
                .is_some_and(|(index, _)| *index + lookahead < self.frame)
            {
                self.peaks.pop_front();
            }
            while self.peaks.back().is_some_and(|(_, value)| *value <= peak) {
                self.peaks.pop_back();
            }
            self.peaks.push_back((self.frame, peak));
            let window_peak = self.peaks.front().map_or(0.0, |(_, value)| *value);
            let target = if window_peak > CEILING {
                CEILING / window_peak
            } else {
                1.0
            };
            self.gain = if target < self.gain {
                (self.gain - self.attack_step).max(target)
            } else {
                self.gain + (target - self.gain) * self.release
            };
            let delayed = self.delay[self.cursor];
            self.delay[self.cursor] = input;
            self.cursor = (self.cursor + 1) % self.delay.len();
            self.frame += 1;
            // This bound only corrects accumulated floating-point error in the
            // attack ramp. Both channels always receive the same gain.
            let delayed_peak = delayed[0].abs().max(delayed[1].abs());
            let safe_gain = if delayed_peak > CEILING {
                self.gain.min(CEILING / delayed_peak)
            } else {
                self.gain
            };
            stereo[0] = delayed[0] * safe_gain;
            stereo[1] = delayed[1] * safe_gain;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn below_ceiling_is_unchanged_except_fixed_delay() {
        let mut limiter = OutputLimiter::new(48_000);
        let delay = limiter.delay.len();
        let original: Vec<f32> = (0..2000).map(|i| (i as f32 * 0.13).sin() * 0.2).collect();
        let mut output = original.clone();
        limiter.process(&mut output, 1000, 1.0);
        assert!(output[..delay * 2].iter().all(|s| *s == 0.0));
        assert_eq!(
            &output[delay * 2..],
            &original[..original.len() - delay * 2]
        );
    }

    #[test]
    fn overload_preserves_sine_shape_and_stereo_balance() {
        let mut limiter = OutputLimiter::new(48_000);
        let mut output: Vec<f32> = (0..4800)
            .flat_map(|i| {
                let signal = (i as f32 * std::f32::consts::TAU / 48.0).sin();
                [signal * 4.0, signal]
            })
            .collect();
        limiter.process(&mut output, 4800, 1.0);
        for (i, frame) in output.chunks_exact(2).enumerate() {
            assert!(frame[0].abs() <= CEILING + 0.000001);
            assert!((frame[1] * 4.0 - frame[0]).abs() < 0.000001);
            if i > 1000 {
                let expected = ((i - 240) as f32 * std::f32::consts::TAU / 48.0).sin() * CEILING;
                assert!((frame[0] - expected).abs() < 0.0001);
            }
        }
    }

    #[test]
    fn state_is_chunk_independent_and_peak_storage_stays_bounded() {
        for rate in [44_100, 48_000, 96_000] {
            let source: Vec<f32> = (0..8000)
                .flat_map(|i| {
                    let sample = if i % 997 == 0 {
                        100.0
                    } else {
                        (i as f32 * 0.21).sin() * 3.0
                    };
                    [sample, -sample * 0.17]
                })
                .collect();
            let mut reference = source.clone();
            OutputLimiter::new(rate).process(&mut reference, 8000, 1.0);
            for chunk_frames in [1, 63, 512] {
                let mut limiter = OutputLimiter::new(rate);
                let capacity = limiter.peaks.capacity();
                let mut output = source.clone();
                for chunk in output.chunks_mut(chunk_frames * 2) {
                    limiter.process(chunk, chunk.len() / 2, 1.0);
                }
                assert_eq!(output, reference);
                assert_eq!(limiter.peaks.capacity(), capacity);
                assert!(output.iter().all(|s| s.abs() <= CEILING + 0.000001));
            }
        }
    }

    #[test]
    fn malformed_samples_do_not_poison_future_output() {
        let mut limiter = OutputLimiter::new(48_000);
        let mut samples = vec![0.25; 2048];
        samples[0] = f32::NAN;
        samples[1] = f32::INFINITY;
        limiter.process(&mut samples, 1024, 1.0);
        assert!(samples.iter().all(|s| s.is_finite()));
        assert_eq!(samples[2047], 0.25);
    }
}
