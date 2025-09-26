pub mod audio_data;
pub mod config;
pub mod engine;
pub mod error;
pub mod events;
pub mod math;
pub mod world;

pub use config::PetalSonicWorldDesc;
pub use engine::{AudioFillCallback, PetalSonicEngine};
pub use error::PetalSonicError;
pub use events::PetalSonicEvent;
pub use world::{PetalSonicAudioListener, PetalSonicAudioSource, PetalSonicWorld};

#[cfg(test)]
mod tests {
    use crate::audio_data::load_audio_file;

    #[test]
    fn test_play_audio_file() {
        use crate::audio_data::LoadOptions;
        use crate::config::PetalSonicWorldDesc;
        use crate::engine::PetalSonicEngine;
        use crate::world::PetalSonicWorld;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let wav_path = "res/cicada_test_96k.wav";

        let load_options = LoadOptions::default();

        println!("Loading audio file: {}", wav_path);
        let audio_data =
            load_audio_file(wav_path, &load_options).expect("Failed to load audio file");

        let config = PetalSonicWorldDesc {
            sample_rate: 48000,
            enable_spatialization: false,
            ..Default::default()
        };

        let mut world =
            PetalSonicWorld::new(config.clone()).expect("Failed to create PetalSonicWorld");

        match world.add_source(audio_data) {
            Ok(source_id) => {
                // Get the audio source by ID and play it
                if let Some(audio_data) = world.get_audio_data(source_id) {
                    println!("✓ Retrieved audio data for source ID: {}", source_id);

                    // Extract samples for playback
                    let samples = audio_data.samples().to_vec();
                    let sample_rate = audio_data.sample_rate();
                    println!("Sample rate: {} Hz", sample_rate);

                    // Create and use the new audio engine with callback
                    let mut engine =
                        PetalSonicEngine::new(config).expect("Failed to create engine");

                    // Create a playback position tracker
                    let position = Arc::new(AtomicUsize::new(0));
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
                                let current_pos = position_clone.fetch_add(1, Ordering::Relaxed);

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
                            println!("✓ Audio engine started");

                            // Calculate playback duration
                            let duration_secs = samples_arc.len() as f64 / sample_rate as f64;
                            println!("Playing for {:.2} seconds...", duration_secs);

                            // Wait for playback to complete (with a small buffer)
                            std::thread::sleep(std::time::Duration::from_secs_f64(
                                duration_secs + 0.5,
                            ));

                            engine.stop().expect("Failed to stop engine");
                            println!("✓ Audio playback completed successfully");
                        }
                        Err(e) => println!("✗ Audio playback failed: {}", e),
                    }
                } else {
                    println!(
                        "✗ Failed to retrieve audio data for source ID: {}",
                        source_id
                    );
                }
            }
            Err(e) => {
                println!("✗ Failed to add audio source: {}", e);
            }
        }
    }
}
