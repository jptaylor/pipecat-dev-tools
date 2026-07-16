//! Downsampled waveform storage: min/max per fixed time bin, with coarser
//! LOD levels so long sessions render fast at any zoom.

pub const FINE_BIN_MS: u64 = 2;
pub const LOD_FACTOR: usize = 8; // each level is 8x coarser: 2ms, 16ms, 128ms, 1024ms
pub const NUM_LEVELS: usize = 4;

#[derive(Debug, Clone, Copy, Default)]
pub struct Bin {
    pub min: f32,
    pub max: f32,
}

impl Bin {
    pub fn merge(&mut self, other: &Bin) {
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
    }

    pub fn is_empty(&self) -> bool {
        self.min == 0.0 && self.max == 0.0
    }
}

#[derive(Default)]
pub struct LaneBins {
    levels: [Vec<Bin>; NUM_LEVELS],
}

impl LaneBins {
    pub fn clear(&mut self) {
        for l in &mut self.levels {
            l.clear();
        }
    }

    /// Duration of one bin at `level`, in ms.
    pub fn bin_ms(level: usize) -> u64 {
        FINE_BIN_MS * (LOD_FACTOR as u64).pow(level as u32)
    }

    /// Pick the finest level whose bins are at least ~1px wide.
    pub fn level_for_zoom(px_per_ms: f32) -> usize {
        for level in 0..NUM_LEVELS {
            let bin_px = Self::bin_ms(level) as f32 * px_per_ms;
            if bin_px >= 1.0 {
                return level;
            }
        }
        NUM_LEVELS - 1
    }

    pub fn level(&self, level: usize) -> &[Bin] {
        &self.levels[level.min(NUM_LEVELS - 1)]
    }

    /// Append a fine bin at the given fine index (ms-since-session / 2ms).
    /// Pads gaps with empty bins and merges duplicates so the array stays
    /// aligned with the timeline even across clock-sync corrections.
    pub fn push_fine_at(&mut self, idx: usize, bin: Bin) {
        let len = self.levels[0].len();
        if idx < len {
            // Clock nudged backwards: merge into the most recent bin.
            if let Some(last) = self.levels[0].last_mut() {
                last.merge(&bin);
            }
            return;
        }
        while self.levels[0].len() < idx {
            self.push_fine(Bin::default());
        }
        self.push_fine(bin);
    }

    fn push_fine(&mut self, bin: Bin) {
        self.levels[0].push(bin);
        // Cascade completed groups into coarser levels.
        for level in 1..NUM_LEVELS {
            let finer_len = self.levels[level - 1].len();
            if finer_len % LOD_FACTOR != 0 || finer_len == 0 {
                break;
            }
            let start = finer_len - LOD_FACTOR;
            let mut merged = self.levels[level - 1][start];
            for b in &self.levels[level - 1][start + 1..finer_len] {
                merged.merge(b);
            }
            self.levels[level].push(merged);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lod_cascade() {
        let mut bins = LaneBins::default();
        for i in 0..64 {
            bins.push_fine_at(i, Bin { min: -1.0, max: i as f32 });
        }
        assert_eq!(bins.level(0).len(), 64);
        assert_eq!(bins.level(1).len(), 8);
        assert_eq!(bins.level(2).len(), 1);
        assert_eq!(bins.level(1)[0].max, 7.0);
        assert_eq!(bins.level(2)[0].max, 63.0);
    }

    #[test]
    fn gap_padding() {
        let mut bins = LaneBins::default();
        bins.push_fine_at(0, Bin { min: 0.0, max: 1.0 });
        bins.push_fine_at(5, Bin { min: 0.0, max: 2.0 });
        assert_eq!(bins.level(0).len(), 6);
        assert!(bins.level(0)[3].is_empty());
    }
}
