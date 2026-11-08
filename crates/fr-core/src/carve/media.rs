//! Checked container walks. Payload sizes are skipped without decoding or allocation.
use super::stream::Stream;

fn vlq(s: &mut Stream, p: &mut u64, end: u64) -> Option<u64> {
    let mut value = 0;
    for _ in 0..4 {
        if *p >= end {
            return None;
        }
        let b = s.u8(*p)?;
        *p += 1;
        value = (value << 7) | (b & 127) as u64;
        if b & 128 == 0 {
            return Some(value);
        }
    }
    None
}

fn track(s: &mut Stream, mut p: u64, end: u64) -> Option<()> {
    let mut running = 0;
    while p < end {
        vlq(s, &mut p, end)?;
        if p >= end {
            return None;
        }
        let mut status = s.u8(p)?;
        if status < 128 {
            if running == 0 {
                return None;
            }
            status = running;
        } else {
            p += 1;
        }
        match status {
            0x80..=0xef => {
                running = status;
                let n = if matches!(status >> 4, 12 | 13) { 1 } else { 2 };
                if p + n > end || s.bytes(p, n as usize)?.iter().any(|b| b & 128 != 0) {
                    return None;
                }
                p += n;
            }
            0xff => {
                running = 0;
                if p >= end {
                    return None;
                }
                let ty = s.u8(p)?;
                p += 1;
                let len = vlq(s, &mut p, end)?;
                if ty == 0x2f {
                    return (len == 0 && p == end).then_some(());
                }
                p = p.checked_add(len)?;
                if p > end {
                    return None;
                }
            }
            0xf0 | 0xf7 => {
                running = 0;
                let len = vlq(s, &mut p, end)?;
                p = p.checked_add(len)?;
                if p > end {
                    return None;
                }
            }
            _ => return None,
        }
    }
    None // Every track must end with an End Of Track event.
}

pub fn midi(s: &mut Stream) -> Option<u64> {
    if s.bytes(0, 4)? != b"MThd" {
        return None;
    }
    let header = s.u32be(4)? as u64;
    let format = s.u16be(8)?;
    let tracks = s.u16be(10)?;
    let division = s.u16be(12)?;
    if header < 6 || format > 2 || tracks == 0 || (format == 0 && tracks != 1) || division == 0 {
        return None;
    }
    if division & 0x8000 != 0 && (![-24i8, -25, -29, -30].contains(&((division >> 8) as i8)) || division & 255 == 0) {
        return None;
    }
    let mut p = 8 + header;
    let mut found = 0;
    while found < tracks {
        let tag: [u8; 4] = s.bytes(p, 4)?.try_into().ok()?;
        let len = s.u32be(p + 4)? as u64;
        let end = p.checked_add(8)?.checked_add(len)?;
        if end > s.cap() || !tag.iter().all(u8::is_ascii_alphabetic) {
            return None;
        }
        if &tag == b"MTrk" {
            track(s, p + 8, end)?;
            found += 1;
        }
        p = end;
    }
    Some(p)
}

pub fn aiff(s: &mut Stream) -> Option<u64> {
    s.metadata_reads();
    if s.bytes(0, 4)? != b"FORM" {
        return None;
    }
    let compressed = match s.bytes(8, 4)? {
        b"AIFF" => false,
        b"AIFC" => true,
        _ => return None,
    };
    let end = 8 + s.u32be(4)? as u64;
    if end < 12 || end > s.cap() {
        return None;
    }
    let (mut common, mut sound) = (false, false);
    let mut p = 12;
    while p < end {
        let tag: [u8; 4] = s.bytes(p, 4)?.try_into().ok()?;
        if !tag.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
            return None;
        }
        let len = s.u32be(p + 4)? as u64;
        let next = p.checked_add(8)?.checked_add(len)?.checked_add(len & 1)?;
        if next > end {
            return None;
        }
        match &tag {
            b"COMM" => {
                if common
                    || len < if compressed { 22 } else { 18 }
                    || !(1..=64).contains(&s.u16be(p + 8)?)
                    || !(1..=64).contains(&s.u16be(p + 14)?)
                    || s.u16be(p + 16)? & 0x7fff == 0x7fff
                {
                    return None;
                }
                common = true;
            }
            b"SSND" => {
                if sound || len < 8 || s.u32be(p + 8)? as u64 > len - 8 {
                    return None;
                }
                sound = true;
            }
            _ => {}
        }
        p = next;
    }
    (p == end && common && sound).then_some(end)
}

pub fn sfnt(s: &mut Stream) -> Option<u64> {
    s.metadata_reads();
    let version = s.u32be(0)?;
    if ![0x00010000, 0x4f54544f, 0x74727565].contains(&version) {
        return None;
    }
    let tables = s.u16be(4)? as u64;
    if tables == 0 || tables > (s.cap().saturating_sub(12)) / 16 {
        return None;
    }
    let directory = 12 + tables * 16;
    let mut ranges = Vec::new();
    let mut previous = 0;
    let mut required = 0;
    let mut end = directory;
    for i in 0..tables {
        let p = 12 + 16 * i;
        let tag = s.u32be(p)?;
        if i > 0 && tag <= previous {
            return None;
        }
        previous = tag;
        let off = s.u32be(p + 8)? as u64;
        let len = s.u32be(p + 12)? as u64;
        let limit = off.checked_add(len)?;
        if off < directory || !off.is_multiple_of(4) || limit > s.cap() {
            return None;
        }
        match &tag.to_be_bytes() {
            b"head" => {
                if len < 54 || s.u32be(off + 12)? != 0x5f0f3cf5 {
                    return None;
                }
                required |= 1;
            }
            b"cmap" => {
                if len < 4 || s.u16be(off)? != 0 || s.u16be(off + 2)? == 0 {
                    return None;
                }
                required |= 2;
            }
            b"maxp" => {
                if len < 6 {
                    return None;
                }
                required |= 4;
            }
            b"name" => {
                if len < 6 {
                    return None;
                }
                required |= 8;
            }
            b"glyf" => required |= 16,
            b"loca" => required |= 32,
            b"CFF " | b"CFF2" => required |= 64,
            _ => {}
        }
        if len != 0 {
            ranges.push((off, limit));
        }
        end = end.max(limit);
    }
    ranges.sort_unstable();
    if ranges.windows(2).any(|w| w[0].1 > w[1].0)
        || required & 15 != 15
        || if version == 0x4f54544f { required & 64 == 0 } else { required & 48 != 48 }
    {
        return None;
    }
    let padded = end.div_ceil(4) * 4;
    if padded <= s.cap() && s.bytes(end, (padded - end) as usize)?.iter().all(|b| *b == 0) {
        end = padded;
    }
    Some(end)
}

pub fn swf(s: &mut Stream) -> Option<u64> {
    s.metadata_reads();
    if s.bytes(0, 3)? != b"FWS" || !(1..=50).contains(&s.u8(3)?) {
        return None;
    }
    let end = s.u32le(4)? as u64;
    let bits = s.u8(8)? >> 3;
    if bits == 0 {
        return None;
    }
    let mut p = 8 + (5 + 4 * bits as u64).div_ceil(8) + 4;
    if end > s.cap() || p + 2 > end || s.u16le(p - 2)? == 0 {
        return None;
    }
    while p + 2 <= end {
        let tag = s.u16le(p)?;
        p += 2;
        let len = if tag & 63 == 63 {
            if p + 4 > end {
                return None;
            }
            let len = s.u32le(p)? as u64;
            p += 4;
            len
        } else {
            (tag & 63) as u64
        };
        if tag >> 6 == 0 {
            return (len == 0 && p == end).then_some(end);
        }
        p = p.checked_add(len)?;
        if p > end {
            return None;
        }
    }
    None
}

fn u24(s: &mut Stream, p: u64) -> Option<u64> {
    let b = s.bytes(p, 3)?;
    Some(((b[0] as u64) << 16) | ((b[1] as u64) << 8) | b[2] as u64)
}

/// FLV has no mandatory terminal tag: return only its validated prefix, marked partial.
pub fn flv(s: &mut Stream) -> Option<u64> {
    s.metadata_reads();
    if s.bytes(0, 3)? != b"FLV" || s.u8(3)? != 1 || s.u8(4)? & !5 != 0 {
        return None;
    }
    let header = s.u32be(5)? as u64;
    if header < 9 || s.u32be(header)? != 0 {
        return None;
    }
    let mut p = header + 4;
    let mut last = None;
    while p + 15 <= s.cap() {
        if ![8, 9, 18].contains(&s.u8(p)?) || u24(s, p + 8)? != 0 {
            break;
        }
        let size = u24(s, p + 1)?;
        let next = p.checked_add(11)?.checked_add(size)?;
        if next + 4 > s.cap() || s.u32be(next)? as u64 != size + 11 {
            break;
        }
        p = next + 4;
        last = Some(p);
    }
    last
}

/// EA VP6 (MVhd), with a declared video-frame count and checked chunk lengths.
pub fn ea_vp6(s: &mut Stream) -> Option<u64> {
    s.metadata_reads();
    if s.bytes(0, 4)? != b"MVhd" {
        return None;
    }
    let first = s.u32le(4)?;
    let big = first > first.swap_bytes();
    let first = if big { first.swap_bytes() } else { first } as u64;
    let frames = s.u32le(16)? as u64;
    if first < 32 || first > s.cap() || frames == 0 || frames > s.cap() / 8 || s.u32le(24)? == 0 || s.u32le(28)? == 0 {
        return None;
    }
    let mut p = first;
    let mut read = 0;
    loop {
        if p + 8 > s.cap() {
            return (read == frames).then_some(p);
        }
        let tag: [u8; 4] = s.bytes(p, 4)?.try_into().ok()?;
        let len = if big { s.u32be(p + 4)? } else { s.u32le(p + 4)? } as u64;
        let ending = matches!(&tag, b"SCEl" | b"SEND" | b"SEEN" | b"\0\0\0\0");
        if !ending && !matches!(&tag, b"MV0K" | b"MV0F" | b"AVhd" | b"AV0K" | b"AV0F" | b"SCHl" | b"SCDl" | b"SNDC") {
            return (read == frames).then_some(p);
        }
        if len < 8 || p.checked_add(len)? > s.cap() {
            return (read == frames && tag == [0; 4]).then_some(p);
        }
        if matches!(&tag, b"MV0K" | b"MV0F") {
            if len == 8 {
                return None;
            }
            read += 1;
            if read > frames {
                return None;
            }
        }
        p += len;
        if ending {
            return (read == frames).then_some(p);
        }
    }
}
