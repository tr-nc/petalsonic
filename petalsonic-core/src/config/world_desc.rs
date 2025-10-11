use std::time::Duration;

/// Configuration descriptor for a PetalSonic world
#[derive(Debug, Clone)]
pub struct PetalSonicWorldDesc {
    /// Sample rate for the world processing (may differ from device sample rate)
    pub sample_rate: u32,
    /// Block size in world sample rate (number of frames to generate per audio processing chunk).
    /// This is the fixed number of frames generated at the world's sample rate, which are then
    /// resampled to the device's sample rate (producing variable output based on the ratio).
    pub block_size: usize,
    /// Number of audio channels (typically 2 for stereo)
    pub channels: u16,
    /// Buffer duration for audio processing
    pub buffer_duration: Duration,
    /// Maximum number of concurrent audio sources
    pub max_sources: usize,
    /// Optional path to a custom HRTF SOFA file (None uses Steam Audio's default HRTF)
    pub hrtf_path: Option<String>,
}

impl Default for PetalSonicWorldDesc {
    fn default() -> Self {
        Self {
            sample_rate: 48000,
            block_size: 1024,
            channels: 2,
            buffer_duration: Duration::from_millis(10),
            max_sources: 64,
            hrtf_path: None,
        }
    }
}
