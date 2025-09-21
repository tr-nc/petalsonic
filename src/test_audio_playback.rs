//! Simple audio playback for testing purposes

use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use std::sync::{Arc, Mutex};

/// Play audio samples using cpal with WASAPI shared mode
pub fn play_audio_samples(samples: Vec<f32>, sample_rate: u32) -> Result<()> {
    let frames = samples.len();

    println!("=== CPAL Audio Device Debug Info ===");
    println!(
        "Playing {} frames at {} Hz ({} samples)",
        frames,
        sample_rate,
        samples.len()
    );

    // Get the default host and device
    let host = cpal::default_host();
    println!("Host: {:?}", host.id());

    // List all available output devices
    println!("\n📋 Available Output Devices:");
    let output_devices: Vec<_> = host.output_devices()?.collect();
    for (i, device) in output_devices.iter().enumerate() {
        let device_name = device
            .name()
            .unwrap_or_else(|_| "Unknown Device".to_string());
        println!("  {}. {}", i + 1, device_name);

        // Show default config for each device
        if let Ok(default_config) = device.default_output_config() {
            println!(
                "     Default: {}ch, {}Hz, {:?}",
                default_config.channels(),
                default_config.sample_rate().0,
                default_config.sample_format()
            );
        }
    }

    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("No default output device available"))?;

    println!("\n🎯 Selected Default Device: {}", device.name()?);

    // Get the default output config to see what the device supports
    let default_config = device.default_output_config()?;
    println!("\n⚙️ Default Device Configuration:");
    println!("  Channels: {}", default_config.channels());
    println!("  Sample Rate: {} Hz", default_config.sample_rate().0);
    println!("  Sample Format: {:?}", default_config.sample_format());
    println!("  Buffer Size: {:?}", default_config.buffer_size());

    // Check if the device supports our desired sample rate
    let supported_configs: Vec<_> = device.supported_output_configs()?.collect();
    println!("\n🔧 All Supported Configurations:");
    for (i, config) in supported_configs.iter().enumerate() {
        println!(
            "  {}. Channels: {}, Rate: {}-{}Hz, Format: {:?}",
            i + 1,
            config.channels(),
            config.min_sample_rate().0,
            config.max_sample_rate().0,
            config.sample_format()
        );
    }

    // Try to find a supported config that matches our sample rate
    println!("\n🎯 Selecting Configuration for {}Hz...", sample_rate);
    let config_to_use = if let Some(supported) = supported_configs.iter().find(|config| {
        config.min_sample_rate() <= cpal::SampleRate(sample_rate)
            && config.max_sample_rate() >= cpal::SampleRate(sample_rate)
    }) {
        // Found a supported config that can handle our sample rate
        println!("✓ Found native support for {}Hz", sample_rate);
        cpal::StreamConfig {
            channels: supported.channels(),
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        }
    } else {
        // Fall back to default config - WASAPI will resample
        println!(
            "⚠ Sample rate {} Hz not directly supported, using default config",
            sample_rate
        );
        println!(
            "  WASAPI will handle resampling from {}Hz to {}Hz",
            sample_rate,
            default_config.sample_rate().0
        );
        default_config.config()
    };

    println!("\n✅ Final Stream Configuration:");
    println!("  Channels: {}", config_to_use.channels);
    println!("  Sample Rate: {} Hz", config_to_use.sample_rate.0);
    println!("  Buffer Size: {:?}", config_to_use.buffer_size);
    println!("  Sample Format: {:?}", default_config.sample_format());
    println!("  WASAPI shared mode will handle any necessary resampling");

    // Prepare audio data
    let audio_data = Arc::new(Mutex::new((samples, 0usize))); // (samples, current_position)

    // Create the audio stream based on the default format
    let stream = match default_config.sample_format() {
        cpal::SampleFormat::F32 => run_stream::<f32>(&device, &config_to_use, audio_data)?,
        cpal::SampleFormat::I16 => run_stream::<i16>(&device, &config_to_use, audio_data)?,
        cpal::SampleFormat::U16 => run_stream::<u16>(&device, &config_to_use, audio_data)?,
        _ => return Err(anyhow::anyhow!("Unsupported sample format")),
    };

    // Start the stream
    stream.play()?;

    println!("\n🔊 Playing audio... (press Ctrl+C to stop)");

    // Keep the stream alive for the duration of the audio
    let duration_secs = frames as f64 / sample_rate as f64;
    std::thread::sleep(std::time::Duration::from_secs_f64(duration_secs + 1.0));

    println!("✓ Playback finished");

    Ok(())
}

fn run_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    audio_data: Arc<Mutex<(Vec<f32>, usize)>>,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;

    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            let mut audio_guard = audio_data.lock().unwrap();
            let (samples, position) = &mut *audio_guard;

            // Fill the output buffer - WASAPI handles any sample rate conversion
            for frame in data.chunks_mut(channels) {
                let sample = if *position < samples.len() {
                    let current_sample = samples[*position];
                    *position += 1; // 1:1 playback - WASAPI resamples as needed
                    current_sample
                } else {
                    0.0 // Silence when we've played all samples
                };

                // Fill all channels with the same sample (mono to stereo/multi-channel)
                for channel_sample in frame.iter_mut() {
                    *channel_sample = T::from_sample(sample);
                }
            }
        },
        move |err| {
            eprintln!("Audio stream error: {}", err);
        },
        None,
    )?;

    Ok(stream)
}
