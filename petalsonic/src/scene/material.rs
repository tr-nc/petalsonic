//! Acoustic material properties for spatial audio simulation.
//!
//! Materials define how surfaces interact with sound through three frequency bands
//! (400 Hz, 2.5 KHz, 15 KHz) as used by Steam Audio.

/// Acoustic properties of a surface material.
///
/// Materials define how sound interacts with surfaces through three properties:
/// - **Absorption**: Energy absorbed (not reflected) at each frequency band
/// - **Scattering**: How diffuse (vs. specular) reflections are
/// - **Transmission**: Energy transmitted through the surface (for occlusion)
///
/// Values are defined across three frequency bands:
/// - Low (400 Hz): Bass/low-frequency sounds
/// - Mid (2.5 KHz): Speech and mid-range content
/// - High (15 KHz): High-frequency detail
///
/// # Example
///
/// ```
/// use petalsonic_core::scene::AudioMaterial;
///
/// // Use a preset material
/// let wall_material = AudioMaterial::CONCRETE;
///
/// // Or create a custom material
/// let custom_material = AudioMaterial {
///     absorption: [0.10, 0.20, 0.30],
///     scattering: 0.05,
///     transmission: [0.10, 0.05, 0.03],
/// };
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioMaterial {
    /// Fraction of sound energy absorbed at [low, mid, high] frequencies (0.0 - 1.0)
    ///
    /// Higher values = "softer" surface (carpet, fabric)
    /// Lower values = "harder" surface (metal, glass)
    pub absorption: [f32; 3],

    /// Fraction of sound energy scattered in random direction on reflection (0.0 - 1.0)
    ///
    /// 0.0 = pure specular (mirror-like), 1.0 = pure diffuse (scattered)
    /// Most materials use 0.05 for slight diffusion
    pub scattering: f32,

    /// Fraction of sound energy transmitted through surface at [low, mid, high] frequencies (0.0 - 1.0)
    ///
    /// Used for direct occlusion calculations
    /// Higher values = more transparent (thin wood, glass)
    /// Lower values = more blocking (concrete, metal)
    pub transmission: [f32; 3],
}

impl AudioMaterial {
    /// Generic default material with moderate acoustic properties
    pub const GENERIC: Self = Self {
        absorption: [0.10, 0.20, 0.30],
        scattering: 0.05,
        transmission: [0.100, 0.050, 0.030],
    };

    /// Brick material - moderately reflective, good sound blocking
    pub const BRICK: Self = Self {
        absorption: [0.03, 0.04, 0.07],
        scattering: 0.05,
        transmission: [0.015, 0.015, 0.015],
    };

    /// Concrete material - very reflective, excellent sound blocking
    pub const CONCRETE: Self = Self {
        absorption: [0.05, 0.07, 0.08],
        scattering: 0.05,
        transmission: [0.015, 0.002, 0.001],
    };

    /// Ceramic material - highly reflective, moderate transmission
    pub const CERAMIC: Self = Self {
        absorption: [0.01, 0.02, 0.02],
        scattering: 0.05,
        transmission: [0.060, 0.044, 0.011],
    };

    /// Gravel material - highly absorptive, moderate blocking
    pub const GRAVEL: Self = Self {
        absorption: [0.60, 0.70, 0.80],
        scattering: 0.05,
        transmission: [0.031, 0.012, 0.008],
    };

    /// Carpet material - highly absorptive (especially high frequencies)
    pub const CARPET: Self = Self {
        absorption: [0.24, 0.69, 0.73],
        scattering: 0.05,
        transmission: [0.020, 0.005, 0.003],
    };

    /// Glass material - reflective with moderate transmission
    pub const GLASS: Self = Self {
        absorption: [0.06, 0.03, 0.02],
        scattering: 0.05,
        transmission: [0.060, 0.044, 0.011],
    };

    /// Plaster material - moderately reflective
    pub const PLASTER: Self = Self {
        absorption: [0.12, 0.06, 0.04],
        scattering: 0.05,
        transmission: [0.056, 0.056, 0.004],
    };

    /// Wood material - moderately absorptive
    pub const WOOD: Self = Self {
        absorption: [0.11, 0.07, 0.06],
        scattering: 0.05,
        transmission: [0.070, 0.014, 0.005],
    };

    /// Metal material - variable absorption, good low-frequency transmission
    pub const METAL: Self = Self {
        absorption: [0.20, 0.07, 0.06],
        scattering: 0.05,
        transmission: [0.200, 0.025, 0.010],
    };

    /// Rock material - moderately absorptive, excellent sound blocking
    pub const ROCK: Self = Self {
        absorption: [0.13, 0.20, 0.24],
        scattering: 0.05,
        transmission: [0.015, 0.002, 0.001],
    };

    /// Validates that all material properties are within valid range [0.0, 1.0]
    pub fn validate(&self) -> Result<(), &'static str> {
        for &val in &self.absorption {
            if !(0.0..=1.0).contains(&val) {
                return Err("Absorption values must be between 0.0 and 1.0");
            }
        }

        if !(0.0..=1.0).contains(&self.scattering) {
            return Err("Scattering value must be between 0.0 and 1.0");
        }

        for &val in &self.transmission {
            if !(0.0..=1.0).contains(&val) {
                return Err("Transmission values must be between 0.0 and 1.0");
            }
        }

        Ok(())
    }
}

impl Default for AudioMaterial {
    fn default() -> Self {
        Self::GENERIC
    }
}

/// Material lookup table for ray tracer callbacks.
///
/// Maps material indices (u8) to AudioMaterial properties. Used by ray tracers
/// to return material information to the audio engine.
///
/// # Example
///
/// ```
/// use petalsonic_core::scene::{AudioMaterial, MaterialTable};
///
/// let mut materials = MaterialTable::new();
/// let wall_idx = materials.add(AudioMaterial::CONCRETE);
/// let floor_idx = materials.add(AudioMaterial::WOOD);
/// let ceiling_idx = materials.add(AudioMaterial::PLASTER);
///
/// // Later, retrieve materials by index
/// if let Some(material) = materials.get(wall_idx) {
///     println!("Wall absorption: {:?}", material.absorption);
/// }
/// ```
#[derive(Debug, Clone)]
pub struct MaterialTable {
    materials: Vec<AudioMaterial>,
}

impl MaterialTable {
    /// Creates a new empty material table
    pub fn new() -> Self {
        Self {
            materials: Vec::new(),
        }
    }

    /// Creates a material table pre-loaded with all common material presets
    ///
    /// Materials are added in the following order:
    /// - 0: GENERIC
    /// - 1: BRICK
    /// - 2: CONCRETE
    /// - 3: CERAMIC
    /// - 4: GRAVEL
    /// - 5: CARPET
    /// - 6: GLASS
    /// - 7: PLASTER
    /// - 8: WOOD
    /// - 9: METAL
    /// - 10: ROCK
    pub fn with_presets() -> Self {
        let mut table = Self::new();
        table.add(AudioMaterial::GENERIC);
        table.add(AudioMaterial::BRICK);
        table.add(AudioMaterial::CONCRETE);
        table.add(AudioMaterial::CERAMIC);
        table.add(AudioMaterial::GRAVEL);
        table.add(AudioMaterial::CARPET);
        table.add(AudioMaterial::GLASS);
        table.add(AudioMaterial::PLASTER);
        table.add(AudioMaterial::WOOD);
        table.add(AudioMaterial::METAL);
        table.add(AudioMaterial::ROCK);
        table
    }

    /// Adds a material to the table and returns its index
    ///
    /// # Panics
    ///
    /// Panics if the table already contains 256 materials (the maximum)
    ///
    /// # Errors
    ///
    /// Returns an error if the material's properties are invalid
    pub fn add(&mut self, material: AudioMaterial) -> u8 {
        material.validate().expect("Invalid material properties");

        if self.materials.len() >= 256 {
            panic!("Material table is full (max 256 materials)");
        }

        let index = self.materials.len() as u8;
        self.materials.push(material);
        index
    }

    /// Retrieves a material by its index
    pub fn get(&self, index: u8) -> Option<&AudioMaterial> {
        self.materials.get(index as usize)
    }

    /// Returns the number of materials in the table
    pub fn len(&self) -> usize {
        self.materials.len()
    }

    /// Returns true if the table contains no materials
    pub fn is_empty(&self) -> bool {
        self.materials.is_empty()
    }

    /// Returns an iterator over all materials and their indices
    pub fn iter(&self) -> impl Iterator<Item = (u8, &AudioMaterial)> {
        self.materials.iter().enumerate().map(|(i, m)| (i as u8, m))
    }
}

impl Default for MaterialTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_material_validation() {
        let valid_material = AudioMaterial::CONCRETE;
        assert!(valid_material.validate().is_ok());

        let invalid_absorption = AudioMaterial {
            absorption: [0.5, 1.5, 0.3],
            scattering: 0.05,
            transmission: [0.1, 0.05, 0.03],
        };
        assert!(invalid_absorption.validate().is_err());

        let invalid_scattering = AudioMaterial {
            absorption: [0.5, 0.5, 0.3],
            scattering: 1.5,
            transmission: [0.1, 0.05, 0.03],
        };
        assert!(invalid_scattering.validate().is_err());
    }

    #[test]
    fn test_material_table() {
        let mut table = MaterialTable::new();
        assert_eq!(table.len(), 0);
        assert!(table.is_empty());

        let idx1 = table.add(AudioMaterial::CONCRETE);
        let idx2 = table.add(AudioMaterial::WOOD);

        assert_eq!(idx1, 0);
        assert_eq!(idx2, 1);
        assert_eq!(table.len(), 2);

        assert_eq!(table.get(idx1), Some(&AudioMaterial::CONCRETE));
        assert_eq!(table.get(idx2), Some(&AudioMaterial::WOOD));
        assert_eq!(table.get(99), None);
    }

    #[test]
    fn test_material_table_with_presets() {
        let table = MaterialTable::with_presets();
        assert_eq!(table.len(), 11);

        assert_eq!(table.get(0), Some(&AudioMaterial::GENERIC));
        assert_eq!(table.get(2), Some(&AudioMaterial::CONCRETE));
        assert_eq!(table.get(8), Some(&AudioMaterial::WOOD));
    }
}
