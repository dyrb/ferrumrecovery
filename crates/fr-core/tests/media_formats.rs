//! Container boundaries, truncated input and byte-exact reads of recovered candidates.
use fr_core::{
    Source,
    carve::{self, Fmt, stream::Stream},
};

fn be16(b: &mut [u8], p: usize, n: u16) {
    b[p..p + 2].copy_from_slice(&n.to_be_bytes());
}
fn be32(b: &mut [u8], p: usize, n: u32) {
    b[p..p + 4].copy_from_slice(&n.to_be_bytes());
}
fn le32(b: &mut [u8], p: usize, n: u32) {
    b[p..p + 4].copy_from_slice(&n.to_le_bytes());
}

fn midi() -> Vec<u8> {
    let track = [0, 0x90, 60, 100, 0, 60, 0, 0, 0xff, 0x2f, 0];
    let mut b = b"MThd\0\0\0\x06\0\0\0\x01\0\x60MTrk".to_vec();
    b.extend_from_slice(&(track.len() as u32).to_be_bytes());
    b.extend_from_slice(&track);
    b
}
fn aiff() -> Vec<u8> {
    let mut b = b"FORM\0\0\0\0AIFFCOMM\0\0\0\x12".to_vec();
    b.extend_from_slice(&[0, 1, 0, 0, 0, 2, 0, 16, 0x40, 0x0e, 0xac, 0x44, 0, 0, 0, 0, 0, 0]);
    b.extend_from_slice(b"SSND\0\0\0\x0c\0\0\0\0\0\0\0\0\0\x01\0\x02");
    let n = b.len() as u32 - 8;
    be32(&mut b, 4, n);
    b
}
fn swf() -> Vec<u8> {
    b"FWS\x09\x12\0\0\0\x08\0\0\x18\x01\0\x40\0\0\0".to_vec()
}
fn ea(big: bool) -> Vec<u8> {
    let mut b = vec![0; 32];
    b[..4].copy_from_slice(b"MVhd");
    if big {
        be32(&mut b, 4, 32);
    } else {
        le32(&mut b, 4, 32);
    }
    le32(&mut b, 16, 1);
    le32(&mut b, 24, 30);
    le32(&mut b, 28, 1);
    b.extend_from_slice(b"MV0K");
    b.extend_from_slice(&if big { 12u32.to_be_bytes() } else { 12u32.to_le_bytes() });
    b.extend_from_slice(&[0x46, 0x31, 0x10, 0x10]);
    b.extend_from_slice(b"SCEl");
    b.extend_from_slice(&if big { 8u32.to_be_bytes() } else { 8u32.to_le_bytes() });
    b
}
fn flv() -> Vec<u8> {
    b"FLV\x01\x01\0\0\0\x09\0\0\0\0\x09\0\0\x03\0\0\0\0\0\0\0\x14\0\0\0\0\0\x0e".to_vec()
}
fn font(otf: bool) -> Vec<u8> {
    let mut tables: Vec<([u8; 4], Vec<u8>)> = vec![
        (*b"cmap", vec![0, 0, 0, 1, 0, 3, 0, 1, 0, 0, 0, 12]),
        (*b"head", vec![0; 54]),
        (*b"maxp", vec![0, 1, 0, 0, 0, 1]),
        (*b"name", vec![0; 6]),
    ];
    if otf {
        tables.push((*b"CFF ", vec![1, 0, 4, 1]));
    } else {
        tables.extend([(*b"glyf", vec![0; 4]), (*b"loca", vec![0; 4])]);
    }
    tables.sort_by_key(|t| t.0);
    let mut b = vec![0; 12 + tables.len() * 16];
    b[..4].copy_from_slice(if otf { b"OTTO" } else { b"\0\x01\0\0" });
    be16(&mut b, 4, tables.len() as u16);
    for (i, (tag, mut data)) in tables.into_iter().enumerate() {
        if &tag == b"head" {
            be32(&mut data, 12, 0x5f0f3cf5);
        }
        let off = b.len() as u32;
        let n = data.len() as u32;
        let at = 12 + i * 16;
        b[at..at + 4].copy_from_slice(&tag);
        be32(&mut b, at + 8, off);
        be32(&mut b, at + 12, n);
        b.extend_from_slice(&data);
        b.resize(b.len().div_ceil(4) * 4, 0);
    }
    b
}

#[test]
fn media_have_checked_ends_and_restore_identical_contents() {
    let files = [
        (Fmt::Midi, midi()),
        (Fmt::Aiff, aiff()),
        (Fmt::Swf, swf()),
        (Fmt::EaVp6, ea(false)),
        (Fmt::EaVp6, ea(true)),
        (Fmt::Sfnt, font(false)),
        (Fmt::Sfnt, font(true)),
        (Fmt::Flv, flv()),
    ];
    let path = std::env::temp_dir().join(format!("fr-media-{}.img", std::process::id()));
    let mut image = Vec::new();
    let mut starts = Vec::new();
    for (_, file) in &files {
        starts.push(image.len() as u64 / 512);
        image.extend_from_slice(file);
        image.resize(image.len().div_ceil(512) * 512 + 512, 0);
    }
    std::fs::write(&path, &image).unwrap();
    let src = Source::open(path.to_str().unwrap()).unwrap();
    let mut carver = carve::Carver::default();
    for (i, b) in image.chunks(512).enumerate() {
        carver.observe(i as u64, b);
    }
    let area = carve::Area { src: &src, base: 0, cs: 512, clusters: image.len() as u64 / 512 };
    let found = carve::resolve_indexed(&area, &carver, image.len() as u64, None);
    assert_eq!(found.len(), files.len());
    for ((got, (fmt, file)), start) in found.iter().zip(&files).zip(starts) {
        assert_eq!(got.fmt, *fmt);
        assert_eq!(got.lcn, start);
        assert_eq!(got.len, file.len() as u64, "{fmt:?}");
        assert_eq!(got.complete, *fmt != Fmt::Flv);
        assert_eq!(src.read_bytes(start * 512, got.len as usize).unwrap(), *file);
    }
    drop(src);
    assert_eq!(std::fs::read(&path).unwrap(), image);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn raw_source_smaller_than_one_sector_is_not_discarded() {
    let path = std::env::temp_dir().join(format!("fr-media-tail-{}.img", std::process::id()));
    let original = midi();
    std::fs::write(&path, &original).unwrap();
    let src = Source::open(path.to_str().unwrap()).unwrap();
    let cat = carve::scan_raw(&src, 0, original.len() as u64, None);
    let i = (0..cat.entries.len() as u32).find(|&i| cat.name(i).ends_with(".mid")).unwrap();
    let mut restored = Vec::new();
    let stats = fr_core::data::Reader::new(&src, &cat)
        .unwrap()
        .read(i, &mut |b| {
            restored.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
    assert_eq!(stats.bad + stats.missing + stats.corrupt_units as u64, 0);
    assert_eq!(restored, original);
    drop(src);
    assert_eq!(std::fs::read(&path).unwrap(), original);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn corrupted_sizes_tracks_and_chunk_links_are_rejected() {
    let path = std::env::temp_dir().join(format!("fr-media-bad-{}.img", std::process::id()));
    let mut cases = Vec::new();
    let mut b = midi();
    be32(&mut b, 18, u32::MAX);
    cases.push((Fmt::Midi, b));
    let mut b = midi();
    *b.last_mut().unwrap() = 1;
    cases.push((Fmt::Midi, b));
    let mut b = aiff();
    be32(&mut b, 4, u32::MAX);
    cases.push((Fmt::Aiff, b));
    let mut b = swf();
    b[16] = 0x40;
    cases.push((Fmt::Swf, b));
    let mut b = ea(false);
    le32(&mut b, 36, u32::MAX);
    cases.push((Fmt::EaVp6, b));
    let mut b = font(false);
    be32(&mut b, 20, u32::MAX);
    cases.push((Fmt::Sfnt, b));
    let mut b = flv();
    *b.last_mut().unwrap() = 0;
    cases.push((Fmt::Flv, b));
    for (fmt, bytes) in cases {
        std::fs::write(&path, &bytes).unwrap();
        let src = Source::open(path.to_str().unwrap()).unwrap();
        let mut s = Stream::new(&src, 0, bytes.len() as u64).unwrap();
        assert_eq!(carve::formats::length(fmt, &mut s), None, "{fmt:?}");
        drop(s);
        drop(src);
    }
    std::fs::remove_file(path).unwrap();
}
