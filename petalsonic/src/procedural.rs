//! Source-level procedural audio support.
//!
//! Procedural sources render mono audio at the PetalSonic world sample rate. The
//! rendered mono block is then mixed directly for non-spatial sources or fed into
//! the existing spatial processor for spatial sources.

/// A stateful procedural audio generator.
///
/// Implementations are owned by the render thread after playback starts. They
/// should avoid blocking work and per-sample allocation in [`render_mono`].
pub trait ProceduralAudioSource: Send {
    /// Render mono samples into `out` at the world sample rate.
    ///
    /// Implementations should fill every sample in `out`. PetalSonic clears the
    /// destination before calling this method so generators may either overwrite
    /// or add into the buffer, but overwriting is preferred.
    fn render_mono(&mut self, out: &mut [f32]);

    /// Reset generator state for replay or seek-to-start operations.
    fn reset(&mut self) {}
}

/// Factory used to create render-thread-owned procedural source instances.
///
/// The factory is registered on the world thread and cloned into playback
/// commands. Each playback instance gets its own generator created with the
/// PetalSonic world sample rate.
pub trait ProceduralAudioFactory: Send + Sync {
    /// Create a fresh source instance for `sample_rate`.
    fn create(&self, sample_rate: u32) -> Box<dyn ProceduralAudioSource>;
}

impl<F> ProceduralAudioFactory for F
where
    F: Fn(u32) -> Box<dyn ProceduralAudioSource> + Send + Sync,
{
    fn create(&self, sample_rate: u32) -> Box<dyn ProceduralAudioSource> {
        self(sample_rate)
    }
}
