//! Event types for PetalSonic

use crate::config::{LatencyProfile, SpatialQuality};
use crate::domain::{Emitter, PlaybackControl, PlaybackTag};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Observable state of the world-owned output runtime.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeState {
    Running = 0,
    Recovering = 1,
    Failed = 2,
    Closing = 3,
    Closed = 4,
}

/// Pull-based health snapshot. Device names are diagnostic, not stable identifiers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeStatus {
    pub state: RuntimeState,
    pub recovery_attempts: u64,
    pub active_output_device: Option<String>,
}

/// Cumulative, allocation-free runtime health counters captured at one instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeDiagnostics {
    pub frames_processed: usize,
    pub underrun_count: usize,
    pub active_emitters: usize,
    pub active_voices: usize,
    pub control_queue_depth: usize,
    pub control_queue_high_water: usize,
    pub lifecycle_queue_depth: usize,
    pub lifecycle_queue_high_water: usize,
    pub event_queue_depth: usize,
    pub event_queue_high_water: usize,
    pub timing_queue_depth: usize,
    pub timing_queue_high_water: usize,
    pub rejected_commands: u64,
    pub dropped_events: u64,
    pub dropped_timing_events: u64,
    pub render_iterations: u64,
    pub render_time_p50_us: u64,
    pub render_time_p95_us: u64,
    pub render_time_p99_us: u64,
    pub render_time_max_us: u64,
    pub acoustic_solve_count: u64,
    pub acoustic_superseded_solve_count: u64,
    pub acoustic_published_response_count: u64,
    pub acoustic_response_spatial_revision: u64,
    pub acoustic_response_geometry_version: u64,
    pub acoustic_last_solve_time_us: u64,
    pub acoustic_solve_time_p50_us: u64,
    pub acoustic_solve_time_p95_us: u64,
    pub acoustic_solve_time_p99_us: u64,
    pub acoustic_solve_time_max_us: u64,
    pub acoustic_response_age_ms: u64,
    pub device_generation: u64,
    pub recovery_attempts: u64,
    pub output_sample_rate: u32,
    pub output_channels: u16,
    pub spatial_quality: SpatialQuality,
    pub latency_profile: LatencyProfile,
}

pub(crate) struct RuntimeCounters {
    pub(crate) control_queue_high_water: AtomicUsize,
    pub(crate) lifecycle_queue_high_water: AtomicUsize,
    pub(crate) event_queue_high_water: AtomicUsize,
    pub(crate) timing_queue_high_water: AtomicUsize,
    pub(crate) rejected_commands: AtomicU64,
    pub(crate) dropped_events: AtomicU64,
    pub(crate) dropped_timing_events: AtomicU64,
    pub(crate) device_generation: AtomicU64,
    pub(crate) output_sample_rate: AtomicUsize,
    pub(crate) output_channels: AtomicUsize,
    render_time_max_us: AtomicU64,
    render_histogram: [AtomicU64; 64],
}

impl Default for RuntimeCounters {
    fn default() -> Self {
        Self {
            control_queue_high_water: AtomicUsize::new(0),
            lifecycle_queue_high_water: AtomicUsize::new(0),
            event_queue_high_water: AtomicUsize::new(0),
            timing_queue_high_water: AtomicUsize::new(0),
            rejected_commands: AtomicU64::new(0),
            dropped_events: AtomicU64::new(0),
            dropped_timing_events: AtomicU64::new(0),
            device_generation: AtomicU64::new(0),
            output_sample_rate: AtomicUsize::new(0),
            output_channels: AtomicUsize::new(0),
            render_time_max_us: AtomicU64::new(0),
            render_histogram: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl RuntimeCounters {
    pub(crate) fn observe_high_water(counter: &AtomicUsize, depth: usize) {
        let mut current = counter.load(Ordering::Relaxed);
        while depth > current {
            match counter.compare_exchange_weak(
                current,
                depth,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    pub(crate) fn record_render_time(&self, elapsed_us: u64) {
        self.render_time_max_us
            .fetch_max(elapsed_us, Ordering::Relaxed);
        let bucket = if elapsed_us == 0 {
            0
        } else {
            (u64::BITS - (elapsed_us - 1).leading_zeros()) as usize + 1
        }
        .min(self.render_histogram.len() - 1);
        self.render_histogram[bucket].fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn render_summary(&self) -> (u64, u64, u64, u64, u64) {
        let total = self
            .render_histogram
            .iter()
            .map(|bucket| bucket.load(Ordering::Relaxed))
            .sum();
        (
            total,
            self.percentile(total, 50),
            self.percentile(total, 95),
            self.percentile(total, 99),
            self.render_time_max_us.load(Ordering::Relaxed),
        )
    }

    fn percentile(&self, total: u64, percentile: u64) -> u64 {
        if total == 0 {
            return 0;
        }
        let target = total.saturating_mul(percentile).div_ceil(100);
        let mut cumulative = 0;
        for (index, bucket) in self.render_histogram.iter().enumerate() {
            cumulative += bucket.load(Ordering::Relaxed);
            if cumulative >= target {
                return match index {
                    0 => 0,
                    63 => u64::MAX,
                    _ => 1u64 << (index - 1),
                };
            }
        }
        self.render_time_max_us.load(Ordering::Relaxed)
    }
}

/// Timing information for a single render iteration
/// Used for performance profiling and stress testing
#[derive(Debug, Clone, Copy)]
pub struct RenderTimingEvent {
    /// Time spent mixing audio sources (microseconds)
    pub mixing_time_us: u64,
    /// Time spent on spatial processing (microseconds)
    pub spatial_time_us: u64,
    /// Time spent mixing non-spatial (direct) sources (microseconds)
    pub direct_mixing_time_us: u64,
    /// Number of spatial sources processed.
    pub spatial_source_count: usize,
    /// Time spent running the spatial physics simulation step (microseconds)
    pub spatial_simulation_time_us: u64,
    /// Time spent applying direct-path processing (microseconds)
    pub direct_processing_time_us: u64,
    /// Time spent encoding sources into the ambisonics field (microseconds)
    pub ambisonics_encoding_time_us: u64,
    /// Time spent decoding ambisonics back to listener channels (microseconds)
    pub ambisonics_decoding_time_us: u64,
    /// Time spent rendering HRTF/binaural output (microseconds)
    pub hrtf_rendering_time_us: u64,
    /// Time spent rendering the shared late-reverb bus (microseconds)
    pub late_reverb_time_us: u64,
    /// Time spent filtering, delaying, and spatializing early reflections (microseconds)
    pub early_reflection_time_us: u64,
    /// Time spent selecting native HRTF directions (microseconds)
    pub native_hrtf_direction_lookup_time_us: u64,
    /// Time spent in native HRTF FIR convolution (microseconds)
    pub native_hrtf_convolution_time_us: u64,
    /// Time spent on resampling (microseconds)
    pub resampling_time_us: u64,
    /// Total time for the entire render iteration (microseconds)
    pub total_time_us: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PetalSonicEvent {
    /// A controlled one-shot reached its natural end.
    PlaybackCompleted {
        emitter: Emitter,
        control: PlaybackControl,
        tag: PlaybackTag,
    },
    /// The output runtime entered a different lifecycle state.
    RuntimeStateChanged(RuntimeState),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_histogram_reports_bounded_cumulative_percentiles() {
        let counters = RuntimeCounters::default();
        for elapsed in [1, 2, 3, 4, 100] {
            counters.record_render_time(elapsed);
        }

        let (count, p50, p95, p99, max) = counters.render_summary();
        assert_eq!(count, 5);
        assert_eq!(p50, 4);
        assert_eq!(p95, 128);
        assert_eq!(p99, 128);
        assert_eq!(max, 100);
    }
}
