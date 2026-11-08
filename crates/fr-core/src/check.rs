//! Sample file data for zero-filled ranges and unexpected signatures. Neither check
//! establishes whether TRIM occurred or whether the complete file is intact.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread;

use crate::model::{Catalog, State};
use crate::source::{ReadSlot, Source};

/// Bytes read at the start and at the end of the data.
const PROBE: u64 = 64 << 10;

/// Volume byte ranges to probe for entry `i`: the first and the last `PROBE` bytes of its
/// real clusters (one range for small files).
fn probes(cat: &Catalog, i: u32) -> impl Iterator<Item = (u64, u64)> {
    let cs = cat.unit as u64;
    let e = &cat.entries[i as usize];
    let runs = cat.runs_of(i);
    let mut out = [(0, 0); 2];
    let mut count = 0;
    if let (Some(first), Some(last)) = (runs.iter().find(|r| r.is_real()), runs.iter().rfind(|r| r.is_real())) {
        let need = e.valid.min(e.size).div_ceil(cs).max(1);
        let head = PROBE.min(first.len * cs).min(need * cs);
        out[0] = (first.lcn * cs, head);
        count = 1;
        let tail = PROBE.min(last.len * cs);
        let tail_at = (last.lcn + last.len) * cs - tail;
        if cat.real_clusters(i) * cs > PROBE && tail_at != first.lcn * cs {
            out[1] = (tail_at, tail);
            count = 2;
        }
    }
    out.into_iter().take(count)
}

/// What the check found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CheckStats {
    /// Files whose sampled ranges contained zeros.
    pub zeroed: usize,
    /// Data that does not start like the file type.
    pub mismatch: usize,
}

#[derive(Default)]
pub struct Progress {
    pub total: AtomicU64,
    pub done: AtomicU64,
}

fn eligible(cat: &Catalog, i: u32) -> bool {
    let e = &cat.entries[i as usize];
    matches!(e.state, State::Good | State::Partial)
        && e.size > 0
        && !e.is_dir()
        && cat.real_clusters(i) > 0
        && e.flags & (crate::ef::SIGNATURE | crate::ef::INCOMPLETE) != (crate::ef::SIGNATURE | crate::ef::INCOMPLETE)
}

fn wrong_signature(want: Option<crate::carve::Fmt>, head: &[u8]) -> bool {
    want.is_some_and(|want| head.len() >= 512 && head.iter().any(|&b| b != 0) && crate::carve::detect(head) != Some(want))
}

struct Sample {
    start: u64,
    end: u64,
    job: u32,
    head: bool,
}

struct Job {
    node: u32,
    needed: u32,
    seen: u32,
    expected: Option<crate::carve::Fmt>,
    nonzero: bool,
    head_nonzero: bool,
    mismatch: bool,
    head_seen: bool,
}

/// Small verdicts, not copies of sampled contents. Failed or missing sweep blocks
/// are never accepted as zero-filled evidence; those jobs use the direct reader.
pub struct SweepChecks {
    samples: Vec<Sample>,
    jobs: Vec<Job>,
    active: Vec<usize>,
    cursor: usize,
    observed_end: u64,
    original_entries: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SweepStats {
    pub stats: CheckStats,
    pub reused: usize,
    pub fallback: usize,
    pub sampled_bytes: u64,
}

impl SweepChecks {
    pub fn new(cat: &Catalog) -> Self {
        let mut out = Self {
            samples: Vec::new(),
            jobs: Vec::new(),
            active: Vec::new(),
            cursor: 0,
            observed_end: 0,
            original_entries: cat.entries.len(),
        };
        let base = cat.fs.offset + cat.fs.data_offset;
        for i in 0..cat.entries.len() as u32 {
            if !eligible(cat, i) {
                continue;
            }
            let e = &cat.entries[i as usize];
            let expected = if e.flags & crate::ef::COMPRESSED != 0 {
                None
            } else {
                crate::carve::formats::expected(cat.name(i).rsplit_once('.').map_or("", |x| x.1))
            };
            let job = out.jobs.len() as u32;
            let mut needed = 0;
            let mut ranges = probes(cat, i).peekable();
            let head_seen = expected.is_none() || ranges.peek().is_none_or(|r| r.1 < 512);
            for (k, (at, len)) in ranges.enumerate() {
                needed += len as u32;
                out.samples.push(Sample { start: base + at, end: base + at + len, job, head: k == 0 });
            }
            out.jobs.push(Job { node: i, needed, seen: 0, expected, nonzero: false, head_nonzero: false, mismatch: false, head_seen });
        }
        out.samples.sort_unstable_by_key(|s| s.start);
        out
    }

    /// Blocks must arrive in ascending source order. An overlapping prefix is ignored.
    pub fn observe(&mut self, at: u64, bytes: &[u8], readable: bool) {
        let end = at + bytes.len() as u64;
        let from = at.max(self.observed_end);
        self.observed_end = self.observed_end.max(end);
        self.active.retain(|&i| self.samples[i].end > from);
        while self.cursor < self.samples.len() && self.samples[self.cursor].start < end {
            if self.samples[self.cursor].end > from {
                self.active.push(self.cursor);
            }
            self.cursor += 1;
        }
        if !readable || from >= end {
            return;
        }
        for &i in &self.active {
            let s = &self.samples[i];
            let start = s.start.max(from);
            let stop = s.end.min(end);
            if start >= stop {
                continue;
            }
            let bytes = &bytes[(start - at) as usize..(stop - at) as usize];
            let job = &mut self.jobs[s.job as usize];
            job.seen += bytes.len() as u32;
            if !job.nonzero || s.head && !job.head_nonzero {
                let nonzero = bytes.iter().any(|&b| b != 0);
                job.nonzero |= nonzero;
                job.head_nonzero |= s.head && nonzero;
            }
            if s.head && start == s.start && bytes.len() >= 512 {
                // A PE signature can be beyond the first sector. Cross-block probes
                // use the direct reader instead of guessing from a short prefix.
                if job.expected == Some(crate::carve::Fmt::Pe) {
                    job.head_seen = stop == s.end;
                    if job.head_seen {
                        job.mismatch = wrong_signature(job.expected, bytes);
                    }
                } else {
                    job.head_seen = true;
                    job.mismatch = job.expected.is_some_and(|want| crate::carve::detect(&bytes[..512]) != Some(want));
                }
            }
        }
    }

    pub fn finish(self, src: &Source, cat: &mut Catalog, threads: usize, progress: Option<&Progress>) -> SweepStats {
        let mut result = SweepStats::default();
        let mut pending = Vec::new();
        for job in self.jobs {
            if job.seen != job.needed || !job.head_seen {
                pending.push(job.node);
                continue;
            }
            result.reused += 1;
            result.sampled_bytes += job.needed as u64;
            if !job.nonzero {
                cat.entries[job.node as usize].state = State::Zeroed;
                result.stats.zeroed += 1;
            } else if job.head_nonzero && job.mismatch {
                cat.entries[job.node as usize].state = State::Mismatch;
                result.stats.mismatch += 1;
            }
        }
        pending.extend((self.original_entries as u32..cat.entries.len() as u32).filter(|&i| eligible(cat, i)));
        result.fallback = pending.len();
        let checked = check_progress(src, cat, &pending, threads, progress);
        result.stats.zeroed += checked.zeroed;
        result.stats.mismatch += checked.mismatch;
        if let Some(p) = progress {
            let total = (result.reused + result.fallback) as u64;
            p.total.store(total, Ordering::Relaxed);
            p.done.store(total, Ordering::Relaxed);
        }
        result
    }
}

/// Read the start (and end) of every entry of `list` that looks recoverable: all zeros
/// make it [`State::Zeroed`]; a start that is not what its extension needs (a JPEG that
/// does not start like one) makes it [`State::Mismatch`].
pub fn check(src: &Source, cat: &mut Catalog, list: &[u32], threads: usize) -> CheckStats {
    check_progress(src, cat, list, threads, None)
}

pub fn check_progress(src: &Source, cat: &mut Catalog, list: &[u32], threads: usize, progress: Option<&Progress>) -> CheckStats {
    let mut todo: Vec<u32> = list.iter().copied().filter(|&i| eligible(cat, i)).collect();
    todo.sort_unstable_by_key(|&i| cat.runs_of(i).iter().find(|r| r.is_real()).map_or(0, |r| r.lcn));
    if let Some(p) = progress {
        p.total.store(todo.len() as u64, Ordering::Relaxed);
        p.done.store(0, Ordering::Relaxed);
    }
    if todo.is_empty() {
        return CheckStats::default();
    }
    let next = AtomicUsize::new(0);
    let hits: Vec<(u32, State)> = thread::scope(|s| {
        let (todo, next, cat) = (&todo, &next, &*cat);
        let hs: Vec<_> = (0..threads.max(1))
            .map(|_| {
                s.spawn(move || {
                    let mut out = Vec::new();
                    let Ok(mut slot) = ReadSlot::new() else { return out };
                    let mut buf = src.buffer(PROBE as usize + 8192);
                    let sec = src.sector as u64;
                    loop {
                        if src.checkpoint().is_err() {
                            break;
                        }
                        let k = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&i) = todo.get(k) else { break };
                        let mut zero = true;
                        let mut first = true;
                        let mut verdict = None;
                        for (off, len) in probes(cat, i) {
                            let absolute = cat.fs.offset + cat.fs.data_offset + off;
                            let aligned = absolute / sec * sec;
                            let pad = (absolute - aligned) as usize;
                            let n = (len + pad as u64).div_ceil(sec) * sec;
                            let outside = aligned.saturating_add(n).saturating_sub(src.size).min(n);
                            let bad = src.read_robust(&mut slot, aligned, &mut buf[..n as usize]).saturating_sub(outside);
                            if bad != 0 {
                                zero = false;
                                break;
                            }
                            let bytes = &buf[pad..pad + len as usize];
                            if first {
                                first = false;
                                let name = cat.name(i);
                                let ext = name.rsplit_once('.').map_or("", |x| x.1);
                                // Compressed files keep compressed data on disk: not comparable.
                                let compressed = cat.entries[i as usize].flags & crate::model::ef::COMPRESSED != 0;
                                let want = if compressed { None } else { crate::carve::formats::expected(ext) };
                                if wrong_signature(want, bytes) {
                                    verdict = Some(State::Mismatch);
                                }
                            }
                            if bytes.iter().any(|&b| b != 0) {
                                zero = false;
                            }
                        }
                        if zero {
                            verdict = Some(State::Zeroed);
                        }
                        if let Some(v) = verdict {
                            out.push((i, v));
                        }
                        if let Some(p) = progress {
                            p.done.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    out
                })
            })
            .collect();
        hs.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    let mut st = CheckStats::default();
    for &(i, v) in &hits {
        cat.entries[i as usize].state = v;
        match v {
            State::Zeroed => st.zeroed += 1,
            _ => st.mismatch += 1,
        }
    }
    st
}

/// Only the zero check of [`check`] (for callers that count zero-filled samples alone).
pub fn zeroed(src: &Source, cat: &mut Catalog, list: &[u32], threads: usize) -> usize {
    check(src, cat, list, threads).zeroed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{Builder, Entry, FsInfo, Run, ef},
        part::FsKind,
    };

    fn catalog() -> Catalog {
        let mut b = Builder::default();
        for (id, name, lcn, len, extra) in [
            (40, "a.jpg", 0, 192, 0),
            (41, "b.png", 256, 8, 0),
            (42, "c.bin", 384, 128, 0),
            (43, "d.jpg", 640, 16, ef::COMPRESSED),
            (44, "pe.exe", 768, 16, 0),
        ] {
            b.add(
                Entry { id, parent: crate::NONE, flags: ef::DELETED | extra, size: len * 512, valid: len * 512, ..Default::default() },
                name,
                &[Run { lcn, len }],
            );
        }
        b.finish(FsInfo {
            kind: FsKind::Ntfs,
            offset: 0,
            size: 512 << 10,
            cluster_size: 512,
            record_size: 1024,
            mft: vec![],
            serial: 0,
            root: 5,
            data_offset: 0,
        })
    }

    #[test]
    fn sweep_checks_match_direct_reads_and_retry_unobserved_samples() {
        let path = std::env::temp_dir().join(format!("fr-check-sweep-{}.img", std::process::id()));
        let mut bytes = vec![0; 512 << 10];
        bytes[4096] = 17; // A zero prefix followed by nonzero data is a mismatch, not zeroed.
        bytes[128 << 10..(128 << 10) + 8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        bytes[320 << 10] = 23; // Compressed bytes cannot be tested against the JPEG magic.
        bytes[384 << 10..(384 << 10) + 2].copy_from_slice(b"MZ");
        bytes[(384 << 10) + 0x3c..(384 << 10) + 0x40].copy_from_slice(&1024u32.to_le_bytes());
        bytes[(384 << 10) + 1024..(384 << 10) + 1028].copy_from_slice(b"PE\0\0");
        std::fs::write(&path, &bytes).unwrap();
        let src = Source::open(path.to_str().unwrap()).unwrap().with_stats(std::sync::Arc::new(Default::default()));
        let mut direct = catalog();
        let stats = check(&src, &mut direct, &[0, 1, 2, 3, 4], 1);
        assert_eq!(stats, CheckStats { zeroed: 1, mismatch: 1 });
        for failed in [false, true] {
            let mut cat = catalog();
            let mut checks = SweepChecks::new(&cat);
            for (i, block) in bytes.chunks(8192).enumerate() {
                checks.observe((i * 8192) as u64, block, !(failed && i == 0));
            }
            let before = src.io_snapshot();
            let result = checks.finish(&src, &mut cat, 1, None);
            assert_eq!(result.stats, stats);
            assert_eq!(result.reused, if failed { 4 } else { 5 });
            assert_eq!(result.fallback, usize::from(failed));
            if !failed {
                assert_eq!(src.io_snapshot().since(before).calls, 0);
            }
            assert_eq!(cat.entries.iter().map(|e| e.state).collect::<Vec<_>>(), direct.entries.iter().map(|e| e.state).collect::<Vec<_>>());
        }
        let mut split = catalog();
        let mut checks = SweepChecks::new(&split);
        for (i, block) in bytes.chunks(512).enumerate() {
            checks.observe((i * 512) as u64, block, true);
        }
        let retried = checks.finish(&src, &mut split, 1, None);
        assert_eq!(retried.fallback, 1);
        assert_eq!(split.entries.iter().map(|e| e.state).collect::<Vec<_>>(), direct.entries.iter().map(|e| e.state).collect::<Vec<_>>());
        drop(src);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::remove_file(path).unwrap();
    }
}
