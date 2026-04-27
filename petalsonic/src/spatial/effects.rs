use crate::error::{PetalSonicError, Result};
use crate::world::SourceId;
use audionimbus::{
    num_ambisonics_channels, AmbisonicsEncodeEffect, AmbisonicsEncodeEffectSettings,
    AudioSettings, Context, Convolution, CustomRayTracer, Direct, DirectEffect,
    DirectEffectSettings, ReflectionEffect, ReflectionEffectSettings, Reflections,
    SimulationFlags, Simulator, Source, SourceSettings,
};
use std::collections::HashMap;

const REFLECTIONS_ORDER: u32 = 2;
const REFLECTION_IR_DURATION_SECONDS: u32 = 2;

type SpatialSimulator = Simulator<'static, CustomRayTracer, Direct, Reflections>;
type SpatialSource = Source<'static, Direct, Reflections>;

/// Per-source spatial effects (DirectEffect + AmbisonicsEncodeEffect)
pub struct SpatialSourceEffects {
    /// Steam Audio source object for simulation
    pub source: SpatialSource,
    /// Direct effect (distance attenuation, air absorption)
    pub direct_effect: DirectEffect,
    /// Reflection effect (ambisonics reverb / early reflections)
    pub reflection_effect: ReflectionEffect<Convolution>,
    /// Ambisonics encode effect (spatial encoding)
    pub ambisonics_encode_effect: AmbisonicsEncodeEffect,
}

unsafe impl Send for SpatialSourceEffects {}

impl SpatialSourceEffects {
    /// Create effects for a new spatial source
    pub fn new(
        context: &Context,
        simulator: &SpatialSimulator,
        audio_settings: &AudioSettings,
    ) -> Result<Self> {
        let source: SpatialSource = Source::try_new(
            simulator,
            &SourceSettings {
                flags: SimulationFlags::DIRECT | SimulationFlags::REFLECTIONS,
            },
        )
        .map_err(|e| PetalSonicError::SpatialAudio(format!("Failed to create source: {}", e)))?;

        let direct_effect = DirectEffect::try_new(
            context,
            audio_settings,
            &DirectEffectSettings { num_channels: 1 }, // Mono input
        )
        .map_err(|e| {
            PetalSonicError::SpatialAudio(format!("Failed to create DirectEffect: {}", e))
        })?;

        let reflection_effect = ReflectionEffect::<Convolution>::try_new(
            context,
            audio_settings,
            &ReflectionEffectSettings {
                impulse_response_size: audio_settings.sampling_rate * REFLECTION_IR_DURATION_SECONDS,
                num_channels: num_ambisonics_channels(REFLECTIONS_ORDER),
            },
        )
        .map_err(|e| {
            PetalSonicError::SpatialAudio(format!("Failed to create ReflectionEffect: {}", e))
        })?;

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

        Ok(Self {
            source,
            direct_effect,
            reflection_effect,
            ambisonics_encode_effect,
        })
    }
}

/// Manages spatial effects for all active spatial sources
pub struct SpatialEffectsManager {
    effects: HashMap<SourceId, SpatialSourceEffects>,
}

impl SpatialEffectsManager {
    pub fn new() -> Self {
        Self {
            effects: HashMap::new(),
        }
    }

    /// Create effects for a spatial source
    pub fn create_effects_for_source(
        &mut self,
        source_id: SourceId,
        context: &Context,
        simulator: &mut SpatialSimulator,
        audio_settings: &AudioSettings,
    ) -> Result<()> {
        if self.effects.contains_key(&source_id) {
            log::warn!("Effects for source {} already exist, replacing", source_id);
        }

        let effects = SpatialSourceEffects::new(context, simulator, audio_settings)?;

        // Add source to simulator
        simulator.add_source(&effects.source);

        self.effects.insert(source_id, effects);
        log::debug!("Created spatial effects for source {}", source_id);
        Ok(())
    }

    /// Remove effects for a spatial source
    pub fn remove_effects_for_source(&mut self, source_id: SourceId) {
        if self.effects.remove(&source_id).is_some() {
            log::debug!("Removed spatial effects for source {}", source_id);
        }
    }

    /// Get effects for a source
    #[allow(dead_code)]
    pub fn get_effects(&self, source_id: SourceId) -> Option<&SpatialSourceEffects> {
        self.effects.get(&source_id)
    }

    /// Get mutable effects for a source
    pub fn get_effects_mut(&mut self, source_id: SourceId) -> Option<&mut SpatialSourceEffects> {
        self.effects.get_mut(&source_id)
    }

    /// Check if effects exist for a source
    pub fn has_effects(&self, source_id: SourceId) -> bool {
        self.effects.contains_key(&source_id)
    }

    /// Clear all effects
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.effects.clear();
        log::debug!("Cleared all spatial effects");
    }
}
