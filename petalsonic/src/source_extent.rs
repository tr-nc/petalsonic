use crate::error::{PetalSonicError, Result};
use crate::math::Vec3;
use std::sync::Arc;

/// Maximum number of stable samples in one source extent.
///
/// A producer with a denser representation must reduce it to stable representatives before
/// publication so worker and render costs stay bounded.
pub const MAX_EXTENT_SAMPLES: usize = 8;

/// Defensive local-radius bound used to keep distance and ray arithmetic reliable.
///
/// PetalSonic does not otherwise prescribe an application's world scale.
pub const MAX_EXTENT_RADIUS_WORLD_UNITS: f32 = 1_000_000.0;

/// Caller-stable identity for one weighted extent sample.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ExtentSampleId(pub u64);

/// One immutable local-space representative of source power.
#[derive(Clone, Debug, PartialEq)]
pub struct ExtentSample {
    id: ExtentSampleId,
    local_position: Vec3,
    power_weight: f32,
}

impl ExtentSample {
    /// Creates one sample with positive relative power.
    ///
    /// Relative powers are normalized once by [`SourceExtent::weighted_samples`].
    pub fn new(id: ExtentSampleId, local_position: Vec3, power_weight: f32) -> Result<Self> {
        if !local_position.is_finite() {
            return Err(invalid_extent("sample local positions must be finite"));
        }
        let radius = local_position.length();
        if !radius.is_finite() || radius > MAX_EXTENT_RADIUS_WORLD_UNITS {
            return Err(invalid_extent(format!(
                "sample radius must not exceed {MAX_EXTENT_RADIUS_WORLD_UNITS} world units"
            )));
        }
        if !power_weight.is_finite() || power_weight <= 0.0 {
            return Err(invalid_extent(
                "sample power weights must be finite and greater than zero",
            ));
        }
        Ok(Self {
            id,
            local_position,
            power_weight,
        })
    }

    pub fn id(&self) -> ExtentSampleId {
        self.id
    }

    pub fn local_position(&self) -> Vec3 {
        self.local_position
    }

    /// Normalized fractional source power after extent construction.
    pub fn power_weight(&self) -> f32 {
        self.power_weight
    }
}

/// Validated, deterministically ordered weighted source representatives.
#[derive(Clone, Debug, PartialEq)]
pub struct WeightedSamples {
    samples: Arc<[ExtentSample]>,
    bounding_radius: f32,
}

impl WeightedSamples {
    fn new(mut samples: Vec<ExtentSample>) -> Result<Self> {
        if samples.is_empty() || samples.len() > MAX_EXTENT_SAMPLES {
            return Err(invalid_extent(format!(
                "weighted samples must contain 1..={MAX_EXTENT_SAMPLES} entries"
            )));
        }
        samples.sort_by_key(ExtentSample::id);
        if samples.windows(2).any(|pair| pair[0].id == pair[1].id) {
            return Err(invalid_extent(
                "weighted samples must have unique stable IDs",
            ));
        }

        let total_power = samples
            .iter()
            .map(|sample| f64::from(sample.power_weight))
            .sum::<f64>();
        if !total_power.is_finite() || total_power <= 0.0 {
            return Err(invalid_extent(
                "weighted sample power must have a finite positive sum",
            ));
        }
        for sample in &mut samples {
            sample.power_weight = (f64::from(sample.power_weight) / total_power) as f32;
        }
        // Correct f32 normalization drift deterministically on the greatest stable ID.
        let normalized_sum = samples
            .iter()
            .map(|sample| sample.power_weight)
            .sum::<f32>();
        if let Some(last) = samples.last_mut() {
            last.power_weight += 1.0 - normalized_sum;
        }
        if samples
            .iter()
            .any(|sample| !sample.power_weight.is_finite() || sample.power_weight <= 0.0)
        {
            return Err(invalid_extent(
                "normalization produced a non-positive sample power",
            ));
        }

        let bounding_radius = samples
            .iter()
            .map(|sample| sample.local_position.length())
            .fold(0.0_f32, f32::max);
        Ok(Self {
            samples: samples.into(),
            bounding_radius,
        })
    }

    pub fn samples(&self) -> &[ExtentSample] {
        &self.samples
    }

    pub fn bounding_radius(&self) -> f32 {
        self.bounding_radius
    }
}

/// Immutable local domain of one Voice's source power.
///
/// `Point` is the compatibility default. `WeightedSamples` is a finite discretization of an
/// arbitrary area or volume; future analytic shapes can extend this enum without changing route
/// placement or occlusion policy.
#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq)]
pub enum SourceExtent {
    #[default]
    Point,
    WeightedSamples(WeightedSamples),
}

impl SourceExtent {
    /// Validates, normalizes, and stable-ID-orders a weighted source extent.
    pub fn weighted_samples(samples: Vec<ExtentSample>) -> Result<Self> {
        Ok(Self::WeightedSamples(WeightedSamples::new(samples)?))
    }

    pub fn sample_count(&self) -> usize {
        match self {
            Self::Point => 1,
            Self::WeightedSamples(weighted) => weighted.samples.len(),
        }
    }

    pub fn weighted(&self) -> Option<&WeightedSamples> {
        match self {
            Self::Point => None,
            Self::WeightedSamples(weighted) => Some(weighted),
        }
    }

    pub fn bounding_radius(&self) -> f32 {
        match self {
            Self::Point => 0.0,
            Self::WeightedSamples(weighted) => weighted.bounding_radius,
        }
    }
}

fn invalid_extent(reason: impl Into<String>) -> PetalSonicError {
    PetalSonicError::InvalidConfiguration {
        field: "source_extent",
        reason: reason.into(),
    }
}
