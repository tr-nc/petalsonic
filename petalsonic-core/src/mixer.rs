// Mixer module - handles mixing of audio sources
// This contains the mixing logic for both spatial and non-spatial sources

use crate::playback::{LoopMode, PlayState, PlaybackInstance};
use crate::spatial::SpatialProcessor;
use crate::world::SourceId;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Result of mixing - contains both the number of frames and loop events
pub struct MixResult {
    pub frames_filled: usize,
    pub completed_sources: Vec<SourceId>,
    pub looped_sources: Vec<SourceId>,
}

/// Mix all active playback instances into the buffer
/// Returns MixResult containing:
/// - The number of frames filled
/// - Vector of source IDs that completed (LoopMode::Once finished)
/// - Vector of source IDs that looped (LoopMode::Infinite completed one iteration)
///
/// # Arguments
/// * `world_buffer` - Output buffer to fill with mixed audio
/// * `channels` - Number of audio channels (typically 2 for stereo)
/// * `active_playback` - Map of active playback instances
/// * `spatial_processor` - Optional spatial processor for 3D audio
///
/// # Loop Event Detection
///
/// All loop modes emit events when reaching the end of playback:
/// - `LoopMode::Once`: Emits `SourceCompleted`, stops playing, removed from active_playback
/// - `LoopMode::Infinite`: Emits `SourceLooped`, continues playing (loops automatically)
pub fn mix_playback_instances(
    world_buffer: &mut [f32],
    channels: u16,
    active_playback: &Arc<Mutex<HashMap<SourceId, PlaybackInstance>>>,
    spatial_processor: Option<&mut SpatialProcessor>,
) -> MixResult {
    let Ok(mut active_playback) = active_playback.try_lock() else {
        log::warn!("Failed to acquire active playback lock in mixer");
        return MixResult {
            frames_filled: 0,
            completed_sources: Vec::new(),
            looped_sources: Vec::new(),
        };
    };

    // Separate spatial and non-spatial sources FIRST
    let mut spatial_instances = Vec::new();
    let mut non_spatial_instances = Vec::new();

    log::debug!(
        "Mixer: Starting mix with {} active sources",
        active_playback.len()
    );

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

        log::debug!(
            "Mixer: Processing source {} - frame {}/{} (spatial: {})",
            source_id,
            instance.info.current_frame,
            instance.audio_data.samples().len(),
            instance.config.is_spatial()
        );

        if instance.config.is_spatial() {
            spatial_instances.push((*source_id, instance as &mut PlaybackInstance));
        } else {
            non_spatial_instances.push(instance);
        }
    }

    let mut frames_filled_max = 0;

    // Process non-spatial sources first
    for instance in non_spatial_instances {
        let frames_filled = instance.fill_buffer(world_buffer, channels);
        frames_filled_max = frames_filled_max.max(frames_filled);
    }

    // Process spatial sources if spatial processor is available
    if let Some(processor) = spatial_processor {
        if !spatial_instances.is_empty() {
            match processor.process_spatial_sources(&mut spatial_instances, world_buffer) {
                Ok(frames_filled) => {
                    frames_filled_max = frames_filled_max.max(frames_filled);
                }
                Err(e) => {
                    log::error!("Error processing spatial sources: {}", e);
                }
            }
        }
    } else if !spatial_instances.is_empty() {
        log::warn!(
            "Spatial processor not available, {} spatial sources will be silent",
            spatial_instances.len()
        );
    }

    // NOW check for sources that reached the end during this mix iteration
    // This must happen AFTER fill_buffer() has been called on all sources
    let mut completed_sources = Vec::new();
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
                    // Source finished - will be removed and emit SourceCompleted
                    log::info!(
                        "Mixer: Source {} completed (Once mode), will be removed",
                        source_id
                    );
                    completed_sources.push(*source_id);
                }
                LoopMode::Infinite => {
                    // Source reached end - explicitly restart from beginning
                    log::info!(
                        "Mixer: Source {} reached end (Infinite mode), restarting from beginning",
                        source_id
                    );
                    instance.play_from_beginning();
                    looped_sources.push(*source_id);
                }
            }
        }
    }

    // Only remove instances that are actually finished (stopped playing)
    // Infinite looping sources were explicitly restarted, so they keep playing
    let removed_count = active_playback.len();
    active_playback.retain(|_, instance| !instance.info.is_finished());
    let removed = removed_count - active_playback.len();
    if removed > 0 {
        log::debug!(
            "Mixer: Removed {} finished sources from active playback",
            removed
        );
    }

    MixResult {
        frames_filled: frames_filled_max,
        completed_sources,
        looped_sources,
    }
}
