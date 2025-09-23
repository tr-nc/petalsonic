use crate::{
    audio_data::{LoadOptions, PetalSonicAudioData},
    error::{PetalSonicError, Result},
};
use std::fs::File;
use std::path::Path;
use std::time::Duration;
use symphonia::{
    core::{
        audio::SampleBuffer, codecs::DecoderOptions, errors::Error, formats::FormatOptions,
        io::MediaSourceStream, meta::MetadataOptions, probe::Hint,
    },
    default::{get_codecs, get_probe},
};

pub fn load_audio_file(path: &str, options: &LoadOptions) -> Result<PetalSonicAudioData> {
    let file = File::open(path)
        .map_err(|e| PetalSonicError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, e)))?;

    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = Path::new(path).extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probe = get_probe();
    let probed = probe
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| {
            PetalSonicError::AudioLoading(format!("Failed to probe audio format: {:?}", e))
        })?;

    let mut format = probed.format;

    let track = format
        .default_track()
        .ok_or_else(|| PetalSonicError::AudioLoading("No default audio track found".to_string()))?;

    let sample_rate = track
        .codec_params
        .sample_rate
        .ok_or_else(|| PetalSonicError::AudioLoading("Sample rate not found".to_string()))?
        as u32;

    let channels = track
        .codec_params
        .channels
        .ok_or_else(|| PetalSonicError::AudioLoading("Channel count not found".to_string()))?
        .count() as u16;

    let mut decoder = get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| PetalSonicError::AudioLoading(format!("Failed to create decoder: {:?}", e)))?;

    let mut samples: Vec<f32> = Vec::new();
    let max_frames = options
        .max_duration
        .map(|d| (d.as_secs_f64() * sample_rate as f64) as usize)
        .unwrap_or(usize::MAX);

    let mut frames_decoded = 0;

    loop {
        if frames_decoded >= max_frames {
            break;
        }

        // Read the next packet from the container
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(Error::IoError(_)) => break, // end-of-file
            Err(e) => {
                return Err(PetalSonicError::AudioLoading(format!(
                    "Error reading packet: {:?}",
                    e
                )));
            }
        };

        // Decode the packet into audio samples
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(Error::IoError(_)) => break, // also EOF in some formats
            Err(Error::DecodeError(_)) => continue, // recoverable corruption
            Err(e) => {
                return Err(PetalSonicError::AudioLoading(format!(
                    "Error decoding packet: {:?}",
                    e
                )));
            }
        };

        // Convert the sample buffer into f32 samples using SampleBuffer
        let spec = *decoded.spec();
        let capacity = decoded.capacity();

        // Always convert to f32
        let mut tmp = SampleBuffer::<f32>::new(capacity as u64, spec);
        tmp.copy_interleaved_ref(decoded);

        if options.mono_channel.is_some() {
            // Extract specific channel for mono
            let mono_ch = options.mono_channel.unwrap();
            if mono_ch >= channels as usize {
                return Err(PetalSonicError::AudioFormat(format!(
                    "Channel {} out of range (max: {})",
                    mono_ch,
                    channels - 1
                )));
            }
            samples.extend(
                tmp.samples()
                    .chunks(channels as usize)
                    .map(|frame| frame[mono_ch]),
            );
        } else {
            // Keep all channels
            samples.extend_from_slice(tmp.samples());
        }

        frames_decoded += capacity / channels as usize;
    }

    // Apply mono conversion if requested
    let final_samples;
    let final_channels;

    if options.convert_to_mono && channels > 1 {
        if options.mono_channel.is_some() {
            // Already extracted single channel during decoding
            final_samples = samples;
            final_channels = 1;
        } else {
            // Downmix all channels to mono
            final_samples = samples
                .chunks(channels as usize)
                .map(|frame| {
                    let sum: f32 = frame.iter().sum();
                    sum / channels as f32
                })
                .collect();
            final_channels = 1;
        }
    } else {
        final_samples = samples;
        final_channels = channels;
    }

    let duration = Duration::from_secs_f64(
        final_samples.len() as f64 / (sample_rate * final_channels as u32) as f64,
    );

    let mut audio_data =
        PetalSonicAudioData::new(final_samples, sample_rate, final_channels, duration);

    // Apply resampling if requested
    if let Some(target_rate) = options.target_sample_rate {
        if target_rate != sample_rate {
            audio_data = audio_data.resample(target_rate)?;
        }
    }

    Ok(audio_data)
}

/// Convenience function to load audio with default options
pub fn load_audio_file_simple(path: &str) -> Result<PetalSonicAudioData> {
    load_audio_file(path, &LoadOptions::default())
}
