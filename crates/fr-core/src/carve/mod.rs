//! Deep scan by file signatures: files whose records are gone entirely, found by their
//! content. Free-space search visits cluster starts; full search visits every 512-byte
//! boundary, including allocated space. Format parsers determine contiguous file ends.
//! Fragmentation cannot in general be reconstructed from a signature alone.
//!
//! Formats without a stated end (FLAC, GZIP) or data that breaks its format end at the
//! first empty cluster or at the next file found, and are marked partial.

mod boundaries;
pub mod formats;
mod legacy;
mod media;
pub mod stream;
mod text;
mod zeros;
mod zip;

use crate::model::{Catalog, Data, Entry, Run, State, ef};
use crate::source::Source;
use crate::types::Group;
pub use formats::{Fmt, detect};
use stream::Stream;

/// Ids of entries found by signature start here, above the other found entries.
pub const CARVE_ID_BASE: u64 = 30_000_000_000;

/// Name of the folder that holds files found by signature.
pub const CARVE_ROOT: &str = "<Found by signature>";
pub const TEXT_ROOT: &str = "<Text fragments>";

/// Collects cluster starts that look like the start of a file.
#[derive(Default)]
pub struct Carver {
    pub cands: Vec<(u64, Fmt)>,
    zip: zip::Index,
    text: text::Index,
    boundaries: boundaries::Index,
    zeros: zeros::Index,
}

impl Carver {
    pub fn observe(&mut self, lcn: u64, cluster: &[u8]) {
        self.observe_content(lcn * cluster.len() as u64, cluster, cluster.len());
        self.observe_start(lcn, cluster);
    }

    pub fn observe_zip(&mut self, at: u64, bytes: &[u8]) {
        self.zip.observe(at, bytes);
    }

    pub fn observe_content(&mut self, at: u64, bytes: &[u8], unit: usize) {
        self.observe_content_status(at, bytes, unit, true);
    }

    pub fn observe_content_status(&mut self, at: u64, bytes: &[u8], unit: usize, readable: bool) {
        self.zip.observe(at, bytes);
        self.text.observe(at, bytes, unit);
        if readable {
            self.boundaries.observe(at, bytes);
            self.zeros.observe(at, bytes);
        }
    }

    /// Enable negative end-marker lookups over the ranges observed successfully.
    pub fn finish_content(&mut self, reliable: bool) {
        self.boundaries.trust(reliable);
        self.zeros.trust(reliable);
    }

    pub fn observe_start(&mut self, lcn: u64, cluster: &[u8]) {
        if let Some(f) = detect(cluster) {
            self.cands.push((lcn, f));
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Carved {
    pub lcn: u64,
    pub len: u64,
    pub fmt: Fmt,
    pub ext: &'static str,
    pub name: Option<String>,
    /// The end came from the format itself.
    pub complete: bool,
}

/// Where clusters are: cluster 0 at byte `base` of `src`.
pub struct Area<'a> {
    pub src: &'a Source,
    pub base: u64,
    pub cs: u32,
    pub clusters: u64,
}

/// Lengths, extensions and names of the candidates. `usable(c)`: cluster `c` may belong
/// to a carved file (free, and not part of a file the catalog already has).
pub fn resolve(area: &Area, cands: &[(u64, Fmt)], usable: &dyn Fn(u64) -> bool) -> Vec<Carved> {
    resolve_impl(area, cands, Some(usable), area.clusters * area.cs as u64, None, None)
}

/// Full-range carving does not trust the current allocation map or suppress overlaps.
pub fn resolve_all(area: &Area, cands: &[(u64, Fmt)], bytes: u64) -> Vec<Carved> {
    resolve_impl(area, cands, None, bytes, None, None)
}

/// Progress counts candidates whose format analysis has finished.
#[derive(Default)]
pub struct ResolveProgress {
    pub total: std::sync::atomic::AtomicU64,
    pub done: std::sync::atomic::AtomicU64,
}

pub fn resolve_indexed(area: &Area, carver: &Carver, bytes: u64, progress: Option<&ResolveProgress>) -> Vec<Carved> {
    let mut out = resolve_impl(area, &carver.cands, None, bytes, Some(carver), progress);
    append_text(&mut out, area, carver, bytes);
    out
}

fn append_text(out: &mut Vec<Carved>, area: &Area, carver: &Carver, bytes: u64) {
    let mut spans = carver.text.spans().peekable();
    if spans.peek().is_none() {
        return;
    }
    let complete: std::collections::HashSet<_> = out.iter().filter(|c| c.complete).map(|c| (c.lcn, c.len)).collect();
    let cs = area.cs as u64;
    for span in spans {
        if !span.start.is_multiple_of(cs) || span.start.checked_add(span.len).is_none_or(|end| end > bytes) {
            continue;
        }
        let lcn = span.start / cs;
        if complete.contains(&(lcn, span.len)) {
            continue;
        }
        if let Ok(i) = carver.cands.binary_search_by_key(&lcn, |c| c.0)
            && matches!(carver.cands[i].1, Fmt::Xml | Fmt::Html | Fmt::Rtf)
        {
            continue;
        }
        out.push(Carved { lcn, len: span.len, fmt: Fmt::Text, ext: span.ext, name: None, complete: false });
    }
}

fn resolve_impl(
    area: &Area,
    cands: &[(u64, Fmt)],
    usable: Option<&dyn Fn(u64) -> bool>,
    bytes: u64,
    indexes: Option<&Carver>,
    progress: Option<&ResolveProgress>,
) -> Vec<Carved> {
    use std::sync::atomic::Ordering::Relaxed;
    if let Some(p) = progress {
        p.done.store(0, Relaxed);
        p.total.store(cands.len() as u64, Relaxed);
    }
    let cs = area.cs as u64;
    let mut out = Vec::new();
    if cands.is_empty() {
        return out;
    }
    let Ok(mut s) = Stream::new(area.src, 0, 0) else { return out };
    s.set_rtf_candidates(cands, area.base, cs);
    if let Some(indexes) = indexes {
        s.set_boundaries(&indexes.boundaries, area.base);
        s.set_zeros(&indexes.zeros, area.base);
    }
    let zip_ends = indexes.map(|i| i.zip.resolve(area.src, area.base, bytes));
    // End of the run of usable clusters containing the current candidate.
    let mut region = (0u64, 0u64);
    for (k, &(lcn, fmt)) in cands.iter().enumerate() {
        if let Some(p) = progress {
            p.done.store(k as u64, Relaxed);
        }
        if area.src.checkpoint().is_err() {
            break;
        }
        if lcn >= area.clusters || usable.is_some_and(|f| !f(lcn)) {
            continue;
        }
        if !(region.0..region.1).contains(&lcn) {
            let mut e = lcn + 1;
            if let Some(f) = usable {
                while e < area.clusters && f(e) {
                    e += 1;
                }
            } else {
                e = area.clusters;
            }
            region = (lcn, e);
        }
        let cap = ((region.1 - lcn) * cs).min(bytes.saturating_sub(lcn * cs)).min(fmt.max_len());
        s.reset(area.base + lcn * cs, cap);
        if fmt == Fmt::Zip
            && let Some(ends) = &zip_ends
        {
            s.zip_indexed = true;
            s.zip_hint = ends.get(&(lcn * cs)).copied();
        }
        let next = cands[k + 1..].iter().find(|c| c.0 > lcn).map(|c| c.0);
        let textual = matches!(fmt, Fmt::Xml | Fmt::Html);
        let hint = indexes.and_then(|index| textual.then(|| index.text.at(lcn * cs)).flatten());
        if textual && hint.is_none() {
            s.reset(area.base + lcn * cs, next.map_or(cap, |n| ((n - lcn) * cs).min(cap)));
        }
        // Head metadata belongs before forward payload parsing; PDF titles need the resolved tail.
        let head_name = matches!(fmt, Fmt::Jpeg | Fmt::Mp3).then(|| formats::meta_name(fmt, &mut s, cap));
        let head_ext =
            matches!(fmt, Fmt::Tiff | Fmt::Riff | Fmt::Bmff | Fmt::Ebml | Fmt::Psd | Fmt::Pe).then(|| formats::extension(fmt, &mut s));
        let length = hint.map(|span| span.len.min(cap)).or_else(|| formats::length(fmt, &mut s));
        let (len, complete) = match length {
            Some(l) if l > 0 && l <= cap => (l, !matches!(fmt, Fmt::Flv | Fmt::Xml | Fmt::Html)),
            _ if matches!(
                fmt,
                Fmt::Cab | Fmt::Pcx | Fmt::Dcx | Fmt::Ico | Fmt::Tga | Fmt::Midi | Fmt::Aiff | Fmt::Sfnt | Fmt::Swf | Fmt::Flv | Fmt::EaVp6
            ) =>
            {
                continue;
            }
            // Unknown ZIP boundaries stay partial; do not reread their payload to
            // look for zero clusters, often the entire remaining selected range.
            _ if fmt == Fmt::Zip => (next.map_or(cap, |n| ((n - lcn) * cs).min(cap)), false),
            _ => (fallback_len(&mut s, cs, lcn, next), false),
        };
        let ext = head_ext.unwrap_or_else(|| formats::extension(fmt, &mut s));
        let name = head_name.unwrap_or_else(|| formats::meta_name(fmt, &mut s, len));
        out.push(Carved { lcn, len, fmt, ext, name, complete });
    }
    if let Some(p) = progress {
        p.done.store(cands.len() as u64, Relaxed);
    }
    out
}

/// End of a file whose format does not tell: the first empty cluster, the next file
/// found, or the cap.
fn fallback_len(s: &mut Stream, cs: u64, lcn: u64, next: Option<u64>) -> u64 {
    let limit = next.map_or(s.cap(), |n| ((n - lcn) * cs).min(s.cap()));
    if let Some(len) = s.indexed_fallback(cs, limit) {
        return len;
    }
    let probe = cs.min(4096) as usize;
    let mut off = cs;
    while off < limit {
        match s.bytes(off, probe) {
            Some(b) if b.iter().all(|&x| x == 0) => return off,
            Some(_) => off += cs,
            None => break,
        }
    }
    limit
}

/// Merge only identical contiguous contents with matching format and logical size.
/// A larger, fragmented, compressed or damaged metadata claim cannot hide a candidate.
pub fn merge_carved(cat: &mut Catalog, carved: Vec<Carved>, full: bool) -> u64 {
    use std::collections::HashMap;
    if carved.is_empty() {
        return 0;
    }
    // Index candidates, not millions of metadata rows. All matching hard links
    // are confirmed during the one metadata pass without per-entry heap lists.
    let exact: HashMap<_, _> = carved.iter().enumerate().filter(|(_, c)| c.complete).map(|(i, c)| ((c.lcn, c.len, c.fmt), i)).collect();
    let mut matched = vec![false; carved.len()];
    for i in 0..cat.entries.len() {
        let e = cat.entries[i];
        if e.is_dir() || e.cu != 0 || e.valid != e.size || e.flags & (ef::INCOMPLETE | ef::ENCRYPTED) != 0 {
            continue;
        }
        let runs = cat.runs_of(i as u32);
        if runs.len() != 1 || !runs[0].is_real() || runs[0].len * (cat.unit as u64) < e.size {
            continue;
        }
        let Some(fmt) = formats::expected(cat.name(i as u32).rsplit('.').next().unwrap_or("")) else { continue };
        if let Some(&k) = exact.get(&(runs[0].lcn, e.size, fmt)) {
            cat.entries[i].flags |= ef::SIGNATURE;
            matched[k] = true;
        }
    }
    let retained = carved.into_iter().enumerate().filter_map(|(i, c)| (!matched[i]).then_some(c)).collect();
    let first = cat.entries.len();
    let count = add_to_catalog(cat, retained);
    if full {
        for e in &mut cat.entries[first..] {
            e.flags &= !ef::DELETED;
            if !e.is_dir() && e.flags & ef::INCOMPLETE == 0 {
                e.state = State::Unknown;
            }
        }
    }
    count
}

/// Carve the candidates into `cat`: lengths within clusters `free(c)` that no catalog file
/// owns. Returns how many files were added.
pub fn carve_into(cat: &mut Catalog, src: &Source, carver: &Carver, free: &dyn Fn(u64) -> bool) -> u64 {
    if carver.cands.is_empty() && carver.text.spans().next().is_none() {
        return 0;
    }
    let fs = &cat.fs;
    let bytes = fs.size - fs.data_offset;
    let area = Area { src, base: fs.offset + fs.data_offset, cs: cat.unit, clusters: bytes.div_ceil(cat.unit as u64) };
    let mut carved = resolve_impl(&area, &carver.cands, Some(free), bytes, Some(carver), None);
    append_text(&mut carved, &area, carver, bytes);
    merge_carved(cat, carved, false)
}

/// Carve a whole region with no usable file system: `size` bytes from `offset` of `src`,
/// looked at sector by sector. The catalog has only the found files.
pub fn scan_raw(src: &Source, offset: u64, size: u64, progress: Option<&crate::sweep::SweepProgress>) -> Catalog {
    let cs = src.sector.max(512);
    let clusters = size / cs as u64;
    let mut carver = Carver::default();
    crate::sweep::sweep(src, offset, cs, clusters, &|_| true, progress, &mut |c, b| carver.observe(c, b));
    let tail_at = clusters * cs as u64;
    if tail_at < size
        && let Ok(mut tail) = src.read_bytes(offset + tail_at, (size - tail_at) as usize)
    {
        carver.observe_content(tail_at, &tail, cs as usize);
        tail.resize(cs as usize, 0);
        carver.observe_start(clusters, &tail);
    }
    let fs = crate::model::FsInfo {
        kind: crate::part::FsKind::Unknown,
        offset,
        size,
        cluster_size: cs,
        record_size: 0,
        mft: Vec::new(),
        serial: 0,
        root: 0,
        data_offset: 0,
    };
    let mut cat = Catalog::new(fs, Vec::new(), Vec::new(), Vec::new());
    let (name_off, name_len) = cat.add_name("");
    cat.entries.push(Entry { id: 0, parent: crate::NONE, name_off, name_len, flags: ef::DIR, ..Default::default() });
    carve_into(&mut cat, src, &carver, &|_| true);
    cat
}

fn group_folder(g: Group) -> &'static str {
    match g {
        Group::Photo => "Photos",
        Group::Video => "Video",
        Group::Audio => "Audio",
        Group::Document => "Documents",
        Group::Archive => "Archives",
        Group::Mail => "Mail",
        Group::Other => "Other",
    }
}

/// Add carved files to the catalog under `<Found by signature>\<group>`.
pub fn add_to_catalog(cat: &mut Catalog, mut carved: Vec<Carved>) -> u64 {
    if carved.is_empty() {
        return 0;
    }
    carved.sort_by_key(|c| c.lcn);
    let cs = cat.unit as u64;
    let root = cat.find(cat.fs.root).unwrap_or(crate::NONE);
    let folder = |cat: &mut Catalog, id: u64, parent: u32, name: &str| -> u32 {
        let (name_off, name_len) = cat.add_name(name);
        cat.entries.push(Entry { id, parent, name_off, name_len, flags: ef::DIR | ef::DELETED | ef::FOUND, ..Default::default() });
        (cat.entries.len() - 1) as u32
    };
    let top = folder(cat, CARVE_ID_BASE, root, CARVE_ROOT);
    let text_root = carved.iter().any(|c| c.fmt == Fmt::Text).then(|| folder(cat, CARVE_ID_BASE + 20, top, TEXT_ROOT));
    let mut groups = [crate::NONE; Group::ALL.len()];
    let names: Vec<String> =
        carved.iter().map(|c| format!("{}.{}", c.name.clone().unwrap_or_else(|| format!("f{}", c.lcn)), c.ext)).collect();
    for (g, slot) in Group::ALL.iter().zip(groups.iter_mut()) {
        if names.iter().zip(&carved).any(|(n, c)| c.fmt != Fmt::Text && Group::of(n) == *g) {
            *slot = folder(cat, CARVE_ID_BASE + 1 + *g as u64, top, group_folder(*g));
        }
    }
    for (k, (c, name)) in carved.iter().zip(&names).enumerate() {
        let (name_off, name_len) = cat.add_name(name);
        let start = cat.runs.len() as u32;
        cat.runs.push(Run { lcn: c.lcn, len: c.len.div_ceil(cs).max(1) });
        cat.entries.push(Entry {
            id: CARVE_ID_BASE + 100 + k as u64,
            parent: if c.fmt == Fmt::Text { text_root.unwrap() } else { groups[Group::of(name) as usize] },
            name_off,
            name_len,
            flags: ef::DELETED | ef::FOUND | ef::SIGNATURE | if c.complete { 0 } else { ef::INCOMPLETE },
            state: if c.complete { State::Good } else { State::Partial },
            size: c.len,
            valid: c.len,
            data: Data::Runs { start, count: 1 },
            ..Default::default()
        });
    }
    carved.len() as u64
}

#[cfg(test)]
mod tests {
    use super::formats::*;
    use super::*;

    /// Source over a temporary image with `files` laid out from cluster 0, 4 KiB clusters.
    fn image(name: &str, data: &[u8]) -> (std::path::PathBuf, Source) {
        let dir = std::env::temp_dir().join(format!("fr-carve-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("c.img");
        std::fs::write(&p, data).unwrap();
        let src = Source::open(p.to_str().unwrap()).unwrap();
        (dir, src)
    }

    fn pad(v: &mut Vec<u8>, to: usize) {
        v.resize(v.len().div_ceil(to) * to, 0);
    }

    fn png_file(extra: usize) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        for (t, n) in [(b"IHDR", 13usize), (b"IDAT", 5000 + extra), (b"IEND", 0)] {
            v.extend_from_slice(&(n as u32).to_be_bytes());
            v.extend_from_slice(t);
            v.extend(std::iter::repeat_n(0xAB, n));
            v.extend_from_slice(&[0, 0, 0, 0]);
        }
        v
    }

    fn jpeg_file() -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8];
        // APP1 with EXIF: Make, Model, and DateTimeOriginal in an Exif IFD.
        let mut t = b"II*\0\x08\0\0\0".to_vec();
        let ifd0 = 8usize;
        let make_at = ifd0 + 2 + 3 * 12 + 4;
        let model_at = make_at + 6;
        let exif_ifd = model_at + 8;
        let date_at = exif_ifd + 2 + 12 + 4;
        t.extend_from_slice(&3u16.to_le_bytes());
        for (tag, ty, count, val) in
            [(0x010Fu16, 2u16, 6u32, make_at as u32), (0x0110, 2, 8, model_at as u32), (0x8769, 4, 1, exif_ifd as u32)]
        {
            t.extend_from_slice(&tag.to_le_bytes());
            t.extend_from_slice(&ty.to_le_bytes());
            t.extend_from_slice(&count.to_le_bytes());
            t.extend_from_slice(&val.to_le_bytes());
        }
        t.extend_from_slice(&[0, 0, 0, 0]);
        t.extend_from_slice(b"Canon\0");
        t.extend_from_slice(b"EOS 80D\0");
        t.extend_from_slice(&1u16.to_le_bytes());
        t.extend_from_slice(&0x9003u16.to_le_bytes());
        t.extend_from_slice(&2u16.to_le_bytes());
        t.extend_from_slice(&20u32.to_le_bytes());
        t.extend_from_slice(&(date_at as u32).to_le_bytes());
        t.extend_from_slice(&[0, 0, 0, 0]);
        t.extend_from_slice(b"2026:03:15 12:34:56\0");
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend_from_slice(&t);
        v.extend_from_slice(&[0xFF, 0xE1]);
        v.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        v.extend_from_slice(&app1);
        // SOS with entropy data containing stuffed FF00 and a restart marker.
        v.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 1, 2, 3, 4, 5, 6]);
        for i in 0..9000u32 {
            v.push((i % 251) as u8);
            if i % 1000 == 0 {
                v.extend_from_slice(&[0xFF, 0x00]);
            }
        }
        v.extend_from_slice(&[0xFF, 0xD3, 7, 7, 0xFF, 0xD9]);
        v
    }

    fn zip_docx() -> Vec<u8> {
        let name = b"word/document.xml";
        let mut v = b"PK\x03\x04".to_vec();
        v.extend_from_slice(&[0u8; 22]);
        v.extend_from_slice(&(name.len() as u16).to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(name);
        v.extend(std::iter::repeat_n(b'x', 6000));
        v[18..22].copy_from_slice(&6000u32.to_le_bytes());
        v[22..26].copy_from_slice(&6000u32.to_le_bytes());
        let cd = v.len();
        v.extend_from_slice(b"PK\x01\x02");
        v.extend_from_slice(&[0u8; 24]);
        v.extend_from_slice(&(name.len() as u16).to_le_bytes());
        v.extend_from_slice(&[0u8; 16]);
        v.extend_from_slice(name);
        let cd_size = v.len() - cd;
        v.extend_from_slice(b"PK\x05\x06");
        v.extend_from_slice(&[0u8; 4]);
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&(cd_size as u32).to_le_bytes());
        v.extend_from_slice(&(cd as u32).to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v
    }

    #[test]
    fn carves_by_structure_and_names_from_metadata() {
        const CS: usize = 4096;
        let files: Vec<Vec<u8>> = vec![
            jpeg_file(),
            png_file(0),
            zip_docx(),
            b"%PDF-1.7\n1 0 obj\n<< /Title (Annual report) >>\nendobj\ntrailer\n%%EOF\n".to_vec(),
        ];
        let mut img = Vec::new();
        let mut starts = Vec::new();
        for f in &files {
            starts.push(img.len() / CS);
            img.extend_from_slice(f);
            pad(&mut img, CS);
            img.extend(vec![0u8; CS]); // an empty cluster between files
        }
        let (dir, src) = image("structure", &img);
        let mut carver = Carver::default();
        for (c, cl) in img.chunks(CS).enumerate() {
            carver.observe(c as u64, cl);
        }
        let area = Area { src: &src, base: 0, cs: CS as u32, clusters: (img.len() / CS) as u64 };
        let got = resolve(&area, &carver.cands, &|_| true);
        assert_eq!(got.len(), 4);
        for ((g, f), s) in got.iter().zip(&files).zip(&starts) {
            assert_eq!(g.lcn, *s as u64);
            assert_eq!(g.len, f.len() as u64, "{:?}", g.fmt);
            assert!(g.complete);
        }
        assert_eq!(got[0].name.as_deref(), Some("2026-03-15 12-34-56 Canon EOS 80D"));
        assert_eq!(got[2].ext, "docx");
        assert_eq!(got[3].name.as_deref(), Some("Annual report"));
        drop(src);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unknown_end_stops_at_an_empty_cluster() {
        const CS: usize = 4096;
        let mut img = vec![0x1F, 0x8B, 0x08, 0x00];
        img.extend(std::iter::repeat_n(0x55, 3 * CS - 4));
        img.extend(vec![0u8; CS]);
        img.extend(vec![0x66u8; CS]);
        let (dir, src) = image("fallback", &img);
        let mut carver = Carver::default();
        for (c, cl) in img.chunks(CS).enumerate() {
            carver.observe(c as u64, cl);
        }
        let area = Area { src: &src, base: 0, cs: CS as u32, clusters: 5 };
        let got = resolve(&area, &carver.cands, &|_| true);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].len, got[0].complete, got[0].ext), (3 * CS as u64, false, "gz"));
        drop(src);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn indexed_partial_boundaries_keep_lengths_and_avoid_a_second_payload_pass() {
        let mut bytes = vec![0x55; (8 << 20) + 123];
        bytes[..4].copy_from_slice(&[0x1f, 0x8b, 8, 0]);
        bytes[3 << 20..(3 << 20) + 512].fill(0);
        let at = 4 << 20;
        bytes[at..at + 5].copy_from_slice(b"fLaC\0");
        let (dir, src) = image("indexed-partials", &bytes);
        let mut carver = Carver::default();
        for (i, b) in bytes.chunks(8192).enumerate() {
            carver.observe_content(i as u64 * 8192, b, 512);
        }
        carver.cands = vec![(0, Fmt::Gzip), (at as u64 / 512, Fmt::Flac)];
        let area = Area { src: &src, base: 0, cs: 512, clusters: bytes.len().div_ceil(512) as u64 };
        stream::TEST_READ_BYTES.with(|n| n.set(0));
        let direct = resolve_all(&area, &carver.cands, bytes.len() as u64);
        let direct_bytes = stream::TEST_READ_BYTES.with(|n| n.get());
        carver.finish_content(true);
        stream::TEST_READ_BYTES.with(|n| n.set(0));
        let indexed = resolve_indexed(&area, &carver, bytes.len() as u64, None);
        assert_eq!(indexed, direct);
        assert_eq!(indexed[0].len, 3 << 20);
        assert_eq!(indexed[1].len, bytes.len() as u64 - at as u64);
        assert!(indexed.iter().all(|c| !c.complete));
        assert!(direct_bytes > 6 << 20);
        assert_eq!(stream::TEST_READ_BYTES.with(|n| n.get()), 0);
        let mut unobserved = Carver { cands: carver.cands.clone(), ..Default::default() };
        unobserved.observe_content_status(0, &bytes, 512, false);
        unobserved.finish_content(true);
        stream::TEST_READ_BYTES.with(|n| n.set(0));
        assert_eq!(resolve_indexed(&area, &unobserved, bytes.len() as u64, None), direct);
        assert!(stream::TEST_READ_BYTES.with(|n| n.get()) > 6 << 20);
        drop(src);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn head_metadata_precedes_payload_reads_without_changing_the_result() {
        let mut bytes = jpeg_file();
        let end = bytes.split_off(bytes.len() - 2);
        bytes.resize(4 << 20, 0x55);
        bytes.extend_from_slice(&end);
        bytes.resize(bytes.len() + 4, 0);
        let (dir, src) = image("head-order", &bytes);
        let mut reference = Stream::new(&src, 0, bytes.len() as u64).unwrap();
        let len = formats::length(Fmt::Jpeg, &mut reference).unwrap();
        let ext = formats::extension(Fmt::Jpeg, &mut reference);
        let name = formats::meta_name(Fmt::Jpeg, &mut reference, len);
        let old_reads = reference.read_bytes;
        let area = Area { src: &src, base: 0, cs: 512, clusters: bytes.len().div_ceil(512) as u64 };
        stream::TEST_READ_BYTES.with(|n| n.set(0));
        let got = resolve_all(&area, &[(0, Fmt::Jpeg)], bytes.len() as u64);
        assert_eq!(got, vec![Carved { lcn: 0, len, fmt: Fmt::Jpeg, ext, name, complete: true }]);
        assert!(stream::TEST_READ_BYTES.with(|n| n.get()) < old_reads);
        drop(reference);
        drop(src);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn nested_rtf_candidates_share_parsing_and_keep_every_file() {
        let mut bytes = vec![b'X'; 8 << 20];
        let cands: Vec<_> = (0..64).map(|i| (i, Fmt::Rtf)).collect();
        for &(i, _) in &cands {
            bytes[i as usize * 512..i as usize * 512 + 7].copy_from_slice(b"{\\rtf1 ");
        }
        let end = bytes.len();
        bytes[end - 64..].fill(b'}');
        let (dir, src) = image("nested-rtf", &bytes);
        let area = Area { src: &src, base: 0, cs: 512, clusters: bytes.len() as u64 / 512 };
        stream::TEST_READ_BYTES.with(|n| n.set(0));
        let found = resolve_all(&area, &cands, bytes.len() as u64);
        assert_eq!(found.len(), 64);
        for (i, f) in found.iter().enumerate() {
            assert!(f.complete);
            assert_eq!(f.len, bytes.len() as u64 - i as u64 * 513);
        }
        let read = stream::TEST_READ_BYTES.with(|n| n.get());
        assert!(read < 16 << 20, "Nested RTF analysis requested {read} bytes");
        drop(src);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rtf_escape_state_crosses_windows() {
        let mut bytes = b"{\\rtf1 ".to_vec();
        bytes.resize((60 << 10) - 1, b'X');
        bytes.extend_from_slice(b"\\} kept}");
        let len = bytes.len() as u64;
        let (dir, src) = image("rtf-escape", &bytes);
        let mut s = Stream::new(&src, 0, len).unwrap();
        assert_eq!(length(Fmt::Rtf, &mut s), Some(len));
        drop(s);
        drop(src);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn indexed_tga_absence_avoids_repeated_payload_reads_and_untrusted_data_falls_back() {
        let mut bytes = vec![0x55; 8 << 20];
        let mut carver = Carver::default();
        for i in 0..64 {
            let h = &mut bytes[i * 512..i * 512 + 18];
            h.fill(0);
            h[2] = 2;
            h[12] = 1;
            h[14] = 1;
            h[16] = 24;
        }
        let (dir, src) = image("indexed-tga", &bytes);
        for (i, b) in bytes.chunks(512).enumerate() {
            carver.observe(i as u64, b);
        }
        assert_eq!(carver.cands.len(), 64);
        let area = Area { src: &src, base: 0, cs: 512, clusters: bytes.len() as u64 / 512 };
        carver.finish_content(true);
        stream::TEST_READ_BYTES.with(|n| n.set(0));
        let found = resolve_indexed(&area, &carver, bytes.len() as u64, None);
        let indexed_reads = stream::TEST_READ_BYTES.with(|n| n.get());
        assert!(found.is_empty());
        assert!(indexed_reads < 128 << 10, "TGA index requested {indexed_reads} bytes");
        carver.finish_content(false);
        stream::TEST_READ_BYTES.with(|n| n.set(0));
        assert_eq!(resolve_indexed(&area, &carver, bytes.len() as u64, None), found);
        assert!(stream::TEST_READ_BYTES.with(|n| n.get()) > 64 << 20);
        drop(src);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn indexed_pdf_and_tga_keep_the_direct_parser_results() {
        let mut bytes = vec![0; 32768];
        bytes[..18].copy_from_slice(&[0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 0, 24, 0]);
        bytes[21..47].fill(0);
        bytes[29..47].copy_from_slice(b"TRUEVISION-XFILE.\0");
        let pdf = b"%PDF-1.7\n%%EOF\n1 0 obj\n<< /Title (Kept) >>\nendobj\n%%EOF\n";
        bytes[512..512 + pdf.len()].copy_from_slice(pdf);
        let (dir, src) = image("indexed-formats", &bytes);
        let mut carver = Carver::default();
        for (i, b) in bytes.chunks(512).enumerate() {
            carver.observe(i as u64, b);
        }
        carver.finish_content(true);
        let area = Area { src: &src, base: 0, cs: 512, clusters: bytes.len() as u64 / 512 };
        let direct = resolve_all(&area, &carver.cands, bytes.len() as u64);
        let indexed: Vec<_> =
            resolve_indexed(&area, &carver, bytes.len() as u64, None).into_iter().filter(|f| f.fmt != Fmt::Text).collect();
        assert_eq!(indexed, direct);
        assert_eq!(indexed.len(), 2);
        assert_eq!(indexed[1].len, pdf.len() as u64);
        let mut incomplete = Carver { cands: carver.cands.clone(), ..Default::default() };
        incomplete.observe_content_status(0, &bytes[..512], 512, false);
        incomplete.observe_content_status(512, &bytes[512..], 512, true);
        incomplete.finish_content(true);
        let retried: Vec<_> =
            resolve_indexed(&area, &incomplete, bytes.len() as u64, None).into_iter().filter(|f| f.fmt != Fmt::Text).collect();
        assert_eq!(retried, direct);
        drop(src);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn nearby_header_candidates_reuse_reads() {
        let mut bytes = vec![0x55; 8 << 20];
        let wave = b"RIFF\x24\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x40\x1f\0\0\x40\x1f\0\0\x01\0\x08\0data\0\0\0\0";
        let cands: Vec<_> = (0..1000).map(|n| (n, Fmt::Riff)).collect();
        for &(n, _) in &cands {
            let at = 4096 + n as usize * 512;
            bytes[at..at + wave.len()].copy_from_slice(wave);
        }
        let (dir, src) = image("nearby-headers", &bytes);
        stream::TEST_READ_BYTES.with(|n| n.set(0));
        let area = Area { src: &src, base: 4096, cs: 512, clusters: (bytes.len() as u64 - 4096) / 512 };
        let found = resolve_all(&area, &cands, bytes.len() as u64 - 4096);
        let read = stream::TEST_READ_BYTES.with(|n| n.get());
        assert_eq!(found.len(), cands.len());
        for (f, &(lcn, fmt)) in found.iter().zip(&cands) {
            assert_eq!((f.lcn, f.fmt, f.len, f.complete, f.ext), (lcn, fmt, wave.len() as u64, true, "wav"));
        }
        drop(src);
        assert_eq!(std::fs::read(dir.join("c.img")).unwrap(), bytes);
        std::fs::remove_dir_all(dir).unwrap();
        assert!(read < 2 << 20, "1,000 header-only candidates requested {read} bytes");
    }

    #[test]
    fn text_collected_by_the_sweep_does_not_issue_analysis_reads() {
        let text = b"A line of ordinary document text, without a file signature.\n".repeat(10000);
        let (dir, src) = image("text-io", &text);
        let mut carver = Carver::default();
        for (i, piece) in text.chunks(8192).enumerate() {
            carver.observe_content(i as u64 * 8192, piece, 512);
        }
        stream::TEST_READ_BYTES.with(|n| n.set(0));
        let area = Area { src: &src, base: 0, cs: 512, clusters: (text.len() as u64).div_ceil(512) };
        let found = resolve_indexed(&area, &carver, text.len() as u64, None);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].len, text.len() as u64);
        assert!(!found[0].complete);
        assert_eq!(stream::TEST_READ_BYTES.with(|n| n.get()), 0);
        drop(src);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn user_signature_lists() {
        let v = parse_user_signatures("# mine\nblend 424C454E444552 0 2G\n.abc 0A0B 4\n").unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].ext, v[0].magic.as_slice(), v[0].max), ("blend", &b"BLENDER"[..], 2 << 30));
        assert_eq!((v[1].ext, v[1].offset), ("abc", 4));
        assert!(parse_user_signatures("x 0").is_err());
        assert!(parse_user_signatures("x zz11").is_err());
    }

    #[test]
    fn mpeg_frame_lengths() {
        // MPEG-1 layer III, 128 kbit/s, 44.1 kHz: 417 bytes, 418 with padding.
        let h = [0xFF, 0xFB, 0x90, 0x00];
        let mut d = b"ID3\x03\0\0\0\0\0\0".to_vec();
        for _ in 0..3 {
            d.extend_from_slice(&h);
            d.extend(vec![0u8; 413]);
        }
        let (dir, src) = image("mp3", &{
            let mut x = d.clone();
            x.resize(8192, 0x11);
            x
        });
        let mut st = Stream::new(&src, 0, d.len() as u64).unwrap();
        assert_eq!(length(Fmt::Mp3, &mut st), Some(d.len() as u64));
        drop(st);
        drop(src);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
