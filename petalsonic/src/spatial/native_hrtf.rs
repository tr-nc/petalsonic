use crate::error::{PetalSonicError, Result};
use crate::math::Vec3;
use std::sync::Arc;

const DEFAULT_DIRECTION: Vec3 = Vec3::NEG_Z;

/// One directional HRIR pair in a native PetalSonic HRTF table.
#[derive(Debug, Clone)]
pub struct NativeHrtfDirection {
    /// Unit direction in listener-local coordinates.
    pub direction: Vec3,
    /// Left-ear impulse response taps.
    pub left: Vec<f32>,
    /// Right-ear impulse response taps.
    pub right: Vec<f32>,
}

impl NativeHrtfDirection {
    pub fn new(direction: Vec3, left: Vec<f32>, right: Vec<f32>) -> Self {
        Self {
            direction,
            left,
            right,
        }
    }
}

/// Runtime-ready HRTF table used by the native binaural renderer.
#[derive(Debug, Clone)]
pub struct NativeHrtfTable {
    sample_rate: u32,
    taps: usize,
    directions: Vec<NativeHrtfDirection>,
}

impl NativeHrtfTable {
    pub fn new(sample_rate: u32, directions: Vec<NativeHrtfDirection>) -> Result<Self> {
        if sample_rate == 0 {
            return Err(PetalSonicError::Configuration(
                "native HRTF sample rate must be non-zero".to_string(),
            ));
        }

        let Some(first) = directions.first() else {
            return Err(PetalSonicError::Configuration(
                "native HRTF table must contain at least one direction".to_string(),
            ));
        };

        let taps = first.left.len();
        if taps == 0 {
            return Err(PetalSonicError::Configuration(
                "native HRTF impulse responses must contain at least one tap".to_string(),
            ));
        }

        let mut normalized_directions = Vec::with_capacity(directions.len());
        for (index, mut entry) in directions.into_iter().enumerate() {
            if entry.left.len() != taps || entry.right.len() != taps {
                return Err(PetalSonicError::Configuration(format!(
                    "native HRTF direction {index} has mismatched tap count"
                )));
            }

            if !entry.left.iter().all(|sample| sample.is_finite())
                || !entry.right.iter().all(|sample| sample.is_finite())
            {
                return Err(PetalSonicError::Configuration(format!(
                    "native HRTF direction {index} contains non-finite taps"
                )));
            }

            entry.direction = normalize_direction(entry.direction);
            normalized_directions.push(entry);
        }

        Ok(Self {
            sample_rate,
            taps,
            directions: normalized_directions,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn taps(&self) -> usize {
        self.taps
    }

    pub fn direction_count(&self) -> usize {
        self.directions.len()
    }

    pub fn direction(&self, index: usize) -> Option<&NativeHrtfDirection> {
        self.directions.get(index)
    }

    pub fn nearest_direction_index(&self, direction: Vec3) -> usize {
        let direction = normalize_direction(direction);
        let mut best_index = 0usize;
        let mut best_dot = f32::NEG_INFINITY;

        for (index, entry) in self.directions.iter().enumerate() {
            let dot = direction.dot(entry.direction);
            if dot > best_dot {
                best_dot = dot;
                best_index = index;
            }
        }

        best_index
    }
}

/// Per-source convolution state for native HRTF rendering.
#[derive(Debug, Clone)]
pub struct NativeHrtfSourceState {
    delay_line: Vec<f32>,
    write_index: usize,
}

impl NativeHrtfSourceState {
    fn new(taps: usize) -> Self {
        Self {
            delay_line: vec![0.0; taps],
            write_index: 0,
        }
    }

    pub fn reset(&mut self) {
        self.delay_line.fill(0.0);
        self.write_index = 0;
    }
}

/// Time-domain native HRTF renderer.
///
/// This is intentionally simple for the first native HRTF step: nearest-direction
/// lookup plus FIR convolution. Later phases can add direction interpolation,
/// block crossfades, SIMD, or partitioned convolution without changing the table
/// format.
#[derive(Debug, Clone)]
pub struct NativeHrtfRenderer {
    table: Arc<NativeHrtfTable>,
}

impl NativeHrtfRenderer {
    pub fn new(table: Arc<NativeHrtfTable>) -> Self {
        Self { table }
    }

    pub fn table(&self) -> &NativeHrtfTable {
        &self.table
    }

    pub fn create_source_state(&self) -> NativeHrtfSourceState {
        NativeHrtfSourceState::new(self.table.taps)
    }

    /// Render a mono source block into an interleaved stereo output buffer.
    ///
    /// Output is accumulated, not cleared, so callers can mix multiple sources.
    pub fn render_source(
        &self,
        state: &mut NativeHrtfSourceState,
        direction: Vec3,
        input: &[f32],
        output_interleaved: &mut [f32],
    ) -> Result<()> {
        let frames = input.len();
        if output_interleaved.len() < frames * 2 {
            return Err(PetalSonicError::Configuration(format!(
                "native HRTF output buffer too small: need {}, got {} samples",
                frames * 2,
                output_interleaved.len()
            )));
        }

        if state.delay_line.len() != self.table.taps {
            *state = self.create_source_state();
        }

        let direction_index = self.table.nearest_direction_index(direction);
        let hrir = &self.table.directions[direction_index];
        let taps = self.table.taps;

        for (frame_index, input_sample) in input.iter().copied().enumerate() {
            state.delay_line[state.write_index] = input_sample;

            let mut left = 0.0f32;
            let mut right = 0.0f32;
            for tap in 0..taps {
                let delay_index = (state.write_index + taps - tap) % taps;
                let delayed = state.delay_line[delay_index];
                left += delayed * hrir.left[tap];
                right += delayed * hrir.right[tap];
            }

            let out_index = frame_index * 2;
            output_interleaved[out_index] += left;
            output_interleaved[out_index + 1] += right;

            state.write_index = (state.write_index + 1) % taps;
        }

        Ok(())
    }
}

fn normalize_direction(direction: Vec3) -> Vec3 {
    if direction.is_finite() && direction.length_squared() > f32::EPSILON {
        direction.normalize()
    } else {
        DEFAULT_DIRECTION
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_renderer() -> (NativeHrtfRenderer, NativeHrtfSourceState) {
        let table = NativeHrtfTable::new(
            48_000,
            vec![NativeHrtfDirection::new(
                Vec3::NEG_Z,
                vec![1.0, 0.25],
                vec![0.5, -0.25],
            )],
        )
        .unwrap();
        let renderer = NativeHrtfRenderer::new(Arc::new(table));
        let state = renderer.create_source_state();
        (renderer, state)
    }

    #[test]
    fn table_validates_shape_and_sample_rate() {
        assert!(NativeHrtfTable::new(0, Vec::new()).is_err());
        assert!(NativeHrtfTable::new(48_000, Vec::new()).is_err());
        assert!(
            NativeHrtfTable::new(
                48_000,
                vec![NativeHrtfDirection::new(
                    Vec3::NEG_Z,
                    vec![1.0],
                    vec![1.0, 0.0],
                )],
            )
            .is_err()
        );
    }

    #[test]
    fn nearest_direction_uses_largest_dot_product() {
        let table = NativeHrtfTable::new(
            48_000,
            vec![
                NativeHrtfDirection::new(Vec3::X, vec![1.0], vec![0.0]),
                NativeHrtfDirection::new(Vec3::NEG_X, vec![0.0], vec![1.0]),
            ],
        )
        .unwrap();

        assert_eq!(table.nearest_direction_index(Vec3::new(10.0, 0.0, 0.1)), 0);
        assert_eq!(table.nearest_direction_index(Vec3::new(-2.0, 0.0, 0.0)), 1);
    }

    #[test]
    fn render_source_convolves_and_accumulates_stereo_output() {
        let (renderer, mut state) = test_renderer();
        let input = [1.0, 0.0, 0.0];
        let mut output = [10.0, 20.0, 0.0, 0.0, 0.0, 0.0];

        renderer
            .render_source(&mut state, Vec3::NEG_Z, &input, &mut output)
            .unwrap();

        assert!((output[0] - 11.0).abs() < 1e-6);
        assert!((output[1] - 20.5).abs() < 1e-6);
        assert!((output[2] - 0.25).abs() < 1e-6);
        assert!((output[3] + 0.25).abs() < 1e-6);
        assert_eq!(output[4], 0.0);
        assert_eq!(output[5], 0.0);
    }

    #[test]
    fn render_source_preserves_delay_line_across_blocks() {
        let (renderer, mut state) = test_renderer();
        let mut first = [0.0; 2];
        renderer
            .render_source(&mut state, Vec3::NEG_Z, &[1.0], &mut first)
            .unwrap();

        let mut second = [0.0; 2];
        renderer
            .render_source(&mut state, Vec3::NEG_Z, &[0.0], &mut second)
            .unwrap();

        assert!((second[0] - 0.25).abs() < 1e-6);
        assert!((second[1] + 0.25).abs() < 1e-6);
    }
}
