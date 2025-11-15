/// Configuration descriptor for a PetalSonic world
#[derive(Debug, Clone)]
pub struct PetalSonicWorldDesc {
    /// Sample rate for the world processing (may differ from device sample rate)
    pub sample_rate: u32,
    /// Block size in world sample rate (number of frames to generate per audio processing chunk).
    ///
    /// This is the fixed number of frames generated at the world's sample rate, which are then
    /// resampled to the device's sample rate (producing variable output based on the ratio).
    pub block_size: usize,
    /// Number of audio channels (typically 2 for stereo)
    pub channels: u16,
    /// Maximum number of concurrent audio sources
    pub max_sources: usize,
    /// Optional path to a custom HRTF SOFA file (None uses Steam Audio's default HRTF)
    pub hrtf_path: Option<String>,
    /// HRTF gain compensation in decibels (default: 0.0 dB = no change)
    ///
    /// Different HRTF datasets can have different overall gain levels.
    /// Use this to compensate for volume differences between HRTFs.
    ///
    /// # Examples
    /// - `0.0`: No change (unity gain)
    /// - `6.0`: Approximately double the perceived loudness (+6 dB)
    /// - `-6.0`: Approximately half the perceived loudness (-6 dB)
    /// - `3.0`: Modest increase (~40% louder)
    /// - `-20.0`: Very quiet (1/10th perceived loudness)
    pub hrtf_gain: f32,
}

impl Default for PetalSonicWorldDesc {
    fn default() -> Self {
        Self {
            sample_rate: 48000,
            block_size: 1024,
            channels: 2,
            max_sources: 2048,
            hrtf_path: None,
            hrtf_gain: 0.0,
        }
    }
}
