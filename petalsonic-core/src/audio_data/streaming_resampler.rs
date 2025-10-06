use crate::error::{PetalSonicError, Result};
use rubato::{FastFixedOut, Resampler};

/// A real-time streaming resampler that converts audio from one sample rate to another
/// in real-time with minimal latency. Uses a fixed-output-size approach suitable for
/// audio device callbacks where the output buffer size is known.
pub struct StreamingResampler {
    resampler: FastFixedOut<f32>,
    source_sample_rate: u32,
    target_sample_rate: u32,
    channels: u16,
    /// Input buffer accumulator for partial frames (per channel, non-interleaved)
    input_buffer: Vec<Vec<f32>>,
}

impl StreamingResampler {
    /// Creates a new streaming resampler
    ///
    /// # Arguments
    /// * `source_sample_rate` - The sample rate of the audio being produced by the engine
    /// * `target_sample_rate` - The sample rate required by the audio device
    /// * `channels` - Number of audio channels
    /// * `output_frames` - The number of frames the device expects per callback
    ///
    /// # Returns
    /// A new `StreamingResampler` instance configured for real-time processing
    pub fn new(
        source_sample_rate: u32,
        target_sample_rate: u32,
        channels: u16,
        output_frames: usize,
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

        if output_frames == 0 {
            return Err(PetalSonicError::AudioFormat(
                "Output frames must be greater than 0".to_string(),
            ));
        }

        // Create the rubato resampler with fixed output size
        let resampler = FastFixedOut::new(
            target_sample_rate as f64 / source_sample_rate as f64,
            1.0,
            rubato::PolynomialDegree::Septic,
            output_frames,
            channels as usize,
        )
        .map_err(|e| {
            PetalSonicError::AudioLoading(format!("Failed to create streaming resampler: {}", e))
        })?;

        // Initialize empty input buffers for each channel
        let input_buffer: Vec<Vec<f32>> = (0..channels).map(|_| Vec::new()).collect();

        Ok(Self {
            resampler,
            source_sample_rate,
            target_sample_rate,
            channels,
            input_buffer,
        })
    }

    /// Processes interleaved audio samples and resamples them to the target rate
    ///
    /// # Arguments
    /// * `input_samples` - Interleaved f32 samples at the source sample rate
    /// * `output_samples` - Interleaved f32 buffer to fill with resampled audio
    ///
    /// # Returns
    /// The number of frames written to output_samples
    ///
    /// # Note
    /// This function accumulates input samples and processes them when enough
    /// samples are available. It may return 0 if not enough input has been accumulated yet.
    pub fn process_interleaved(
        &mut self,
        input_samples: &[f32],
        output_samples: &mut [f32],
    ) -> Result<usize> {
        let channels = self.channels as usize;

        // De-interleave input and append to our buffers
        for frame in input_samples.chunks_exact(channels) {
            for (ch_idx, &sample) in frame.iter().enumerate() {
                if ch_idx < self.input_buffer.len() {
                    self.input_buffer[ch_idx].push(sample);
                }
            }
        }

        // Check how many input frames we need for the next resampling operation
        let frames_needed = self.resampler.input_frames_next();
        let frames_available = self.input_buffer[0].len();

        // If we don't have enough input yet, return 0 (fill output with silence)
        if frames_available < frames_needed {
            output_samples.fill(0.0);
            return Ok(0);
        }

        // Take exactly the number of frames the resampler needs from each channel
        let mut input_waves: Vec<Vec<f32>> = Vec::with_capacity(channels);
        for channel_buffer in &mut self.input_buffer {
            let samples: Vec<f32> = channel_buffer.drain(..frames_needed).collect();
            input_waves.push(samples);
        }

        // Process the samples through the resampler
        let output_waves = self.resampler.process(&input_waves, None).map_err(|e| {
            PetalSonicError::AudioLoading(format!("Streaming resampling error: {}", e))
        })?;

        // Re-interleave the output
        let output_frames = output_waves[0].len();
        let mut frames_written = 0;

        for frame_idx in 0..output_frames {
            for ch_idx in 0..channels {
                let output_idx = frame_idx * channels + ch_idx;
                if output_idx < output_samples.len() && ch_idx < output_waves.len() {
                    output_samples[output_idx] = output_waves[ch_idx][frame_idx];
                }
            }
            frames_written += 1;

            // Stop if we've filled the output buffer
            if (frame_idx + 1) * channels >= output_samples.len() {
                break;
            }
        }

        Ok(frames_written)
    }

    /// Returns true if the resampler needs more input samples to produce output
    pub fn needs_input(&self) -> bool {
        let frames_needed = self.resampler.input_frames_next();
        self.input_buffer[0].len() < frames_needed
    }

    /// Returns how many input frames are needed for the next process call
    pub fn input_frames_needed(&self) -> usize {
        self.resampler.input_frames_next()
    }

    /// Returns how many input frames are currently buffered
    pub fn buffered_frames(&self) -> usize {
        self.input_buffer[0].len()
    }

    /// Returns the target (output) sample rate in Hz
    pub fn target_sample_rate(&self) -> u32 {
        self.target_sample_rate
    }

    /// Returns the source (input) sample rate in Hz
    pub fn source_sample_rate(&self) -> u32 {
        self.source_sample_rate
    }

    /// Returns the resampling ratio (source/target)
    pub fn resample_ratio(&self) -> f64 {
        self.source_sample_rate as f64 / self.target_sample_rate as f64
    }

    /// Reset the internal state of the resampler
    pub fn reset(&mut self) {
        for channel_buffer in &mut self.input_buffer {
            channel_buffer.clear();
        }
        self.resampler.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_streaming_resampler_creation() {
        let resampler = StreamingResampler::new(48000, 44100, 2, 512);
        assert!(resampler.is_ok());
    }

    #[test]
    fn test_downsampling_streaming() {
        let mut resampler = StreamingResampler::new(48000, 44100, 2, 512).unwrap();

        // Generate a simple test signal
        let input_frames = 4096;
        let mut input = Vec::new();
        for i in 0..input_frames {
            let t = i as f32 / 48000.0;
            let sample = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
            input.push(sample); // Left
            input.push(sample); // Right
        }

        let mut output = vec![0.0f32; 512 * 2];
        let result = resampler.process_interleaved(&input, &mut output);
        assert!(result.is_ok());

        // Should produce output
        let frames = result.unwrap();
        assert!(frames > 0, "Should produce output frames");
    }

    #[test]
    fn test_upsampling_streaming() {
        let mut resampler = StreamingResampler::new(44100, 48000, 2, 512).unwrap();

        // Generate a simple test signal
        let input_frames = 4096;
        let mut input = Vec::new();
        for i in 0..input_frames {
            let t = i as f32 / 44100.0;
            let sample = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
            input.push(sample); // Left
            input.push(sample); // Right
        }

        let mut output = vec![0.0f32; 512 * 2];
        let result = resampler.process_interleaved(&input, &mut output);
        assert!(result.is_ok());

        // Should produce output
        let frames = result.unwrap();
        assert!(frames > 0, "Should produce output frames");
    }

    #[test]
    fn test_incremental_feeding() {
        let mut resampler = StreamingResampler::new(48000, 44100, 2, 512).unwrap();

        // Feed small chunks incrementally
        let chunk_size = 128;
        for chunk_idx in 0..20 {
            let mut input = Vec::new();
            for i in 0..chunk_size {
                let sample_idx = chunk_idx * chunk_size + i;
                let t = sample_idx as f32 / 48000.0;
                let sample = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
                input.push(sample); // Left
                input.push(sample); // Right
            }

            let mut output = vec![0.0f32; 512 * 2];
            let result = resampler.process_interleaved(&input, &mut output);
            assert!(result.is_ok());
        }
    }
}
