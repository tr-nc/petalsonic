pub mod audio_data;
pub mod config;
pub mod engine;
pub mod error;
pub mod events;
pub mod math;
pub mod test_audio_playback;
pub mod world;

pub use config::PetalSonicConfig;
pub use error::PetalSonicError;
pub use events::PetalSonicEvent;
pub use world::{PetalSonicAudioListener, PetalSonicAudioSource, PetalSonicWorld};

#[cfg(test)]
mod tests {
    use crate::audio_data::{LoadOptions, load_audio_file};

    #[test]
    fn test_decode_wav_file() {
        const WAV_PATH: &str = "res/cicada_test_96k.wav";
        let load_options = LoadOptions::default();
        match load_audio_file(WAV_PATH, &load_options) {
            Ok(audio_data) => {
                println!("Successfully loaded audio file:");
                println!("  Sample rate: {} Hz", audio_data.sample_rate());
                println!("  Channels: {}", audio_data.channels());
                println!("  Duration: {:?}", audio_data.duration());
                println!("  Total frames: {}", audio_data.total_frames());
                println!("  Total samples: {}", audio_data.len());
            }
            Err(e) => {
                println!("Error loading audio file: {}", e);
                panic!("Failed to load WAV file: {}", e);
            }
        }
    }

    #[test]
    fn test_play_audio_file() {
        use crate::audio_data::LoadOptions;
        use crate::config::PetalSonicConfig;
        use crate::world::PetalSonicWorld;
        use std::sync::Arc;

        println!("=== PetalSonic Audio Playback Test ===");

        // Load the audio file
        let wav_path = "res/cicada_test_96k.wav";
        let config = PetalSonicConfig {
            sample_rate: 44100,
            enable_spatialization: false,
            ..Default::default()
        };

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

                // Get the audio source by ID and play it
                if let Some(audio_data) = world.get_audio_data(source_id) {
                    println!("✓ Retrieved audio data for source ID: {}", source_id);
                    println!("🎵 Playing audio for 20 seconds...");

                    // Extract samples for playback
                    let samples = audio_data.samples().to_vec();
                    let sample_rate = audio_data.sample_rate();

                    // Play the audio using the test playback module
                    match crate::test_audio_playback::play_audio_samples(samples, sample_rate) {
                        Ok(_) => println!("✓ Audio playback completed successfully"),
                        Err(e) => println!("✗ Audio playback failed: {}", e),
                    }

                    // Additional sleep to ensure we wait 20 seconds total
                    println!("⏱️ Sleeping for 20 seconds...");
                    std::thread::sleep(std::time::Duration::from_secs(20));
                    println!("✓ Sleep completed");
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

        // Stop the world
        println!("\nStopping world...");
        world.stop().expect("Failed to stop world");
        println!("✓ World stopped successfully");

        println!("=== Audio playback test completed ===");
    }
}
