//! Disk imaging for failing drives.
//!
//! Pass 1 reads large blocks and does not stop at errors: a block that fails is marked
//! for later and the pass moves on, so the healthy areas are saved first, before the
//! drive gets worse. Pass 2 goes back to the failed blocks sector by sector. A map file
//! next to the image records what is done, so an interrupted imaging resumes where it
//! stopped. Unreadable sectors stay zeros in the image.
//!
//! Map format, one range per line: `offset length status`, status `+` copied, `-` failed
//! in pass 1, `*` unreadable after pass 2, `?` not tried yet.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, Seek, SeekFrom, Write};
use std::os::windows::fs::FileExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::source::{ReadSlot, Source};

const BLOCK: u64 = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Untried,
    Copied,
    Failed,
    Bad,
}

impl Status {
    fn ch(self) -> char {
        match self {
            Status::Untried => '?',
            Status::Copied => '+',
            Status::Failed => '-',
            Status::Bad => '*',
        }
    }
    fn of(c: &str) -> Option<Status> {
        Some(match c {
            "?" => Status::Untried,
            "+" => Status::Copied,
            "-" => Status::Failed,
            "*" => Status::Bad,
            _ => return None,
        })
    }
}

/// Ranges of the source (relative to the imaged start), sorted, adjacent ones merged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Map {
    pub ranges: Vec<(u64, u64, Status)>,
    identity: Option<String>,
}

impl Map {
    pub fn new(len: u64) -> Map {
        Map { ranges: vec![(0, len, Status::Untried)], identity: None }
    }

    pub fn total(&self) -> u64 {
        self.ranges.iter().map(|r| r.1).sum()
    }

    pub fn bytes(&self, st: Status) -> u64 {
        self.ranges.iter().filter(|r| r.2 == st).map(|r| r.1).sum()
    }

    /// Mark `off..off+len` as `st`.
    pub fn set(&mut self, off: u64, len: u64, st: Status) {
        let end = off + len;
        let mut out = Vec::with_capacity(self.ranges.len() + 2);
        for &(s, l, x) in &self.ranges {
            let e = s + l;
            if e <= off || s >= end {
                out.push((s, l, x));
                continue;
            }
            if s < off {
                out.push((s, off - s, x));
            }
            if e > end {
                out.push((end, e - end, x));
            }
        }
        out.push((off, len, st));
        out.sort_by_key(|r| r.0);
        let mut merged: Vec<(u64, u64, Status)> = Vec::with_capacity(out.len());
        for r in out {
            match merged.last_mut() {
                Some(m) if m.2 == r.2 && m.0 + m.1 == r.0 => m.1 += r.1,
                _ => merged.push(r),
            }
        }
        self.ranges = merged;
    }

    pub fn of(&self, st: Status) -> Vec<(u64, u64)> {
        self.ranges.iter().filter(|r| r.2 == st).map(|r| (r.0, r.1)).collect()
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let tmp = path.with_extension("map.tmp");
        let file = File::create_new(&tmp)?;
        let result = (|| {
            let mut f = io::BufWriter::new(file);
            writeln!(f, "# FerrumRecovery imaging map: offset length status (+ copied, - failed, * unreadable, ? not tried)")?;
            if let Some(identity) = &self.identity {
                writeln!(f, "# identity {identity}")?;
            }
            for &(s, l, x) in &self.ranges {
                writeln!(f, "{s} {l} {}", x.ch())?;
            }
            f.flush()?;
            f.get_ref().sync_all()?;
            drop(f);
            std::fs::rename(&tmp, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    pub fn load(path: &Path, len: u64) -> io::Result<Map> {
        let mut m = Map::new(len);
        m.ranges.clear();
        let mut covered = 0u64;
        let bad = || io::Error::new(io::ErrorKind::InvalidData, crate::error::Error::new("invalid or incomplete imaging map"));
        if std::fs::metadata(path)?.len() > 64 << 20 {
            return Err(bad());
        }
        for line in io::BufReader::new(File::open(path)?).lines() {
            let line = line?;
            if let Some(identity) = line.strip_prefix("# identity ") {
                m.identity = Some(identity.to_string());
                continue;
            }
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let mut it = line.split_whitespace();
            let (Some(a), Some(b), Some(c)) = (it.next(), it.next(), it.next()) else {
                return Err(bad());
            };
            let (Ok(s), Ok(l), Some(x)) = (a.parse::<u64>(), b.parse::<u64>(), Status::of(c)) else {
                return Err(bad());
            };
            if it.next().is_some() || s != covered || l == 0 || s.checked_add(l).is_none_or(|n| n > len) {
                return Err(bad());
            }
            covered = s + l;
            m.ranges.push((s, l, x));
        }
        if covered != len {
            return Err(bad());
        }
        Ok(m)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Progress {
    pub pass: u8,
    pub done: u64,
    pub total: u64,
    pub copied: u64,
    pub bad: u64,
    pub elapsed: Duration,
}

/// Image `len` bytes from `start` of `src` into `out`, resuming from `map_path` if it
/// exists. `stop()` is polled to interrupt (the map keeps the place).
pub fn image(
    src: &Source,
    start: u64,
    len: u64,
    out: &Path,
    map_path: &Path,
    progress: &mut dyn FnMut(&Progress),
    stop: &dyn Fn() -> bool,
) -> io::Result<Map> {
    src.checkpoint()?;
    let invalid = |s: &'static str| crate::error::Error::new(s).into_io(io::ErrorKind::InvalidInput);
    if len == 0 || start.checked_add(len).is_none_or(|end| end > src.size) {
        return Err(invalid("imaging range is outside the source"));
    }
    let out = std::path::absolute(out)?;
    let map_path = std::path::absolute(map_path)?;
    if out == map_path {
        return Err(invalid("image and map paths must differ"));
    }
    let _target = crate::recover::check_target(src, start, len, out.parent().ok_or_else(|| invalid("no output folder"))?)
        .map_err(|e| e.into_io(io::ErrorKind::InvalidInput))?;
    let _map_target = crate::recover::check_target(src, start, len, map_path.parent().ok_or_else(|| invalid("no map folder"))?)
        .map_err(|e| e.into_io(io::ErrorKind::InvalidInput))?;
    for path in [&out, &map_path] {
        if path.exists() && matches!(src.kind, crate::source::SourceKind::Image(_)) && src.same_file(&File::open(path)?)? {
            return Err(invalid("destination refers to the source image"));
        }
    }
    let resume = map_path.exists();
    if resume && (!out.exists() || std::fs::metadata(&out)?.len() != len) {
        return Err(invalid("resume requires the original output image with its original length"));
    }
    if !resume && out.exists() {
        return Err(invalid("output already exists without a resume map"));
    }
    let mut map = if resume { Map::load(&map_path, len)? } else { Map::new(len) };
    let source_identity = src.identity()?;
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if !resume {
        options.create_new(true);
    }
    let mut f = options.open(&out)?;
    let identity = format!("{source_identity}:{start}:{len}:{:?}", crate::source::file_key(f.as_raw_handle())?);
    if resume && map.identity.as_deref() != Some(&identity) {
        return Err(invalid("resume map does not match this source, range and output image"));
    }
    map.identity = Some(identity);
    if f.seek(SeekFrom::End(0))? < len {
        f.set_len(len)?;
    }
    if !resume {
        f.sync_data()?;
        if let Err(e) = map.save(&map_path) {
            drop(f);
            let _ = std::fs::remove_file(&out);
            return Err(e);
        }
    }
    let sec = src.sector as u64;
    let mut slot = ReadSlot::new()?;
    let mut buf = src.buffer(BLOCK as usize);
    let t0 = Instant::now();
    let mut last_save = Instant::now();
    let mut p = Progress { total: len, ..Default::default() };
    let mut report = |map: &Map, p: &mut Progress, pass: u8| {
        p.pass = pass;
        p.copied = map.bytes(Status::Copied);
        p.bad = map.bytes(Status::Failed) + map.bytes(Status::Bad);
        p.done = len - map.bytes(Status::Untried) - if pass == 2 { map.bytes(Status::Failed) } else { 0 };
        p.elapsed = t0.elapsed();
        progress(p);
    };

    // Pass 1: large blocks, failures are skipped.
    for (s, l) in map.of(Status::Untried) {
        let mut k = 0;
        while k < l {
            if stop() {
                f.sync_data()?;
                map.save(&map_path)?;
                return Ok(map);
            }
            let n = BLOCK.min(l - k);
            let rd = if src.is_device() { n.div_ceil(sec) * sec } else { n };
            let at = s + k;
            let read = src.read_with(&mut slot, start + at, &mut buf[..rd as usize]);
            if read.as_ref().is_err_and(|e| e.kind() == io::ErrorKind::Interrupted) {
                f.sync_data()?;
                map.save(&map_path)?;
                return if stop() { Ok(map) } else { Err(read.unwrap_err()) };
            }
            if stop() {
                f.sync_data()?;
                map.save(&map_path)?;
                return Ok(map);
            }
            if read.is_ok() {
                write_at_all(&f, &buf[..n as usize], at)?;
                map.set(at, n, Status::Copied);
            } else {
                map.set(at, n, Status::Failed);
            }
            k += n;
            if last_save.elapsed() > Duration::from_secs(5) {
                f.sync_data()?;
                map.save(&map_path)?;
                last_save = Instant::now();
            }
            report(&map, &mut p, 1);
        }
    }
    f.sync_data()?;
    map.save(&map_path)?;

    // Pass 2: failed blocks sector by sector.
    for (s, l) in map.of(Status::Failed) {
        let mut k = 0;
        while k < l {
            if stop() {
                f.sync_data()?;
                map.save(&map_path)?;
                return Ok(map);
            }
            let n = sec.min(l - k);
            let at = s + k;
            let rd = if src.is_device() { sec } else { n };
            let read = src.read_with(&mut slot, start + at, &mut buf[..rd as usize]);
            if read.as_ref().is_err_and(|e| e.kind() == io::ErrorKind::Interrupted) {
                f.sync_data()?;
                map.save(&map_path)?;
                return if stop() { Ok(map) } else { Err(read.unwrap_err()) };
            }
            if stop() {
                f.sync_data()?;
                map.save(&map_path)?;
                return Ok(map);
            }
            let ok = read.is_ok();
            if ok {
                write_at_all(&f, &buf[..n as usize], at)?;
            }
            map.set(at, n, if ok { Status::Copied } else { Status::Bad });
            k += n;
            if last_save.elapsed() > Duration::from_secs(5) {
                f.sync_data()?;
                map.save(&map_path)?;
                last_save = Instant::now();
            }
            report(&map, &mut p, 2);
        }
    }
    f.flush()?;
    f.sync_data()?;
    map.save(&map_path)?;
    report(&map, &mut p, 2);
    Ok(map)
}

fn write_at_all(file: &File, mut bytes: &[u8], mut at: u64) -> io::Result<()> {
    while !bytes.is_empty() {
        let n = file.seek_write(bytes, at)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, crate::error::Error::new("short image write")));
        }
        bytes = &bytes[n..];
        at += n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_during_retry_does_not_turn_readable_sectors_bad() {
        let dir = std::env::temp_dir().join(format!("fr-image-cancel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("a.img");
        let output = dir.join("b.img");
        let map_path = dir.join("b.map");
        let bytes = vec![23u8; 2048];
        std::fs::write(&input, &bytes).unwrap();
        let source = Source::open(input.to_str().unwrap()).unwrap();
        let mut map = image(&source, 0, 2048, &output, &map_path, &mut |_| {}, &|| true).unwrap();
        map.set(0, 2048, Status::Failed);
        map.save(&map_path).unwrap();
        let control = std::sync::Arc::new(crate::control::Control::default());
        let controlled = source.try_clone().unwrap().with_control(control.clone());
        let result = image(
            &controlled,
            0,
            2048,
            &output,
            &map_path,
            &mut |p| {
                if p.pass == 2 && p.copied >= 512 {
                    control.cancel();
                }
            },
            &|| false,
        );
        assert!(result.is_err());
        let map = Map::load(&map_path, 2048).unwrap();
        assert_eq!(map.bytes(Status::Bad), 0);
        assert_eq!(map.bytes(Status::Failed), 1536);
        image(&source, 0, 2048, &output, &map_path, &mut |_| {}, &|| false).unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), bytes);
        drop(controlled);
        drop(source);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn image_keeps_a_non_sector_aligned_tail() {
        let dir = std::env::temp_dir().join(format!("fr-image-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("a.img");
        let output = dir.join("b.img");
        let map = dir.join("b.map");
        let bytes: Vec<_> = (0..1000).map(|i| (i % 251) as u8).collect();
        std::fs::write(&input, &bytes).unwrap();
        let src = Source::open(input.to_str().unwrap()).unwrap();
        let status = image(&src, 0, 1000, &output, &map, &mut |_| {}, &|| false).unwrap();
        assert_eq!(status.bytes(Status::Bad), 0);
        assert_eq!(std::fs::read(&output).unwrap(), bytes);
        drop(src);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn imaging_refuses_source_as_destination() {
        let dir = std::env::temp_dir().join(format!("fr-image-self-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("source.img");
        let map = dir.join("source.map");
        let content = vec![17u8; 4096];
        std::fs::write(&path, &content).unwrap();
        let src = Source::open(path.to_str().unwrap()).unwrap();
        let result = image(&src, 0, src.size, &path, &map, &mut |_| {}, &|| false);
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), content);
        drop(src);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resume_requires_its_original_source_and_output() {
        let dir = std::env::temp_dir().join(format!("fr-image-resume-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.img");
        let b = dir.join("b.img");
        let out = dir.join("copy.img");
        let map = dir.join("copy.map");
        std::fs::write(&a, vec![1; 4096]).unwrap();
        std::fs::write(&b, vec![2; 4096]).unwrap();
        let src = Source::open(a.to_str().unwrap()).unwrap();
        image(&src, 0, 4096, &out, &map, &mut |_| {}, &|| false).unwrap();
        let other = Source::open(b.to_str().unwrap()).unwrap();
        assert!(image(&other, 0, 4096, &out, &map, &mut |_| {}, &|| false).is_err());
        std::fs::remove_file(&out).unwrap();
        assert!(image(&src, 0, 4096, &out, &map, &mut |_| {}, &|| false).is_err());
        assert!(!out.exists());
        drop(src);
        drop(other);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn map_marks_and_merges() {
        let mut m = Map::new(100);
        m.set(10, 20, Status::Copied);
        m.set(30, 10, Status::Copied);
        m.set(50, 5, Status::Failed);
        assert_eq!(
            m.ranges,
            vec![
                (0, 10, Status::Untried),
                (10, 30, Status::Copied),
                (40, 10, Status::Untried),
                (50, 5, Status::Failed),
                (55, 45, Status::Untried)
            ]
        );
        m.set(0, 100, Status::Copied);
        assert_eq!(m.ranges, vec![(0, 100, Status::Copied)]);
    }

    #[test]
    fn image_copies_and_resumes() {
        let dir = std::env::temp_dir().join(format!("fr-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src_path = dir.join("src.img");
        let data: Vec<u8> = (0..3 * BLOCK as usize + 1024).map(|i| (i * 7 % 251) as u8).collect();
        std::fs::write(&src_path, &data).unwrap();
        let src = Source::open(src_path.to_str().unwrap()).unwrap();
        let out = dir.join("out.img");
        let map = dir.join("out.img.map");
        // Interrupted after the first block, then resumed.
        let count = std::cell::Cell::new(0);
        let m = image(&src, 0, src.size, &out, &map, &mut |_| {}, &|| {
            count.set(count.get() + 1);
            count.get() > 1
        })
        .unwrap();
        assert!(m.bytes(Status::Untried) > 0);
        let m = image(&src, 0, src.size, &out, &map, &mut |_| {}, &|| false).unwrap();
        assert_eq!(m.ranges, vec![(0, src.size, Status::Copied)]);
        assert_eq!(std::fs::read(&out).unwrap(), data);
        assert_eq!(Map::load(&map, src.size).unwrap(), m);
        drop(src);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
