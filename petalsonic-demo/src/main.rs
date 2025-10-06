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

    let config = PetalSonicWorldDesc {
        sample_rate: 96000,
        enable_spatialization: false,
        ..Default::default()
    };

    log::info!("Creating world and loading audio file: {}", wav_path);
    let mut world = PetalSonicWorld::new(config.clone()).expect("Failed to create PetalSonicWorld");

    // Load audio file using the new API
    let audio_data = PetalSonicAudioData::from_path(wav_path)?;
    let audio_id = world.add_source(audio_data)?;
    log::info!("Audio loaded with ID: {}", audio_id);

    // Create engine with the world
    let world_arc = Arc::new(world);
    let mut engine =
        PetalSonicEngine::new(config, world_arc.clone()).expect("Failed to create engine");

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

fn test_play_audio_file() -> Result<()> {
    let wav_path = "res/cicada_test_96k.wav";

    let load_options = petalsonic_core::audio_data::LoadOptions::default();

    log::info!("Loading audio file: {}", wav_path);
    let audio_data = PetalSonicAudioData::from_path_with_options(wav_path, &load_options)?;

    let config = PetalSonicWorldDesc {
        sample_rate: 48000,
        enable_spatialization: false,
        ..Default::default()
    };

    let mut world = PetalSonicWorld::new(config.clone()).expect("Failed to create PetalSonicWorld");

    let source_id = world.add_source(audio_data)?;
    let audio_data = world.get_audio_data(source_id).ok_or(anyhow::anyhow!(
        "Failed to get audio data for source ID: {}",
        source_id
    ))?;

    log::debug!("Retrieved audio data for source ID: {}", source_id);

    // Extract samples for playback
    let samples = audio_data.samples().to_vec();
    let sample_rate = audio_data.sample_rate();
    log::debug!("Sample rate: {} Hz", sample_rate);

    // Create and use the new audio engine with callback
    let world_arc = Arc::new(world);
    let mut engine =
        PetalSonicEngine::new(config, world_arc.clone()).expect("Failed to create engine");

    // Create a playback position tracker
    let position = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let samples_arc = Arc::new(samples);

    // Set up the callback to play the loaded audio
    let position_clone = position.clone();
    let samples_clone = samples_arc.clone();
    engine.set_fill_callback(
        move |buffer: &mut [f32], _sample_rate: u32, channels: u16| {
            let channels_usize = channels as usize;
            let frame_count = buffer.len() / channels_usize;
            let mut frames_filled = 0;

            for frame_idx in 0..frame_count {
                let current_pos = position_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                let sample = if current_pos < samples_clone.len() {
                    samples_clone[current_pos]
                } else {
                    0.0 // Silence when we've played all samples
                };

                // Fill all channels with the same sample
                for channel in 0..channels_usize {
                    let buffer_idx = frame_idx * channels_usize + channel;
                    if buffer_idx < buffer.len() {
                        buffer[buffer_idx] = sample;
                    }
                }

                frames_filled += 1;

                // Stop when we've played all samples
                if current_pos >= samples_clone.len() {
                    break;
                }
            }

            frames_filled
        },
    );

    // Start the engine and play
    match engine.start() {
        Ok(_) => {
            log::info!("Audio engine started");

            // Calculate playback duration
            let duration_secs = samples_arc.len() as f64 / sample_rate as f64;
            log::info!("Playing for {:.2} seconds...", duration_secs);

            // Wait for playback to complete (with a small buffer)
            std::thread::sleep(std::time::Duration::from_secs_f64(duration_secs + 0.5));

            engine.stop().expect("Failed to stop engine");
            log::info!("Audio playback completed successfully");
        }
        Err(e) => log::error!("Audio playback failed: {}", e),
    }

    Ok(())
}
