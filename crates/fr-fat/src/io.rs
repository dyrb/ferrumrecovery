//! Volume reads shared by the FAT and exFAT scanners.

use fr_core::source::{AlignedBuf, ReadSlot, Source};

/// A volume inside a source, read with bad-sector handling.
pub(crate) struct Vol<'a> {
    pub src: &'a Source,
    pub base: u64,
    pub limit: u64,
    slot: ReadSlot,
    buf: AlignedBuf,
    /// Bytes that could not be read so far.
    pub bad: u64,
}

impl<'a> Vol<'a> {
    pub fn new(src: &'a Source, base: u64) -> std::io::Result<Self> {
        Ok(Vol { src, base, limit: src.size.saturating_sub(base), slot: ReadSlot::new()?, buf: src.buffer((1 << 20) + 8192), bad: 0 })
    }

    /// `len` bytes at volume byte `off` (both sector aligned); unreadable parts are zeros.
    pub fn read(&mut self, off: u64, len: usize) -> Vec<u8> {
        let sec = self.src.sector as usize;
        let mut out = Vec::with_capacity(len);
        let mut k = 0usize;
        while k < len {
            let n = (len - k).min(1 << 20);
            if off.checked_add(k as u64 + n as u64).is_none_or(|end| end > self.limit) {
                self.bad += (len - k) as u64;
                out.resize(len, 0);
                break;
            }
            let absolute = self.base + off + k as u64;
            let pad = absolute as usize % sec;
            let rd = (n + pad).div_ceil(sec) * sec;
            self.bad += self.src.read_robust(&mut self.slot, absolute - pad as u64, &mut self.buf[..rd]);
            out.extend_from_slice(&self.buf[pad..pad + n]);
            k += n;
        }
        out
    }
}

/// Merge a list of clusters (already 0-based) into runs.
pub(crate) fn runs_of(clusters: &[u64]) -> Vec<fr_core::Run> {
    let mut out: Vec<fr_core::Run> = Vec::new();
    for &c in clusters {
        match out.last_mut() {
            Some(r) if r.lcn + r.len == c => r.len += 1,
            _ => out.push(fr_core::Run { lcn: c, len: 1 }),
        }
    }
    out
}

/// Set bits in `lcn..lcn+len` of an allocation bitmap whose bit 0 is cluster 0 of the
/// runs; clusters past its end count as set.
pub(crate) fn count_set(bm: &[u8], lcn: u64, len: u64) -> u64 {
    (lcn..lcn + len).filter(|&c| bm.get((c / 8) as usize).is_none_or(|b| b >> (c % 8) & 1 != 0)).count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::Run;

    #[test]
    fn runs_merge() {
        assert_eq!(runs_of(&[5, 6, 7, 10, 11, 3]), vec![Run { lcn: 5, len: 3 }, Run { lcn: 10, len: 2 }, Run { lcn: 3, len: 1 }]);
        assert!(runs_of(&[]).is_empty());
    }

    #[test]
    fn bitmap_counts() {
        let bm = [0b0000_0101u8, 0xFF];
        assert_eq!(count_set(&bm, 0, 3), 2);
        assert_eq!(count_set(&bm, 8, 8), 8);
        assert_eq!(count_set(&bm, 14, 4), 4, "past the end counts as set");
    }
}
