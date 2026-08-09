use crate::error::{PetalSonicError, Result};
use crate::world::SourceId;
use audionimbus::{
    AmbisonicsEncodeEffect, AmbisonicsEncodeEffectSettings, AudioSettings, BinauralEffect,
    BinauralEffectSettings, Context, Convolution, CustomRayTracer, Direct, DirectEffect,
    DirectEffectSettings, Hrtf, ReflectionEffect, ReflectionEffectSettings, Reflections,
    SimulationFlags, Simulator, Source, SourceSettings, num_ambisonics_channels,
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
    /// Steam Audio direct binaural effect used when Ambisonics is disabled.
    pub binaural_effect: Option<BinauralEffect>,
}

unsafe impl Send for SpatialSourceEffects {}

impl SpatialSourceEffects {
    /// Create effects for a new spatial source
    pub fn new(
        context: &Context,
        simulator: &SpatialSimulator,
        audio_settings: &AudioSettings,
        hrtf: Option<&Hrtf>,
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
                impulse_response_size: audio_settings.sampling_rate
                    * REFLECTION_IR_DURATION_SECONDS,
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
            source,
            direct_effect,
            reflection_effect,
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
        simulator: &mut SpatialSimulator,
        audio_settings: &AudioSettings,
        hrtf: Option<&Hrtf>,
    ) -> Result<()> {
        let effects = SpatialSourceEffects::new(context, simulator, audio_settings, hrtf)?;

        // Add source to simulator
        simulator.add_source(&effects.source);

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
}
