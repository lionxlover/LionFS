use std::collections::HashMap;
use std::sync::Mutex;

/// A lightweight predictive engine based on Markov chains
/// tracks sequences of block accesses to predict read-ahead targets.
pub struct PredictiveReadEngine {
    // Maps a logical block to the next block and a confidence score
    transitions: Mutex<HashMap<u64, (u64, u8)>>,
}

impl Default for PredictiveReadEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl PredictiveReadEngine {
    pub fn new() -> Self {
        Self {
            transitions: Mutex::new(HashMap::new()),
        }
    }

    pub fn record_sequence(&self, block_a: u64, block_b: u64) {
        let mut map = self.transitions.lock().unwrap();
        let entry = map.entry(block_a).or_insert((block_b, 0));

        if entry.0 == block_b {
            // Increase confidence if it's the same sequence
            entry.1 = entry.1.saturating_add(1);
        } else {
            // Decrease confidence if it changed, eventually swapping
            if entry.1 == 0 {
                entry.0 = block_b;
                entry.1 = 1;
            } else {
                entry.1 -= 1;
            }
        }
    }

    pub fn predict_next(&self, current_block: u64) -> Option<u64> {
        let map = self.transitions.lock().unwrap();
        if let Some((next_block, confidence)) = map.get(&current_block) {
            // Only predict if confidence is high enough
            if *confidence > 2 {
                return Some(*next_block);
            }
        }
        None
    }
}
