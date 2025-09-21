//! Configuration for PetalSonic

use std::time::Duration;

#[derive(Debug, Clone)]
pub struct PetalSonicConfig {
    pub sample_rate: u32,
    pub block_size: usize,
    pub channels: u16,
    pub buffer_duration: Duration,
    pub max_sources: usize,
    pub enable_spatialization: bool,
}

impl Default for PetalSonicConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48000,
            block_size: 512,
            channels: 2,
            buffer_duration: Duration::from_millis(10),
            max_sources: 64,
            enable_spatialization: true,
        }
    }
}

impl PetalSonicConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sample_rate(mut self, rate: u32) -> Self {
        self.sample_rate = rate;
        self
    }

    pub fn block_size(mut self, size: usize) -> Self {
        self.block_size = size;
        self
    }

    pub fn channels(mut self, channels: u16) -> Self {
        self.channels = channels;
        self
    }

    pub fn buffer_duration(mut self, duration: Duration) -> Self {
        self.buffer_duration = duration;
        self
    }

    pub fn max_sources(mut self, max: usize) -> Self {
        self.max_sources = max;
        self
    }

    pub fn enable_spatialization(mut self, enable: bool) -> Self {
        self.enable_spatialization = enable;
        self
    }
}
