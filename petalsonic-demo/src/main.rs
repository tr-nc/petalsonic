use anyhow::Result;
use petalsonic_core::audio_data::PetalSonicAudioData;
use petalsonic_core::config::PetalSonicWorldDesc;
use petalsonic_core::engine::PetalSonicEngine;
use petalsonic_core::world::PetalSonicWorld;
use std::sync::Arc;

fn main() {
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .init();

    test_new_playback_system().unwrap();
}

fn test_new_playback_system() -> Result<()> {
    let wav_path = "res/cicada_test_96k.wav";

    let world_desc = PetalSonicWorldDesc {
        sample_rate: 44100,
        block_size: 1024,
        enable_spatialization: false,
        ..Default::default()
    };

    log::info!("Creating world and loading audio file: {}", wav_path);
    let mut world =
        PetalSonicWorld::new(world_desc.clone()).expect("Failed to create PetalSonicWorld");

    // Load audio file using the new API
    let audio_data = PetalSonicAudioData::from_path(wav_path)?;
    let audio_id = world.add_source(audio_data)?;
    log::info!("Audio loaded with ID: {}", audio_id);

    // Create engine with the world
    let world_arc = Arc::new(world);
    let mut engine =
        PetalSonicEngine::new(world_desc, world_arc.clone()).expect("Failed to create engine");

    // Start the engine
    match engine.start() {
        Ok(_) => {
            log::info!("Audio engine started");

            // Now use the simple API to play audio
            log::info!("Starting playback...");
            world_arc.play(audio_id)?;

            // Let it play for a few seconds
            std::thread::sleep(std::time::Duration::from_secs(3));

            // Pause playback
            log::info!("Pausing playback...");
            world_arc.pause(audio_id)?;
            std::thread::sleep(std::time::Duration::from_secs(1));

            // Resume playback
            log::info!("Resuming playback...");
            world_arc.play(audio_id)?;
            std::thread::sleep(std::time::Duration::from_secs(2));

            // Stop playback
            log::info!("Stopping playback...");
            world_arc.stop(audio_id)?;

            engine.stop().expect("Failed to stop engine");
            log::info!("Audio playback test completed successfully");
        }
        Err(e) => log::error!("Audio playback failed: {}", e),
    }

    Ok(())
}
