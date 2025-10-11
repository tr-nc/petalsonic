use anyhow::Result;
use petalsonic_core::SourceConfig;
use petalsonic_core::audio_data::PetalSonicAudioData;
use petalsonic_core::config::PetalSonicWorldDesc;
use petalsonic_core::engine::PetalSonicEngine;
use petalsonic_core::math::{Pose, Quat, Vec3};
use petalsonic_core::playback::LoopMode;
use petalsonic_core::world::PetalSonicWorld;
use std::sync::Arc;

pub fn run_cli_tests() {
    log::info!("=== Running Non-Spatial Audio Test ===");
    test_non_spatial_audio().unwrap();

    log::info!("\n=== Running Spatial Audio Test ===");
    test_spatial_audio().unwrap();
}

fn test_non_spatial_audio() -> Result<()> {
    let wav_path = "petalsonic-demo/asset/sound/cicada_test_96k.wav";

    let world_desc = PetalSonicWorldDesc {
        sample_rate: 48000,
        block_size: 1024,
        ..Default::default()
    };

    log::info!("Creating world and loading audio file: {}", wav_path);
    let mut world =
        PetalSonicWorld::new(world_desc.clone()).expect("Failed to create PetalSonicWorld");

    // Load audio file using the new API
    let audio_data = PetalSonicAudioData::from_path(wav_path)?;
    let audio_id = world.add_source(audio_data, SourceConfig::NonSpatial)?;
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
            world_arc.play(audio_id, LoopMode::Infinite)?;

            // Let it play for a few seconds
            std::thread::sleep(std::time::Duration::from_secs(30));

            // Pause playback
            log::info!("Pausing playback...");
            world_arc.pause(audio_id)?;
            std::thread::sleep(std::time::Duration::from_secs(1));

            // Resume playback
            log::info!("Resuming playback...");
            world_arc.play(audio_id, LoopMode::Once)?;
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

fn test_spatial_audio() -> Result<()> {
    let wav_path = "petalsonic-demo/asset/sound/cicada_test_96k.wav";

    let world_desc = PetalSonicWorldDesc {
        sample_rate: 48000,
        block_size: 1024,
        hrtf_path: Some("petalsonic-demo/asset/hrtf/hrtf_b_nh172.sofa".to_string()),
        ..Default::default()
    };

    log::info!("Creating world and loading audio file: {}", wav_path);
    let mut world =
        PetalSonicWorld::new(world_desc.clone()).expect("Failed to create PetalSonicWorld");

    // Set up listener pose at origin (0, 0, 0) looking at -Z with up being +Y
    let listener_pose = Pose::new(
        Vec3::new(0.0, 0.0, 0.0), // Position at origin
        Quat::IDENTITY,           // Looking at -Z (forward) by default
    );
    world.set_listener_pose(listener_pose);
    log::info!(
        "Listener pose set: position = {:?}, forward = {:?}, up = {:?}",
        listener_pose.position,
        listener_pose.forward(),
        listener_pose.up()
    );

    // Load audio file and create a spatial source at (0, 0, -1) - directly in front
    let audio_data = PetalSonicAudioData::from_path(wav_path)?;
    let spatial_position = Vec3::new(0.0, 0.0, -0.1);
    let audio_id = world.add_source(
        audio_data,
        SourceConfig::spatial_with_volume(spatial_position, 1.0),
    )?;
    log::info!(
        "Spatial audio loaded with ID: {} at position {:?}",
        audio_id,
        spatial_position
    );

    // Create engine with the world
    let world_arc = Arc::new(world);
    let mut engine =
        PetalSonicEngine::new(world_desc, world_arc.clone()).expect("Failed to create engine");

    // Start the engine
    match engine.start() {
        Ok(_) => {
            log::info!("Audio engine started with spatial audio");

            // Play the spatial audio source
            log::info!("Starting spatial playback...");
            world_arc.play(audio_id, LoopMode::Once)?;

            // Let it play for 5 seconds to hear the spatial effect
            log::info!("Playing spatial audio for 5 seconds...");
            std::thread::sleep(std::time::Duration::from_secs(5));

            // Stop playback
            log::info!("Stopping spatial playback...");
            world_arc.stop(audio_id)?;

            engine.stop().expect("Failed to stop engine");
            log::info!("Spatial audio test completed successfully");
        }
        Err(e) => log::error!("Spatial audio playback failed: {}", e),
    }

    Ok(())
}
