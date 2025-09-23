use crate::error::{PetalSonicError, Result};

pub struct AudioResampler {
    source_sample_rate: u32,
    target_sample_rate: u32,
    channels: u16,
    chunk_size: usize,
}

impl AudioResampler {
    pub fn new(
        source_sample_rate: u32,
        target_sample_rate: u32,
        channels: u16,
        chunk_size: Option<usize>,
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

        Ok(Self {
            source_sample_rate,
            target_sample_rate,
            channels,
            chunk_size: chunk_size.unwrap_or(1024),
        })
    }

    pub fn resample_channel(&self, channel_samples: &[f32]) -> Result<Vec<f32>> {
        if self.source_sample_rate == self.target_sample_rate {
            return Ok(channel_samples.to_vec());
        }

        use rubato::{FftFixedIn, Resampler};

        let mut resampler = FftFixedIn::new(
            self.source_sample_rate as usize,
            self.target_sample_rate as usize,
            self.chunk_size,
            2, // sub_chunks
            1, // single channel
        )
        .map_err(|e| PetalSonicError::AudioLoading(format!("Failed to create resampler: {}", e)))?;

        let mut output_buffer = Vec::new();
        let mut input_index = 0;

        while input_index < channel_samples.len() {
            let remaining_samples = channel_samples.len() - input_index;
            let samples_to_process = remaining_samples.min(self.chunk_size);

            if samples_to_process == 0 {
                break;
            }

            // Pad the chunk to chunk_size if needed
            let mut input_chunk = vec![0.0f32; self.chunk_size];
            let end_index = (input_index + samples_to_process).min(channel_samples.len());
            input_chunk[..samples_to_process]
                .copy_from_slice(&channel_samples[input_index..end_index]);

            let waves_in = vec![input_chunk];
            let waves_out = resampler
                .process(&waves_in, None)
                .map_err(|e| PetalSonicError::AudioLoading(format!("Resampling error: {}", e)))?;

            if let Some(first_channel) = waves_out.first() {
                output_buffer.extend_from_slice(first_channel);
            }

            input_index += samples_to_process;
        }

        Ok(output_buffer)
    }

    pub fn resample_interleaved(&self, interleaved_samples: &[f32]) -> Result<Vec<f32>> {
        if self.source_sample_rate == self.target_sample_rate {
            return Ok(interleaved_samples.to_vec());
        }

        // Split into channels
        let mut channel_samples = Vec::new();
        for ch in 0..self.channels as usize {
            let channel_data: Vec<f32> = interleaved_samples
                .chunks(self.channels as usize)
                .map(|frame| frame.get(ch).copied().unwrap_or(0.0))
                .collect();
            channel_samples.push(channel_data);
        }

        // Resample each channel
        let mut resampled_channels = Vec::new();
        for channel_data in &channel_samples {
            let resampled = self.resample_channel(channel_data)?;
            resampled_channels.push(resampled);
        }

        // Interleave the resampled channels
        let mut interleaved_samples = Vec::new();
        let new_frames = resampled_channels[0].len();

        for frame_idx in 0..new_frames {
            for resampled_channel in resampled_channels.iter().take(self.channels as usize) {
                if frame_idx < resampled_channel.len() {
                    interleaved_samples.push(resampled_channel[frame_idx]);
                }
            }
        }

        Ok(interleaved_samples)
    }

    pub fn target_sample_rate(&self) -> u32 {
        self.target_sample_rate
    }

    pub fn source_sample_rate(&self) -> u32 {
        self.source_sample_rate
    }

    pub fn resample_ratio(&self) -> f64 {
        self.target_sample_rate as f64 / self.source_sample_rate as f64
    }
}
