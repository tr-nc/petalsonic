use crate::{
    audio_data::PetalSonicAudioData,
    error::{PetalSonicError, Result},
};
use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use symphonia::{
    core::{
        audio::SampleBuffer, codecs::DecoderOptions, errors::Error, formats::FormatOptions,
        io::MediaSourceStream, meta::MetadataOptions, probe::Hint,
    },
    default::{get_codecs, get_probe},
};

/// Default audio loader implementation using the Symphonia decoder library.
///
/// This loader supports various audio formats (MP3, WAV, FLAC, OGG, etc.) and decodes them
/// into f32 PCM samples. The audio data can be optionally converted to mono based on the
/// provided options.
///
/// # Examples
///
/// ```ignore
/// use petalsonic_core::audio_data::{DefaultAudioLoader, AudioDataLoader, LoadOptions};
///
/// let loader = DefaultAudioLoader;
/// let audio_data = loader.load("path/to/audio.mp3", &LoadOptions::default())?;
/// ```
pub(crate) struct DefaultAudioLoader;

impl DefaultAudioLoader {
    pub(crate) fn load(&self, path: &str) -> Result<Arc<PetalSonicAudioData>> {
        let file = File::open(path).map_err(|e| {
            PetalSonicError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, e))
        })?;

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

        let track = format.default_track().ok_or_else(|| {
            PetalSonicError::AudioLoading("No default audio track found".to_string())
        })?;

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
            .map_err(|e| {
                PetalSonicError::AudioLoading(format!("Failed to create decoder: {:?}", e))
            })?;

        let mut samples: Vec<f32> = Vec::new();

        loop {
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

            samples.extend_from_slice(tmp.samples());
        }

        let duration =
            Duration::from_secs_f64(samples.len() as f64 / (sample_rate * channels as u32) as f64);

        let audio_data = PetalSonicAudioData::new(samples, sample_rate, channels, duration);

        Ok(Arc::new(audio_data))
    }
}
