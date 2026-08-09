// Mixer module - handles mixing of audio sources
// This contains the mixing logic for both spatial and non-spatial sources

use crate::playback::{LoopMode, PlayState, PlaybackInstance};
use crate::spatial::{SpatialProcessingMetrics, SpatialProcessingSummary, SpatialProcessor};
use crate::world::SourceId;
use crate::{BusParams, gain};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Result of mixing - contains both the number of frames and loop events
pub struct MixResult {
    pub completed_playbacks: Vec<CompletedPlayback>,
    pub looped_sources: Vec<SourceId>,
}

#[derive(Clone, Copy, Debug)]
pub struct CompletedPlayback {
    pub voice_id: SourceId,
    pub emitter: crate::domain::Emitter,
    pub completion_tag: Option<crate::domain::PlaybackTag>,
}

/// Per-frame timing breakdown emitted by the mixer.
#[derive(Debug, Default, Clone, Copy)]
pub struct MixProfilingSummary {
    pub direct_mix_time_us: u64,
    pub spatial_mix_time_us: u64,
    pub spatial_metrics: Option<SpatialProcessingMetrics>,
}

/// Mixes all active voices and returns completion plus bounded timing summaries.
pub fn mix_playback_instances_with_metrics(
    world_buffer: &mut [f32],
    channels: u16,
    active_playback: &Arc<Mutex<HashMap<SourceId, PlaybackInstance>>>,
    spatial_processor: Option<&mut SpatialProcessor>,
    buses: &[BusParams],
) -> (MixResult, MixProfilingSummary) {
    let Ok(mut active_playback) = active_playback.try_lock() else {
        log::debug!("Failed to acquire active playback lock in mixer");
        return (
            MixResult {
                completed_playbacks: Vec::new(),
                looped_sources: Vec::new(),
            },
            MixProfilingSummary::default(),
        );
    };

    // Separate spatial and non-spatial sources FIRST
    let mut spatial_instances = Vec::new();
    let mut non_spatial_instances = Vec::new();

    log::debug!(
        "Mixer: Starting mix with {} active sources",
        active_playback.len()
    );

    let output_frames = world_buffer.len() / channels.max(1) as usize;
    for (source_id, instance) in active_playback.iter_mut() {
        // Only process playing instances
        if !matches!(instance.info.play_state, PlayState::Playing) {
            log::debug!(
                "Mixer: Skipping source {} - not playing (state: {:?})",
                source_id,
                instance.info.play_state
            );
            continue;
        }

        let bus = effective_bus_params(instance.bus_index, buses);
        if bus.paused {
            continue;
        }
        instance.set_mix_parameters(bus);
        if bus.muted || gain::db_to_linear(bus.gain_db) == 0.0 {
            instance.advance_silently(output_frames);
            continue;
        }

        if instance.config.is_spatial() {
            spatial_instances.push((*source_id, instance as &mut PlaybackInstance));
        } else {
            non_spatial_instances.push(instance);
        }
    }

    let mut profiling = MixProfilingSummary::default();

    // Process non-spatial sources first
    let direct_start = Instant::now();
    for instance in non_spatial_instances {
        instance.fill_buffer(world_buffer, channels);
    }
    profiling.direct_mix_time_us = direct_start.elapsed().as_micros() as u64;

    // Process spatial sources if spatial processor is available
    if let Some(processor) = spatial_processor {
        if !spatial_instances.is_empty() {
            let spatial_start = Instant::now();
            match processor
                .process_spatial_sources_with_metrics(&mut spatial_instances, world_buffer)
            {
                Ok(SpatialProcessingSummary {
                    frames_processed: _,
                    metrics,
                }) => {
                    profiling.spatial_mix_time_us = spatial_start.elapsed().as_micros() as u64;
                    profiling.spatial_metrics = Some(metrics);
                }
                Err(e) => {
                    log::error!("Error processing spatial sources: {}", e);
                }
            }
        }
    } else if !spatial_instances.is_empty() {
        for (_, instance) in spatial_instances {
            instance.advance_silently(output_frames);
        }
    }

    track_mix_peak(world_buffer);

    // NOW check for sources that reached the end during this mix iteration
    // This must happen AFTER fill_buffer() has been called on all sources
    let mut completed_playbacks = Vec::new();
    let mut looped_sources = Vec::new();

    log::debug!("Mixer: Checking for completed/looped sources...");

    for (source_id, instance) in active_playback.iter_mut() {
        log::debug!(
            "Mixer: Checking source {} - reached_end_flag: {}, state: {:?}",
            source_id,
            instance.reached_end_this_iteration,
            instance.info.play_state
        );

        if let Some(loop_mode) = instance.check_and_clear_end_flag() {
            log::debug!(
                "Mixer: Source {} reached end with loop mode: {:?}",
                source_id,
                loop_mode
            );
            match loop_mode {
                LoopMode::Once => {
                    completed_playbacks.push(CompletedPlayback {
                        voice_id: *source_id,
                        emitter: instance.emitter,
                        completion_tag: instance.completion_tag,
                    });
                }
                LoopMode::Infinite => {
                    // No longer need to restart - wraparound already handled in fill_buffer
                    looped_sources.push(*source_id);
                }
            }
        }
    }

    // Only remove instances that are actually finished (stopped playing)
    // Infinite looping sources wrap around automatically, so they keep playing
    let removed_count = active_playback.len();
    active_playback.retain(|_, instance| !instance.info.is_finished());
    let removed = removed_count - active_playback.len();
    if removed > 0 {
        log::debug!(
            "Mixer: Removed {} finished sources from active playback",
            removed
        );
    }

    (
        MixResult {
            completed_playbacks,
            looped_sources,
        },
        profiling,
    )
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

fn track_mix_peak(world_buffer: &[f32]) {
    let mut block_peak = 0.0f32;

    for sample in world_buffer {
        let abs = sample.abs();
        if abs > block_peak {
            block_peak = abs;
        }
    }

    update_global_peak(block_peak);
}

fn update_global_peak(block_peak: f32) {
    if !block_peak.is_finite() {
        return;
    }

    let global_peak = global_peak_amplitude();
    let mut current_bits = global_peak.load(Ordering::Relaxed);

    loop {
        let current_peak = f32::from_bits(current_bits);
        if block_peak <= current_peak {
            return;
        }

        match global_peak.compare_exchange_weak(
            current_bits,
            block_peak.to_bits(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => {
                return;
            }
            Err(observed_bits) => {
                current_bits = observed_bits;
            }
        }
    }
}

fn global_peak_amplitude() -> &'static AtomicU32 {
    static PEAK: OnceLock<AtomicU32> = OnceLock::new();
    PEAK.get_or_init(|| AtomicU32::new(0.0f32.to_bits()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
