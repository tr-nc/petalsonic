use std::time::Duration;

#[derive(Debug, Clone)]
pub struct PetalSonicWorldDesc {
    pub sample_rate: u32,
    pub block_size: usize,
    pub channels: u16,
    pub buffer_duration: Duration,
    pub max_sources: usize,
    pub enable_spatialization: bool,
}

impl Default for PetalSonicWorldDesc {
    fn default() -> Self {
        Self {
            sample_rate: 48000,
            block_size: 1024,
            channels: 2,
            buffer_duration: Duration::from_millis(10),
            max_sources: 64,
            enable_spatialization: true,
        }
    }
}
