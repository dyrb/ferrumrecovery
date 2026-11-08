//! Random access to the bytes of a candidate file, from its first cluster up to a cap,
//! through a cached window.

use crate::source::{AlignedBuf, ReadSlot, Source};

const WINDOW: usize = 1 << 20;
const INITIAL_WINDOW: usize = 4096;

#[cfg(test)]
thread_local! { pub(super) static TEST_READ_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }

struct Candidates<'a> {
    starts: &'a [(u64, super::Fmt)],
    base: u64,
    unit: u64,
}

pub struct Stream<'a> {
    src: &'a Source,
    /// Absolute byte of the file's first byte (sector aligned).
    start: u64,
    cap: u64,
    /// Absolute source range, retained when moving to another candidate.
    win_off: u64,
    win_len: usize,
    buf: AlignedBuf,
    slot: ReadSlot,
    pub(super) zip: Option<Option<super::zip::Info>>,
    pub(super) zip_hint: Option<super::zip::End>,
    pub(super) zip_indexed: bool,
    read_ahead: usize,
    max_ahead: usize,
    boundaries: Option<(&'a super::boundaries::Index, u64)>,
    zeros: Option<(&'a super::zeros::Index, u64)>,
    rtf_candidates: Option<Candidates<'a>>,
    rtf_ends: std::collections::HashMap<u64, Option<u64>>,
    unreadable: bool,
    #[cfg(test)]
    pub(super) read_bytes: u64,
}

impl<'a> Stream<'a> {
    pub fn new(src: &'a Source, start: u64, cap: u64) -> std::io::Result<Self> {
        let cap = cap.min(src.size.saturating_sub(start));
        Ok(Stream {
            src,
            start,
            cap,
            win_off: u64::MAX,
            win_len: 0,
            buf: src.buffer(WINDOW + (64 << 10)),
            slot: ReadSlot::new()?,
            zip: None,
            zip_hint: None,
            zip_indexed: false,
            read_ahead: INITIAL_WINDOW,
            max_ahead: WINDOW,
            boundaries: None,
            zeros: None,
            rtf_candidates: None,
            rtf_ends: std::collections::HashMap::new(),
            unreadable: false,
            #[cfg(test)]
            read_bytes: 0,
        })
    }

    /// Bytes the file may span at most.
    pub fn cap(&self) -> u64 {
        self.cap
    }

    pub(super) fn reset(&mut self, start: u64, cap: u64) {
        self.start = start;
        self.cap = cap.min(self.src.size.saturating_sub(start));
        self.zip = None;
        self.zip_hint = None;
        self.zip_indexed = false;
        self.read_ahead = INITIAL_WINDOW;
        self.max_ahead = WINDOW;
        self.unreadable = false;
    }

    pub(super) fn metadata_reads(&mut self) {
        self.read_ahead = INITIAL_WINDOW;
        self.max_ahead = INITIAL_WINDOW;
    }

    pub(super) fn set_boundaries(&mut self, index: &'a super::boundaries::Index, base: u64) {
        self.boundaries = Some((index, base));
    }

    pub(super) fn set_zeros(&mut self, index: &'a super::zeros::Index, base: u64) {
        self.zeros = Some((index, base));
    }

    pub(super) fn indexed_fallback(&self, stride: u64, limit: u64) -> Option<u64> {
        let (index, base) = self.zeros?;
        index.length(self.start.checked_sub(base)?, self.cap, limit, stride)
    }

    pub(super) fn set_rtf_candidates(&mut self, candidates: &'a [(u64, super::Fmt)], base: u64, unit: u64) {
        self.rtf_candidates = Some(Candidates { starts: candidates, base, unit });
    }

    pub(super) fn is_rtf_candidate(&self, off: u64) -> bool {
        if off == 0 {
            return true;
        }
        let Some(candidates) = &self.rtf_candidates else { return false };
        let Some(at) = self.start.checked_add(off).and_then(|at| at.checked_sub(candidates.base)) else { return false };
        at.is_multiple_of(candidates.unit)
            && candidates
                .starts
                .binary_search_by_key(&(at / candidates.unit), |c| c.0)
                .ok()
                .is_some_and(|i| candidates.starts[i].1 == super::Fmt::Rtf)
    }

    pub(super) fn rtf_length(&self) -> Option<Option<u64>> {
        self.rtf_ends.get(&self.start).map(|end| end.and_then(|end| end.checked_sub(self.start)).filter(|len| *len <= self.cap))
    }

    pub(super) fn remember_rtf(&mut self, off: u64, end: Option<u64>) {
        if !self.unreadable {
            self.rtf_ends.insert(self.start + off, end.map(|end| self.start + end));
        }
    }

    /// `len` bytes at file offset `off` (at most 64 KiB), or `None` past the cap.
    pub fn bytes(&mut self, off: u64, len: usize) -> Option<&[u8]> {
        if off.checked_add(len as u64)? > self.cap || len > 64 << 10 {
            return None;
        }
        if len == 0 {
            return Some(&[]);
        }
        let abs = self.start.checked_add(off)?;
        let end = abs.checked_add(len as u64)?;
        let win_end = self.win_off.saturating_add(self.win_len as u64);
        if self.win_len == 0 || abs < self.win_off || end > win_end {
            // Small structural probes stay small; sequential payload parsing ramps up.
            if self.win_len != 0 && abs >= self.win_off && abs <= win_end.saturating_add(INITIAL_WINDOW as u64) {
                self.read_ahead = (self.read_ahead.max(len) * 4).min(self.max_ahead);
            } else {
                self.read_ahead = INITIAL_WINDOW;
            }
            let sec = self.src.sector as u64;
            let aligned = abs / sec * sec;
            let pad = (abs - aligned) as usize;
            let want = (self.read_ahead as u64).min(self.cap - off).max(len as u64) + pad as u64;
            let rd = want.div_ceil(sec) * sec;
            let rd = rd.min(self.buf.len() as u64);
            self.unreadable |= self.src.read_robust(&mut self.slot, aligned, &mut self.buf[..rd as usize]) != 0;
            #[cfg(test)]
            {
                self.read_bytes += rd;
                TEST_READ_BYTES.with(|n| n.set(n.get() + rd));
            }
            self.win_off = aligned;
            self.win_len = rd as usize;
        }
        let s = (abs - self.win_off) as usize;
        Some(&self.buf[s..s + len])
    }

    pub fn u8(&mut self, off: u64) -> Option<u8> {
        self.bytes(off, 1).map(|b| b[0])
    }
    pub fn u16le(&mut self, off: u64) -> Option<u16> {
        self.bytes(off, 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }
    pub fn u16be(&mut self, off: u64) -> Option<u16> {
        self.bytes(off, 2).map(|b| u16::from_be_bytes([b[0], b[1]]))
    }
    pub fn u32le(&mut self, off: u64) -> Option<u32> {
        self.bytes(off, 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    }
    pub fn u32be(&mut self, off: u64) -> Option<u32> {
        self.bytes(off, 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()))
    }
    pub fn u64le(&mut self, off: u64) -> Option<u64> {
        self.bytes(off, 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }
    pub fn u64be(&mut self, off: u64) -> Option<u64> {
        self.bytes(off, 8).map(|b| u64::from_be_bytes(b.try_into().unwrap()))
    }

    /// First occurrence of `pat` at or after `from`, before `limit` (and the cap).
    pub fn find(&mut self, from: u64, pat: &[u8], limit: u64) -> Option<u64> {
        let end = limit.min(self.cap);
        if let Some((index, base)) = self.boundaries
            && let Some(start) = self.start.checked_sub(base)
            && let Some(found) = index.find(start.checked_add(from)?, start.checked_add(end)?, start.checked_add(self.cap)?, pat)
        {
            return found.map(|at| at - start);
        }
        let step = 60u64 << 10;
        let mut p = from;
        while p < end {
            let n = step.min(end - p) as usize;
            let take = (n + pat.len() - 1).min((self.cap - p) as usize).min(64 << 10);
            let chunk = self.bytes(p, take)?;
            if let Some(i) = memchr::memmem::find(chunk, pat)
                && (i as u64) < n as u64
            {
                return Some(p + i as u64);
            }
            p += n as u64;
        }
        None
    }

    /// First byte equal to `b` at or after `from`, before the cap.
    pub fn find_byte(&mut self, from: u64, b: u8) -> Option<u64> {
        let mut p = from;
        while p < self.cap {
            let n = (60u64 << 10).min(self.cap - p) as usize;
            let chunk = self.bytes(p, n)?;
            if let Some(i) = memchr::memchr(b, chunk) {
                return Some(p + i as u64);
            }
            p += n as u64;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(name: &str, len: usize) -> (std::path::PathBuf, Source, Vec<u8>) {
        let path = std::env::temp_dir().join(format!("fr-stream-{}-{name}.img", std::process::id()));
        let bytes: Vec<_> = (0..len).map(|i| ((i * 17 + i / 512) % 251) as u8).collect();
        std::fs::write(&path, &bytes).unwrap();
        let src = Source::open(path.to_str().unwrap()).unwrap();
        (path, src, bytes)
    }

    #[test]
    fn retained_windows_obey_each_candidate_boundary() {
        let (path, src, bytes) = image("bounds", 128 << 10);
        let mut s = Stream::new(&src, 513, 10000).unwrap();
        assert_eq!(s.bytes(100, 32).unwrap(), &bytes[613..645]);
        let read = s.read_bytes;
        s.reset(1027, 4);
        assert_eq!(s.bytes(0, 4).unwrap(), &bytes[1027..1031]);
        assert_eq!(s.read_bytes, read);
        assert!(s.bytes(4, 1).is_none());
        assert!(s.bytes(u64::MAX, 2).is_none());
        s.reset(7011, 20);
        assert_eq!(s.bytes(0, 20).unwrap(), &bytes[7011..7031]);
        s.reset(0, 16);
        assert_eq!(s.bytes(0, 16).unwrap(), &bytes[..16]);
        s.reset(src.size - 7, 100);
        assert_eq!(s.bytes(0, 7).unwrap(), &bytes[bytes.len() - 7..]);
        assert!(s.bytes(0, 8).is_none());
        drop(s);
        drop(src);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn sequential_reads_grow_while_metadata_probes_stay_small() {
        let (path, src, bytes) = image("sequential", 4 << 20);
        let mut s = Stream::new(&src, 0, src.size).unwrap();
        for (i, part) in bytes.chunks(32 << 10).enumerate() {
            assert_eq!(s.bytes((i * (32 << 10)) as u64, part.len()).unwrap(), part);
        }
        assert_eq!(s.read_ahead, WINDOW);
        assert!(s.read_bytes < src.size + (src.size / 8));
        s.reset(0, src.size);
        s.metadata_reads();
        let read = s.read_bytes;
        for at in [0, 1 << 20, 2 << 20] {
            assert_eq!(s.bytes(at, 16).unwrap(), &bytes[at as usize..at as usize + 16]);
        }
        assert!(s.read_bytes - read <= 3 * (INITIAL_WINDOW as u64));
        drop(s);
        drop(src);
        std::fs::remove_file(path).unwrap();
    }
}
