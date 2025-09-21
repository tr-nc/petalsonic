//! World API for PetalSonic

use crate::audio::PetalSonicAudioData;
use crate::config::PetalSonicConfig;
use crate::error::Result;
use crate::events::PetalSonicEvent;
use crate::math::{Pose, Vec3};
use std::sync::Arc;

pub struct PetalSonicWorld {
    config: PetalSonicConfig,
}

impl PetalSonicWorld {
    pub fn new(config: PetalSonicConfig) -> Result<Self> {
        Ok(Self { config })
    }

    pub fn start(&mut self) -> Result<()> {
        println!("World started - ready for audio playback");
        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        Ok(())
    }

    pub fn poll_events(&mut self) -> Vec<PetalSonicEvent> {
        Vec::new()
    }

    /// Add an audio source and play it immediately (basic implementation)
    pub fn add_source(&mut self, audio_data: Arc<PetalSonicAudioData>) -> Result<u64> {
        println!("🎵 Playing audio source directly (non-spatial)...");

        // Get samples from the first channel
        let samples = audio_data.channel_samples(0)?;
        let sample_rate = audio_data.sample_rate();

        println!("  - Samples: {}", samples.len());
        println!("  - Sample rate: {} Hz", sample_rate);
        println!("  - Duration: {:?}", audio_data.duration());

        // For now, we'll use a simple approach - play the audio directly
        // In a real implementation, this would be queued for the audio thread
        #[cfg(test)]
        {
            use crate::test_audio_playback::play_audio_samples;
            match play_audio_samples(samples.to_vec(), sample_rate) {
                Ok(()) => println!("✓ Audio playback completed successfully"),
                Err(e) => println!("✗ Audio playback failed: {}", e),
            }
        }

        // Return a dummy source ID
        Ok(1)
    }
}

pub struct PetalSonicAudioSource {
    pub(crate) id: u64,
    pub(crate) position: Vec3,
    pub(crate) volume: f32,
}

impl PetalSonicAudioSource {
    pub fn position(&self) -> Vec3 {
        self.position
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }
}

pub struct PetalSonicAudioListener {
    pub(crate) pose: Pose,
}

impl PetalSonicAudioListener {
    pub fn pose(&self) -> Pose {
        self.pose
    }
}
