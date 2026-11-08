//! Shared end markers from the sweep. Absence is conclusive only over observed bytes.
const TGA: &[u8] = b"TRUEVISION-XFILE.\0";
const PDF: &[u8] = b"%%EOF";
const TAIL: usize = 17;

#[derive(Default)]
pub(super) struct Index {
    tga: Vec<u64>,
    pdf: Vec<u64>,
    coverage: Vec<(u64, u64)>,
    tail: Vec<u8>,
    trusted: bool,
}

impl Index {
    pub fn trust(&mut self, trusted: bool) {
        self.trusted = trusted;
    }

    pub fn observe(&mut self, at: u64, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let contiguous = self.coverage.last().is_some_and(|r| r.1 == at);
        if contiguous && !self.tail.is_empty() {
            let mut join = [0u8; TAIL * 2];
            let old = self.tail.len();
            join[..old].copy_from_slice(&self.tail);
            let n = bytes.len().min(TAIL);
            join[old..old + n].copy_from_slice(&bytes[..n]);
            self.collect(at - old as u64, &join[..old + n]);
        }
        self.collect(at, bytes);
        if contiguous {
            self.coverage.last_mut().unwrap().1 = at + bytes.len() as u64;
        } else {
            self.coverage.push((at, at + bytes.len() as u64));
            self.tail.clear();
        }
        if bytes.len() >= TAIL {
            self.tail.clear();
            self.tail.extend_from_slice(&bytes[bytes.len() - TAIL..]);
        } else {
            self.tail.extend_from_slice(bytes);
            let excess = self.tail.len().saturating_sub(TAIL);
            self.tail.drain(..excess);
        }
    }

    fn collect(&mut self, at: u64, bytes: &[u8]) {
        for (pat, offsets) in [(TGA, &mut self.tga), (PDF, &mut self.pdf)] {
            for i in memchr::memmem::find_iter(bytes, pat) {
                let offset = at + i as u64;
                if offsets.last().is_none_or(|last| *last < offset) {
                    offsets.push(offset);
                }
            }
        }
    }

    /// Outer None means direct reading is required. Match starts must precede `end`,
    /// but, like Stream::find, a signature may extend past it up to the candidate cap.
    pub fn find(&self, from: u64, end: u64, cap: u64, pat: &[u8]) -> Option<Option<u64>> {
        let offsets = if pat == TGA {
            &self.tga
        } else if pat == PDF {
            &self.pdf
        } else {
            return None;
        };
        if !self.trusted {
            return None;
        }
        if from >= end {
            return Some(None);
        }
        let required = end.saturating_add(pat.len() as u64 - 1).min(cap);
        let i = self.coverage.partition_point(|r| r.0 <= from);
        if i == 0 || self.coverage[i - 1].1 < required {
            return None;
        }
        let i = offsets.partition_point(|&p| p < from);
        Some(offsets.get(i).copied().filter(|&p| p < end && p + pat.len() as u64 <= cap))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_markers_gaps_and_unreadable_ranges() {
        let mut b = [b'X'; 200];
        b[62..80].copy_from_slice(TGA);
        b[120..125].copy_from_slice(PDF);
        let mut ix = Index::default();
        for (i, part) in b.chunks(7).enumerate() {
            ix.observe(i as u64 * 7, part);
        }
        assert_eq!(ix.find(0, 200, 200, TGA), None);
        ix.trust(true);
        assert_eq!(ix.find(0, 63, 200, TGA), Some(Some(62)));
        assert_eq!(ix.find(0, 62, 200, TGA), Some(None));
        assert_eq!(ix.find(0, 200, 200, PDF), Some(Some(120)));
        ix.observe(300, b"%%EOF");
        assert_eq!(ix.find(200, 305, 305, PDF), None);
        ix.trust(false);
        assert_eq!(ix.find(0, 200, 200, PDF), None);
    }
}
