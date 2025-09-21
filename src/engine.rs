//! Audio engine for PetalSonic

use crate::config::PetalSonicConfig;
use crate::error::Result;

pub struct AudioEngine {
    config: PetalSonicConfig,
}

impl AudioEngine {
    pub fn new(config: PetalSonicConfig) -> Result<Self> {
        Ok(Self { config })
    }

    pub fn start(&mut self) -> Result<()> {
        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}
