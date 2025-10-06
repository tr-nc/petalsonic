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
    /// Input buffer accumulator for partial frames
    input_buffer: Vec<Vec<f32>>,
    /// Number of input samples needed per output buffer
    input_frames_needed: usize,
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
            source_sample_rate as f64 / target_sample_rate as f64,
            1.0, // max_resample_ratio_relative - we're not changing it dynamically
            rubato::PolynomialDegree::Septic,
            output_frames,
            channels as usize,
        )
        .map_err(|e| {
            PetalSonicError::AudioLoading(format!("Failed to create streaming resampler: {}", e))
        })?;

        // Initialize empty input buffers for each channel
        let input_buffer: Vec<Vec<f32>> = (0..channels).map(|_| Vec::new()).collect();

        // Calculate how many input frames we need
        let input_frames_needed = resampler.input_frames_next();

        Ok(Self {
            resampler,
            source_sample_rate,
            target_sample_rate,
            channels,
            input_buffer,
            input_frames_needed,
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
    /// This function may need to be called multiple times to fill the output buffer
    /// if not enough input samples are provided initially. Use `needs_input()` to
    /// check if more input is required.
    pub fn process_interleaved(
        &mut self,
        input_samples: &[f32],
        output_samples: &mut [f32],
    ) -> Result<usize> {
        let channels = self.channels as usize;

        // De-interleave input and add to buffers
        for (frame_idx, frame) in input_samples.chunks(channels).enumerate() {
            for (ch_idx, &sample) in frame.iter().enumerate() {
                if ch_idx < self.input_buffer.len() {
                    self.input_buffer[ch_idx].push(sample);
                }
            }

            // Check if we've accumulated enough samples
            if frame_idx > 0
                && (frame_idx + 1) % self.input_frames_needed == 0
                && self.input_buffer[0].len() >= self.input_frames_needed
            {
                // We have enough samples, process them
                if let Ok(frames_written) = self.process_accumulated_samples(output_samples) {
                    if frames_written > 0 {
                        return Ok(frames_written);
                    }
                }
            }
        }

        // Try to process whatever we have if we have enough
        if self.input_buffer[0].len() >= self.input_frames_needed {
            self.process_accumulated_samples(output_samples)
        } else {
            // Not enough input yet, fill with silence
            output_samples.fill(0.0);
            Ok(0)
        }
    }

    /// Internal method to process accumulated input samples
    fn process_accumulated_samples(&mut self, output_samples: &mut [f32]) -> Result<usize> {
        let channels = self.channels as usize;

        // Take exactly the number of frames we need from each channel buffer
        let input_waves: Vec<Vec<f32>> = self
            .input_buffer
            .iter_mut()
            .map(|channel_buffer| {
                let needed = self.input_frames_needed.min(channel_buffer.len());
                let samples: Vec<f32> = channel_buffer.drain(..needed).collect();
                samples
            })
            .collect();

        // Check if all channels have enough samples
        if input_waves[0].len() < self.input_frames_needed {
            return Ok(0);
        }

        // Process the samples
        let output_waves = self.resampler.process(&input_waves, None).map_err(|e| {
            PetalSonicError::AudioLoading(format!("Streaming resampling error: {}", e))
        })?;

        // Re-interleave the output
        let output_frames = output_waves[0].len();

        for frame_idx in 0..output_frames {
            for ch_idx in 0..channels {
                let output_idx = frame_idx * channels + ch_idx;
                if output_idx < output_samples.len() && ch_idx < output_waves.len() {
                    output_samples[output_idx] = output_waves[ch_idx][frame_idx];
                }
            }
        }

        // Update how many input frames we'll need next time
        self.input_frames_needed = self.resampler.input_frames_next();

        Ok(output_frames)
    }

    /// Returns true if the resampler needs more input samples
    pub fn needs_input(&self) -> bool {
        self.input_buffer[0].len() < self.input_frames_needed
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
        self.input_frames_needed = self.resampler.input_frames_next();
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
    fn test_no_resampling_needed() {
        let mut resampler = StreamingResampler::new(48000, 48000, 2, 512).unwrap();

        // Generate some test input (silence)
        let input = vec![0.0f32; 1024 * 2]; // 1024 frames, 2 channels
        let mut output = vec![0.0f32; 512 * 2]; // 512 frames, 2 channels

        let result = resampler.process_interleaved(&input, &mut output);
        assert!(result.is_ok());
    }

    #[test]
    fn test_upsampling() {
        let mut resampler = StreamingResampler::new(44100, 48000, 2, 512).unwrap();

        // Generate a simple sine wave at 440 Hz
        let input_frames = 2048;
        let mut input = Vec::new();
        for i in 0..input_frames {
            let t = i as f32 / 44100.0;
            let sample = (2.0 * std::f32::consts::PI * 440.0 * t).sin();
            input.push(sample); // Left
            input.push(sample); // Right
        }

        let mut output = vec![0.0f32; 512 * 2];
        let result = resampler.process_interleaved(&input, &mut output);
        assert!(result.is_ok());
    }

    #[test]
    fn test_downsampling() {
        let mut resampler = StreamingResampler::new(48000, 44100, 2, 512).unwrap();

        // Generate a simple sine wave at 440 Hz
        let input_frames = 2048;
        let mut input = Vec::new();
        for i in 0..input_frames {
            let t = i as f32 / 48000.0;
            let sample = (2.0 * std::f32::consts::PI * 440.0 * t).sin();
            input.push(sample); // Left
            input.push(sample); // Right
        }

        let mut output = vec![0.0f32; 512 * 2];
        let result = resampler.process_interleaved(&input, &mut output);
        assert!(result.is_ok());
    }
}
