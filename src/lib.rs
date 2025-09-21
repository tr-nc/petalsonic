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

#[cfg(test)]
pub mod test_audio_playback;

pub use config::PetalSonicConfig;
pub use error::PetalSonicError;
pub use events::PetalSonicEvent;
pub use world::{PetalSonicAudioListener, PetalSonicAudioSource, PetalSonicWorld};

#[cfg(test)]
mod tests {
    use crate::audio::{LoadOptions, load_audio_file};

    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }

    #[test]
    fn test_decode_wav_file() {
        let wav_path = "res/cicada_test.wav";
        let load_options = LoadOptions::default();

        match load_audio_file(wav_path, &load_options) {
            Ok(audio_data) => {
                println!("Successfully loaded audio file:");
                println!("  Sample rate: {} Hz", audio_data.sample_rate());
                println!("  Channels: {}", audio_data.channels());
                println!("  Duration: {:?}", audio_data.duration());
                println!("  Total frames: {}", audio_data.total_frames());
                println!("  Total samples: {}", audio_data.len());

                // Get the first 10 values of the left channel (channel 0)
                match audio_data.channel_samples(0) {
                    Ok(left_channel) => {
                        println!("\nFirst 10 values of left channel:");
                        for (i, &sample) in left_channel.iter().take(10).enumerate() {
                            println!("  Sample {}: {:.6}", i, sample);
                        }

                        // Show samples 1000-1010
                        println!("\nSamples 1000-1010 of left channel:");
                        for (i, &sample) in left_channel.iter().skip(1000).take(11).enumerate() {
                            println!("  Sample {}: {:.6}", i + 1000, sample);
                        }

                        // Also show some basic statistics
                        let max_val = left_channel.iter().fold(0.0f32, |max, &x| max.max(x.abs()));
                        println!("\nMax absolute value in left channel: {:.6}", max_val);
                    }
                    Err(e) => {
                        println!("Error getting left channel samples: {}", e);
                    }
                }
            }
            Err(e) => {
                println!("Error loading audio file: {}", e);
                panic!("Failed to load WAV file: {}", e);
            }
        }
    }

    #[test]
    fn test_play_audio_file() {
        // load the audio, create the world, play the audio inside the world.
        // no spatial is required for now. just play the audio directly.

        use crate::audio::{LoadOptions, load_audio_file};
        use crate::config::PetalSonicConfig;
        use crate::world::PetalSonicWorld;
        use std::sync::Arc;

        println!("=== PetalSonic Audio Playback Test ===");

        // Load the audio file
        let wav_path = "res/cicada_test_96k.wav";
        let config = PetalSonicConfig::default();
        let load_options = LoadOptions::default().target_sample_rate(config.sample_rate); // Resample to match playback device

        println!("Loading audio file: {}", wav_path);
        let audio_data =
            load_audio_file(wav_path, &load_options).expect("Failed to load audio file");

        println!("✓ Audio file loaded successfully:");
        println!("  - Sample rate: {} Hz", audio_data.sample_rate());
        println!("  - Channels: {}", audio_data.channels());
        println!("  - Duration: {:?}", audio_data.duration());
        println!("  - Total frames: {}", audio_data.total_frames());
        println!("  - Total samples: {}", audio_data.len());

        // Create world configuration
        let config = config.enable_spatialization(false); // No spatialization for direct playback
        println!("✓ World configuration created (spatialization disabled)");

        // Create and start the world
        println!("Creating PetalSonicWorld...");
        let mut world = PetalSonicWorld::new(config).expect("Failed to create PetalSonicWorld");

        println!("Starting world...");
        world.start().expect("Failed to start world");
        println!("✓ World started successfully");

        // Now actually add the audio source and play it!
        println!("\n🎵 Adding audio source to world...");
        let audio_data_arc = Arc::new(audio_data);
        match world.add_source(audio_data_arc) {
            Ok(source_id) => {
                println!("✓ Audio source added successfully with ID: {}", source_id);
                println!("🔊 Audio should be playing now with full CPAL device debug info!");
            }
            Err(e) => {
                println!("✗ Failed to add audio source: {}", e);
            }
        }

        // Stop the world
        println!("\nStopping world...");
        world.stop().expect("Failed to stop world");
        println!("✓ World stopped successfully");

        println!("=== Audio playback test completed ===");
    }
}
