use crate::error::{PetalSonicError, Result};
use crate::world::SourceId;
use audionimbus::{
    AmbisonicsEncodeEffect, AmbisonicsEncodeEffectSettings, AudioSettings, BinauralEffect,
    BinauralEffectSettings, Context, Hrtf,
};
use std::collections::HashMap;

const REFLECTIONS_ORDER: u32 = 2;

/// Per-source Steam Audio effects that do not access host scene callbacks.
pub struct SpatialSourceEffects {
    /// Ambisonics encode effect (spatial encoding)
    pub ambisonics_encode_effect: AmbisonicsEncodeEffect,
    /// Steam Audio direct binaural effect used when Ambisonics is disabled.
    pub binaural_effect: Option<BinauralEffect>,
}

// SAFETY: the wrapped Steam Audio effects are never accessed concurrently. They
// are created and used by one render runtime, then moved only for destruction on
// the supervisor thread after removal from the render-owned map.
unsafe impl Send for SpatialSourceEffects {}

impl SpatialSourceEffects {
    /// Create effects for a new spatial source
    pub fn new(
        context: &Context,
        audio_settings: &AudioSettings,
        hrtf: Option<&Hrtf>,
    ) -> Result<Self> {
        let ambisonics_encode_effect = AmbisonicsEncodeEffect::try_new(
            context,
            audio_settings,
            &AmbisonicsEncodeEffectSettings {
                max_order: REFLECTIONS_ORDER,
            }, // Order 2 ambisonics (9 channels)
        )
        .map_err(|e| {
            PetalSonicError::SpatialAudio(format!("Failed to create AmbisonicsEncodeEffect: {}", e))
        })?;

        let binaural_effect = if let Some(hrtf) = hrtf {
            Some(
                BinauralEffect::try_new(context, audio_settings, &BinauralEffectSettings { hrtf })
                    .map_err(|e| {
                        PetalSonicError::SpatialAudio(format!(
                            "Failed to create BinauralEffect: {}",
                            e
                        ))
                    })?,
            )
        } else {
            None
        };

        Ok(Self {
            ambisonics_encode_effect,
            binaural_effect,
        })
    }
}

/// Manages spatial effects for all active spatial sources
pub struct SpatialEffectsManager {
    effects: HashMap<SourceId, SpatialSourceEffects>,
}

impl SpatialEffectsManager {
    pub fn new(max_sources: usize) -> Self {
        Self {
            effects: HashMap::with_capacity(max_sources),
        }
    }

    /// Create effects for a spatial source
    pub fn create_effects_for_source(
        &mut self,
        source_id: SourceId,
        context: &Context,
        audio_settings: &AudioSettings,
        hrtf: Option<&Hrtf>,
    ) -> Result<()> {
        let effects = SpatialSourceEffects::new(context, audio_settings, hrtf)?;
        self.effects.insert(source_id, effects);
        Ok(())
    }

    /// Get mutable effects for a source
    pub fn get_effects_mut(&mut self, source_id: SourceId) -> Option<&mut SpatialSourceEffects> {
        self.effects.get_mut(&source_id)
    }

    /// Check if effects exist for a source
    pub fn has_effects(&self, source_id: SourceId) -> bool {
        self.effects.contains_key(&source_id)
    }

    pub fn retire_source(&mut self, source_id: SourceId) -> Option<SpatialSourceEffects> {
        self.effects.remove(&source_id)
    }
}
