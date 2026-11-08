//! Where the MFT lies, and reading every record of it in parallel.
//!
//! ```text
//!  $MFT record 0 (or its copy in $MFTMirr) ──► data runs ──► read plan: jobs of whole records
//!
//!  worker 1..N:  claim next job (atomic counter)
//!                ├─ read it into the worker's own aligned buffer; on a read error retry
//!                │  piece by piece down to single sectors, zero-filling what stays bad
//!                └─ hand every record to the visitor with the worker's own state
//! ```
//!
//! Unlike a size scan, recovery reads every record, used or free: a free record is exactly
//! where a deleted file is described.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread;

use fr_core::source::{AlignedBuf, IO_ALIGN, ReadSlot, Source};

use crate::boot::BootSector;
use crate::error::{Error, Result};
use crate::record::{AttrValue, attr, attributes, fixup_record, parse_attribute_list};
use crate::runlist::Extent;

/// An NTFS volume inside a source.
pub struct Volume<'a> {
    pub src: &'a Source,
    /// Byte offset of the volume in the source.
    pub base: u64,
    pub boot: BootSector,
}

impl<'a> Volume<'a> {
    pub fn open(src: &'a Source, base: u64) -> Result<Volume<'a>> {
        let b = src.read_bytes(base, 512)?;
        let boot = BootSector::parse(&b)?;
        Ok(Volume { src, base, boot })
    }

    /// Aligned read at a volume byte offset.
    pub fn read_at(&self, off: u64, buf: &mut [u8]) -> Result<()> {
        if off.checked_add(buf.len() as u64).is_none_or(|end| end > self.boot.volume_bytes()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                fr_core::error::Error::new("read range is outside the source"),
            )
            .into());
        }
        let absolute = self.base + off;
        let sector = self.src.sector as u64;
        if !absolute.is_multiple_of(sector) || !(buf.len() as u64).is_multiple_of(sector) {
            buf.copy_from_slice(&self.src.read_bytes(absolute, buf.len())?);
            Ok(())
        } else {
            Ok(self.src.read_at(absolute, buf)?)
        }
    }

    pub fn clusters(&self) -> u64 {
        self.boot.total_clusters()
    }
}

#[derive(Clone, Debug)]
pub struct Layout {
    pub extents: Vec<Extent>,
    pub data_size: u64,
    pub initialized: u64,
    pub record_size: u32,
    pub cluster_size: u32,
    /// Record 0 came from `$MFTMirr` because the MFT's own copy was unreadable.
    pub from_mirror: bool,
}

impl Layout {
    /// Records that can hold data (bounded by the valid data length).
    pub fn records(&self) -> u64 {
        self.data_size.min(self.initialized) / self.record_size as u64
    }
}

fn round_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}

/// Read an entire non-resident attribute (sparse runs are zeros).
pub fn read_nonresident(vol: &Volume, extents: &[Extent], size: u64) -> Result<Vec<u8>> {
    let cs = vol.boot.cluster_size as u64;
    let total = round_up(size.max(1), cs);
    if total > 1 << 32 {
        return Err(Error::Unsupported("metadata attribute too large"));
    }
    let mut buf = vol.src.buffer(total as usize);
    for e in extents {
        let start = e.vcn * cs;
        if start >= total {
            break;
        }
        let len = (e.len * cs).min(total - start);
        if let Some(lcn) = e.lcn {
            vol.read_at(lcn * cs, &mut buf[start as usize..(start + len) as usize])?;
        }
    }
    Ok(buf[..size as usize].to_vec())
}

#[derive(Debug, Clone, Copy)]
struct Piece {
    disk: u64,
    buf_off: u32,
    len: u32,
}

#[derive(Debug, Clone)]
struct Job {
    first: u64,
    count: u32,
    pieces: Vec<Piece>,
}

/// Byte range of the MFT -> disk pieces through its extents.
fn map_range(extents: &[Extent], cs: u64, start: u64, end: u64, out: &mut Vec<Piece>) -> Result<()> {
    let mut pos = start;
    let mut i = extents.partition_point(|e| (e.vcn + e.len) * cs <= start);
    while pos < end {
        let e = extents.get(i).ok_or(Error::BadMftRecord("$MFT shorter than its data size"))?;
        let es = e.vcn * cs;
        let ee = (e.vcn + e.len) * cs;
        if pos < es {
            return Err(Error::BadMftRecord("hole in $MFT runlist"));
        }
        let lcn = e.lcn.ok_or(Error::BadMftRecord("sparse $MFT run"))?;
        let n = ee.min(end) - pos;
        out.push(Piece { disk: lcn * cs + (pos - es), buf_off: (pos - start) as u32, len: n as u32 });
        pos += n;
        i += 1;
    }
    Ok(())
}

#[derive(Default)]
struct Segments {
    data: Vec<(u64, Vec<Extent>)>,
    data_size: Option<(u64, u64)>,
}

fn collect_segments(attrs: &[crate::record::Attribute], seg: &mut Segments) {
    for a in attrs {
        if a.ty == attr::DATA
            && a.name.is_empty()
            && let AttrValue::NonResident(nr) = &a.value
        {
            if nr.lowest_vcn == 0 {
                seg.data_size = Some((nr.data_size, nr.initialized));
            }
            if !seg.data.iter().any(|(v, _)| *v == nr.lowest_vcn) {
                seg.data.push((nr.lowest_vcn, nr.extents.clone()));
            }
        }
    }
}

fn flatten(mut segs: Vec<(u64, Vec<Extent>)>) -> Vec<Extent> {
    segs.sort_by_key(|(v, _)| *v);
    segs.into_iter().flat_map(|(_, e)| e).collect()
}

/// Record 0 read from `lcn`, fixed up.
fn read_record0(vol: &Volume, lcn: u64) -> Result<Vec<u8>> {
    let rs = vol.boot.record_size as usize;
    let mut buf = vol.src.buffer(rs.max(IO_ALIGN).max(vol.src.sector as usize));
    let n = round_up(rs as u64, vol.src.sector as u64) as usize;
    vol.read_at(lcn * vol.boot.cluster_size as u64, &mut buf[..n])?;
    let mut rec = buf[..rs].to_vec();
    let h = fixup_record(&mut rec)?;
    if !h.in_use() {
        return Err(Error::BadMftRecord("$MFT record not in use"));
    }
    Ok(rec)
}

/// Locate the MFT, following `$ATTRIBUTE_LIST` if it is so fragmented that its runs spill
/// into extension records. Falls back to `$MFTMirr` when record 0 is damaged.
pub fn load_layout(vol: &Volume) -> Result<Layout> {
    let b = vol.boot;
    let rs = b.record_size as usize;
    let cs = b.cluster_size as u64;
    let (rec0, from_mirror) = match read_record0(vol, b.mft_lcn) {
        Ok(r) => (r, false),
        Err(first) => match read_record0(vol, b.mftmirr_lcn) {
            Ok(r) => (r, true),
            Err(_) => return Err(first),
        },
    };
    let attrs = attributes(&rec0)?;
    let mut seg = Segments::default();
    collect_segments(&attrs, &mut seg);

    if let Some(al) = attrs.iter().find(|a| a.ty == attr::ATTRIBUTE_LIST) {
        let raw = match &al.value {
            AttrValue::Resident(v) => v.clone(),
            AttrValue::NonResident(nr) => read_nonresident(vol, &nr.extents, nr.data_size)?,
        };
        let mut pending: Vec<u64> = parse_attribute_list(&raw)?
            .into_iter()
            .filter(|e| e.ty == attr::DATA && e.name_len == 0 && e.segment != 0)
            .map(|e| e.segment)
            .collect();
        pending.sort_unstable();
        pending.dedup();
        let mut buf = vol.src.buffer(rs.max(IO_ALIGN));
        // Extension records are located through the runs known so far; repeat while the
        // known part of the runlist grows.
        while !pending.is_empty() {
            let known = flatten(seg.data.clone());
            let before = pending.len();
            let mut rest = Vec::new();
            for &r in &pending {
                let off = r * rs as u64;
                let mut pieces = Vec::new();
                if map_range(&known, cs, off, off + rs as u64, &mut pieces).is_err() {
                    rest.push(r);
                    continue;
                }
                for p in &pieces {
                    let n = round_up(p.len as u64, vol.src.sector as u64) as usize;
                    let mut tmp = vol.src.buffer(n.max(IO_ALIGN));
                    vol.read_at(p.disk, &mut tmp[..n])?;
                    buf[p.buf_off as usize..(p.buf_off + p.len) as usize].copy_from_slice(&tmp[..p.len as usize]);
                }
                let rec = &mut buf[..rs];
                fixup_record(rec)?;
                collect_segments(&attributes(rec)?, &mut seg);
            }
            if rest.len() == before {
                return Err(Error::BadMftRecord("unresolvable $MFT attribute list"));
            }
            pending = rest;
        }
    }

    let (data_size, initialized) = seg.data_size.ok_or(Error::BadMftRecord("no $DATA"))?;
    let extents = flatten(seg.data);
    if data_size > b.volume_bytes()
        || initialized > data_size
        || extents.iter().any(|e| {
            e.lcn.and_then(|lcn| lcn.checked_add(e.len)).and_then(|end| end.checked_mul(cs)).is_none_or(|end| end > b.volume_bytes())
        })
    {
        return Err(Error::BadMftRecord("$MFT data lies outside the volume"));
    }
    Ok(Layout { extents, data_size, initialized, record_size: b.record_size, cluster_size: b.cluster_size, from_mirror })
}

/// Cut the whole MFT into jobs of at most `chunk_bytes`.
fn plan(layout: &Layout, chunk_bytes: usize) -> Result<Vec<Job>> {
    let rs = layout.record_size as u64;
    let cs = layout.cluster_size as u64;
    let per = (chunk_bytes as u64 / rs).max(1);
    let n = layout.records();
    let mut jobs = Vec::new();
    let mut i = 0u64;
    while i < n {
        let e = (i + per).min(n);
        let mut pieces = Vec::with_capacity(2);
        map_range(&layout.extents, cs, i * rs, e * rs, &mut pieces)?;
        jobs.push(Job { first: i, count: (e - i) as u32, pieces });
        i = e;
    }
    Ok(jobs)
}

/// Counting semaphore bounding concurrent device reads.
struct Gate {
    free: Mutex<usize>,
    cv: Condvar,
}

impl Gate {
    fn new(n: usize) -> Self {
        Gate { free: Mutex::new(n.max(1)), cv: Condvar::new() }
    }
    fn acquire(&self) -> GatePass<'_> {
        let mut f = self.free.lock().unwrap();
        while *f == 0 {
            f = self.cv.wait(f).unwrap();
        }
        *f -= 1;
        GatePass(self)
    }
}

struct GatePass<'a>(&'a Gate);

impl Drop for GatePass<'_> {
    fn drop(&mut self) {
        *self.0.free.lock().unwrap() += 1;
        self.0.cv.notify_one();
    }
}

/// Read all pieces of `job` into `buf`, all requests in flight at once. On any error the
/// job is read again piece by piece with bad-sector handling; returns unreadable bytes.
fn read_job(vol: &Volume, job: &Job, buf: &mut AlignedBuf, slots: &mut Vec<ReadSlot>) -> Result<u64> {
    let sector = vol.src.sector as u64;
    for p in &job.pieces {
        if p.disk.checked_add(p.len as u64).is_none_or(|end| end > vol.boot.volume_bytes()) {
            return Err(fr_core::error::Error::new("read range is outside the source").into_io(std::io::ErrorKind::UnexpectedEof).into());
        }
    }
    if job.pieces.iter().any(|p| !(vol.base + p.disk).is_multiple_of(sector) || !(p.len as u64).is_multiple_of(sector)) {
        let largest = job.pieces.iter().map(|p| p.len as usize).max().unwrap_or(0);
        let mut scratch = vol.src.buffer(largest + 8192);
        let mut slot = ReadSlot::new()?;
        let mut bad = 0;
        for p in &job.pieces {
            let absolute = vol.base + p.disk;
            let pad = absolute % sector;
            let n = (pad + p.len as u64).div_ceil(sector) * sector;
            bad += vol.src.read_robust(&mut slot, absolute - pad, &mut scratch[..n as usize]).min(p.len as u64);
            buf[p.buf_off as usize..(p.buf_off + p.len) as usize].copy_from_slice(&scratch[pad as usize..pad as usize + p.len as usize]);
        }
        return Ok(bad);
    }
    while slots.len() < job.pieces.len() {
        slots.push(ReadSlot::new()?);
    }
    let mut started = 0;
    let mut failed = false;
    for (p, slot) in job.pieces.iter().zip(slots.iter_mut()) {
        // SAFETY: every started request is waited for below before `buf` is touched again.
        match unsafe { vol.src.start_read(vol.base + p.disk, buf.as_mut_ptr().add(p.buf_off as usize), p.len, slot) } {
            Ok(()) => started += 1,
            Err(_) => {
                failed = true;
                break;
            }
        }
    }
    for slot in &mut slots[..started] {
        if vol.src.finish_read(slot).is_err() {
            failed = true;
        }
    }
    if !failed {
        return Ok(0);
    }
    let mut bad = 0;
    for p in &job.pieces {
        let dst = &mut buf[p.buf_off as usize..(p.buf_off + p.len) as usize];
        bad += vol.src.read_robust(&mut slots[0], vol.base + p.disk, dst);
    }
    Ok(bad)
}

/// Live progress, readable from another thread.
#[derive(Debug, Default)]
pub struct Progress {
    pub total: AtomicU64,
    pub done: AtomicU64,
    /// Deleted files found so far.
    pub found: AtomicU64,
}

/// Every record of the MFT handed to `visit(state, record number, record bytes)` on
/// `threads` workers, each with its own `make()` state. Returns the states and the number
/// of unreadable bytes.
pub fn read_records<W, M, V>(
    vol: &Volume,
    layout: &Layout,
    threads: usize,
    progress: Option<&Progress>,
    make: M,
    visit: V,
) -> Result<(Vec<W>, u64)>
where
    W: Send,
    M: Fn() -> W + Sync,
    V: Fn(&mut W, u64, &mut [u8]) + Sync,
{
    let rs = layout.record_size as usize;
    let chunk_bytes = ((4usize << 20) / rs).max(1) * rs;
    let jobs = plan(layout, chunk_bytes)?;
    if let Some(p) = progress {
        p.total.store(layout.records(), Ordering::Relaxed);
        p.done.store(0, Ordering::Relaxed);
    }
    let threads = threads.max(1).min(jobs.len().max(1));
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    // Budget SATA SSDs and HDDs slow down under deep queues; image files are memory copies.
    let qd = if vol.src.is_device() { 4 } else { threads }.clamp(1, threads);
    let gate = Gate::new(qd);
    let results: Vec<Result<(W, u64)>> = thread::scope(|s| {
        let (jobs, next, stop, gate, make, visit) = (&jobs, &next, &stop, &gate, &make, &visit);
        let hs: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(move || -> Result<(W, u64)> {
                    let mut buf = vol.src.buffer(chunk_bytes);
                    let mut slots = Vec::with_capacity(2);
                    let mut state = make();
                    let mut bad = 0u64;
                    loop {
                        vol.src.checkpoint()?;
                        let j = next.fetch_add(1, Ordering::Relaxed);
                        if j >= jobs.len() || stop.load(Ordering::Relaxed) {
                            break;
                        }
                        let job = &jobs[j];
                        let pass = gate.acquire();
                        match read_job(vol, job, &mut buf, &mut slots) {
                            Ok(b) => bad += b,
                            Err(e) => {
                                stop.store(true, Ordering::Relaxed);
                                return Err(e);
                            }
                        }
                        drop(pass);
                        for (i, raw) in buf.chunks_exact_mut(rs).take(job.count as usize).enumerate() {
                            visit(&mut state, job.first + i as u64, raw);
                        }
                        if let Some(p) = progress {
                            p.done.fetch_add(job.count as u64, Ordering::Relaxed);
                        }
                    }
                    Ok((state, bad))
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().expect("MFT worker panicked")).collect()
    });
    let mut out = Vec::with_capacity(results.len());
    let mut bad = 0;
    for r in results {
        let (w, b) = r?;
        out.push(w);
        bad += b;
    }
    Ok((out, bad))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_range_splits_across_fragments() {
        let ext = vec![Extent { vcn: 0, len: 2, lcn: Some(100) }, Extent { vcn: 2, len: 3, lcn: Some(500) }];
        let mut p = Vec::new();
        map_range(&ext, 4096, 4096, 3 * 4096 + 1024, &mut p).unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!((p[0].disk, p[0].buf_off, p[0].len), (101 * 4096, 0, 4096));
        assert_eq!((p[1].disk, p[1].buf_off, p[1].len), (500 * 4096, 4096, 4096 + 1024));
    }

    #[test]
    fn plan_covers_every_record() {
        let layout = Layout {
            extents: vec![Extent { vcn: 0, len: 16, lcn: Some(10) }],
            data_size: 64 * 1024,
            initialized: 64 * 1024,
            record_size: 1024,
            cluster_size: 4096,
            from_mirror: false,
        };
        let jobs = plan(&layout, 16 * 1024).unwrap();
        assert_eq!(jobs.len(), 4);
        assert_eq!(jobs.iter().map(|j| j.count).sum::<u32>(), 64);
    }
}
