use petalsonic::{PetalSonicEngine, PetalSonicWorldDesc};
use std::sync::{Arc, Mutex};

/// Example demonstrating the callback function for filling audio samples
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Testing PetalSonic Audio Engine with Callback");

    // Create engine configuration
    let config = PetalSonicWorldDesc {
        sample_rate: 48000,
        block_size: 1024,
        channels: 2,
        ..Default::default()
    };

    // Create the audio engine
    let mut engine = PetalSonicEngine::new(config.clone())?;

    // Create a simple test oscillator for the callback
    let frequency = 440.0; // A4 note
    let phase = Arc::new(Mutex::new(0.0f32));

    // Set up the callback function that fills audio samples
    let phase_clone = phase.clone();
    engine.set_fill_callback(move |buffer: &mut [f32], sample_rate: u32, channels: u16| {
        let mut phase_guard = phase_clone.lock().unwrap();
        let channels_usize = channels as usize;
        let frame_count = buffer.len() / channels_usize;

        for frame_idx in 0..frame_count {
            // Generate a sine wave sample
            let sample = (*phase_guard * 2.0 * std::f32::consts::PI).sin() * 0.1; // Low volume

            // Fill all channels with the same sample
            for channel in 0..channels_usize {
                let buffer_idx = frame_idx * channels_usize + channel;
                if buffer_idx < buffer.len() {
                    buffer[buffer_idx] = sample;
                }
            }

            // Advance phase
            *phase_guard += frequency / sample_rate as f32;
            if *phase_guard >= 1.0 {
                *phase_guard -= 1.0;
            }
        }

        frame_count
    });

    println!("Starting audio engine...");
    engine.start()?;

    println!("Playing 440Hz sine wave for 3 seconds...");
    println!(
        "Sample rate: {}Hz, Channels: {}, Block size: {}",
        config.sample_rate, config.channels, config.block_size
    );

    // Let it play for 3 seconds
    std::thread::sleep(std::time::Duration::from_secs(3));

    println!("Frames processed: {}", engine.frames_processed());

    println!("Stopping audio engine...");
    engine.stop()?;

    println!("✓ Test completed successfully");
    Ok(())
}
