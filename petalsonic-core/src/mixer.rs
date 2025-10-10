// Mixer module - handles mixing of audio sources
// This will contain the mixing logic extracted from engine.rs

use crate::playback::PlaybackInstance;
use crate::world::SourceId;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Mix all active playback instances into the buffer
/// Returns the number of frames filled
pub fn mix_playback_instances(
    world_buffer: &mut [f32],
    channels: u16,
    active_playback: &Arc<Mutex<HashMap<SourceId, PlaybackInstance>>>,
) -> usize {
    let Ok(mut active_playback) = active_playback.try_lock() else {
        log::warn!("Failed to acquire active playback lock in mixer");
        return 0;
    };

    // Only keep the instances that are not finished
    active_playback.retain(|_, instance| !instance.info.is_finished());

    let mut frames_filled_max = 0;
    for instance in active_playback.values_mut() {
        // Check if this is a spatial or non-spatial source
        if instance.config.is_spatial() {
            // Spatial sources: output silence for now (placeholder for Stage 2)
            // In Stage 2, this will be handled by the spatial processor
            continue;
        } else {
            // Non-spatial sources: use current mixing logic
            let frames_filled = instance.fill_buffer(world_buffer, channels);
            frames_filled_max = frames_filled_max.max(frames_filled);
        }
    }

    frames_filled_max
}
