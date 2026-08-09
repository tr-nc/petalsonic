//! Internal immutable PCM storage, decoding, and resampling.

mod batch_resampler;
mod default_loader;
mod streaming_resampler;

use crate::error::Result;
pub use batch_resampler::BatchResampler;
use default_loader::DefaultAudioLoader;
use std::sync::Arc;
use std::time::Duration;
pub use streaming_resampler::{ResamplerType, StreamingResampler};

/// Container for loaded audio data with reference-counted sharing.
///
/// # Data Format
/// All audio samples are stored in **INTERLEAVED** format internally.
/// See the internal documentation for details on the data layout.
#[derive(Debug, Clone)]
pub(crate) struct PetalSonicAudioData {
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
        _duration: Duration,
    ) -> Self {
        let total_frames = samples.len() / channels as usize;
        Self {
            inner: Arc::new(AudioDataInner {
                samples,
                sample_rate,
                channels,
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
        loader.load(path)
    }

    pub fn sample_rate(&self) -> u32 {
        self.inner.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.inner.channels
    }

    pub fn samples(&self) -> &[f32] {
        &self.inner.samples
    }

    pub fn total_frames(&self) -> usize {
        self.inner.total_frames
    }

    /// Resample to a different sample rate using rubato, returns a new `PetalSonicAudioData` instance
    pub fn resample(&self, target_sample_rate: u32) -> Result<Self> {
        if target_sample_rate == self.inner.sample_rate {
            return Ok(self.clone());
        }

        let resampler = BatchResampler::new(
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
