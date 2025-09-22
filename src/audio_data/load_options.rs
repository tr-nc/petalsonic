use std::time::Duration;

#[derive(Debug, Clone)]
pub struct LoadOptions {
    /// Target sample rate for resampling (None = keep original)
    pub target_sample_rate: Option<u32>,
    /// Convert to mono after loading
    pub convert_to_mono: bool,
    /// Maximum duration to load (None = load entire file)
    pub max_duration: Option<Duration>,
    /// Which channel to use for mono conversion (None = mix all channels)
    pub mono_channel: Option<usize>,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            target_sample_rate: None,
            convert_to_mono: false,
            max_duration: None,
            mono_channel: None,
        }
    }
}

impl LoadOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn target_sample_rate(mut self, rate: u32) -> Self {
        self.target_sample_rate = Some(rate);
        self
    }

    pub fn convert_to_mono(mut self, convert: bool) -> Self {
        self.convert_to_mono = convert;
        self
    }

    pub fn max_duration(mut self, duration: Duration) -> Self {
        self.max_duration = Some(duration);
        self
    }

    pub fn mono_channel(mut self, channel: usize) -> Self {
        self.mono_channel = Some(channel);
        self
    }
}
