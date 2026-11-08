//! ZIP boundaries come from the sweep's end records or from local-header sizes.
//! Never search the payload again for every archive member's local header.
use super::stream::Stream;
use crate::source::Source;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug)]
pub(super) struct End {
    len: u64,
    cd: u64,
    cd_end: u64,
    entries: u64,
}

pub(super) struct Info {
    pub len: u64,
    pub names: Vec<String>,
}

#[derive(Default)]
pub(super) struct Index {
    ends: Vec<(u64, bool)>,
    tail: [u8; 3],
    next: u64,
}

impl Index {
    pub fn observe(&mut self, at: u64, bytes: &[u8]) {
        if at == self.next && at >= 3 {
            let mut join = [0; 6];
            join[..3].copy_from_slice(&self.tail);
            let n = bytes.len().min(3);
            join[3..3 + n].copy_from_slice(&bytes[..n]);
            self.collect(at - 3, &join[..3 + n]);
        }
        self.collect(at, bytes);
        self.next = at + bytes.len() as u64;
        if bytes.len() >= 3 {
            self.tail.copy_from_slice(&bytes[bytes.len() - 3..]);
        } else {
            self.next = u64::MAX;
        }
    }

    fn collect(&mut self, at: u64, bytes: &[u8]) {
        for i in memchr::memmem::find_iter(bytes, b"PK") {
            if let Some(&[kind @ (5 | 6), 6]) = bytes.get(i + 2..i + 4) {
                self.ends.push((at + i as u64, kind == 6));
            }
        }
    }

    pub fn resolve(&self, src: &Source, base: u64, bytes: u64) -> HashMap<u64, End> {
        let mut out = HashMap::new();
        let Ok(mut s) = Stream::new(src, base, bytes) else { return out };
        s.metadata_reads();
        for &(at, wide) in &self.ends {
            if src.checkpoint().is_err() {
                break;
            }
            let end = if wide { zip64_end(&mut s, at) } else { classic_end(&mut s, at) };
            if let Some((origin, end)) = end {
                // A false marker must not hide another valid end for the same start.
                let len = end.len;
                s.reset(base + origin, len);
                s.metadata_reads();
                if validate(&mut s, end).is_some() {
                    out.entry(origin).or_insert(end);
                }
                s.reset(base, bytes);
                s.metadata_reads();
            }
        }
        out
    }
}

fn classic_end(s: &mut Stream, at: u64) -> Option<(u64, End)> {
    if s.bytes(at, 4)? != b"PK\x05\x06" || s.u32le(at + 4)? != 0 {
        return None;
    }
    let entries = s.u16le(at + 10)? as u64;
    if entries == 0 || entries == 65535 || s.u16le(at + 8)? as u64 != entries {
        return None;
    }
    let size = s.u32le(at + 12)? as u64;
    let cd = s.u32le(at + 16)? as u64;
    if size == u32::MAX as u64 || cd == u32::MAX as u64 {
        return None;
    }
    let origin = at.checked_sub(cd.checked_add(size)?)?;
    let len = at.checked_add(22 + s.u16le(at + 20)? as u64)?.checked_sub(origin)?;
    if origin.checked_add(len)? > s.cap() {
        return None;
    }
    Some((origin, End { len, cd, cd_end: cd + size, entries }))
}

fn zip64_end(s: &mut Stream, at: u64) -> Option<(u64, End)> {
    if s.bytes(at, 4)? != b"PK\x06\x06" || s.u64le(at + 16)? != 0 {
        return None;
    }
    let record_size = s.u64le(at + 4)?;
    if record_size < 44 {
        return None;
    }
    let entries = s.u64le(at + 32)?;
    if entries == 0 || s.u64le(at + 24)? != entries {
        return None;
    }
    let size = s.u64le(at + 40)?;
    let cd = s.u64le(at + 48)?;
    let origin = at.checked_sub(cd.checked_add(size)?)?;
    let locator = at.checked_add(12)?.checked_add(record_size)?;
    if s.bytes(locator, 4)? != b"PK\x06\x07"
        || s.u32le(locator + 4)? != 0
        || s.u64le(locator + 8)? != at - origin
        || s.u32le(locator + 16)? != 1
    {
        return None;
    }
    let e = locator.checked_add(20)?;
    if s.bytes(e, 4)? != b"PK\x05\x06" || s.u32le(e + 4)? != 0 {
        return None;
    }
    let len = e.checked_add(22 + s.u16le(e + 20)? as u64)?.checked_sub(origin)?;
    if origin.checked_add(len)? > s.cap() {
        return None;
    }
    Some((origin, End { len, cd, cd_end: at - origin, entries }))
}

fn validate(s: &mut Stream, end: End) -> Option<Info> {
    if end.len > s.cap()
        || end.cd >= end.cd_end
        || end.cd_end > end.len
        || end.entries > (end.cd_end - end.cd) / 46
        || s.bytes(0, 4)? != b"PK\x03\x04"
    {
        return None;
    }
    let mut names = Vec::new();
    let mut p = end.cd;
    for _ in 0..end.entries {
        if s.bytes(p, 4)? != b"PK\x01\x02" {
            return None;
        }
        let n = s.u16le(p + 28)? as u64;
        let x = s.u16le(p + 30)? as u64;
        let c = s.u16le(p + 32)? as u64;
        if s.u16le(p + 34)? != 0 {
            return None;
        }
        let local = s.u32le(p + 42)?;
        if local != u32::MAX && local as u64 >= end.cd {
            return None;
        }
        let next = p.checked_add(46 + n + x + c)?;
        if next > end.cd_end {
            return None;
        }
        if names.len() < 256 {
            names.push(String::from_utf8_lossy(s.bytes(p + 46, n as usize)?).into_owned());
        }
        p = next;
    }
    // Optional central-directory digital signature.
    if p < end.cd_end && s.bytes(p, 4)? == b"PK\x05\x05" {
        p = p.checked_add(6 + s.u16le(p + 4)? as u64)?;
    }
    (p == end.cd_end).then_some(Info { len: end.len, names })
}

fn parse(s: &mut Stream) -> Option<Info> {
    s.metadata_reads();
    if let Some(end) = s.zip_hint {
        return validate(s, end);
    }
    if s.zip_indexed {
        return None;
    }
    let mut p = 0;
    loop {
        match s.bytes(p, 4)? {
            b"PK\x03\x04" => {
                // Unknown sizes use the end index, including unsigned descriptors.
                if s.u16le(p + 6)? & 8 != 0 {
                    return None;
                }
                let mut size = s.u32le(p + 18)? as u64;
                let unpacked = s.u32le(p + 22)?;
                let n = s.u16le(p + 26)? as u64;
                let x = s.u16le(p + 28)? as u64;
                let data = p.checked_add(30 + n + x)?;
                if size == u32::MAX as u64 {
                    let mut extra = p + 30 + n;
                    let mut found = None;
                    while extra.checked_add(4)? <= data {
                        let id = s.u16le(extra)?;
                        let len = s.u16le(extra + 2)? as u64;
                        let next = extra.checked_add(4 + len)?;
                        if next > data {
                            return None;
                        }
                        if id == 1 {
                            let skip = if unpacked == u32::MAX { 8 } else { 0 };
                            if len < skip + 8 {
                                return None;
                            }
                            found = s.u64le(extra + 4 + skip);
                            break;
                        }
                        extra = next;
                    }
                    size = found?;
                }
                p = data.checked_add(size)?;
            }
            b"PK\x01\x02" => {
                let cd = p;
                while s.bytes(p, 4)? == b"PK\x01\x02" {
                    let size = 46 + s.u16le(p + 28)? as u64 + s.u16le(p + 30)? as u64 + s.u16le(p + 32)? as u64;
                    p = p.checked_add(size)?;
                }
                if s.bytes(p, 4)? == b"PK\x05\x05" {
                    p = p.checked_add(6 + s.u16le(p + 4)? as u64)?;
                }
                let (origin, end) = if s.bytes(p, 4)? == b"PK\x06\x06" { zip64_end(s, p)? } else { classic_end(s, p)? };
                return (origin == 0 && end.cd == cd).then(|| validate(s, end)).flatten();
            }
            _ => return None,
        }
    }
}

pub(super) fn info<'s>(s: &'s mut Stream<'_>) -> Option<&'s Info> {
    if s.zip.is_none() {
        s.zip = Some(parse(s));
    }
    s.zip.as_ref()?.as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::carve::{Area, Carver, Fmt, formats, resolve_indexed};
    use std::io::{Seek, SeekFrom, Write};

    fn put16(b: &mut [u8], at: usize, n: u16) {
        b[at..at + 2].copy_from_slice(&n.to_le_bytes());
    }
    fn put32(b: &mut [u8], at: usize, n: u32) {
        b[at..at + 4].copy_from_slice(&n.to_le_bytes());
    }
    fn put64(b: &mut [u8], at: usize, n: u64) {
        b[at..at + 8].copy_from_slice(&n.to_le_bytes());
    }

    fn local(size: u64, descriptor: bool, wide: bool) -> Vec<u8> {
        let name = b"word/document.xml";
        let mut b = vec![0; 30];
        b[..4].copy_from_slice(b"PK\x03\x04");
        put16(&mut b, 4, if wide { 45 } else { 20 });
        put16(&mut b, 6, if descriptor { 8 } else { 0 });
        put32(
            &mut b,
            18,
            if wide {
                u32::MAX
            } else if descriptor {
                0
            } else {
                size as u32
            },
        );
        put32(
            &mut b,
            22,
            if wide {
                u32::MAX
            } else if descriptor {
                0
            } else {
                size as u32
            },
        );
        put16(&mut b, 26, name.len() as u16);
        put16(&mut b, 28, if wide { 20 } else { 0 });
        b.extend_from_slice(name);
        if wide {
            b.extend_from_slice(&[1, 0, 16, 0]);
            b.extend_from_slice(&size.to_le_bytes());
            b.extend_from_slice(&size.to_le_bytes());
        }
        b
    }

    fn ending(cd: u64, payload: u64, wide: bool, descriptor: bool) -> Vec<u8> {
        let name = b"word/document.xml";
        let mut b = vec![0; 46];
        b[..4].copy_from_slice(b"PK\x01\x02");
        put16(&mut b, 6, if wide { 45 } else { 20 });
        put16(&mut b, 8, if descriptor { 8 } else { 0 });
        put32(&mut b, 20, if wide { u32::MAX } else { payload as u32 });
        put32(&mut b, 24, if wide { u32::MAX } else { payload as u32 });
        put16(&mut b, 28, name.len() as u16);
        if wide {
            put16(&mut b, 30, 20);
        }
        b.extend_from_slice(name);
        if wide {
            b.extend_from_slice(&[1, 0, 16, 0]);
            b.extend_from_slice(&payload.to_le_bytes());
            b.extend_from_slice(&payload.to_le_bytes());
        }
        let cd_size = b.len() as u64;
        if wide {
            let mut end = vec![0; 56];
            end[..4].copy_from_slice(b"PK\x06\x06");
            put64(&mut end, 4, 44);
            put16(&mut end, 12, 45);
            put16(&mut end, 14, 45);
            put64(&mut end, 24, 1);
            put64(&mut end, 32, 1);
            put64(&mut end, 40, cd_size);
            put64(&mut end, 48, cd);
            b.extend_from_slice(&end);
            b.extend_from_slice(b"PK\x06\x07");
            b.extend_from_slice(&0u32.to_le_bytes());
            b.extend_from_slice(&(cd + cd_size).to_le_bytes());
            b.extend_from_slice(&1u32.to_le_bytes());
        }
        let mut end = vec![0; 22];
        end[..4].copy_from_slice(b"PK\x05\x06");
        put16(&mut end, 8, if wide { u16::MAX } else { 1 });
        put16(&mut end, 10, if wide { u16::MAX } else { 1 });
        put32(&mut end, 12, if wide { u32::MAX } else { cd_size as u32 });
        put32(&mut end, 16, if wide { u32::MAX } else { cd as u32 });
        b.extend_from_slice(&end);
        b
    }

    fn archive(payload: &[u8], descriptor: bool, wide: bool) -> Vec<u8> {
        let mut b = local(payload.len() as u64, descriptor, wide);
        b.extend_from_slice(payload);
        if descriptor {
            // A descriptor without its optional signature is legal too.
            b.extend_from_slice(&0u32.to_le_bytes());
            if wide {
                b.extend_from_slice(&(payload.len() as u64).to_le_bytes());
                b.extend_from_slice(&(payload.len() as u64).to_le_bytes());
            } else {
                b.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                b.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            }
        }
        b.extend_from_slice(&ending(b.len() as u64, payload.len() as u64, wide, descriptor));
        b
    }

    fn path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("fr-zip-{tag}-{}.img", std::process::id()))
    }

    #[test]
    fn indexed_descriptors_zip64_and_nested_archives_keep_exact_ends() {
        for (k, descriptor, wide) in [(0, false, false), (1, true, false), (2, true, true)] {
            let inner = archive(b"inner contents", descriptor, wide);
            let mut payload = vec![0x55; 512 - local(0, descriptor, wide).len()];
            payload.extend_from_slice(&inner);
            let bytes = archive(&payload, descriptor, wide);
            let p = path(&format!("nested-{k}"));
            std::fs::write(&p, &bytes).unwrap();
            let src = Source::open(p.to_str().unwrap()).unwrap();
            let mut carver = Carver::default();
            // Force the four-byte end signatures to cross observation boundaries.
            for (i, piece) in bytes.chunks(7).enumerate() {
                carver.observe_zip(i as u64 * 7, piece);
            }
            carver.cands = vec![(0, Fmt::Zip), (1, Fmt::Zip)];
            let area = Area { src: &src, base: 0, cs: 512, clusters: (bytes.len() as u64).div_ceil(512) };
            let got = resolve_indexed(&area, &carver, bytes.len() as u64, None);
            assert_eq!(got.len(), 2);
            assert_eq!((got[0].len, got[0].complete, got[0].ext), (bytes.len() as u64, true, "docx"));
            assert_eq!((got[1].len, got[1].complete), (inner.len() as u64, true));
            drop(src);
            std::fs::remove_file(p).unwrap();
        }
    }

    #[test]
    fn member_headers_do_not_reread_the_selected_range() {
        let mut bytes = vec![0x55; 2 << 20];
        let header = local(1 << 20, true, false);
        for at in (0..512 * 1000).step_by(512) {
            bytes[at..at + header.len()].copy_from_slice(&header);
        }
        let valid = archive(b"after the misleading member headers", true, false);
        let at = 512 * 1001;
        bytes[at..at + valid.len()].copy_from_slice(&valid);
        let p = path("members");
        std::fs::write(&p, &bytes).unwrap();
        let src = Source::open(p.to_str().unwrap()).unwrap();
        let mut carver = Carver::default();
        for (i, b) in bytes.chunks(512).enumerate() {
            carver.observe(i as u64, b);
        }
        let area = Area { src: &src, base: 0, cs: 512, clusters: bytes.len() as u64 / 512 };
        super::super::stream::TEST_READ_BYTES.with(|n| n.set(0));
        let got = resolve_indexed(&area, &carver, bytes.len() as u64, None);
        let read = super::super::stream::TEST_READ_BYTES.with(|n| n.get());
        assert!(read < 64 << 10, "ZIP analysis reread {read} bytes");
        assert_eq!(got.len(), 1001);
        assert!(got[..1000].iter().all(|c| !c.complete));
        assert!(got.last().unwrap().complete);
        assert_eq!(got.last().unwrap().len, valid.len() as u64);
        drop(src);
        std::fs::remove_file(p).unwrap();
    }

    #[test]
    fn five_gib_zip64_payload_is_skipped_and_extension_reuses_the_parse() {
        let payload = 5u64 << 30;
        let head = local(payload, false, true);
        let cd = head.len() as u64 + payload;
        let tail = ending(cd, payload, true, false);
        let p = path("large");
        let mut f = std::fs::File::create(&p).unwrap();
        // Sparse test file: no multi-gigabyte allocation or disk write.
        unsafe {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::{IO::DeviceIoControl, Ioctl::FSCTL_SET_SPARSE};
            let mut returned = 0;
            assert_ne!(
                DeviceIoControl(
                    f.as_raw_handle(),
                    FSCTL_SET_SPARSE,
                    std::ptr::null(),
                    0,
                    std::ptr::null_mut(),
                    0,
                    &mut returned,
                    std::ptr::null_mut()
                ),
                0
            );
        }
        f.write_all(&head).unwrap();
        f.seek(SeekFrom::Start(cd)).unwrap();
        f.write_all(&tail).unwrap();
        drop(f);
        let src = Source::open(p.to_str().unwrap()).unwrap();
        let mut s = Stream::new(&src, 0, cd + tail.len() as u64).unwrap();
        assert_eq!(formats::length(Fmt::Zip, &mut s), Some(cd + tail.len() as u64));
        assert!(s.read_bytes < 64 << 10, "read {} bytes", s.read_bytes);
        let read = s.read_bytes;
        assert_eq!(formats::extension(Fmt::Zip, &mut s), "docx");
        assert_eq!(s.read_bytes, read);
        drop(s);
        drop(src);
        std::fs::remove_file(p).unwrap();
    }

    #[test]
    fn failed_parse_is_cached_and_truncated_comment_is_not_complete() {
        let mut bytes = archive(b"data", false, false);
        let eocd = bytes.len() - 22;
        put16(&mut bytes, eocd + 20, 100);
        let p = path("truncated");
        std::fs::write(&p, &bytes).unwrap();
        let src = Source::open(p.to_str().unwrap()).unwrap();
        let mut s = Stream::new(&src, 0, bytes.len() as u64).unwrap();
        assert_eq!(formats::length(Fmt::Zip, &mut s), None);
        let read = s.read_bytes;
        assert_eq!(formats::extension(Fmt::Zip, &mut s), "zip");
        assert_eq!(s.read_bytes, read);
        drop(s);
        drop(src);
        std::fs::remove_file(p).unwrap();
    }
}
