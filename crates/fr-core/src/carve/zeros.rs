//! Empty-sector runs from the sweep, for the existing partial-file boundary rule.
//! Missing observations never establish absence; a bounded index falls back to reads.
const UNIT: u64 = 512;
const RANGES: usize = 1 << 18;

#[derive(Default)]
pub(super) struct Index {
    zeros: Vec<(u64, u64)>,
    coverage: Vec<(u64, u64)>,
    limited: bool,
    trusted: bool,
}

impl Index {
    pub fn trust(&mut self, trusted: bool) {
        self.trusted = trusted;
    }

    pub fn observe(&mut self, at: u64, bytes: &[u8]) {
        self.observe_bounded(at, bytes, RANGES);
    }

    fn observe_bounded(&mut self, at: u64, bytes: &[u8], limit: usize) {
        if self.limited || !at.is_multiple_of(UNIT) || bytes.len() < UNIT as usize {
            return;
        }
        if self.coverage.last().is_none_or(|r| r.1 != at) {
            if self.zeros.len() + self.coverage.len() >= limit {
                self.limited = true;
                return;
            }
            self.coverage.push((at, at));
        }
        let mut end = at;
        for bytes in bytes.chunks_exact(UNIT as usize) {
            if bytes.iter().all(|&b| b == 0) {
                if let Some(last) = self.zeros.last_mut().filter(|r| r.1 == end) {
                    last.1 += UNIT;
                } else {
                    if self.zeros.len() + self.coverage.len() >= limit {
                        self.limited = true;
                        break;
                    }
                    self.zeros.push((end, end + UNIT));
                }
            }
            end += UNIT;
        }
        self.coverage.last_mut().unwrap().1 = end;
    }

    /// Same first zero probe, next-candidate limit and partial tail as fallback_len.
    pub fn length(&self, start: u64, cap: u64, limit: u64, stride: u64) -> Option<u64> {
        if !self.trusted || stride == 0 || !start.is_multiple_of(UNIT) || !stride.is_multiple_of(UNIT) {
            return None;
        }
        let probe = stride.min(4096);
        let last = limit.saturating_sub(1).checked_div(stride)?.min(cap.saturating_sub(probe) / stride);
        if last == 0 {
            return Some(limit);
        }
        let from = start.checked_add(stride)?;
        let required = start.checked_add(last.checked_mul(stride)?)?.checked_add(probe)?;
        let i = self.coverage.partition_point(|r| r.0 <= from);
        if i == 0 || self.coverage[i - 1].1 < required {
            return None;
        }
        let first = self.zeros.partition_point(|r| r.1 <= from);
        for &(s, e) in &self.zeros[first..] {
            let off = s.saturating_sub(start).div_ceil(stride).max(1).checked_mul(stride)?;
            if off > last * stride {
                break;
            }
            if start.checked_add(off)?.checked_add(probe)? <= e {
                return Some(off);
            }
        }
        Some(limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_probes_equal_direct_reads_for_strides_caps_and_limits() {
        let mut bytes = vec![0; 128 << 10];
        for (i, sector) in bytes.chunks_mut(512).enumerate() {
            if i % 31 < 11 {
                sector[i % 512] = 42;
            }
        }
        let mut index = Index::default();
        for (i, b) in bytes.chunks(8192).enumerate() {
            index.observe(i as u64 * 8192, b);
        }
        index.trust(true);
        for start in (0..32 << 10).step_by(512) {
            for stride in [512u64, 1024, 4096, 8192, 16384] {
                for cap in [511, 512, 513, 8191, 8192, 65535, 65536] {
                    for limit in [cap, cap / 2, cap.min(stride * 2)] {
                        let mut expected = limit;
                        for off in (stride..limit).step_by(stride as usize) {
                            if off + stride.min(4096) > cap {
                                break;
                            }
                            let b = &bytes[(start + off) as usize..(start + off + stride.min(4096)) as usize];
                            if b.iter().all(|&b| b == 0) {
                                expected = off;
                                break;
                            }
                        }
                        assert_eq!(index.length(start, cap, limit, stride), Some(expected));
                    }
                }
            }
        }
    }

    #[test]
    fn gaps_limits_alignment_and_partial_observations_require_direct_reads() {
        let mut index = Index::default();
        index.observe_bounded(0, &[42; 1024], 2);
        index.observe_bounded(1024, &[0; 512], 2);
        index.observe_bounded(1536, &[42; 512], 2);
        index.observe_bounded(2048, &[0; 512], 2); // Budget reached, no negative inference past 2048.
        index.trust(true);
        assert_eq!(index.length(0, 2048, 2048, 512), Some(1024));
        assert_eq!(index.length(0, 2560, 2560, 512), None);
        assert_eq!(index.length(7, 512, 512, 512), None);
        let mut gap = Index::default();
        gap.observe(0, &[42; 1024]);
        gap.observe(1536, &[42; 1024]);
        gap.trust(true);
        assert_eq!(gap.length(0, 2560, 2560, 512), None);
        gap.observe(2560, &[0; 511]);
        assert_eq!(gap.length(1536, 1536, 1536, 512), None);
        gap.trust(false);
        assert_eq!(gap.length(0, 1024, 1024, 512), None);
    }
}
