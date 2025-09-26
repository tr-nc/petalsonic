#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvertToMono {
    /// Keep original channels: stereo if input is stereo, mono if input is mono
    Original,
    /// Force mono: if input is mono use it, if input is stereo combine both channels into mono
    ForceMono,
}

#[derive(Debug, Clone)]
pub struct LoadOptions {
    /// How to handle mono conversion
    pub convert_to_mono: ConvertToMono,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            convert_to_mono: ConvertToMono::Original,
        }
    }
}

impl LoadOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn convert_to_mono(mut self, convert: ConvertToMono) -> Self {
        self.convert_to_mono = convert;
        self
    }
}
