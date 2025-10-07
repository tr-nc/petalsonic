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

        // target/source (output/input)
        let resample_ratio = target_sample_rate as f64 / source_sample_rate as f64;

        let resampler = FastFixedOut::new(
            resample_ratio,
            1.0, // we're not changing it dynamically
            rubato::PolynomialDegree::Septic,
            output_frames,
            channels as usize,
        )
        .map_err(|e| {
            PetalSonicError::AudioLoading(format!("Failed to create streaming resampler: {}", e))
        })?;

        // creates a 2D buffer (a vector of vectors) for multi‑channel floating‑point audio samples
        let input_buffer: Vec<Vec<f32>> = (0..channels).map(|_| Vec::new()).collect();

        Ok(Self {
            resampler,
            source_sample_rate,
            target_sample_rate,
            channels,
            input_buffer,
        })
    }

    /// De-interleaves input samples and appends them to the internal buffers
    fn deinterleave_and_buffer_input(&mut self, input_samples: &[f32]) {
        let channels = self.channels as usize;
        for frame in input_samples.chunks_exact(channels) {
            for (ch_idx, &sample) in frame.iter().enumerate() {
                if ch_idx < self.input_buffer.len() {
                    self.input_buffer[ch_idx].push(sample);
                }
            }
        }
    }

    /// Attempts to produce one resampled chunk if enough input is available
    ///
    /// # Returns
    /// A tuple of (output_frames_written, input_frames_consumed) if successful,
    /// or (0, 0) if not enough input is available
    fn try_produce_resampled_chunk(
        &mut self,
        output_samples: &mut [f32],
        out_frames_written: usize,
        out_frames_capacity: usize,
    ) -> Result<(usize, usize)> {
        let channels = self.channels as usize;
        let frames_needed = self.resampler.input_frames_next();

        // Not enough input accumulated to produce another chunk
        if self.input_buffer[0].len() < frames_needed {
            return Ok((0, 0));
        }

        // Drain exactly frames_needed per channel
        let mut input_waves: Vec<Vec<f32>> = Vec::with_capacity(channels);
        for ch in 0..channels {
            let samples: Vec<f32> = self.input_buffer[ch].drain(..frames_needed).collect();
            input_waves.push(samples);
        }

        // Resample
        let output_waves = self.resampler.process(&input_waves, None).map_err(|e| {
            PetalSonicError::AudioLoading(format!("Streaming resampling error: {}", e))
        })?;

        let produced_frames = output_waves[0].len();

        // Re-interleave into output buffer (may be truncated to fit)
        let frames_to_copy = produced_frames.min(out_frames_capacity - out_frames_written);
        for f in 0..frames_to_copy {
            let dst_frame_idx = out_frames_written + f;
            for ch in 0..channels {
                output_samples[dst_frame_idx * channels + ch] = output_waves[ch][f];
            }
        }

        Ok((frames_to_copy, frames_needed))
    }

    /// Zero-fills the remainder of the output buffer if not completely filled
    fn zero_fill_output(&self, output_samples: &mut [f32], out_frames_written: usize) {
        let channels = self.channels as usize;
        let out_frames_capacity = output_samples.len() / channels;

        if out_frames_written < out_frames_capacity {
            let start = out_frames_written * channels;
            output_samples[start..].fill(0.0);
        }
    }

    /// Processes interleaved audio samples and resamples them to the target rate
    ///
    /// # Arguments
    /// * `input_samples` - Interleaved f32 samples at the source sample rate
    /// * `output_samples` - Interleaved f32 buffer to fill with resampled audio
    ///
    /// # Returns
    /// A tuple of (output_frames_written, input_frames_consumed)
    ///
    /// # Important
    /// - Always advance your audio source by `input_frames_consumed`, NOT by `output_frames_written`
    /// - The function will fill as much of the output buffer as possible
    /// - Any unfilled portion of the output buffer is zero-filled
    pub fn process_interleaved(
        &mut self,
        input_samples: &[f32],
        output_samples: &mut [f32],
    ) -> Result<(usize, usize)> {
        let channels = self.channels as usize;
        let out_frames_capacity = output_samples.len() / channels;
        let mut out_frames_written = 0usize;
        let mut in_frames_consumed = 0usize;

        // 1) De-interleave and append new input to our internal buffers
        self.deinterleave_and_buffer_input(input_samples);

        // 2) Produce output chunks until we fill the output buffer or run out of input
        while out_frames_written < out_frames_capacity {
            let (chunk_out_frames, chunk_in_frames) = self.try_produce_resampled_chunk(
                output_samples,
                out_frames_written,
                out_frames_capacity,
            )?;

            // No more input available to produce chunks
            if chunk_out_frames == 0 {
                break;
            }

            out_frames_written += chunk_out_frames;
            in_frames_consumed += chunk_in_frames;
        }

        // 3) Zero-fill any remainder in the device buffer
        self.zero_fill_output(output_samples, out_frames_written);

        Ok((out_frames_written, in_frames_consumed))
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

    /// Returns the resampling ratio (source/target) - for diagnostic purposes
    /// Note: This is NOT the ratio passed to rubato, which uses target/source
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

        let (out_frames, in_frames) = result.unwrap();
        assert!(out_frames > 0, "Should produce output frames");
        assert!(in_frames > 0, "Should consume input frames");
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

        let (out_frames, in_frames) = result.unwrap();
        assert!(out_frames > 0, "Should produce output frames");
        assert!(in_frames > 0, "Should consume input frames");
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
            let _result = resampler.process_interleaved(&input, &mut output);
        }
    }
}
