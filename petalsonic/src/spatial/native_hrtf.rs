use crate::error::{PetalSonicError, Result};
use crate::math::Vec3;
use std::path::Path;
use std::sync::Arc;

const DEFAULT_DIRECTION: Vec3 = Vec3::NEG_Z;
const PETALHRTF_MAGIC: &[u8; 8] = b"PETHRTF\0";
const PETALHRTF_VERSION: u32 = 1;
const PETALHRTF_HEADER_BYTES: usize = 8 + 4 + 4 + 4 + 4;
const F32_BYTES: usize = std::mem::size_of::<f32>();

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

    /// Load a native PetalSonic HRTF table from a `.petalhrtf` file.
    pub fn from_petalhrtf_file(path: impl AsRef<Path>) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        Self::from_petalhrtf_bytes(&bytes)
    }

    /// Decode a native PetalSonic HRTF table from bytes.
    ///
    /// Binary format, little-endian:
    ///
    /// - magic: `PETHRTF\0` (8 bytes)
    /// - version: `u32` (`1`)
    /// - sample rate: `u32`
    /// - direction count: `u32`
    /// - taps per ear: `u32`
    /// - repeated direction records:
    ///   - listener-local unit direction: `f32 x, y, z`
    ///   - left HRIR taps: `taps * f32`
    ///   - right HRIR taps: `taps * f32`
    pub fn from_petalhrtf_bytes(bytes: &[u8]) -> Result<Self> {
        let mut reader = PetalHrtfReader::new(bytes);
        let magic = reader.read_bytes(PETALHRTF_MAGIC.len())?;
        if magic != PETALHRTF_MAGIC {
            return Err(PetalSonicError::Configuration(
                "invalid native HRTF magic; expected PETHRTF".to_string(),
            ));
        }

        let version = reader.read_u32()?;
        if version != PETALHRTF_VERSION {
            return Err(PetalSonicError::Configuration(format!(
                "unsupported native HRTF version {version}; expected {PETALHRTF_VERSION}"
            )));
        }

        let sample_rate = reader.read_u32()?;
        let direction_count = reader.read_u32()? as usize;
        let taps = reader.read_u32()? as usize;
        validate_petalhrtf_size(bytes.len(), direction_count, taps)?;

        let mut directions = Vec::with_capacity(direction_count);
        for _ in 0..direction_count {
            let direction = Vec3::new(reader.read_f32()?, reader.read_f32()?, reader.read_f32()?);

            let mut left = Vec::with_capacity(taps);
            for _ in 0..taps {
                left.push(reader.read_f32()?);
            }

            let mut right = Vec::with_capacity(taps);
            for _ in 0..taps {
                right.push(reader.read_f32()?);
            }

            directions.push(NativeHrtfDirection::new(direction, left, right));
        }
        reader.finish()?;

        Self::new(sample_rate, directions)
    }

    /// Encode this table as `.petalhrtf` bytes.
    pub fn to_petalhrtf_bytes(&self) -> Vec<u8> {
        let direction_record_bytes = (3 + self.taps * 2) * F32_BYTES;
        let total_bytes = PETALHRTF_HEADER_BYTES + self.directions.len() * direction_record_bytes;
        let mut bytes = Vec::with_capacity(total_bytes);

        bytes.extend_from_slice(PETALHRTF_MAGIC);
        bytes.extend_from_slice(&PETALHRTF_VERSION.to_le_bytes());
        bytes.extend_from_slice(&self.sample_rate.to_le_bytes());
        bytes.extend_from_slice(&(self.directions.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(self.taps as u32).to_le_bytes());

        for entry in &self.directions {
            bytes.extend_from_slice(&entry.direction.x.to_le_bytes());
            bytes.extend_from_slice(&entry.direction.y.to_le_bytes());
            bytes.extend_from_slice(&entry.direction.z.to_le_bytes());
            for sample in &entry.left {
                bytes.extend_from_slice(&sample.to_le_bytes());
            }
            for sample in &entry.right {
                bytes.extend_from_slice(&sample.to_le_bytes());
            }
        }

        bytes
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

struct PetalHrtfReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> PetalHrtfReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn read_bytes(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self.offset.checked_add(count).ok_or_else(|| {
            PetalSonicError::Configuration("native HRTF read offset overflow".to_string())
        })?;

        let Some(slice) = self.bytes.get(self.offset..end) else {
            return Err(PetalSonicError::Configuration(
                "native HRTF file ended unexpectedly".to_string(),
            ));
        };

        self.offset = end;
        Ok(slice)
    }

    fn read_u32(&mut self) -> Result<u32> {
        let bytes = self.read_bytes(4)?;
        Ok(u32::from_le_bytes(
            bytes.try_into().expect("u32 slice size"),
        ))
    }

    fn read_f32(&mut self) -> Result<f32> {
        let bytes = self.read_bytes(4)?;
        Ok(f32::from_le_bytes(
            bytes.try_into().expect("f32 slice size"),
        ))
    }

    fn finish(&self) -> Result<()> {
        if self.offset != self.bytes.len() {
            return Err(PetalSonicError::Configuration(format!(
                "native HRTF file has {} trailing bytes",
                self.bytes.len() - self.offset
            )));
        }

        Ok(())
    }
}

fn validate_petalhrtf_size(bytes_len: usize, direction_count: usize, taps: usize) -> Result<()> {
    let taps_per_direction = taps.checked_mul(2).ok_or_else(|| {
        PetalSonicError::Configuration("native HRTF tap count overflow".to_string())
    })?;
    let floats_per_direction = 3usize.checked_add(taps_per_direction).ok_or_else(|| {
        PetalSonicError::Configuration("native HRTF direction size overflow".to_string())
    })?;
    let bytes_per_direction = floats_per_direction.checked_mul(F32_BYTES).ok_or_else(|| {
        PetalSonicError::Configuration("native HRTF direction byte size overflow".to_string())
    })?;
    let directions_bytes = direction_count
        .checked_mul(bytes_per_direction)
        .ok_or_else(|| {
            PetalSonicError::Configuration("native HRTF table byte size overflow".to_string())
        })?;
    let expected_bytes = PETALHRTF_HEADER_BYTES
        .checked_add(directions_bytes)
        .ok_or_else(|| {
            PetalSonicError::Configuration("native HRTF total byte size overflow".to_string())
        })?;

    if bytes_len != expected_bytes {
        return Err(PetalSonicError::Configuration(format!(
            "native HRTF file size mismatch: expected {expected_bytes} bytes, got {bytes_len}"
        )));
    }

    Ok(())
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
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    fn unique_test_suffix() -> usize {
        static NEXT_SUFFIX: AtomicUsize = AtomicUsize::new(0);
        NEXT_SUFFIX.fetch_add(1, Ordering::Relaxed)
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
    fn petalhrtf_bytes_round_trip_table_data() {
        let table = NativeHrtfTable::new(
            44_100,
            vec![
                NativeHrtfDirection::new(Vec3::X * 2.0, vec![1.0, 0.5], vec![0.25, 0.0]),
                NativeHrtfDirection::new(Vec3::NEG_Z, vec![0.0, -0.5], vec![1.0, 0.125]),
            ],
        )
        .unwrap();

        let decoded = NativeHrtfTable::from_petalhrtf_bytes(&table.to_petalhrtf_bytes()).unwrap();

        assert_eq!(decoded.sample_rate(), 44_100);
        assert_eq!(decoded.direction_count(), 2);
        assert_eq!(decoded.taps(), 2);
        let first = decoded.direction(0).unwrap();
        assert!((first.direction - Vec3::X).length() < 1e-6);
        assert_eq!(first.left, [1.0, 0.5]);
        assert_eq!(first.right, [0.25, 0.0]);
    }

    #[test]
    fn petalhrtf_file_loader_reads_round_trip_bytes() {
        let table = NativeHrtfTable::new(
            48_000,
            vec![NativeHrtfDirection::new(Vec3::NEG_Z, vec![1.0], vec![0.5])],
        )
        .unwrap();
        let path = std::env::temp_dir().join(format!(
            "petalsonic-test-{}-{}.petalhrtf",
            std::process::id(),
            unique_test_suffix()
        ));

        std::fs::write(&path, table.to_petalhrtf_bytes()).unwrap();
        let decoded = NativeHrtfTable::from_petalhrtf_file(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        assert_eq!(decoded.sample_rate(), 48_000);
        assert_eq!(decoded.direction_count(), 1);
        assert_eq!(decoded.taps(), 1);
    }

    #[test]
    fn petalhrtf_rejects_bad_magic_version_and_size() {
        let table = NativeHrtfTable::new(
            48_000,
            vec![NativeHrtfDirection::new(Vec3::NEG_Z, vec![1.0], vec![1.0])],
        )
        .unwrap();
        let mut bytes = table.to_petalhrtf_bytes();

        let mut bad_magic = bytes.clone();
        bad_magic[0] = b'X';
        assert!(NativeHrtfTable::from_petalhrtf_bytes(&bad_magic).is_err());

        let mut bad_version = bytes.clone();
        bad_version[8..12].copy_from_slice(&999u32.to_le_bytes());
        assert!(NativeHrtfTable::from_petalhrtf_bytes(&bad_version).is_err());

        bytes.push(0);
        assert!(NativeHrtfTable::from_petalhrtf_bytes(&bytes).is_err());
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
