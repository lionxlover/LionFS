#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptimizationProfile {
    PerformanceFirst,
    Balanced,
    PowerSaving,
    MaximumReliability,
    MaximumCapacity,
    LowLatency,
}

pub struct PolicyEngine {
    pub current_profile: OptimizationProfile,
}

impl PolicyEngine {
    pub fn new(profile: OptimizationProfile) -> Self {
        Self {
            current_profile: profile,
        }
    }

    pub fn set_profile(&mut self, profile: OptimizationProfile) {
        self.current_profile = profile;
    }
}

impl Default for PolicyEngine {
    fn default() -> Self {
        Self::new(OptimizationProfile::Balanced)
    }
}
