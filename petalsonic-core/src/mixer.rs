// Mixer module - handles mixing of audio sources
// This contains the mixing logic for both spatial and non-spatial sources

use crate::playback::{PlayState, PlaybackInstance};
use crate::spatial::SpatialProcessor;
use crate::world::SourceId;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Mix all active playback instances into the buffer
/// Returns the number of frames filled
///
/// # Arguments
/// * `world_buffer` - Output buffer to fill with mixed audio
/// * `channels` - Number of audio channels (typically 2 for stereo)
/// * `active_playback` - Map of active playback instances
/// * `spatial_processor` - Optional spatial processor for 3D audio
pub fn mix_playback_instances(
    world_buffer: &mut [f32],
    channels: u16,
    active_playback: &Arc<Mutex<HashMap<SourceId, PlaybackInstance>>>,
    spatial_processor: Option<&mut SpatialProcessor>,
) -> usize {
    let Ok(mut active_playback) = active_playback.try_lock() else {
        log::warn!("Failed to acquire active playback lock in mixer");
        return 0;
    };

    // Only keep the instances that are not finished
    active_playback.retain(|_, instance| !instance.info.is_finished());

    // Separate spatial and non-spatial sources
    let mut spatial_instances = Vec::new();
    let mut non_spatial_instances = Vec::new();

    for (source_id, instance) in active_playback.iter_mut() {
        // Only process playing instances
        if !matches!(instance.info.play_state, PlayState::Playing) {
            continue;
        }

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

    frames_filled_max
}
