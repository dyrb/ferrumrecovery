//! Reading the contents of a found entry: bytes stored inside its file record, runs of
//! clusters, NTFS compression units, sparse ranges and ranges whose location is lost.

use std::io;

use crate::lznt1;
use crate::model::{Catalog, Data, Run};
use crate::source::{AlignedBuf, ReadSlot, Source};

/// Bytes handed to the sink at once at most.
pub const CHUNK: usize = 4 << 20;

static ZEROS: [u8; 64 << 10] = [0; 64 << 10];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadStats {
    /// Bytes that could not be read (bad sectors), delivered as zeros.
    pub bad: u64,
    /// Bytes whose location was lost, delivered as zeros.
    pub missing: u64,
    /// Compression units that did not decompress.
    pub corrupt_units: u32,
    /// Every delivered byte was zero.
    pub all_zero: bool,
}

/// Disk pieces (absolute byte offset in the volume, length) of the logical byte range
/// `off..off + len` of something stored in `runs`.
pub fn map_bytes(runs: &[Run], cs: u64, off: u64, len: u64) -> Option<Vec<(u64, u64)>> {
    let mut out = Vec::new();
    let mut vcn_start = 0u64;
    let (mut pos, end) = (off, off.checked_add(len)?);
    for r in runs {
        let rs = vcn_start.checked_mul(cs)?;
        vcn_start = vcn_start.checked_add(r.len)?;
        let re = vcn_start.checked_mul(cs)?;
        if re <= pos {
            continue;
        }
        if !r.is_real() {
            return None;
        }
        let n = re.min(end) - pos;
        out.push((r.lcn.checked_mul(cs)?.checked_add(pos.checked_sub(rs)?)?, n));
        pos += n;
        if pos >= end {
            return Some(out);
        }
    }
    None
}

/// Undo the NTFS update sequence of a multi-sector record. `false` if it is torn.
pub fn apply_fixup(rec: &mut [u8]) -> bool {
    if rec.len() < 0x30 {
        return false;
    }
    let usa_off = u16::from_le_bytes([rec[4], rec[5]]) as usize;
    let usa_cnt = u16::from_le_bytes([rec[6], rec[7]]) as usize;
    if usa_cnt < 2 || usa_off + usa_cnt * 2 > rec.len() || (usa_cnt - 1) * 512 > rec.len() {
        return false;
    }
    let usn = [rec[usa_off], rec[usa_off + 1]];
    for i in 1..usa_cnt {
        let end = i * 512 - 2;
        if rec[end] != usn[0] || rec[end + 1] != usn[1] {
            return false;
        }
        rec[end] = rec[usa_off + 2 * i];
        rec[end + 1] = rec[usa_off + 2 * i + 1];
    }
    true
}

pub struct Reader<'a> {
    src: &'a Source,
    cat: &'a Catalog,
    slot: ReadSlot,
    buf: AlignedBuf,
    unit: Vec<u8>,
}

impl<'a> Reader<'a> {
    pub fn new(src: &'a Source, cat: &'a Catalog) -> io::Result<Self> {
        Ok(Reader { src, cat, slot: ReadSlot::new()?, buf: src.buffer(CHUNK + 8192), unit: Vec::new() })
    }

    /// Read `len` bytes at byte `off` of the cluster numbering (`lcn * cluster size`,
    /// cluster aligned) into `self.buf`.
    fn fill(&mut self, off: u64, len: usize) -> u64 {
        let sec = self.src.sector as usize;
        let at = self.cat.fs.offset + self.cat.fs.data_offset + off;
        let shift = at as usize % sec;
        let n = (len + shift).div_ceil(sec) * sec;
        let bad = self.src.read_robust(&mut self.slot, at - shift as u64, &mut self.buf[..n]);
        self.buf.copy_within(shift..shift + len, 0);
        let outside = (at - shift as u64).saturating_add(n as u64).saturating_sub(self.src.size).min(n as u64);
        bad.saturating_sub(outside).min(len as u64)
    }

    /// A file record at volume byte `at` (outside the MFT), fixed up.
    pub fn record_at(&mut self, at: u64) -> io::Result<Vec<u8>> {
        self.record_at_size(at, self.cat.fs.record_size)
    }

    fn record_at_size(&mut self, at: u64, record_size: u32) -> io::Result<Vec<u8>> {
        let rs = record_size as usize;
        if !(512..=CHUNK).contains(&rs) || at.checked_add(rs as u64).is_none_or(|end| end > self.cat.fs.size) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, crate::error::Error::new("invalid file record range")));
        }
        let sec = self.src.sector as u64;
        let absolute = self.cat.fs.offset + at;
        let start = absolute / sec * sec;
        let n = ((absolute + rs as u64).div_ceil(sec) * sec - start) as usize;
        if n > self.buf.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, crate::error::Error::new("file record exceeds read buffer")));
        }
        let bad = self.src.read_robust(&mut self.slot, start, &mut self.buf[..n]);
        if bad != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, crate::error::Error::new("file record unreadable")));
        }
        let s = (absolute - start) as usize;
        let mut rec = self.buf[s..s + rs].to_vec();
        if &rec[..4] != b"FILE" || !apply_fixup(&mut rec) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, crate::error::Error::new("file record damaged")));
        }
        Ok(rec)
    }

    /// The file record `id` as stored now, fixed up.
    pub fn record(&mut self, id: u64) -> io::Result<Vec<u8>> {
        let fs = &self.cat.fs;
        let rs = fs.record_size as u64;
        if !(512..=65536).contains(&rs) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, crate::error::Error::new("invalid record size")));
        }
        let at = id
            .checked_mul(rs)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, crate::error::Error::new("invalid record number")))?;
        let pieces = map_bytes(&fs.mft, self.cat.unit as u64, at, rs)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, crate::error::Error::new("record outside the MFT")))?;
        let mut rec = Vec::with_capacity(rs as usize);
        for (off, len) in pieces {
            let bad = self.fill(off, len as usize);
            if bad != 0 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, crate::error::Error::new("file record unreadable")));
            }
            rec.extend_from_slice(&self.buf[..len as usize]);
        }
        if &rec[..4] != b"FILE" || !apply_fixup(&mut rec) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, crate::error::Error::new("file record damaged")));
        }
        Ok(rec)
    }

    /// Hand the contents of entry `i` to `sink` in order, `size` bytes in total.
    pub fn read(&mut self, i: u32, sink: &mut dyn FnMut(&[u8]) -> io::Result<()>) -> io::Result<ReadStats> {
        self.read_limit(i, self.cat.entries[i as usize].size, sink)
    }

    /// Read a logical prefix. Compressed data may require reading a complete unit.
    pub fn read_limit(&mut self, i: u32, limit: u64, sink: &mut dyn FnMut(&[u8]) -> io::Result<()>) -> io::Result<ReadStats> {
        let mut e = self.cat.entries[i as usize];
        e.size = e.size.min(limit);
        self.src.checkpoint()?;
        let mut st = ReadStats { all_zero: true, ..Default::default() };
        let mut emit = |st: &mut ReadStats, b: &[u8]| -> io::Result<()> {
            self.src.checkpoint()?;
            if st.all_zero && b.iter().any(|&x| x != 0) {
                st.all_zero = false;
            }
            sink(b)
        };
        match e.data {
            Data::None => {
                st.missing = e.size;
                zeros(e.size, &mut |b| emit(&mut st, b))?;
            }
            Data::Resident { off, len } | Data::Inline { off, len, .. } | Data::RecordRuns { off, len, .. } => {
                let rec = match e.data {
                    Data::Inline { at, record_size, .. } => self.record_at_size(at, record_size)?,
                    Data::RecordRuns { start, count, record_size, .. } => {
                        let mut rec = Vec::with_capacity(record_size as usize);
                        let runs = self.cat.runs[start as usize..(start + count) as usize].to_vec();
                        for run in runs {
                            let len = (run.len * self.cat.unit as u64) as usize;
                            if !run.is_real()
                                || rec.len() + len > record_size as usize
                                || self.fill(run.lcn * self.cat.unit as u64, len) != 0
                            {
                                return Err(io::Error::new(io::ErrorKind::InvalidData, crate::error::Error::new("file record unreadable")));
                            }
                            rec.extend_from_slice(&self.buf[..len]);
                        }
                        if rec.len() != record_size as usize || !rec.starts_with(b"FILE") || !apply_fixup(&mut rec) {
                            return Err(io::Error::new(io::ErrorKind::InvalidData, crate::error::Error::new("file record damaged")));
                        }
                        rec
                    }
                    _ => self.record(self.cat.record_of(i))?,
                };
                let seq = u16::from_le_bytes([rec[0x10], rec[0x11]]);
                let end = off as usize + len as usize;
                if seq != e.seq || end > rec.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        crate::error::Error::new("file record was reused after the scan"),
                    ));
                }
                let n = (len as u64).min(e.size) as usize;
                emit(&mut st, &rec[off as usize..off as usize + n])?;
                zeros(e.size - n as u64, &mut |b| emit(&mut st, b))?;
            }
            Data::Runs { .. } if e.cu != 0 => self.read_compressed(i, e.size, &mut st, &mut emit)?,
            Data::Runs { .. } => self.read_plain(i, e.size, &mut st, &mut emit)?,
        }
        Ok(st)
    }

    fn read_plain(
        &mut self,
        i: u32,
        size: u64,
        st: &mut ReadStats,
        emit: &mut dyn FnMut(&mut ReadStats, &[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        let e = self.cat.entries[i as usize];
        let cs = self.cat.unit as u64;
        let valid = e.valid.min(size);
        let mut pos = 0u64;
        for r in self.cat.runs_of(i).to_vec() {
            if pos >= size {
                break;
            }
            let take = (r.len * cs).min(size - pos);
            if !r.is_real() {
                if r.lcn == Run::MISSING {
                    st.missing += take;
                }
                zeros(take, &mut |b| emit(st, b))?;
                pos += take;
                continue;
            }
            let mut k = 0u64;
            while k < take {
                let n = (CHUNK as u64).min(take - k);
                let at = pos + k;
                if at >= valid {
                    zeros(n, &mut |b| emit(st, b))?;
                } else {
                    st.bad += self.fill(r.lcn * cs + k, n as usize);
                    if at + n > valid {
                        self.buf[(valid - at) as usize..n as usize].fill(0);
                    }
                    emit(st, &self.buf[..n as usize])?;
                }
                k += n;
            }
            pos += take;
        }
        if pos < size {
            st.missing += size - pos;
            zeros(size - pos, &mut |b| emit(st, b))?;
        }
        Ok(())
    }

    fn read_compressed(
        &mut self,
        i: u32,
        size: u64,
        st: &mut ReadStats,
        emit: &mut dyn FnMut(&mut ReadStats, &[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        let e = self.cat.entries[i as usize];
        let cs = self.cat.unit as u64;
        let unit = 1u64
            .checked_shl(e.cu as u32)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, crate::error::Error::new("invalid compression unit")))?;
        let ub = unit
            .checked_mul(cs)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, crate::error::Error::new("invalid compression unit")))?;
        if ub > CHUNK {
            return Err(io::Error::new(io::ErrorKind::InvalidData, crate::error::Error::new("compression unit too large")));
        }
        let valid = e.valid.min(size);
        let runs = self.cat.runs_of(i).to_vec();
        let mut cur = Cursor { runs: &runs, idx: 0, vcn: 0 };
        let mut pos = 0u64;
        let mut packed = Vec::new();
        self.unit.resize(ub, 0);
        while pos < size {
            let pieces = cur.take(unit);
            let real: u64 = pieces.iter().filter(|r| r.is_real()).map(|r| r.len).sum();
            let missing: u64 = pieces.iter().filter(|r| r.lcn == Run::MISSING).map(|r| r.len).sum();
            let n = (ub as u64).min(size - pos) as usize;
            st.missing += (missing * cs).min(n as u64);
            if real == 0 {
                self.unit.fill(0);
            } else {
                packed.clear();
                for p in pieces.iter().filter(|r| r.is_real()) {
                    st.bad += self.fill(p.lcn * cs, (p.len * cs) as usize);
                    packed.extend_from_slice(&self.buf[..(p.len * cs) as usize]);
                }
                if real == unit {
                    self.unit.copy_from_slice(&packed[..ub]);
                } else if !lznt1::decompress(&packed, &mut self.unit) {
                    st.corrupt_units += 1;
                }
            }
            if pos + n as u64 > valid {
                let from = valid.saturating_sub(pos) as usize;
                self.unit[from..n].fill(0);
            }
            emit(st, &self.unit[..n])?;
            pos += n as u64;
        }
        Ok(())
    }
}

/// Walks runs in steps of whole compression units.
struct Cursor<'r> {
    runs: &'r [Run],
    idx: usize,
    /// Clusters of `runs[idx]` already consumed.
    vcn: u64,
}

impl Cursor<'_> {
    /// The next `count` clusters as runs; past the end they are missing.
    fn take(&mut self, count: u64) -> Vec<Run> {
        let mut out = Vec::new();
        let mut left = count;
        while left > 0 {
            let Some(r) = self.runs.get(self.idx) else {
                out.push(Run { lcn: Run::MISSING, len: left });
                break;
            };
            let n = (r.len - self.vcn).min(left);
            let lcn = if r.is_real() { r.lcn + self.vcn } else { r.lcn };
            out.push(Run { lcn, len: n });
            self.vcn += n;
            left -= n;
            if self.vcn == r.len {
                self.idx += 1;
                self.vcn = 0;
            }
        }
        out
    }
}

fn zeros(mut n: u64, sink: &mut dyn FnMut(&[u8]) -> io::Result<()>) -> io::Result<()> {
    while n > 0 {
        let k = n.min(ZEROS.len() as u64) as usize;
        sink(&ZEROS[..k])?;
        n -= k as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_bytes_across_runs() {
        let runs = [Run { lcn: 100, len: 2 }, Run { lcn: 500, len: 3 }];
        assert_eq!(map_bytes(&runs, 4096, 4096 + 1024, 4096), Some(vec![(100 * 4096 + 5120, 3072), (500 * 4096, 1024)]));
        assert_eq!(map_bytes(&runs, 4096, 5 * 4096, 1), None);
        assert_eq!(map_bytes(&[Run { lcn: Run::SPARSE, len: 4 }], 4096, 0, 10), None);
    }

    #[test]
    fn cursor_splits_units() {
        let runs = [Run { lcn: 10, len: 20 }, Run { lcn: Run::SPARSE, len: 12 }];
        let mut c = Cursor { runs: &runs, idx: 0, vcn: 0 };
        assert_eq!(c.take(16), vec![Run { lcn: 10, len: 16 }]);
        assert_eq!(c.take(16), vec![Run { lcn: 26, len: 4 }, Run { lcn: Run::SPARSE, len: 12 }]);
        assert_eq!(c.take(16), vec![Run { lcn: Run::MISSING, len: 16 }]);
    }
}
