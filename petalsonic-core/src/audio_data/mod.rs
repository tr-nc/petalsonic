mod default_loader;
mod load_options;
mod loader;
mod resampler;
mod streaming_resampler;

use crate::error::{PetalSonicError, Result};
pub use default_loader::DefaultAudioLoader;
pub use load_options::{ConvertToMono, LoadOptions};
pub use loader::AudioDataLoader;
pub use resampler::AudioResampler;
use std::sync::Arc;
use std::time::Duration;
pub use streaming_resampler::StreamingResampler;

/// Container for loaded audio data with reference-counted sharing.
///
/// # Data Format
/// All audio samples are stored in **INTERLEAVED** format internally.
/// See [`AudioDataInner`] for details on the data layout.
#[derive(Debug, Clone)]
pub struct PetalSonicAudioData {
    inner: Arc<AudioDataInner>,
}

/// Internal audio data storage.
///
/// # Data Format
/// All audio samples are stored in **INTERLEAVED** format, where samples from different
/// channels are mixed together frame by frame.
///
/// ## Interleaved Format (used here)
/// Samples from all channels are stored together, alternating by frame:
/// - Stereo (2-channel): `[L0, R0, L1, R1, L2, R2, ...]`
/// - Mono (1-channel): `[M0, M1, M2, M3, ...]`
/// - 5.1 surround: `[FL0, FR0, C0, LFE0, RL0, RR0, FL1, FR1, C1, LFE1, RL1, RR1, ...]`
///
/// ## Planar Format (alternative, NOT used here)
/// Each channel is stored in a separate contiguous buffer:
/// - Stereo: `Left: [L0, L1, L2, ...], Right: [R0, R1, R2, ...]`
/// - Would require: `Vec<Vec<f32>>` or separate buffers per channel
///
/// ## Why Interleaved?
/// 1. **Audio file compatibility**: Most audio files (WAV, MP3, FLAC) store data interleaved
/// 2. **Hardware/API compatibility**: Audio APIs (CPAL, PortAudio) typically expect interleaved data
/// 3. **Cache locality for playback**: When processing frames sequentially, all channel data
///    for a given time point is adjacent in memory
/// 4. **Simpler API**: Single buffer is easier to manage than per-channel buffers
/// 5. **Frame-based operations**: Makes it trivial to extract/process complete frames
///
/// ## When Planar is Better
/// - Per-channel DSP operations (e.g., independent channel processing)
/// - SIMD operations on single channels
/// - Some audio processing libraries prefer planar (e.g., FFmpeg, some VST plugins)
///
/// **Note**: Functions like `channel_samples()` can extract planar data when needed.
#[derive(Debug)]
pub(crate) struct AudioDataInner {
    /// Audio samples stored in **INTERLEAVED** format.
    ///
    /// # Format: INTERLEAVED
    /// - Samples from all channels are mixed: `[L0, R0, L1, R1, L2, R2, ...]`
    /// - Total length = `total_frames * channels`
    /// - Each frame contains one sample from each channel
    pub samples: Vec<f32>,

    /// Sample rate in Hz (e.g., 44100, 48000)
    pub sample_rate: u32,

    /// Number of audio channels (1 = mono, 2 = stereo, etc.)
    pub channels: u16,

    /// Total duration of the audio
    pub duration: Duration,

    /// Total number of frames (one frame = one sample from each channel)
    ///
    /// Calculated as: `samples.len() / channels`
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

    /// Load audio data from a file path using the default loader.
    ///
    /// This is a convenience method that uses the built-in Symphonia-based loader
    /// with default loading options.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the audio file (supports WAV, MP3, FLAC, OGG, etc.)
    ///
    /// # Returns
    ///
    /// Returns an `Arc<PetalSonicAudioData>` containing the decoded audio on success.
    ///
    /// # Errors
    ///
    /// Returns a `PetalSonicError` if the file cannot be loaded or decoded.
    pub fn from_path(path: &str) -> Result<Arc<Self>> {
        let loader = DefaultAudioLoader;
        loader.load(path, &LoadOptions::default())
    }

    /// Load audio data from a file path with custom loading options.
    ///
    /// This is a convenience method that uses the built-in Symphonia-based loader
    /// with user-specified loading options.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the audio file
    /// * `options` - Loading options that control behavior like mono conversion
    ///
    /// # Returns
    ///
    /// Returns an `Arc<PetalSonicAudioData>` containing the decoded audio on success.
    ///
    /// # Errors
    ///
    /// Returns a `PetalSonicError` if the file cannot be loaded or decoded.
    pub fn from_path_with_options(path: &str, options: &LoadOptions) -> Result<Arc<Self>> {
        let loader = DefaultAudioLoader;
        loader.load(path, options)
    }

    /// Load audio data from a file path using a custom loader.
    ///
    /// This method allows you to use your own audio loading implementation
    /// by providing a custom loader that implements the [`AudioDataLoader`] trait.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the audio file
    /// * `loader` - A custom loader implementing the `AudioDataLoader` trait
    /// * `options` - Loading options that control behavior like mono conversion
    ///
    /// # Returns
    ///
    /// Returns an `Arc<PetalSonicAudioData>` containing the decoded audio on success.
    ///
    /// # Errors
    ///
    /// Returns a `PetalSonicError` if the file cannot be loaded or decoded.
    pub fn from_path_with_loader<L: AudioDataLoader>(
        path: &str,
        loader: &L,
        options: &LoadOptions,
    ) -> Result<Arc<Self>> {
        loader.load(path, options)
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

    /// Resample to a different sample rate using rubato, returns a new `PetalSonicAudioData` instance
    pub fn resample(&self, target_sample_rate: u32) -> Result<Self> {
        if target_sample_rate == self.inner.sample_rate {
            return Ok(self.clone());
        }

        let resampler = AudioResampler::new(
            self.inner.sample_rate,
            target_sample_rate,
            self.inner.channels,
            Some(1024), // chunk_size
        )?;

        let resampled_samples = resampler.resample_interleaved(&self.inner.samples)?;

        let new_duration = Duration::from_secs_f64(
            resampled_samples.len() as f64
                / (target_sample_rate * self.inner.channels as u32) as f64,
        );

        Ok(Self::new(
            resampled_samples,
            target_sample_rate,
            self.inner.channels,
            new_duration,
        ))
    }
}
