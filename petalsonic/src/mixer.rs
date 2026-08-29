// Mixer module - handles mixing of audio sources
// This contains the mixing logic for both spatial and non-spatial sources

use crate::domain::VoiceId;
use crate::events::VoiceTelemetryEvent;
use crate::playback::{PlayState, PlaybackInstance};
use crate::spatial::{SpatialProcessingMetrics, SpatialProcessor, SpatialRenderContext};
use crate::{BusParams, gain};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Clone, Copy, Debug)]
pub struct CompletedPlayback {
    pub voice_id: VoiceId,
    pub emitter: crate::domain::Emitter,
    pub completion_tag: Option<crate::domain::PlaybackTag>,
}

/// Reusable identity lists for one render quantum.
pub struct MixerScratch {
    spatial_voice_ids: Vec<VoiceId>,
    muted_spatial_voice_ids: Vec<VoiceId>,
    non_spatial_voice_ids: Vec<VoiceId>,
    voice_telemetry: Vec<VoiceTelemetryEvent>,
}

impl MixerScratch {
    pub fn new(max_voices: usize) -> Self {
        Self {
            spatial_voice_ids: Vec::with_capacity(max_voices),
            muted_spatial_voice_ids: Vec::with_capacity(max_voices),
            non_spatial_voice_ids: Vec::with_capacity(max_voices),
            voice_telemetry: Vec::with_capacity(max_voices.saturating_mul(4)),
        }
    }

    pub(crate) fn drain_voice_telemetry(
        &mut self,
    ) -> impl Iterator<Item = VoiceTelemetryEvent> + '_ {
        self.voice_telemetry.drain(..)
    }
}

/// Per-frame timing breakdown emitted by the mixer.
#[derive(Debug, Default, Clone, Copy)]
pub struct MixProfilingSummary {
    pub direct_mix_time_us: u64,
    pub spatial_mix_time_us: u64,
    pub spatial_metrics: Option<SpatialProcessingMetrics>,
}

/// Mixes all active voices and returns completion plus bounded timing summaries.
#[allow(clippy::too_many_arguments)] // Explicit borrowed render state keeps this hot path allocation-free.
pub fn mix_playback_instances_with_metrics(
    world_buffer: &mut [f32],
    channels: u16,
    active_playback: &Arc<Mutex<HashMap<VoiceId, PlaybackInstance>>>,
    spatial_processor: Option<&mut SpatialProcessor>,
    buses: &[BusParams],
    render_context: SpatialRenderContext,
    scratch: &mut MixerScratch,
    completed_playbacks: &mut Vec<CompletedPlayback>,
) -> MixProfilingSummary {
    let Ok(mut active_playback) = active_playback.try_lock() else {
        return MixProfilingSummary::default();
    };

    scratch.spatial_voice_ids.clear();
    scratch.muted_spatial_voice_ids.clear();
    scratch.non_spatial_voice_ids.clear();
    scratch.voice_telemetry.clear();

    let output_frames = world_buffer.len() / channels.max(1) as usize;
    for (voice_id, instance) in active_playback.iter_mut() {
        // Only process playing instances
        if !matches!(instance.info.play_state, PlayState::Playing) {
            continue;
        }

        let bus = effective_bus_params(instance.bus_index, buses);
        if bus.paused {
            continue;
        }
        instance.set_mix_parameters(bus);
        let is_spatial = instance.config.is_spatial();
        if bus.muted || gain::db_to_linear(bus.gain_db) == 0.0 {
            instance.advance_silently(output_frames);
            if is_spatial {
                scratch.muted_spatial_voice_ids.push(*voice_id);
            }
            continue;
        }

        if is_spatial {
            scratch.spatial_voice_ids.push(*voice_id);
        } else {
            scratch.non_spatial_voice_ids.push(*voice_id);
        }
    }

    let mut profiling = MixProfilingSummary::default();

    // Process non-spatial sources first
    let direct_start = Instant::now();
    for voice_id in &scratch.non_spatial_voice_ids {
        if let Some(instance) = active_playback.get_mut(voice_id) {
            instance.fill_buffer(world_buffer, channels);
        }
    }
    profiling.direct_mix_time_us = direct_start.elapsed().as_micros() as u64;

    // Process spatial sources if spatial processor is available
    if let Some(processor) = spatial_processor {
        for voice_id in &scratch.muted_spatial_voice_ids {
            processor.silence_voice_state(*voice_id);
        }
        if !scratch.spatial_voice_ids.is_empty()
            || processor.has_environment_tail()
            || processor.has_pending_voice_telemetry()
        {
            let spatial_start = Instant::now();
            if let Ok(metrics) = processor.process_spatial_sources_with_metrics(
                &scratch.spatial_voice_ids,
                &mut active_playback,
                world_buffer,
                render_context,
                &mut scratch.voice_telemetry,
            ) {
                profiling.spatial_mix_time_us = spatial_start.elapsed().as_micros() as u64;
                profiling.spatial_metrics = Some(metrics);
            }
        }
    } else if !scratch.spatial_voice_ids.is_empty() {
        for voice_id in &scratch.spatial_voice_ids {
            if let Some(instance) = active_playback.get_mut(voice_id) {
                instance.advance_silently(output_frames);
            }
        }
    }

    // Reclaim only after every source has completed this quantum. Explicit stops
    // clear their completion tag before entering the de-click ramp.
    for (voice_id, instance) in active_playback.iter_mut() {
        let _ = instance.check_and_clear_end_flag();
        if instance.should_reclaim() {
            completed_playbacks.push(CompletedPlayback {
                voice_id: *voice_id,
                emitter: instance.emitter,
                completion_tag: instance.completion_tag,
            });
        }
    }

    active_playback.retain(|_, instance| !instance.should_reclaim());

    profiling
}

pub(crate) fn effective_bus_params(index: usize, buses: &[BusParams]) -> BusParams {
    let master = buses.first().copied().unwrap_or_default();
    let selected = buses.get(index).copied().unwrap_or(master);
    if index == 0 {
        return master;
    }
    BusParams {
        gain_db: master.gain_db + selected.gain_db,
        muted: master.muted || selected.muted,
        paused: master.paused || selected.paused,
        playback_rate: master.playback_rate * selected.playback_rate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_data::PetalSonicAudioData;
    use crate::config::SourceConfig;
    use crate::domain::Emitter;
    use crate::playback::{LoopMode, VoiceStart};
    use std::time::Duration;

    #[test]
    fn named_bus_controls_compose_directly_with_master() {
        let buses = [
            BusParams {
                gain_db: -3.0,
                playback_rate: 0.5,
                ..BusParams::default()
            },
            BusParams {
                gain_db: -6.0,
                muted: true,
                playback_rate: 2.0,
                ..BusParams::default()
            },
        ];

        let effective = effective_bus_params(1, &buses);
        assert_eq!(effective.gain_db, -9.0);
        assert!(effective.muted);
        assert!(!effective.paused);
        assert_eq!(effective.playback_rate, 1.0);
    }

    #[test]
    fn paused_gameplay_freezes_while_music_keeps_rendering() {
        let active = Arc::new(Mutex::new(HashMap::new()));
        for (id, bus_index, sample) in [(1, 1, 1.0), (2, 2, 0.5)] {
            let audio = Arc::new(PetalSonicAudioData::new(
                vec![sample; 16],
                48_000,
                1,
                Duration::from_secs_f64(16.0 / 48_000.0),
            ));
            let mut voice = PlaybackInstance::from_voice(VoiceStart {
                emitter: Emitter {
                    world_id: 1,
                    index: id,
                    generation: 1,
                },
                audio_data: audio,
                config: SourceConfig::non_spatial(),
                loop_mode: LoopMode::Infinite,
                bus_index,
                playback_rate: 1.0,
                detached: false,
                completion_tag: None,
                direct_path: crate::domain::DirectPath::default(),
                environment_send: crate::domain::EnvironmentSend::default(),
                play_command_id: None,
                source_extent: crate::domain::SourceExtent::Point,
                occlusion_profile: crate::domain::OcclusionProfile::PointExact,
                mono_scratch: vec![0.0; 4],
            });
            voice.play_from_beginning();
            active
                .lock()
                .unwrap()
                .insert(VoiceId::from(id as u64), voice);
        }

        let buses = [
            BusParams::default(),
            BusParams {
                paused: true,
                ..BusParams::default()
            },
            BusParams::default(),
        ];
        let mut output = [0.0; 8];
        let mut scratch = MixerScratch::new(2);
        let mut completed = Vec::with_capacity(2);
        mix_playback_instances_with_metrics(
            &mut output,
            2,
            &active,
            None,
            &buses,
            SpatialRenderContext::default(),
            &mut scratch,
            &mut completed,
        );

        assert_eq!(output, [0.5; 8]);
        let active = active.lock().unwrap();
        assert_eq!(active[&VoiceId::from(1)].info.current_frame, 0);
        assert_eq!(active[&VoiceId::from(2)].info.current_frame, 4);
    }

    #[test]
    fn playback_rate_only_changes_the_selected_bus() {
        let active = Arc::new(Mutex::new(HashMap::new()));
        for (id, bus_index) in [(1, 1), (2, 2)] {
            let audio = Arc::new(PetalSonicAudioData::new(
                (0..32).map(|sample| sample as f32).collect(),
                48_000,
                1,
                Duration::from_secs_f64(32.0 / 48_000.0),
            ));
            let mut voice = PlaybackInstance::from_voice(VoiceStart {
                emitter: Emitter {
                    world_id: 1,
                    index: id,
                    generation: 1,
                },
                audio_data: audio,
                config: SourceConfig::non_spatial(),
                loop_mode: LoopMode::Infinite,
                bus_index,
                playback_rate: 1.0,
                detached: false,
                completion_tag: None,
                direct_path: crate::domain::DirectPath::default(),
                environment_send: crate::domain::EnvironmentSend::default(),
                play_command_id: None,
                source_extent: crate::domain::SourceExtent::Point,
                occlusion_profile: crate::domain::OcclusionProfile::PointExact,
                mono_scratch: vec![0.0; 4],
            });
            voice.play_from_beginning();
            active
                .lock()
                .unwrap()
                .insert(VoiceId::from(id as u64), voice);
        }
        let buses = [
            BusParams::default(),
            BusParams {
                playback_rate: 0.5,
                ..BusParams::default()
            },
            BusParams::default(),
        ];
        let mut output = [0.0; 8];
        let mut scratch = MixerScratch::new(2);
        let mut completed = Vec::with_capacity(2);
        mix_playback_instances_with_metrics(
            &mut output,
            2,
            &active,
            None,
            &buses,
            SpatialRenderContext::default(),
            &mut scratch,
            &mut completed,
        );

        let active = active.lock().unwrap();
        assert_eq!(active[&VoiceId::from(1)].info.current_frame, 2);
        assert_eq!(active[&VoiceId::from(2)].info.current_frame, 4);
    }
}
