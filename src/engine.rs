use crate::config::PetalSonicWorldDesc;
use crate::error::Result;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Callback function type for filling audio samples
///
/// The callback receives:
/// - `buffer`: mutable slice to fill with audio samples
/// - `sample_rate`: target sample rate for the samples
/// - `channels`: number of audio channels
///
/// Returns the number of frames actually filled (frames = samples / channels)
pub type AudioFillCallback = dyn Fn(&mut [f32], u32, u16) -> usize + Send + Sync;

/// Audio engine that manages real-time audio processing and output
pub struct PetalSonicEngine {
    desc: PetalSonicWorldDesc,
    stream: Option<cpal::Stream>,
    is_running: Arc<AtomicBool>,
    frames_processed: Arc<AtomicUsize>,
    fill_callback: Option<Arc<AudioFillCallback>>,
}

impl PetalSonicEngine {
    /// Create a new audio engine with the given configuration
    pub fn new(desc: PetalSonicWorldDesc) -> Result<Self> {
        Ok(Self {
            desc,
            stream: None,
            is_running: Arc::new(AtomicBool::new(false)),
            frames_processed: Arc::new(AtomicUsize::new(0)),
            fill_callback: None,
        })
    }

    /// Set the callback function that will be called to fill audio buffers
    /// This is the non-blocking callback required by the TODO
    pub fn set_fill_callback<F>(&mut self, callback: F)
    where
        F: Fn(&mut [f32], u32, u16) -> usize + Send + Sync + 'static,
    {
        self.fill_callback = Some(Arc::new(callback));
    }

    /// Start the audio engine with the configured callback
    pub fn start(&mut self) -> Result<()> {
        if self.is_running.load(Ordering::Relaxed) {
            return Ok(());
        }

        let fill_callback = self
            .fill_callback
            .clone()
            .ok_or_else(|| crate::error::PetalSonicError::Engine("No fill callback set".into()))?;

        // Get the default audio device
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or_else(|| {
            crate::error::PetalSonicError::AudioDevice("No default output device available".into())
        })?;

        // Configure the stream with the world's sample rate and settings
        let config = cpal::StreamConfig {
            channels: self.desc.channels,
            sample_rate: cpal::SampleRate(self.desc.sample_rate),
            buffer_size: cpal::BufferSize::Fixed(self.desc.block_size as u32),
        };

        let is_running = self.is_running.clone();
        let frames_processed = self.frames_processed.clone();
        let sample_rate = self.desc.sample_rate;
        let channels = self.desc.channels;

        // Create the stream based on the device's default format
        let default_config = device.default_output_config().map_err(|e| {
            crate::error::PetalSonicError::AudioDevice(format!(
                "Failed to get default config: {}",
                e
            ))
        })?;

        let stream = match default_config.sample_format() {
            cpal::SampleFormat::F32 => self.create_stream::<f32>(
                &device,
                &config,
                fill_callback,
                is_running,
                frames_processed,
                sample_rate,
                channels,
            )?,
            cpal::SampleFormat::I16 => self.create_stream::<i16>(
                &device,
                &config,
                fill_callback,
                is_running,
                frames_processed,
                sample_rate,
                channels,
            )?,
            cpal::SampleFormat::U16 => self.create_stream::<u16>(
                &device,
                &config,
                fill_callback,
                is_running,
                frames_processed,
                sample_rate,
                channels,
            )?,
            _ => {
                return Err(crate::error::PetalSonicError::AudioFormat(
                    "Unsupported sample format".into(),
                ));
            }
        };

        stream.play().map_err(|e| {
            crate::error::PetalSonicError::AudioDevice(format!("Failed to start stream: {}", e))
        })?;

        self.stream = Some(stream);
        self.is_running.store(true, Ordering::Relaxed);

        Ok(())
    }

    /// Stop the audio engine
    pub fn stop(&mut self) -> Result<()> {
        if let Some(stream) = self.stream.take() {
            self.is_running.store(false, Ordering::Relaxed);
            drop(stream); // This stops the stream
        }
        Ok(())
    }

    /// Check if the engine is currently running
    pub fn is_running(&self) -> bool {
        self.is_running.load(Ordering::Relaxed)
    }

    /// Get the number of audio frames processed since start
    pub fn frames_processed(&self) -> usize {
        self.frames_processed.load(Ordering::Relaxed)
    }

    /// Get the engine configuration
    pub fn config(&self) -> &PetalSonicWorldDesc {
        &self.desc
    }

    /// Create a typed audio stream
    fn create_stream<T>(
        &self,
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        fill_callback: Arc<AudioFillCallback>,
        is_running: Arc<AtomicBool>,
        frames_processed: Arc<AtomicUsize>,
        sample_rate: u32,
        channels: u16,
    ) -> Result<cpal::Stream>
    where
        T: SizedSample + FromSample<f32>,
    {
        let channels_usize = channels as usize;

        let stream = device
            .build_output_stream(
                config,
                move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                    if !is_running.load(Ordering::Relaxed) {
                        // Fill with silence if not running
                        for sample in data.iter_mut() {
                            *sample = T::from_sample(0.0f32);
                        }
                        return;
                    }

                    // Create a temporary f32 buffer for the callback
                    let _frame_count = data.len() / channels_usize;
                    let mut temp_buffer = vec![0.0f32; data.len()];

                    // Call the user-provided fill callback (non-blocking)
                    let frames_filled = fill_callback(&mut temp_buffer, sample_rate, channels);

                    // Convert and copy to the output buffer
                    for (i, sample) in data.iter_mut().enumerate() {
                        let sample_value = if i < temp_buffer.len() {
                            temp_buffer[i]
                        } else {
                            0.0f32
                        };
                        *sample = T::from_sample(sample_value);
                    }

                    // Update frame counter
                    frames_processed.fetch_add(frames_filled, Ordering::Relaxed);
                },
                move |err| {
                    log::error!("Audio stream error: {}", err);
                },
                None,
            )
            .map_err(|e| {
                crate::error::PetalSonicError::AudioDevice(format!("Failed to build stream: {}", e))
            })?;

        Ok(stream)
    }
}

impl Drop for PetalSonicEngine {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
