use crate::audio_data::{LoadOptions, PetalSonicAudioData};
use crate::error::Result;
use std::sync::Arc;

/// Trait for loading audio data from file paths.
///
/// This trait allows developers to implement custom audio loading logic
/// for different formats or decoders. PetalSonic provides a default implementation
/// using Symphonia, but users can bring their own loaders for specialized formats
/// or requirements.
///
/// # Example
///
/// ```ignore
/// use petalsonic_core::audio_data::{AudioDataLoader, LoadOptions, PetalSonicAudioData};
/// use petalsonic_core::error::Result;
/// use std::sync::Arc;
///
/// struct MyCustomLoader;
///
/// impl AudioDataLoader for MyCustomLoader {
///     fn load(&self, path: &str, options: &LoadOptions) -> Result<Arc<PetalSonicAudioData>> {
///         // Your custom loading logic here
///         todo!()
///     }
/// }
/// ```
pub trait AudioDataLoader {
    /// Loads audio data from a file path.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the audio file to load
    /// * `options` - Loading options that control behavior like mono conversion
    ///
    /// # Returns
    ///
    /// Returns an `Arc<PetalSonicAudioData>` containing the decoded audio on success.
    ///
    /// # Errors
    ///
    /// Returns a `PetalSonicError` if the file cannot be loaded or decoded.
    fn load(&self, path: &str, options: &LoadOptions) -> Result<Arc<PetalSonicAudioData>>;
}
