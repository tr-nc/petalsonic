/// Defines how to handle channel conversion during audio loading.
///
/// This enum controls whether loaded audio should be converted to mono or kept in its
/// original channel configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvertToMono {
    /// Keep original channels: stereo if input is stereo, mono if input is mono.
    ///
    /// This is the default behavior and preserves the audio file's original channel layout.
    Original,

    /// Force mono: if input is mono use it, if input is stereo combine both channels into mono.
    ///
    /// When stereo is converted to mono, channels are averaged together. This is useful for
    /// spatial audio where the spatialization will provide the stereo/3D effect, so starting
    /// with a mono source gives more control.
    ForceMono,
}

/// Options for controlling audio file loading behavior.
///
/// `LoadOptions` provides configuration for how audio files should be decoded and processed
/// when loaded through [`crate::audio_data::PetalSonicAudioData::from_path_with_options`] or custom loaders.
///
/// # Examples
///
/// ```no_run
/// # use petalsonic_core::audio_data::{LoadOptions, ConvertToMono};
/// // Load audio and force conversion to mono
/// let options = LoadOptions::new()
///     .convert_to_mono(ConvertToMono::ForceMono);
/// ```
///
/// ```no_run
/// # use petalsonic_core::audio_data::{LoadOptions, ConvertToMono};
/// // Keep original channels (default)
/// let options = LoadOptions::default();
/// ```
#[derive(Debug, Clone)]
pub struct LoadOptions {
    /// How to handle mono conversion during audio loading.
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
    /// Creates a new `LoadOptions` with default settings.
    ///
    /// This is equivalent to calling `LoadOptions::default()`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the mono conversion option.
    ///
    /// # Arguments
    ///
    /// * `convert` - How to handle channel conversion (`Original` or `ForceMono`)
    ///
    /// # Returns
    ///
    /// Returns `self` to allow method chaining.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use petalsonic_core::audio_data::{LoadOptions, ConvertToMono};
    /// let options = LoadOptions::new()
    ///     .convert_to_mono(ConvertToMono::ForceMono);
    /// ```
    pub fn convert_to_mono(mut self, convert: ConvertToMono) -> Self {
        self.convert_to_mono = convert;
        self
    }
}
