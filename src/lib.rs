//! PetalSonic - Spatial Audio Library
//!
//! A real-time safe spatial audio library using Steam Audio (audionimbus) for spatialization.

pub mod audio;
pub mod config;
pub mod engine;
pub mod error;
pub mod events;
pub mod math;
pub mod world;

pub use config::PetalSonicConfig;
pub use error::PetalSonicError;
pub use events::PetalSonicEvent;
pub use world::{PetalSonicAudioListener, PetalSonicAudioSource, PetalSonicWorld};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
