mod load_options;
mod symphonia_loader;

use crate::error::{PetalSonicError, Result};
pub use load_options::LoadOptions;
use std::sync::Arc;
use std::time::Duration;

pub use symphonia_loader::{load_audio_file, load_audio_file_simple};

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

        use rubato::{FftFixedIn, Resampler};

        let chunk_size = 1024;

        let mut resampler = FftFixedIn::new(
            self.inner.sample_rate as usize,
            target_sample_rate as usize,
            chunk_size,
            2, // sub_chunks
            self.inner.channels as usize,
        )
        .map_err(|e| PetalSonicError::AudioLoading(format!("Failed to create resampler: {}", e)))?;

        let mut resampled_samples = Vec::new();

        // Process each channel separately
        for ch in 0..self.inner.channels as usize {
            let channel_samples = self.channel_samples(ch)?;
            let mut output_buffer = Vec::new();
            let mut input_index = 0;

            while input_index < channel_samples.len() {
                let remaining_samples = channel_samples.len() - input_index;
                let samples_to_process = remaining_samples.min(chunk_size);

                if samples_to_process == 0 {
                    break;
                }

                // Pad the chunk to chunk_size if needed
                let mut input_chunk = vec![0.0f32; chunk_size];
                let end_index = (input_index + samples_to_process).min(channel_samples.len());
                input_chunk[..samples_to_process]
                    .copy_from_slice(&channel_samples[input_index..end_index]);

                let waves_in = vec![input_chunk];
                let waves_out = resampler.process(&waves_in, None).map_err(|e| {
                    PetalSonicError::AudioLoading(format!("Resampling error: {}", e))
                })?;

                if let Some(first_channel) = waves_out.get(0) {
                    output_buffer.extend_from_slice(first_channel);
                }

                input_index += samples_to_process;
            }

            resampled_samples.push(output_buffer);
        }

        // Interleave the resampled channels
        let mut interleaved_samples = Vec::new();
        let new_frames = resampled_samples[0].len();

        for frame_idx in 0..new_frames {
            for ch in 0..self.inner.channels as usize {
                if frame_idx < resampled_samples[ch].len() {
                    interleaved_samples.push(resampled_samples[ch][frame_idx]);
                }
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
