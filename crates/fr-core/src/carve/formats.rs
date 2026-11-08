//! File signatures, the end of each format from its own structure, and names from
//! metadata.

use super::stream::Stream;
use crate::types::Group;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Fmt {
    Jpeg,
    Png,
    Gif,
    Bmp,
    Tiff,
    Riff,
    Bmff,
    Ebml,
    Mp3,
    Flac,
    Ogg,
    Pdf,
    Zip,
    Ole,
    Rtf,
    Rar,
    SevenZip,
    Gzip,
    Sqlite,
    Psd,
    Pe,
    Xml,
    Html,
    Cab,
    Pcx,
    Dcx,
    Ico,
    Tga,
    Midi,
    Aiff,
    Sfnt,
    Swf,
    Flv,
    EaVp6,
    Text,
    /// A signature from the user's list ([`set_user_signatures`]), by index.
    User(u16),
}

const MB: u64 = 1 << 20;

/// A signature given by the user: files with extension `ext` start with `magic` at byte
/// `offset` and are at most `max` bytes long; their end is not known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserSig {
    pub ext: &'static str,
    pub magic: Vec<u8>,
    pub offset: usize,
    pub max: u64,
}

static USER: std::sync::OnceLock<Vec<UserSig>> = std::sync::OnceLock::new();

/// Install the user's signatures (once per process).
pub fn set_user_signatures(sigs: Vec<UserSig>) {
    let _ = USER.set(sigs);
}

fn user_sigs() -> &'static [UserSig] {
    USER.get().map_or(&[], |v| v.as_slice())
}

/// Parse a signature list: one `ext hex-magic [offset] [max-size]` per line, `#` comments.
/// Example: `blend 424C454E444552 0 2G`.
pub fn parse_user_signatures(text: &str) -> Result<Vec<UserSig>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.split_whitespace();
        let err = |w: &str| format!("line {}: {w}", n + 1);
        let ext = it.next().ok_or_else(|| err("no extension"))?.trim_start_matches('.');
        let hex = it.next().ok_or_else(|| err("no signature"))?;
        if !hex.is_ascii() || hex.len() % 2 != 0 || hex.len() < 4 {
            return Err(err("the signature needs at least two bytes in hex"));
        }
        let magic = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
            .collect::<Result<Vec<u8>, _>>()
            .map_err(|_| err("bad hex"))?;
        let offset = it.next().map(|v| v.parse::<usize>()).transpose().map_err(|_| err("bad offset"))?.unwrap_or(0);
        let max = it.next().map(|v| crate::filter::parse_size(v).ok_or_else(|| err("bad size"))).transpose()?.unwrap_or(64 * MB);
        if offset.checked_add(magic.len()).is_none_or(|end| end > 512) {
            return Err(err("the signature must lie in the first 512 bytes"));
        }
        out.push(UserSig { ext: Box::leak(ext.to_ascii_lowercase().into_boxed_str()), magic, offset, max });
    }
    Ok(out)
}

impl Fmt {
    /// Largest size believed for the format.
    pub fn max_len(self) -> u64 {
        match self {
            Fmt::User(i) => user_sigs().get(i as usize).map_or(MB, |u| u.max),
            Fmt::Jpeg | Fmt::Gif => 128 * MB,
            Fmt::Png | Fmt::Mp3 | Fmt::Rtf => 512 * MB,
            Fmt::Xml | Fmt::Html | Fmt::Text | Fmt::Sfnt => 64 * MB,
            Fmt::Midi | Fmt::Swf => 128 * MB,
            Fmt::Bmp | Fmt::Tiff | Fmt::Pdf | Fmt::Ole | Fmt::Pe | Fmt::Ogg => 2048 * MB,
            _ => 16 * 1024 * MB,
        }
    }

    pub fn group(self) -> Group {
        match self {
            Fmt::Jpeg | Fmt::Png | Fmt::Gif | Fmt::Bmp | Fmt::Tiff | Fmt::Psd | Fmt::Pcx | Fmt::Dcx | Fmt::Ico | Fmt::Tga => Group::Photo,
            Fmt::Ebml | Fmt::Swf | Fmt::Flv | Fmt::EaVp6 => Group::Video,
            Fmt::Mp3 | Fmt::Flac | Fmt::Ogg | Fmt::Midi | Fmt::Aiff => Group::Audio,
            Fmt::Pdf | Fmt::Ole | Fmt::Rtf | Fmt::Xml | Fmt::Html | Fmt::Text => Group::Document,
            Fmt::Zip | Fmt::Rar | Fmt::SevenZip | Fmt::Gzip | Fmt::Cab => Group::Archive,
            Fmt::Riff | Fmt::Bmff | Fmt::Sqlite | Fmt::Pe | Fmt::Sfnt => Group::Other,
            Fmt::User(i) => user_sigs().get(i as usize).map_or(Group::Other, |u| Group::of(&format!("x.{}", u.ext))),
        }
    }
}

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u32be(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}

/// Format of a file starting with `h` (at least 512 bytes of a cluster start).
pub fn detect(h: &[u8]) -> Option<Fmt> {
    if h.len() < 512 {
        return None;
    }
    let s = |p: &[u8]| h.starts_with(p);
    let f = if s(&[0xFF, 0xD8, 0xFF]) && (h[3] >= 0xC0) {
        Fmt::Jpeg
    } else if s(b"\x89PNG\r\n\x1a\n") {
        Fmt::Png
    } else if s(b"GIF87a") || s(b"GIF89a") {
        Fmt::Gif
    } else if s(b"BM")
        && u32le(h, 6) == 0
        && (26..4096).contains(&u32le(h, 10))
        && [12, 40, 52, 56, 108, 124].contains(&u32le(h, 14))
        && u32le(h, 2) > 54
    {
        Fmt::Bmp
    } else if (s(b"II*\0") && u32le(h, 4) >= 8) || (s(b"MM\0*") && u32be(h, 4) >= 8) {
        Fmt::Tiff
    } else if s(b"RIFF") && (&h[8..12] == b"WAVE" || &h[8..12] == b"AVI " || &h[8..12] == b"WEBP") {
        Fmt::Riff
    } else if &h[4..8] == b"ftyp" && (8..=256).contains(&u32be(h, 0)) && h[8..12].iter().all(|c| c.is_ascii_graphic() || *c == b' ') {
        Fmt::Bmff
    } else if s(&[0x1A, 0x45, 0xDF, 0xA3]) {
        Fmt::Ebml
    } else if s(b"ID3") && (2..=4).contains(&h[3]) && h[6..10].iter().all(|b| b & 0x80 == 0) {
        Fmt::Mp3
    } else if s(b"fLaC") && h[4] & 0x7F == 0 {
        Fmt::Flac
    } else if s(b"OggS") && h[4] == 0 && h[5] & 0x02 != 0 {
        Fmt::Ogg
    } else if s(b"%PDF-") {
        Fmt::Pdf
    } else if s(b"PK\x03\x04") && u16le(h, 26) > 0 && u16le(h, 26) < 1024 {
        Fmt::Zip
    } else if s(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) {
        Fmt::Ole
    } else if s(b"{\\rtf") {
        Fmt::Rtf
    } else if s(b"Rar!\x1a\x07") {
        Fmt::Rar
    } else if s(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]) {
        Fmt::SevenZip
    } else if s(&[0x1F, 0x8B, 0x08]) && h[3] < 0x20 {
        Fmt::Gzip
    } else if s(b"SQLite format 3\0") {
        Fmt::Sqlite
    } else if s(b"MThd") && u32be(h, 4) >= 6 && u16::from_be_bytes([h[8], h[9]]) <= 2 {
        Fmt::Midi
    } else if s(b"FORM") && matches!(&h[8..12], b"AIFF" | b"AIFC") {
        Fmt::Aiff
    } else if (s(b"\0\x01\0\0") || s(b"OTTO") || s(b"true")) && u16::from_be_bytes([h[4], h[5]]) > 0 {
        Fmt::Sfnt
    } else if s(b"FWS") && (1..=50).contains(&h[3]) && u32le(h, 4) >= 16 {
        Fmt::Swf
    } else if s(b"FLV\x01") && h[4] & !5 == 0 && u32be(h, 5) >= 9 {
        Fmt::Flv
    } else if s(b"MVhd") && ((32..=0xfffff).contains(&u32le(h, 4)) || (32..=0xfffff).contains(&u32be(h, 4))) {
        Fmt::EaVp6
    } else if s(b"MSCF") && u32le(h, 4) == 0 && u32le(h, 12) == 0 && u32le(h, 20) == 0 && h[24..26] == [3, 1] {
        Fmt::Cab
    } else if super::legacy::pcx_header(h) {
        Fmt::Pcx
    } else if s(&[0xb1, 0x68, 0xde, 0x3a]) && u32le(h, 4) >= 4100 {
        Fmt::Dcx
    } else if super::legacy::ico_header(h) {
        Fmt::Ico
    } else if s(b"8BPS") && (h[5] == 1 || h[5] == 2) && h[4] == 0 {
        Fmt::Psd
    } else if s(b"MZ") && {
        let pe = u32le(h, 0x3C) as usize;
        (0x40..h.len() - 4).contains(&pe) && &h[pe..pe + 4] == b"PE\0\0"
    } {
        Fmt::Pe
    } else if s(b"<?xml ") {
        Fmt::Xml
    } else if h[..15].eq_ignore_ascii_case(b"<!doctype html>")
        || h[..6].eq_ignore_ascii_case(b"<html>")
        || h[..6].eq_ignore_ascii_case(b"<html ")
    {
        Fmt::Html
    } else if super::legacy::tga_header(h) {
        Fmt::Tga
    } else {
        Fmt::User(user_sigs().iter().position(|u| h.get(u.offset..u.offset + u.magic.len()) == Some(&u.magic[..]))? as u16)
    };
    Some(f)
}

/// Length of the file from its structure. `None` when the format does not tell or the
/// data does not follow it.
pub fn length(fmt: Fmt, s: &mut Stream) -> Option<u64> {
    match fmt {
        Fmt::Jpeg => jpeg(s),
        Fmt::Png => png(s),
        Fmt::Gif => gif(s),
        Fmt::Bmp => s.u32le(2).map(|v| v as u64).filter(|&v| v <= s.cap()),
        Fmt::Tiff => tiff(s),
        Fmt::Riff => s.u32le(4).map(|v| v as u64 + 8).filter(|&v| v <= s.cap()),
        Fmt::Bmff => bmff(s),
        Fmt::Ebml => ebml(s),
        Fmt::Mp3 => mp3(s),
        Fmt::Ogg => ogg(s),
        Fmt::Pdf => pdf(s),
        Fmt::Zip => super::zip::info(s).map(|z| z.len),
        Fmt::Cab => super::legacy::cab(s),
        Fmt::Pcx => super::legacy::pcx(s, 0),
        Fmt::Dcx => super::legacy::dcx(s),
        Fmt::Ico => super::legacy::ico(s),
        Fmt::Tga => super::legacy::tga(s),
        Fmt::Midi => super::media::midi(s),
        Fmt::Aiff => super::media::aiff(s),
        Fmt::Sfnt => super::media::sfnt(s),
        Fmt::Swf => super::media::swf(s),
        Fmt::Flv => super::media::flv(s),
        Fmt::EaVp6 => super::media::ea_vp6(s),
        Fmt::Ole => ole(s),
        Fmt::Rtf => rtf(s),
        Fmt::Rar => rar(s),
        Fmt::SevenZip => {
            let off = s.u64le(12)?;
            let size = s.u64le(20)?;
            32u64.checked_add(off)?.checked_add(size).filter(|&v| v <= s.cap())
        }
        Fmt::Sqlite => {
            let ps = match s.u16be(16)? {
                1 => 65536,
                v if v.is_power_of_two() && v >= 512 => v as u64,
                _ => return None,
            };
            let pages = s.u32be(28)? as u64;
            (pages > 0).then(|| ps * pages).filter(|&v| v <= s.cap())
        }
        Fmt::Psd => psd(s),
        Fmt::Pe => pe(s),
        Fmt::Xml | Fmt::Html => s.find_byte(0, 0).or(Some(s.cap())),
        Fmt::Flac | Fmt::Gzip | Fmt::User(_) | Fmt::Text => None,
    }
}

fn jpeg(s: &mut Stream) -> Option<u64> {
    let mut p = 2u64;
    loop {
        let m = s.bytes(p, 4)?;
        if m[0] != 0xFF {
            return None;
        }
        let marker = m[1];
        match marker {
            0xFF => p += 1,
            0xD8 | 0x01 | 0xD0..=0xD7 => p += 2,
            0xD9 => return Some(p + 2),
            0xDA => {
                let len = u16::from_be_bytes([m[2], m[3]]) as u64;
                p += 2 + len;
                // Entropy-coded data: a marker is FF followed by anything but 00, a
                // restart marker or another FF.
                loop {
                    let q = s.find_byte(p, 0xFF)?;
                    let b = s.u8(q + 1)?;
                    if b == 0x00 || b == 0xFF || (0xD0..=0xD7).contains(&b) {
                        p = q + 1;
                        continue;
                    }
                    p = q;
                    break;
                }
            }
            _ => {
                let len = u16::from_be_bytes([m[2], m[3]]) as u64;
                if len < 2 {
                    return None;
                }
                p += 2 + len;
            }
        }
    }
}

fn png(s: &mut Stream) -> Option<u64> {
    let mut p = 8u64;
    loop {
        let len = s.u32be(p)? as u64;
        let t = s.bytes(p + 4, 4)?;
        if len > 1 << 31 || !t.iter().all(u8::is_ascii_alphabetic) {
            return None;
        }
        let iend = t == b"IEND";
        p += 12 + len;
        if iend {
            return Some(p);
        }
    }
}

fn skip_sub_blocks(s: &mut Stream, mut p: u64) -> Option<u64> {
    loop {
        let n = s.u8(p)? as u64;
        p += 1 + n;
        if n == 0 {
            return Some(p);
        }
    }
}

fn gif(s: &mut Stream) -> Option<u64> {
    let flags = s.u8(10)?;
    let mut p = 13 + if flags & 0x80 != 0 { 3 << ((flags & 7) + 1) } else { 0 };
    loop {
        match s.u8(p)? {
            0x3B => return Some(p + 1),
            0x21 => p = skip_sub_blocks(s, p + 2)?,
            0x2C => {
                let f = s.u8(p + 9)?;
                p += 10 + if f & 0x80 != 0 { 3 << ((f & 7) + 1) } else { 0 };
                p = skip_sub_blocks(s, p + 1)?;
            }
            _ => return None,
        }
    }
}

/// TIFF and the RAW formats built on it: the end of the last block any IFD points to.
fn tiff(s: &mut Stream) -> Option<u64> {
    let le = s.bytes(0, 2)? == b"II";
    let rd16 = |s: &mut Stream, o: u64| if le { s.u16le(o) } else { s.u16be(o) };
    let rd32 = |s: &mut Stream, o: u64| if le { s.u32le(o) } else { s.u32be(o) };
    let mut ifds = vec![rd32(s, 4)? as u64];
    let mut seen = std::collections::HashSet::new();
    let mut end = 8u64;
    while let Some(off) = ifds.pop() {
        if off < 8 || !seen.insert(off) || seen.len() > 64 {
            continue;
        }
        let n = rd16(s, off)? as u64;
        if n == 0 || n > 4096 {
            continue;
        }
        end = end.max(off + 2 + 12 * n + 4);
        let mut pairs: std::collections::HashMap<u16, Vec<u64>> = std::collections::HashMap::new();
        for k in 0..n {
            let e = off + 2 + 12 * k;
            let tag = rd16(s, e)?;
            let ty = rd16(s, e + 2)?;
            let count = rd32(s, e + 4)? as u64;
            let size = match ty {
                1 | 2 | 6 | 7 => 1,
                3 | 8 => 2,
                4 | 9 | 11 | 13 => 4,
                5 | 10 | 12 => 8,
                _ => continue,
            };
            let bytes = size * count;
            let val_at = if bytes <= 4 { e + 8 } else { rd32(s, e + 8)? as u64 };
            if bytes > 4 {
                end = end.max(val_at + bytes);
            }
            if matches!(tag, 273 | 279 | 324 | 325 | 513 | 514) && count <= 1 << 16 {
                let vals: Vec<u64> = (0..count)
                    .filter_map(|i| if size == 2 { rd16(s, val_at + 2 * i).map(u64::from) } else { rd32(s, val_at + 4 * i).map(u64::from) })
                    .collect();
                pairs.insert(tag, vals);
            }
            if matches!(tag, 330 | 34665 | 34853 | 40965) && size == 4 && count <= 64 {
                for i in 0..count {
                    if let Some(v) = rd32(s, val_at + 4 * i) {
                        ifds.push(v as u64);
                    }
                }
            }
        }
        for (o, c) in [(273, 279), (324, 325), (513, 514)] {
            if let (Some(os), Some(cs)) = (pairs.get(&o), pairs.get(&c)) {
                for (a, b) in os.iter().zip(cs) {
                    end = end.max(a + b);
                }
            }
        }
        if let Some(next) = rd32(s, off + 2 + 12 * n) {
            ifds.push(next as u64);
        }
    }
    (end <= s.cap()).then_some(end)
}

fn bmff(s: &mut Stream) -> Option<u64> {
    const TOP: [&[u8; 4]; 17] = [
        b"ftyp", b"moov", b"mdat", b"free", b"skip", b"wide", b"uuid", b"meta", b"pdin", b"moof", b"mfra", b"styp", b"sidx", b"ssix",
        b"prft", b"emsg", b"jumb",
    ];
    let mut p = 0u64;
    let mut media = false;
    while let Some(size32) = s.u32be(p) {
        let Some(t) = s.bytes(p + 4, 4).map(|t| [t[0], t[1], t[2], t[3]]) else { break };
        if !TOP.contains(&&t) {
            break;
        }
        let size = match size32 {
            1 => s.u64be(p + 8)?,
            0 => return None, // runs to the end of the file: unknown
            v => v as u64,
        };
        if size < 8 || p.checked_add(size)? > s.cap() {
            break;
        }
        media |= &t == b"moov" || &t == b"mdat" || &t == b"meta";
        p += size;
    }
    (media && p > 0).then_some(p)
}

/// EBML variable-length integer at `p`: (value, length); all-ones values are "unknown".
fn vint(s: &mut Stream, p: u64) -> Option<(u64, u64, bool)> {
    let b0 = s.u8(p)?;
    let len = b0.leading_zeros() as u64 + 1;
    if len > 8 {
        return None;
    }
    let mut v = (b0 as u64) & (0xFF >> len);
    for i in 1..len {
        v = v << 8 | s.u8(p + i)? as u64;
    }
    let unknown = v == (1u64 << (7 * len)) - 1;
    Some((v, len, unknown))
}

fn ebml(s: &mut Stream) -> Option<u64> {
    let (hsize, hl, _) = vint(s, 4)?;
    let mut p = 4 + hl + hsize;
    if s.bytes(p, 4)? != [0x18, 0x53, 0x80, 0x67] {
        return None;
    }
    let (size, l, unknown) = vint(s, p + 4)?;
    if unknown {
        return None;
    }
    p += 4 + l + size;
    (p <= s.cap()).then_some(p)
}

/// Length of the MPEG audio frame whose header is `h`.
fn mpeg_frame(h: [u8; 4]) -> Option<u64> {
    if h[0] != 0xFF || h[1] & 0xE0 != 0xE0 {
        return None;
    }
    let ver = (h[1] >> 3) & 3; // 0: 2.5, 2: 2, 3: 1
    let layer = (h[1] >> 1) & 3; // 1: III, 2: II, 3: I
    let br_i = (h[2] >> 4) as usize;
    let sr_i = ((h[2] >> 2) & 3) as usize;
    let pad = ((h[2] >> 1) & 1) as u64;
    if ver == 1 || layer == 0 || br_i == 0 || br_i == 15 || sr_i == 3 {
        return None;
    }
    const BR_V1: [[u32; 16]; 3] = [
        [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0],
        [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0],
        [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448, 0],
    ];
    const BR_V2: [[u32; 16]; 3] = [
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0],
        [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256, 0],
    ];
    const SR: [u32; 3] = [44100, 48000, 32000];
    let li = (layer - 1) as usize; // bits 01: layer III, 10: II, 11: I
    let br = if ver == 3 { BR_V1[li][br_i] } else { BR_V2[li][br_i] } as u64 * 1000;
    let sr = (SR[sr_i]
        >> if ver == 3 {
            0
        } else if ver == 2 {
            1
        } else {
            2
        }) as u64;
    let len = match li {
        2 => (12 * br / sr + pad) * 4,
        0 if ver != 3 => 72 * br / sr + pad,
        _ => 144 * br / sr + pad,
    };
    (len >= 4).then_some(len)
}

fn mp3(s: &mut Stream) -> Option<u64> {
    let h = s.bytes(0, 10)?;
    let tag = ((h[6] as u64) << 21) | ((h[7] as u64) << 14) | ((h[8] as u64) << 7) | h[9] as u64;
    let mut p = 10 + tag + if h[5] & 0x10 != 0 { 10 } else { 0 };
    // Some files pad after the tag.
    let mut pad = 0;
    while s.u8(p)? == 0 && pad < 4096 {
        p += 1;
        pad += 1;
    }
    let mut frames = 0;
    while let Some(b) = s.bytes(p, 4) {
        match mpeg_frame([b[0], b[1], b[2], b[3]]) {
            Some(n) if p + n <= s.cap() => {
                p += n;
                frames += 1;
            }
            _ => break,
        }
    }
    if frames == 0 {
        return None;
    }
    if s.bytes(p, 3) == Some(b"TAG") {
        p += 128;
    }
    Some(p)
}

fn ogg(s: &mut Stream) -> Option<u64> {
    let mut p = 0u64;
    let serial = s.u32le(14)?;
    loop {
        if s.bytes(p, 4)? != b"OggS" {
            return (p > 0).then_some(p);
        }
        let ty = s.u8(p + 5)?;
        let n = s.u8(p + 26)? as u64;
        let segs = s.bytes(p + 27, n as usize)?.iter().map(|&x| x as u64).sum::<u64>();
        let this = s.u32le(p + 14)?;
        p += 27 + n + segs;
        if ty & 0x04 != 0 && this == serial {
            return Some(p);
        }
    }
}

/// After the last `%%EOF` that is not followed by more PDF (incremental updates).
fn pdf(s: &mut Stream) -> Option<u64> {
    let mut from = 0u64;
    let mut end = None;
    while let Some(e) = s.find(from, b"%%EOF", s.cap()) {
        let mut q = e + 5;
        while let Some(c) = s.u8(q) {
            if c == b'\r' || c == b'\n' {
                q += 1;
            } else {
                break;
            }
        }
        end = Some(q);
        // More objects or a cross-reference section follow an update.
        let next = s.bytes(q, 16).map(|b| b.to_vec()).unwrap_or_default();
        let t = next.iter().skip_while(|c| c.is_ascii_whitespace()).copied().collect::<Vec<u8>>();
        let more = t.first().is_some_and(|c| c.is_ascii_digit()) || t.starts_with(b"xref") || t.starts_with(b"%");
        if !more {
            break;
        }
        from = q;
    }
    end
}

fn ole(s: &mut Stream) -> Option<u64> {
    let shift = s.u16le(0x1E)?;
    if shift != 9 && shift != 12 {
        return None;
    }
    let ss = 1u64 << shift;
    let nfat = s.u32le(0x2C)? as u64;
    let mut fat_sectors: Vec<u64> = (0..109.min(nfat)).filter_map(|i| s.u32le(0x4C + 4 * i)).map(u64::from).collect();
    let mut difat = s.u32le(0x44)? as u64;
    let mut guard = 0;
    while (fat_sectors.len() as u64) < nfat && difat < 0xFFFF_FFFA && guard < 4096 {
        let at = (difat + 1) * ss;
        let per = ss / 4 - 1;
        for i in 0..per {
            if (fat_sectors.len() as u64) < nfat {
                fat_sectors.push(s.u32le(at + 4 * i)? as u64);
            }
        }
        difat = s.u32le(at + 4 * per)? as u64;
        guard += 1;
    }
    let mut last = 0u64;
    for (k, &fs) in fat_sectors.iter().enumerate() {
        let at = (fs + 1) * ss;
        for i in 0..ss / 4 {
            if s.u32le(at + 4 * i)? != 0xFFFF_FFFF {
                last = last.max(k as u64 * (ss / 4) + i);
            }
        }
    }
    let len = (last + 2) * ss;
    (len <= s.cap()).then_some(len)
}

/// Stream names in the root of an OLE directory.
fn ole_names(s: &mut Stream) -> Vec<String> {
    let mut out = Vec::new();
    let Some(shift) = s.u16le(0x1E) else { return out };
    if shift != 9 && shift != 12 {
        return out;
    }
    let Some(dir) = s.u32le(0x30) else { return out };
    let at = (dir as u64 + 1) << shift;
    for k in 0..((1u64 << shift) / 128) {
        let Some(e) = s.bytes(at + 128 * k, 128) else { break };
        let n = (u16le(e, 0x40) as usize).min(64);
        let units: Vec<u16> = e[..n].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
        out.push(String::from_utf16_lossy(&units));
    }
    out
}

fn rtf(s: &mut Stream) -> Option<u64> {
    if let Some(length) = s.rtf_length() {
        return length;
    }
    let mut depth = 0i64;
    let mut p = 0u64;
    let mut escaped = false;
    // Keep only recognized file starts, not an allocation for every opening brace.
    let mut nested = Vec::new();
    while p < s.cap() {
        let n = (60u64 << 10).min(s.cap() - p) as usize;
        let chunk = s.bytes(p, n)?.to_vec();
        for (i, &b) in chunk.iter().enumerate() {
            if escaped {
                escaped = false;
                continue;
            }
            match b {
                b'\\' => escaped = true,
                b'{' => {
                    depth += 1;
                    let at = p + i as u64;
                    if s.is_rtf_candidate(at) {
                        nested.push((depth, at));
                    }
                }
                b'}' => {
                    if nested.last().is_some_and(|&(d, _)| d == depth) {
                        let (_, at) = nested.pop().unwrap();
                        s.remember_rtf(at, Some(p + i as u64 + 1));
                    }
                    depth -= 1;
                    if depth == 0 {
                        return Some(p + i as u64 + 1);
                    }
                }
                0 => {
                    for (_, at) in nested {
                        s.remember_rtf(at, None);
                    }
                    return None;
                }
                _ => {}
            }
        }
        p += n as u64;
    }
    None
}

/// RAR 5 variable-length integer: (value, length).
fn rar_vint(s: &mut Stream, p: u64) -> Option<(u64, u64)> {
    let mut v = 0u64;
    for i in 0..10 {
        let b = s.u8(p + i)?;
        v |= ((b & 0x7F) as u64) << (7 * i);
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

fn rar(s: &mut Stream) -> Option<u64> {
    if s.u8(6)? == 0 {
        // RAR 4: blocks of crc(2) type(1) flags(2) size(2) [add size(4)].
        let mut p = 7u64;
        loop {
            let ty = s.u8(p + 2)?;
            let flags = s.u16le(p + 3)?;
            let size = s.u16le(p + 5)? as u64;
            let add = if flags & 0x8000 != 0 { s.u32le(p + 7)? as u64 } else { 0 };
            if size < 7 {
                return None;
            }
            p += size + add;
            if ty == 0x7B {
                return Some(p);
            }
        }
    }
    // RAR 5: crc(4) size(vint) type(vint) flags(vint) [extra(vint)] [data(vint)].
    let mut p = 8u64;
    loop {
        let (hsize, hl) = rar_vint(s, p + 4)?;
        let body = p + 4 + hl;
        let (ty, tl) = rar_vint(s, body)?;
        let (flags, fl) = rar_vint(s, body + tl)?;
        let mut q = body + tl + fl;
        if flags & 1 != 0 {
            q += rar_vint(s, q)?.1;
        }
        let data = if flags & 2 != 0 { rar_vint(s, q)?.0 } else { 0 };
        p = body + hsize + data;
        if ty == 5 {
            return Some(p);
        }
        if hsize == 0 || p > s.cap() {
            return None;
        }
    }
}

fn psd(s: &mut Stream) -> Option<u64> {
    let psb = s.u16be(4)? == 2;
    let channels = s.u16be(12)? as u64;
    let h = s.u32be(14)? as u64;
    let w = s.u32be(18)? as u64;
    let depth = s.u16be(22)? as u64;
    let mut p = 26u64;
    p += 4 + s.u32be(p)? as u64;
    p += 4 + s.u32be(p)? as u64;
    p += if psb { 8 + s.u64be(p)? } else { 4 + s.u32be(p)? as u64 };
    let comp = s.u16be(p)?;
    p += 2;
    match comp {
        0 => p += channels * h * w * depth.max(1) / 8,
        1 => {
            let rows = channels * h;
            let entry = if psb { 4 } else { 2 };
            let mut sum = 0u64;
            for r in 0..rows.min(1 << 20) {
                sum += if psb { s.u32be(p + 4 * r)? as u64 } else { s.u16be(p + 2 * r)? as u64 };
            }
            p += rows * entry + sum;
        }
        _ => return None,
    }
    (p <= s.cap()).then_some(p)
}

fn pe(s: &mut Stream) -> Option<u64> {
    let pe = s.u32le(0x3C)? as u64;
    let nsec = s.u16le(pe + 6)? as u64;
    let opt = s.u16le(pe + 20)? as u64;
    let magic = s.u16le(pe + 24)?;
    let mut end = pe + 24 + opt + 40 * nsec;
    for k in 0..nsec.min(96) {
        let sh = pe + 24 + opt + 40 * k;
        let raw = s.u32le(sh + 16)? as u64;
        let ptr = s.u32le(sh + 20)? as u64;
        if raw > 0 {
            end = end.max(ptr + raw);
        }
    }
    // The certificate table lies past the sections and is given by file offset.
    let dirs = match magic {
        0x10B => pe + 24 + 96,
        0x20B => pe + 24 + 112,
        _ => return None,
    };
    if opt >= (dirs - pe - 24) + 5 * 8 {
        let off = s.u32le(dirs + 32)? as u64;
        let size = s.u32le(dirs + 36)? as u64;
        if off > 0 && size > 0 {
            end = end.max(off + size);
        }
    }
    (end <= s.cap()).then_some(end)
}

/// Format a file named with extension `ext` should start with, for the formats whose
/// start is reliable.
pub fn expected(ext: &str) -> Option<Fmt> {
    let f = match ext.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" | "jpe" | "jfif" => Fmt::Jpeg,
        "png" => Fmt::Png,
        "gif" => Fmt::Gif,
        "bmp" => Fmt::Bmp,
        "tif" | "tiff" | "cr2" | "nef" | "arw" | "dng" => Fmt::Tiff,
        "wav" | "avi" | "webp" => Fmt::Riff,
        "mid" | "midi" => Fmt::Midi,
        "aif" | "aiff" | "aifc" => Fmt::Aiff,
        "ttf" | "otf" => Fmt::Sfnt,
        "flv" => Fmt::Flv,
        "vp6" => Fmt::EaVp6,
        // Old QuickTime files may start with another atom than `ftyp`: not checked.
        "mp4" | "m4v" | "m4a" | "3gp" | "heic" | "heif" | "avif" | "cr3" => Fmt::Bmff,
        "mkv" | "webm" => Fmt::Ebml,
        "flac" => Fmt::Flac,
        "ogg" | "oga" | "opus" => Fmt::Ogg,
        // A PDF may have bytes before `%PDF`: not checked.
        "zip" | "docx" | "xlsx" | "pptx" | "odt" | "ods" | "odp" | "epub" | "jar" | "apk" => Fmt::Zip,
        "doc" | "xls" | "ppt" | "msg" => Fmt::Ole,
        "rtf" => Fmt::Rtf,
        "rar" => Fmt::Rar,
        "cab" => Fmt::Cab,
        "pcx" => Fmt::Pcx,
        "dcx" => Fmt::Dcx,
        "ico" | "cur" => Fmt::Ico,
        "tga" => Fmt::Tga,
        "7z" => Fmt::SevenZip,
        "gz" | "tgz" => Fmt::Gzip,
        "psd" | "psb" => Fmt::Psd,
        "exe" | "dll" => Fmt::Pe,
        _ => return None,
    };
    Some(f)
}

/// File extension, refined from the content where the signature is shared.
pub fn extension(fmt: Fmt, s: &mut Stream) -> &'static str {
    match fmt {
        Fmt::Jpeg => "jpg",
        Fmt::Png => "png",
        Fmt::Gif => "gif",
        Fmt::Midi => "mid",
        Fmt::Aiff => {
            if s.bytes(8, 4) == Some(b"AIFC") {
                "aifc"
            } else {
                "aiff"
            }
        }
        Fmt::Sfnt => {
            if s.bytes(0, 4) == Some(b"OTTO") {
                "otf"
            } else {
                "ttf"
            }
        }
        Fmt::Swf => "swf",
        Fmt::Flv => "flv",
        Fmt::EaVp6 => "vp6",
        Fmt::Text => "txt",
        Fmt::Bmp => "bmp",
        Fmt::Tiff => {
            if s.bytes(8, 2) == Some(b"CR") {
                return "cr2";
            }
            let head = s.bytes(0, 4096.min(s.cap() as usize)).map(|b| b.to_vec()).unwrap_or_default();
            let has = |p: &[u8]| memchr::memmem::find(&head, p).is_some();
            if has(b"NIKON") {
                "nef"
            } else if has(b"SONY") {
                "arw"
            } else if has(&[0xC6, 0x12]) || has(&[0x12, 0xC6]) {
                "dng"
            } else {
                "tif"
            }
        }
        Fmt::Riff => match s.bytes(8, 4) {
            Some(b"WAVE") => "wav",
            Some(b"AVI ") => "avi",
            _ => "webp",
        },
        Fmt::Bmff => match s.bytes(8, 4) {
            Some(b"qt  ") => "mov",
            Some(b"heic") | Some(b"heix") | Some(b"mif1") | Some(b"msf1") | Some(b"hevc") => "heic",
            Some(b"avif") => "avif",
            Some(b"crx ") => "cr3",
            Some(b"M4A ") | Some(b"M4B ") => "m4a",
            Some(b) if b.starts_with(b"3g") => "3gp",
            _ => "mp4",
        },
        Fmt::Ebml => {
            let head = s.bytes(0, 64).map(|b| b.to_vec()).unwrap_or_default();
            if memchr::memmem::find(&head, b"webm").is_some() { "webm" } else { "mkv" }
        }
        Fmt::Mp3 => "mp3",
        Fmt::Flac => "flac",
        Fmt::Ogg => "ogg",
        Fmt::Pdf => "pdf",
        Fmt::Zip => {
            // Some writers use backslashes inside the archive.
            let names: Vec<String> =
                super::zip::info(s).map(|z| z.names.clone()).unwrap_or_default().into_iter().map(|n| n.replace('\\', "/")).collect();
            let any = |p: &str| names.iter().any(|n| n.starts_with(p));
            if any("word/") {
                "docx"
            } else if any("xl/") {
                "xlsx"
            } else if any("ppt/") {
                "pptx"
            } else if any("AndroidManifest.xml") {
                "apk"
            } else if any("META-INF/MANIFEST.MF") {
                "jar"
            } else if any("[Content_Types].xml") {
                "docx"
            } else if names.first().is_some_and(|n| n == "mimetype") {
                let m = s.bytes(30 + 8, 64).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
                if m.contains("epub") {
                    "epub"
                } else if m.contains("spreadsheet") {
                    "ods"
                } else if m.contains("presentation") {
                    "odp"
                } else {
                    "odt"
                }
            } else {
                "zip"
            }
        }
        Fmt::Ole => {
            let names = ole_names(s);
            let has = |n: &str| names.iter().any(|x| x == n || x.starts_with(n));
            if has("WordDocument") {
                "doc"
            } else if has("Workbook") || has("Book") {
                "xls"
            } else if has("PowerPoint Document") {
                "ppt"
            } else if has("__substg1.0_") {
                "msg"
            } else {
                "ole"
            }
        }
        Fmt::Rtf => "rtf",
        Fmt::Rar => "rar",
        Fmt::Cab => "cab",
        Fmt::Pcx => "pcx",
        Fmt::Dcx => "dcx",
        Fmt::Ico => {
            if s.u16le(2) == Some(2) {
                "cur"
            } else {
                "ico"
            }
        }
        Fmt::Tga => "tga",
        Fmt::SevenZip => "7z",
        Fmt::Gzip => "gz",
        Fmt::Sqlite => "sqlite",
        Fmt::Psd => {
            if s.u16be(4) == Some(2) {
                "psb"
            } else {
                "psd"
            }
        }
        Fmt::Pe => {
            let pe = s.u32le(0x3C).unwrap_or(0) as u64;
            if s.u16le(pe + 22).is_some_and(|c| c & 0x2000 != 0) { "dll" } else { "exe" }
        }
        Fmt::Xml => "xml",
        Fmt::Html => "html",
        Fmt::User(i) => user_sigs().get(i as usize).map_or("bin", |u| u.ext),
    }
}

/// A name from the file's own metadata, without extension.
pub fn meta_name(fmt: Fmt, s: &mut Stream, len: u64) -> Option<String> {
    let name = match fmt {
        Fmt::Jpeg => exif_name(s),
        Fmt::Mp3 => id3_name(s),
        Fmt::Pdf => pdf_title(s, len),
        _ => None,
    }?;
    let clean: String = name.chars().filter(|c| !c.is_control()).collect::<String>().trim().chars().take(120).collect();
    (!clean.is_empty()).then_some(clean)
}

/// `2026-03-15 12-34-56 Canon EOS 80D` from the EXIF block of a JPEG.
fn exif_name(s: &mut Stream) -> Option<String> {
    // APP1 is usually the first or second segment.
    let mut p = 2u64;
    for _ in 0..4 {
        let m = s.bytes(p, 4)?;
        if m[0] != 0xFF {
            return None;
        }
        let len = u16::from_be_bytes([m[2], m[3]]) as usize;
        if m[1] == 0xE1 && len > 16 {
            let seg = s.bytes(p + 4, len - 2)?.to_vec();
            if !seg.starts_with(b"Exif\0\0") {
                return None;
            }
            return exif_from_tiff(&seg[6..]);
        }
        p += 2 + len as u64;
    }
    None
}

fn exif_from_tiff(t: &[u8]) -> Option<String> {
    let le = t.get(..2)? == b"II";
    let r16 = |o: usize| t.get(o..o + 2).map(|b| if le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) });
    let r32 = |o: usize| {
        t.get(o..o + 4).map(|b| if le { u32::from_le_bytes(b.try_into().unwrap()) } else { u32::from_be_bytes(b.try_into().unwrap()) })
    };
    let ascii = |o: usize, n: usize| t.get(o..o + n).map(|b| String::from_utf8_lossy(b).trim_end_matches('\0').trim().to_string());
    let tag = |ifd: usize, want: u16| -> Option<(u16, usize, usize)> {
        let n = r16(ifd)? as usize;
        (0..n.min(512)).find_map(|k| {
            let e = ifd + 2 + 12 * k;
            (r16(e)? == want).then(|| {
                let count = r32(e + 4)? as usize;
                let at = if count <= 4 { e + 8 } else { r32(e + 8)? as usize };
                Some((r16(e + 2)?, count, at))
            })?
        })
    };
    let ifd0 = r32(4)? as usize;
    let model = tag(ifd0, 0x0110).and_then(|(_, n, at)| ascii(at, n));
    let make = tag(ifd0, 0x010F).and_then(|(_, n, at)| ascii(at, n));
    let exif = tag(ifd0, 0x8769).and_then(|(_, _, at)| r32(at));
    let date = exif.and_then(|e| tag(e as usize, 0x9003)).or_else(|| tag(ifd0, 0x0132)).and_then(|(_, n, at)| ascii(at, n));
    let date = date
        .filter(|d| d.len() >= 19 && d.as_bytes()[4] == b':')
        .map(|d| format!("{} {}", d[..10].replace(':', "-"), d[11..19].replace(':', "-")));
    let camera = match (make, model) {
        (Some(mk), Some(md)) if !md.to_lowercase().starts_with(&mk.to_lowercase().split(' ').next().unwrap_or("").to_string()) => {
            Some(format!("{mk} {md}"))
        }
        (_, Some(md)) => Some(md),
        (Some(mk), None) => Some(mk),
        _ => None,
    };
    match (date, camera) {
        (Some(d), Some(c)) => Some(format!("{d} {c}")),
        (Some(d), None) => Some(d),
        (None, Some(c)) => Some(c),
        _ => None,
    }
}

/// `Artist - Title` from an ID3v2 tag.
fn id3_name(s: &mut Stream) -> Option<String> {
    let h = s.bytes(0, 10)?.to_vec();
    let ver = h[3];
    let size = ((h[6] as usize) << 21) | ((h[7] as usize) << 14) | ((h[8] as usize) << 7) | h[9] as usize;
    let tag = s.bytes(10, size.min(60 << 10))?.to_vec();
    let mut p = 0;
    let (mut artist, mut title) = (None, None);
    while p + 10 <= tag.len() && ver >= 3 {
        let id = &tag[p..p + 4];
        if id[0] == 0 {
            break;
        }
        let n = if ver == 4 {
            ((tag[p + 4] as usize) << 21) | ((tag[p + 5] as usize) << 14) | ((tag[p + 6] as usize) << 7) | tag[p + 7] as usize
        } else {
            u32be(&tag, p + 4) as usize
        };
        let body = tag.get(p + 10..p + 10 + n)?;
        let text = || -> Option<String> {
            let (enc, b) = body.split_first()?;
            let v = match enc {
                0 => b.iter().map(|&c| c as char).collect::<String>(),
                3 => String::from_utf8_lossy(b).into_owned(),
                1 | 2 => {
                    let (be, b) = match b {
                        [0xFE, 0xFF, rest @ ..] => (true, rest),
                        [0xFF, 0xFE, rest @ ..] => (false, rest),
                        _ => (*enc == 2, b),
                    };
                    let u: Vec<u16> = b
                        .chunks_exact(2)
                        .map(|c| if be { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) })
                        .collect();
                    String::from_utf16_lossy(&u)
                }
                _ => return None,
            };
            Some(v.trim_end_matches('\0').trim().to_string()).filter(|v| !v.is_empty())
        };
        match id {
            b"TPE1" => artist = text(),
            b"TIT2" => title = text(),
            _ => {}
        }
        p += 10 + n;
    }
    match (artist, title) {
        (Some(a), Some(t)) => Some(format!("{a} - {t}")),
        (None, Some(t)) => Some(t),
        _ => None,
    }
}

/// `/Title (...)` from the end of a PDF (its information dictionary).
fn pdf_title(s: &mut Stream, len: u64) -> Option<String> {
    let from = len.saturating_sub(64 << 10);
    let tail = s.bytes(from, (len - from) as usize)?.to_vec();
    let i = memchr::memmem::rfind(&tail, b"/Title")?;
    let rest = &tail[i + 6..];
    let rest = &rest[rest.iter().position(|c| !c.is_ascii_whitespace())?..];
    let bytes: Vec<u8> = match rest.first()? {
        b'(' => {
            let mut out = Vec::new();
            let mut depth = 0;
            let mut k = 1;
            while k < rest.len() {
                let c = rest[k];
                match c {
                    b'\\' if k + 1 < rest.len() => {
                        k += 1;
                        out.push(match rest[k] {
                            b'n' => b' ',
                            b'r' | b't' => b' ',
                            o => o,
                        });
                    }
                    b'(' => {
                        depth += 1;
                        out.push(c)
                    }
                    b')' if depth == 0 => break,
                    b')' => {
                        depth -= 1;
                        out.push(c)
                    }
                    _ => out.push(c),
                }
                k += 1;
            }
            out
        }
        b'<' => {
            let end = rest.iter().position(|&c| c == b'>')?;
            let hex: Vec<u8> = rest[1..end].iter().copied().filter(u8::is_ascii_hexdigit).collect();
            hex.chunks(2).filter_map(|p| u8::from_str_radix(std::str::from_utf8(p).ok()?, 16).ok()).collect()
        }
        _ => return None,
    };
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let u: Vec<u16> = bytes[2..].chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        Some(String::from_utf16_lossy(&u))
    } else {
        Some(bytes.iter().map(|&c| c as char).collect())
    }
}
