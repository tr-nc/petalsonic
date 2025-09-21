//! World API for PetalSonic

use crate::config::PetalSonicConfig;
use crate::error::Result;
use crate::events::PetalSonicEvent;
use crate::math::{Pose, Vec3};

pub struct PetalSonicWorld {
    config: PetalSonicConfig,
}

impl PetalSonicWorld {
    pub fn new(config: PetalSonicConfig) -> Result<Self> {
        Ok(Self { config })
    }

    pub fn start(&mut self) -> Result<()> {
        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        Ok(())
    }

    pub fn poll_events(&mut self) -> Vec<PetalSonicEvent> {
        Vec::new()
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
