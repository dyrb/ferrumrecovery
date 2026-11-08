//! One pass over chosen clusters of a volume (free space, usually), read in large
//! sequential pieces by a background thread while the caller looks at the previous piece.
//! Deep searches (lost folders, file records, file signatures) share it, so the disk is
//! read once whatever is searched for.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::sync_channel;

use crate::source::{AlignedBuf, ReadSlot, Source};

/// Bytes per read at most.
const PIECE: u64 = 8 << 20;

/// Progress of a sweep in clusters, readable from another thread.
#[derive(Debug, Default)]
pub struct SweepProgress {
    pub total: AtomicU64,
    pub done: AtomicU64,
}

/// Runs of wanted clusters among `0..clusters`, each at most `max` long.
pub fn wanted_runs(clusters: u64, max: u64, want: &dyn Fn(u64) -> bool) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut c = 0;
    while c < clusters {
        if !want(c) {
            c += 1;
            continue;
        }
        let s = c;
        while c < clusters && c - s < max && want(c) {
            c += 1;
        }
        out.push((s, c - s));
    }
    out
}

/// Read every cluster `c < clusters` with `want(c)` (cluster 0 at byte `base` of `src`,
/// `cs` bytes each) and hand it to `visit(c, bytes)` in increasing order. Unreadable
/// sectors arrive as zeros. Returns the number of unreadable bytes.
pub fn sweep(
    src: &Source,
    base: u64,
    cs: u32,
    clusters: u64,
    want: &dyn Fn(u64) -> bool,
    progress: Option<&SweepProgress>,
    visit: &mut dyn FnMut(u64, &[u8]),
) -> u64 {
    sweep_chunks(src, base, cs, clusters, want, progress, &mut |c, bytes| {
        for (k, b) in bytes.chunks_exact(cs as usize).enumerate() {
            visit(c + k as u64, b);
        }
    })
}

/// The same bounded pipeline, exposing whole I/O pieces to parsers with lookahead.
pub fn sweep_chunks(
    src: &Source,
    base: u64,
    cs: u32,
    clusters: u64,
    want: &dyn Fn(u64) -> bool,
    progress: Option<&SweepProgress>,
    visit: &mut dyn FnMut(u64, &[u8]),
) -> u64 {
    let cs64 = cs as u64;
    let runs = wanted_runs(clusters, (PIECE / cs64).max(1), want);
    sweep_runs(src, base, cs, runs, progress, visit)
}

/// Whole-range planning visits I/O pieces rather than testing every sector twice.
pub fn sweep_all_chunks(
    src: &Source,
    base: u64,
    cs: u32,
    clusters: u64,
    progress: Option<&SweepProgress>,
    visit: &mut dyn FnMut(u64, &[u8]),
) -> u64 {
    let per = (PIECE / cs as u64).max(1);
    let runs = (0..clusters).step_by(per as usize).map(|start| (start, per.min(clusters - start))).collect();
    sweep_runs(src, base, cs, runs, progress, visit)
}

fn sweep_runs(
    src: &Source,
    base: u64,
    cs: u32,
    runs: Vec<(u64, u64)>,
    progress: Option<&SweepProgress>,
    visit: &mut dyn FnMut(u64, &[u8]),
) -> u64 {
    sweep_runs_status(src, base, cs, runs, progress, &mut |c, bytes, _| visit(c, bytes))
}

/// Full-range blocks with the number of unreadable bytes in each block.
pub fn sweep_all_status(
    src: &Source,
    base: u64,
    cs: u32,
    clusters: u64,
    progress: Option<&SweepProgress>,
    visit: &mut dyn FnMut(u64, &[u8], u64),
) -> u64 {
    let per = (PIECE / cs as u64).max(1);
    let runs = (0..clusters).step_by(per as usize).map(|start| (start, per.min(clusters - start))).collect();
    sweep_runs_status(src, base, cs, runs, progress, visit)
}

fn sweep_runs_status(
    src: &Source,
    base: u64,
    cs: u32,
    runs: Vec<(u64, u64)>,
    progress: Option<&SweepProgress>,
    visit: &mut dyn FnMut(u64, &[u8], u64),
) -> u64 {
    sweep_prepared::<(), 3>(src, base, cs, runs, progress, &mut |_, _, _| (), &mut |c, b, bad, ()| visit(c, b, bad))
}

/// Prepare headers on the reader thread while the caller processes the preceding block.
/// Preparation and delivery both retain physical order; buffers are never copied between lanes.
pub fn sweep_all_prepared<T: Send>(
    src: &Source,
    base: u64,
    cs: u32,
    clusters: u64,
    progress: Option<&SweepProgress>,
    prepare: &mut (impl FnMut(u64, &[u8], u64) -> T + Send),
    visit: &mut impl FnMut(u64, &[u8], u64, T),
) -> u64 {
    let per = (PIECE / cs as u64).max(1);
    let runs = (0..clusters).step_by(per as usize).map(|start| (start, per.min(clusters - start))).collect();
    sweep_prepared::<T, 4>(src, base, cs, runs, progress, prepare, visit)
}

// Own the kernel destination together with its request, including unwinding and cancellation.
struct ReadBlock<'a> {
    src: &'a Source,
    slot: ReadSlot,
    buf: AlignedBuf,
    pending: bool,
    c: u64,
    n: u64,
    off: u64,
    pad: usize,
    rd: usize,
}

impl<'a> ReadBlock<'a> {
    fn new(src: &'a Source, bytes: usize) -> std::io::Result<Self> {
        Ok(Self { src, slot: ReadSlot::new()?, buf: src.buffer(bytes), pending: false, c: 0, n: 0, off: 0, pad: 0, rd: 0 })
    }

    fn start(&mut self, base: u64, cs: u64, c: u64, n: u64) {
        self.c = c;
        self.n = n;
        let absolute = base + c * cs;
        self.pad = (absolute % self.src.sector as u64) as usize;
        self.off = absolute - self.pad as u64;
        self.rd = (n * cs + self.pad as u64).div_ceil(self.src.sector as u64) as usize * self.src.sector as usize;
        // SAFETY: buf and slot are owned by this block; Drop drains a pending request.
        self.pending = unsafe { self.src.start_read(self.off, self.buf.as_mut_ptr(), self.rd as u32, &mut self.slot) }.is_ok();
    }

    fn finish(&mut self, cs: u64) -> u64 {
        let complete = self.pending && self.src.finish_read(&mut self.slot).is_ok();
        self.pending = false;
        let failed = if complete {
            0
        } else if self.off.saturating_add(self.rd as u64) <= self.src.size {
            self.src.retry_read(&mut self.slot, self.off, &mut self.buf[..self.rd])
        } else {
            self.src.read_robust(&mut self.slot, self.off, &mut self.buf[..self.rd])
        };
        let outside = self.off.saturating_add(self.rd as u64).saturating_sub(self.src.size).min(self.rd as u64);
        if self.pad != 0 {
            self.buf.copy_within(self.pad..self.pad + (self.n * cs) as usize, 0);
        }
        failed.saturating_sub(outside)
    }
}

impl Drop for ReadBlock<'_> {
    fn drop(&mut self) {
        if self.pending {
            self.src.cancel_read(&mut self.slot);
        }
    }
}

struct CancelOnPanic<'a>(&'a Source);

impl Drop for CancelOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // The other lane may be waiting for resume rather than a channel message.
            self.0.cancel_operation();
        }
    }
}

fn sweep_prepared<T: Send, const BUFFERS: usize>(
    src: &Source,
    base: u64,
    cs: u32,
    runs: Vec<(u64, u64)>,
    progress: Option<&SweepProgress>,
    prepare: &mut (impl FnMut(u64, &[u8], u64) -> T + Send),
    visit: &mut impl FnMut(u64, &[u8], u64, T),
) -> u64 {
    let cs64 = cs as u64;
    let total: u64 = runs.iter().map(|r| r.1).sum();
    if let Some(p) = progress {
        p.total.store(total, Ordering::Relaxed);
        p.done.store(0, Ordering::Relaxed);
    }
    if runs.is_empty() {
        return 0;
    }
    std::thread::scope(|scope| {
        let _cancel = CancelOnPanic(src);
        // Two reads need two spare destinations even while both CPU lanes own a block.
        // One CPU lane uses three buffers (24 MiB), two lanes use four (32 MiB).
        // Keep channels inside the scope so a panicking visitor disconnects before joining.
        let (full_tx, full_rx) = sync_channel(1);
        let (empty_tx, empty_rx) = sync_channel(BUFFERS);
        let piece = (PIECE / cs64).max(1) * cs64;
        for _ in 0..runs.len().min(BUFFERS) {
            let Ok(block) = ReadBlock::new(src, piece as usize + 8192) else { return total * cs64 };
            empty_tx.send(block).unwrap_or_else(|_| unreachable!());
        }
        let reader = scope.spawn(move || {
            let _cancel = CancelOnPanic(src);
            let mut pending = std::collections::VecDeque::with_capacity(2);
            let mut runs = runs.into_iter().peekable();
            let mut bad = 0;
            loop {
                if src.checkpoint().is_err() {
                    break;
                }
                while pending.len() < 2 && runs.peek().is_some() {
                    let block = if pending.is_empty() { empty_rx.recv().ok() } else { empty_rx.try_recv().ok() };
                    let Some(mut block) = block else { break };
                    let (c, n) = runs.next().unwrap();
                    block.start(base, cs64, c, n);
                    pending.push_back(block);
                }
                let Some(mut block) = pending.pop_front() else { break };
                let failed = block.finish(cs64);
                bad += failed;
                // Refill before CPU work, while this completed block stays exclusively ours.
                if runs.peek().is_some()
                    && let Ok(mut next) = empty_rx.try_recv()
                {
                    let (c, n) = runs.next().unwrap();
                    next.start(base, cs64, c, n);
                    pending.push_back(next);
                }
                if src.checkpoint().is_err() {
                    break;
                }
                let prepared = prepare(block.c, &block.buf[..(block.n * cs64) as usize], failed);
                if full_tx.send((block, failed, prepared)).is_err() {
                    break;
                }
            }
            bad
        });
        for (block, failed, prepared) in full_rx.iter() {
            if src.checkpoint().is_err() {
                break;
            }
            visit(block.c, &block.buf[..(block.n * cs64) as usize], failed, prepared);
            if let Some(p) = progress {
                p.done.fetch_add(block.n, Ordering::Relaxed);
            }
            let _ = empty_tx.send(block);
        }
        drop(full_rx);
        drop(empty_tx);
        match reader.join() {
            Ok(bad) => bad,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_blocks_preserve_order_and_unaligned_source_tails() {
        let dir = std::env::temp_dir().join(format!("fr-queue-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("source.img");
        let bytes: Vec<_> = (0..PIECE as usize * 3 + 1025).map(|i| (i.wrapping_mul(31) >> 9) as u8).collect();
        std::fs::write(&path, &bytes).unwrap();
        let stats = std::sync::Arc::new(crate::source::IoStats::default());
        let src = Source::open(path.to_str().unwrap()).unwrap().with_stats(stats);
        let progress = SweepProgress::default();
        let mut next = 0;
        let mut blocks = 0;
        let (prepared, ready) = std::sync::mpsc::channel();
        let bad = sweep_all_prepared(
            &src,
            3,
            512,
            (bytes.len() as u64 - 3) / 512,
            Some(&progress),
            &mut |c, b, _| {
                if c == 0 {
                    assert_eq!(src.io_snapshot().calls, 3, "read-ahead must be issued before header parsing");
                } else if c == PIECE / 512 {
                    assert_eq!(src.io_snapshot().calls, 4, "two future reads must fit while both CPU lanes own their blocks");
                    prepared.send(()).unwrap();
                }
                b.iter().fold(0u64, |sum, &x| sum + x as u64)
            },
            &mut |c, b, bad, sum| {
                assert_eq!(c * 512, next);
                if c == 0 {
                    ready.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
                }
                assert_eq!(bad, 0);
                assert_eq!(b, &bytes[3 + next as usize..3 + next as usize + b.len()]);
                assert_eq!(sum, b.iter().map(|&x| x as u64).sum::<u64>());
                next += b.len() as u64;
                blocks += 1;
            },
        );
        assert_eq!(bad, 0);
        assert_eq!(blocks, 4);
        assert_eq!(next / 512, progress.done.load(Ordering::Relaxed));
        assert_eq!(progress.done.load(Ordering::Relaxed), progress.total.load(Ordering::Relaxed));
        drop(src);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cancellation_and_panics_disconnect_and_drain_the_queue() {
        let dir = std::env::temp_dir().join(format!("fr-queue-exit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("source.img");
        std::fs::write(&path, vec![42; PIECE as usize * 4]).unwrap();
        for reader_panic in [false, true] {
            let control = std::sync::Arc::new(crate::control::Control::default());
            let src = Source::open(path.to_str().unwrap()).unwrap().with_control(control.clone());
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                sweep_all_prepared(
                    &src,
                    0,
                    512,
                    src.size / 512,
                    None,
                    &mut |_, _, _| {
                        if reader_panic {
                            control.set_paused(true);
                            panic!("preparation failure");
                        }
                    },
                    &mut |_, _, _, ()| {
                        control.set_paused(true);
                        panic!("visitor failure");
                    },
                );
            }));
            assert!(result.is_err());
            assert!(control.is_cancelled());
        }
        let control = std::sync::Arc::new(crate::control::Control::default());
        let src = Source::open(path.to_str().unwrap()).unwrap().with_control(control.clone());
        let mut seen = 0;
        sweep_all_status(&src, 0, 512, src.size / 512, None, &mut |_, _, _| {
            seen += 1;
            control.cancel();
        });
        assert_eq!(seen, 1);
        assert!(src.checkpoint().is_err());
        drop(src);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn runs_of_wanted_clusters() {
        let want = |c: u64| c % 10 < 4;
        assert_eq!(wanted_runs(25, 3, &want), vec![(0, 3), (3, 1), (10, 3), (13, 1), (20, 3), (23, 1)]);
    }

    #[test]
    fn sweeps_an_image_in_order() {
        let dir = std::env::temp_dir().join(format!("fr-sweep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("v.img");
        let data: Vec<u8> = (0..64 * 512u32).map(|i| (i / 512) as u8).collect();
        std::fs::write(&p, &data).unwrap();
        let src = Source::open(p.to_str().unwrap()).unwrap();
        let mut seen = Vec::new();
        let want = |c: u64| !c.is_multiple_of(3);
        sweep(&src, 512, 512, 63, &want, None, &mut |c, b| {
            assert!(b.iter().all(|&x| x as u64 == c + 1));
            seen.push(c);
        });
        assert_eq!(seen, (0..63u64).filter(|c| !c.is_multiple_of(3)).collect::<Vec<_>>());
        drop(src);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
