pub mod audio_data;
pub mod config;
pub mod error;
pub mod events;
pub mod math;
pub mod test_audio_playback;
pub mod world;

pub use config::PetalSonicWorldDesc;
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
        use crate::world::PetalSonicWorld;

        let wav_path = "res/cicada_test_96k.wav";

        let load_options = LoadOptions::default();

        println!("Loading audio file: {}", wav_path);
        let audio_data =
            load_audio_file(wav_path, &load_options).expect("Failed to load audio file");

        let mut world = PetalSonicWorld::new(PetalSonicWorldDesc {
            sample_rate: 48000,
            enable_spatialization: false,
            ..Default::default()
        })
        .expect("Failed to create PetalSonicWorld");

        world.start().expect("Failed to start world");

        let audio_data_arc = audio_data;
        match world.add_source(audio_data_arc) {
            Ok(source_id) => {
                println!("✓ Audio source added successfully with ID: {}", source_id);

                // Get the audio source by ID and play it
                if let Some(audio_data) = world.get_audio_data(source_id) {
                    println!("✓ Retrieved audio data for source ID: {}", source_id);

                    // Extract samples for playback
                    let samples = audio_data.samples().to_vec();
                    let sample_rate = audio_data.sample_rate();
                    println!("Sample rate: {} Hz", sample_rate);

                    // Play the audio using the test playback module
                    match crate::test_audio_playback::play_audio_samples(samples, sample_rate) {
                        Ok(_) => println!("✓ Audio playback completed successfully"),
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

        world.stop().expect("Failed to stop world");
    }
}
