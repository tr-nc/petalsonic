use crate::error::{PetalSonicError, Result};
use audionimbus::{AudioSettings, Context, Hrtf, HrtfSettings, Sofa, VolumeNormalization};

/// Load HRTF with default settings
///
/// This uses Steam Audio's built-in default HRTF. In the future, this can be extended
/// to support custom SOFA files.
pub fn create_default_hrtf(context: &Context, audio_settings: &AudioSettings) -> Result<Hrtf> {
    let hrtf = Hrtf::try_new(
        context,
        audio_settings,
        &HrtfSettings {
            volume_normalization: VolumeNormalization::None,
            sofa_information: None, // Use default HRTF
            ..Default::default()
        },
    )
    .map_err(|e| PetalSonicError::SpatialAudio(format!("Failed to create HRTF: {}", e)))?;

    log::info!("Created default HRTF");
    Ok(hrtf)
}

/// Load HRTF from a custom SOFA file
///
/// # Arguments
/// * `context` - Steam Audio context
/// * `audio_settings` - Audio settings
/// * `sofa_path` - Path to the SOFA file
#[allow(dead_code)]
pub fn create_hrtf_from_file(
    context: &Context,
    audio_settings: &AudioSettings,
    sofa_path: &str,
) -> Result<Hrtf> {
    let hrtf_data = std::fs::read(sofa_path)
        .map_err(|e| PetalSonicError::SpatialAudio(format!("Failed to read HRTF file: {}", e)))?;

    let hrtf = Hrtf::try_new(
        context,
        audio_settings,
        &HrtfSettings {
            volume_normalization: VolumeNormalization::None,
            sofa_information: Some(Sofa::Buffer(hrtf_data)),
            ..Default::default()
        },
    )
    .map_err(|e| {
        PetalSonicError::SpatialAudio(format!("Failed to create HRTF from file: {}", e))
    })?;

    log::info!("Created HRTF from file: {}", sofa_path);
    Ok(hrtf)
}
