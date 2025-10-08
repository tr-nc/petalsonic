use crate::error::{PetalSonicError, Result};
use rubato::{
    FastFixedIn, PolynomialDegree, Resampler, SincFixedIn, SincInterpolationParameters,
    SincInterpolationType, WindowFunction,
};

/// Type of resampler algorithm to use
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResamplerType {
    /// Fast polynomial resampler - lower quality but faster
    Fast,
    /// Sinc interpolation resampler - higher quality but slower
    Sinc,
}

impl Default for ResamplerType {
    fn default() -> Self {
        Self::Sinc
    }
}

enum ResamplerImpl {
    Fast(FastFixedIn<f32>),
    Sinc(SincFixedIn<f32>),
}

impl ResamplerImpl {
    fn process(
        &mut self,
        input: &[Vec<f32>],
    ) -> std::result::Result<Vec<Vec<f32>>, rubato::ResampleError> {
        match self {
            Self::Fast(r) => r.process(input, None),
            Self::Sinc(r) => r.process(input, None),
        }
    }

    fn reset(&mut self) {
        match self {
            Self::Fast(r) => r.reset(),
            Self::Sinc(r) => r.reset(),
        }
    }
}

/// A real-time streaming resampler that converts audio from one sample rate to another
/// in real-time with minimal latency. Uses a fixed-input-size approach where the world
/// generates a fixed number of frames and the resampler produces variable output based
/// on the sample rate ratio.
pub struct StreamingResampler {
    resampler: ResamplerImpl,
    source_sample_rate: u32,
    target_sample_rate: u32,
    channels: u16,
    input_chunk_size: usize,
}

impl StreamingResampler {
    /// Creates a new streaming resampler with fixed input size
    ///
    /// # Arguments
    /// * `source_sample_rate` - The sample rate of the audio being produced by the world
    /// * `target_sample_rate` - The sample rate required by the audio device
    /// * `channels` - Number of audio channels
    /// * `input_frames` - The fixed number of frames to generate at world sample rate per chunk
    /// * `resampler_type` - Type of resampler algorithm to use (defaults to Sinc if None)
    ///
    /// # Returns
    /// A new `StreamingResampler` instance configured for real-time processing
    pub fn new(
        source_sample_rate: u32,
        target_sample_rate: u32,
        channels: u16,
        input_frames: usize,
        resampler_type: Option<ResamplerType>,
    ) -> Result<Self> {
        if source_sample_rate == 0 || target_sample_rate == 0 {
            return Err(PetalSonicError::AudioFormat(
                "Sample rates must be greater than 0".to_string(),
            ));
        }

        if channels == 0 {
            return Err(PetalSonicError::AudioFormat(
                "Channel count must be greater than 0".to_string(),
            ));
        }

        if input_frames == 0 {
            return Err(PetalSonicError::AudioFormat(
                "Input frames must be greater than 0".to_string(),
            ));
        }

        // target/source (output/input)
        let resample_ratio = target_sample_rate as f64 / source_sample_rate as f64;
        let resampler_type = resampler_type.unwrap_or_default();

        log::info!(
            "Creating {:?} resampler: {} Hz -> {} Hz (fixed input: {} frames)",
            resampler_type,
            source_sample_rate,
            target_sample_rate,
            input_frames
        );

        let resampler = match resampler_type {
            ResamplerType::Fast => {
                let fast = FastFixedIn::new(
                    resample_ratio,
                    1.0, // we're not changing it dynamically
                    PolynomialDegree::Septic,
                    input_frames,
                    channels as usize,
                )
                .map_err(|e| {
                    PetalSonicError::AudioLoading(format!("Failed to create fast resampler: {}", e))
                })?;
                ResamplerImpl::Fast(fast)
            }
            ResamplerType::Sinc => {
                let params = SincInterpolationParameters {
                    sinc_len: 256,
                    f_cutoff: 0.95,
                    interpolation: SincInterpolationType::Linear,
                    oversampling_factor: 256,
                    window: WindowFunction::BlackmanHarris2,
                };

                let sinc = SincFixedIn::new(
                    resample_ratio,
                    1.0, // we're not changing it dynamically
                    params,
                    input_frames,
                    channels as usize,
                )
                .map_err(|e| {
                    PetalSonicError::AudioLoading(format!("Failed to create sinc resampler: {}", e))
                })?;
                ResamplerImpl::Sinc(sinc)
            }
        };

        Ok(Self {
            resampler,
            source_sample_rate,
            target_sample_rate,
            channels,
            input_chunk_size: input_frames,
        })
    }

    /// Processes interleaved audio samples and resamples them to the target rate
    ///
    /// # Arguments
    /// * `input_samples` - Interleaved f32 samples at the source sample rate (must be exactly input_chunk_size frames)
    /// * `output_samples` - Interleaved f32 buffer to fill with resampled audio (will be resized as needed)
    ///
    /// # Returns
    /// A tuple of (output_frames_written, input_frames_consumed)
    ///
    /// # Important
    /// - Input must contain exactly `input_chunk_size` frames (input_chunk_size * channels samples)
    /// - Output size will vary based on the resampling ratio
    pub fn process_interleaved(
        &mut self,
        input_samples: &[f32],
        output_samples: &mut [f32],
    ) -> Result<(usize, usize)> {
        let channels = self.channels as usize;
        let input_frames = input_samples.len() / channels;

        if input_frames != self.input_chunk_size {
            return Err(PetalSonicError::AudioFormat(format!(
                "Input size mismatch: expected {} frames, got {} frames",
                self.input_chunk_size, input_frames
            )));
        }

        // De-interleave input
        let mut input_waves: Vec<Vec<f32>> = vec![Vec::with_capacity(input_frames); channels];
        for frame_idx in 0..input_frames {
            for ch in 0..channels {
                input_waves[ch].push(input_samples[frame_idx * channels + ch]);
            }
        }

        // Resample
        let output_waves = self.resampler.process(&input_waves).map_err(|e| {
            PetalSonicError::AudioLoading(format!("Streaming resampling error: {}", e))
        })?;

        let output_frames = output_waves[0].len();
        let output_samples_needed = output_frames * channels;

        // Check if output buffer is large enough
        if output_samples.len() < output_samples_needed {
            return Err(PetalSonicError::AudioFormat(format!(
                "Output buffer too small: need {} samples, got {}",
                output_samples_needed,
                output_samples.len()
            )));
        }

        // Re-interleave output
        for frame_idx in 0..output_frames {
            for ch in 0..channels {
                output_samples[frame_idx * channels + ch] = output_waves[ch][frame_idx];
            }
        }

        Ok((output_frames, input_frames))
    }

    /// Returns the fixed input chunk size (in frames)
    pub fn input_chunk_size(&self) -> usize {
        self.input_chunk_size
    }

    /// Returns the target (output) sample rate in Hz
    pub fn target_sample_rate(&self) -> u32 {
        self.target_sample_rate
    }

    /// Returns the source (input) sample rate in Hz
    pub fn source_sample_rate(&self) -> u32 {
        self.source_sample_rate
    }

    /// Returns the resampling ratio (source/target) - for diagnostic purposes
    /// Note: This is NOT the ratio passed to rubato, which uses target/source
    pub fn resample_ratio(&self) -> f64 {
        self.source_sample_rate as f64 / self.target_sample_rate as f64
    }

    /// Reset the internal state of the resampler
    pub fn reset(&mut self) {
        self.resampler.reset();
    }
}
