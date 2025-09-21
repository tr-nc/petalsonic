//! Audio data and loading functionality

mod symphonia_loader;

pub use symphonia_loader::{load_audio_file, load_audio_file_simple};

use crate::error::{PetalSonicError, Result};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct LoadOptions {
    /// Target sample rate for resampling (None = keep original)
    pub target_sample_rate: Option<u32>,
    /// Convert to mono after loading
    pub convert_to_mono: bool,
    /// Maximum duration to load (None = load entire file)
    pub max_duration: Option<Duration>,
    /// Which channel to use for mono conversion (None = mix all channels)
    pub mono_channel: Option<usize>,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            target_sample_rate: None,
            convert_to_mono: false,
            max_duration: None,
            mono_channel: None,
        }
    }
}

impl LoadOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn target_sample_rate(mut self, rate: u32) -> Self {
        self.target_sample_rate = Some(rate);
        self
    }

    pub fn convert_to_mono(mut self, convert: bool) -> Self {
        self.convert_to_mono = convert;
        self
    }

    pub fn max_duration(mut self, duration: Duration) -> Self {
        self.max_duration = Some(duration);
        self
    }

    pub fn mono_channel(mut self, channel: usize) -> Self {
        self.mono_channel = Some(channel);
        self
    }
}

#[derive(Debug, Clone)]
pub struct PetalSonicAudioData {
    inner: Arc<AudioDataInner>,
}

#[derive(Debug)]
pub(crate) struct AudioDataInner {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u16,
    pub duration: Duration,
    pub total_frames: usize,
}

impl PetalSonicAudioData {
    pub(crate) fn new(
        samples: Vec<f32>,
        sample_rate: u32,
        channels: u16,
        duration: Duration,
    ) -> Self {
        let total_frames = samples.len() / channels as usize;
        Self {
            inner: Arc::new(AudioDataInner {
                samples,
                sample_rate,
                channels,
                duration,
                total_frames,
            }),
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.inner.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.inner.channels
    }

    pub fn duration(&self) -> Duration {
        self.inner.duration
    }

    pub fn samples(&self) -> &[f32] {
        &self.inner.samples
    }

    pub fn total_frames(&self) -> usize {
        self.inner.total_frames
    }

    pub fn is_empty(&self) -> bool {
        self.inner.samples.is_empty()
    }

    pub fn len(&self) -> usize {
        self.inner.samples.len()
    }

    /// Get samples for a specific channel (0-indexed)
    pub fn channel_samples(&self, channel: usize) -> Result<Vec<f32>> {
        if channel >= self.inner.channels as usize {
            return Err(PetalSonicError::AudioFormat(format!(
                "Channel {} out of range (max: {})",
                channel,
                self.inner.channels - 1
            )));
        }

        let channel_samples: Vec<f32> = self
            .inner
            .samples
            .chunks(self.inner.channels as usize)
            .map(|frame| frame[channel])
            .collect();

        Ok(channel_samples)
    }

    /// Get interleaved samples for a specific frame range
    pub fn frame_range(&self, start_frame: usize, end_frame: usize) -> Result<Vec<f32>> {
        if start_frame >= self.inner.total_frames || end_frame > self.inner.total_frames {
            return Err(PetalSonicError::AudioFormat(format!(
                "Frame range {}-{} out of bounds (max: {})",
                start_frame, end_frame, self.inner.total_frames
            )));
        }

        let start_sample = start_frame * self.inner.channels as usize;
        let end_sample = end_frame * self.inner.channels as usize;

        Ok(self.inner.samples[start_sample..end_sample].to_vec())
    }

    /// Convert to mono by downmixing all channels
    pub fn to_mono(&self) -> Result<Self> {
        if self.inner.channels == 1 {
            return Ok(self.clone());
        }

        let mono_samples: Vec<f32> = self
            .inner
            .samples
            .chunks(self.inner.channels as usize)
            .map(|frame| {
                let sum: f32 = frame.iter().sum();
                sum / self.inner.channels as f32
            })
            .collect();

        let mono_duration =
            Duration::from_secs_f64(mono_samples.len() as f64 / self.inner.sample_rate as f64);

        Ok(Self::new(
            mono_samples,
            self.inner.sample_rate,
            1,
            mono_duration,
        ))
    }

    /// Resample to a different sample rate using rubato
    pub fn resample(&self, target_sample_rate: u32) -> Result<Self> {
        if target_sample_rate == self.inner.sample_rate {
            return Ok(self.clone());
        }

        use rubato::{
            Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType,
            WindowFunction,
        };

        let params = SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 256,
            window: WindowFunction::BlackmanHarris2,
        };

        let resample_ratio = target_sample_rate as f64 / self.inner.sample_rate as f64;

        let mut resampler = SincFixedIn::new(
            resample_ratio,
            2.0, // resample_ratio_relative_range
            params,
            1024, // chunk_size
            self.inner.channels as usize,
        )
        .map_err(|e| PetalSonicError::AudioLoading(format!("Failed to create resampler: {}", e)))?;

        let mut resampled_samples = Vec::new();
        let frames_per_channel = self.inner.total_frames;

        // Process each channel separately
        for ch in 0..self.inner.channels as usize {
            let channel_samples = self.channel_samples(ch)?;

            let mut waves_in = vec![channel_samples];
            let mut waves_out = vec![vec![0.0f32; 2048]; 1]; // Output buffer

            let mut output_buffer = Vec::new();
            let mut frame_idx = 0;

            while frame_idx < frames_per_channel {
                let frames_to_process = (frames_per_channel - frame_idx).min(1024);

                // Prepare input for this chunk
                let chunk: Vec<f32> =
                    waves_in[0][frame_idx..frame_idx + frames_to_process].to_vec();
                waves_in[0] = chunk;

                let (_, n_out) = resampler
                    .process_into_buffer(&waves_in, &mut waves_out, None)
                    .map_err(|e| {
                        PetalSonicError::AudioLoading(format!("Resampling error: {}", e))
                    })?;

                if n_out > 0 {
                    output_buffer.extend_from_slice(&waves_out[0][..n_out]);
                }

                frame_idx += frames_to_process;
            }

            resampled_samples.push(output_buffer);
        }

        // Interleave the resampled channels
        let mut interleaved_samples = Vec::new();
        let new_frames = resampled_samples[0].len();

        for frame_idx in 0..new_frames {
            for ch in 0..self.inner.channels as usize {
                interleaved_samples.push(resampled_samples[ch][frame_idx]);
            }
        }

        let new_duration = Duration::from_secs_f64(
            interleaved_samples.len() as f64
                / (target_sample_rate * self.inner.channels as u32) as f64,
        );

        Ok(Self::new(
            interleaved_samples,
            target_sample_rate,
            self.inner.channels,
            new_duration,
        ))
    }
}
