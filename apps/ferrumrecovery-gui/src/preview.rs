//! Bounded in-memory previews. No shell preview handlers or temporary files.
use crate::app;
use crate::lang::{S, t};
use crate::util::*;
use std::ffi::c_void;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::core::GUID;

#[derive(Clone, Copy, Default, PartialEq)]
pub enum Mode {
    #[default]
    Image,
    Text,
    Hex,
}

pub struct Image {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u32>,
}

pub const BYTE_LIMIT: usize = 1 << 20;
pub const IMAGE_LIMIT: usize = 64 << 20;
const EDGE: u32 = 1024;

pub struct Content {
    pub bytes: Vec<u8>,
    pub image: Option<Image>,
    pub notice: Option<S>,
}

fn image_name(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, ext)| {
        ["png", "jpg", "jpeg", "jpe", "bmp", "gif", "tif", "tiff", "ico"].iter().any(|e| ext.eq_ignore_ascii_case(e))
    })
}

pub fn load(src: &fr_core::Source, cat: &fr_core::Catalog, i: u32) -> std::io::Result<Content> {
    use std::io::ErrorKind;
    let entry = &cat.entries[i as usize];
    if matches!(entry.state, fr_core::State::NoData | fr_core::State::Encrypted) {
        return Err(fr_core::error::Error::new("preview unavailable").into_io(ErrorKind::InvalidData));
    }
    let known_image = image_name(cat.name(i));
    let too_large = entry.size > IMAGE_LIMIT as u64;
    let limit = if known_image && !too_large { entry.size } else { entry.size.min(BYTE_LIMIT as u64) };
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(limit as usize)
        .map_err(|_| fr_core::error::Error::new("not enough memory for preview").into_io(ErrorKind::OutOfMemory))?;
    let mut reader = fr_core::data::Reader::new(src, cat)?;
    let mut stats = reader.read_limit(i, limit, &mut |b| {
        bytes.extend_from_slice(b);
        Ok(())
    })?;
    let image_type = known_image || codec(&bytes).is_some();
    // Unusually named images are recognized by their contents; common extensions read once.
    if image_type && !too_large && bytes.len() as u64 != entry.size {
        bytes.clear();
        bytes
            .try_reserve_exact(entry.size as usize)
            .map_err(|_| fr_core::error::Error::new("not enough memory for preview").into_io(ErrorKind::OutOfMemory))?;
        stats = reader.read(i, &mut |b| {
            bytes.extend_from_slice(b);
            Ok(())
        })?;
    }
    src.checkpoint()?;
    let incomplete = stats.bad != 0 || stats.missing != 0 || stats.corrupt_units != 0;
    let image = if image_type && !too_large && !incomplete { decode(&bytes) } else { None };
    src.checkpoint()?;
    let notice = if image_type && too_large {
        Some(S::PreviewImageLimit)
    } else if incomplete {
        Some(S::PreviewIncomplete)
    } else if image_type && image.is_none() {
        Some(S::PreviewDecodeFailed)
    } else {
        None
    };
    bytes.truncate(BYTE_LIMIT);
    bytes.shrink_to_fit();
    Ok(Content { bytes, image, notice })
}

// Built-in Windows codecs, instantiated explicitly (no third-party codec discovery).
fn codec(b: &[u8]) -> Option<GUID> {
    let id = if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        0x389ea17b_5078_4cde_b6ef_25c15175c751
    } else if b.starts_with(&[0xff, 0xd8]) {
        0x9456a480_e88b_43ea_9e73_0b2d9b71b1ca
    } else if b.starts_with(b"BM") {
        0x6b462062_7cbf_400d_9fdb_813dd10f2778
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        0x381dda3c_9ce9_4834_a23e_1f98f8fc52be
    } else if b.starts_with(b"II\x2a\0") || b.starts_with(b"MM\0\x2a") {
        0xb54e85d9_fe23_499f_8b88_6acea713752b
    } else if b.starts_with(&[0, 0, 1, 0]) {
        0xc61bfcdf_2e0f_4aad_a8d7_e06bafebcdfe
    } else {
        return None;
    };
    Some(GUID::from_u128(id))
}

struct Com(*mut c_void);
impl Com {
    // Vtable slots/signatures follow the Windows SDK wincodec.h interfaces.
    unsafe fn method<T: Copy>(&self, slot: usize) -> T {
        unsafe {
            debug_assert_eq!(size_of::<T>(), size_of::<usize>());
            let table = *(self.0 as *const *const usize);
            std::mem::transmute_copy(&*table.add(slot))
        }
    }

    unsafe fn create(class: &GUID, interface: &GUID) -> Option<Self> {
        let mut p = std::ptr::null_mut();
        unsafe { windows_sys::Win32::System::Com::CoCreateInstance(class, std::ptr::null_mut(), 1, interface, &mut p) };
        (!p.is_null()).then_some(Self(p))
    }

    unsafe fn output(&self, slot: usize) -> Option<Self> {
        unsafe {
            let call: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32 = self.method(slot);
            let mut p = std::ptr::null_mut();
            (call(self.0, &mut p) >= 0 && !p.is_null()).then_some(Self(p))
        }
    }
}
impl Drop for Com {
    fn drop(&mut self) {
        unsafe {
            let release: unsafe extern "system" fn(*mut c_void) -> u32 = self.method(2);
            release(self.0);
        }
    }
}

struct Apartment(bool);
impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                windows_sys::Win32::System::Com::CoUninitialize();
            }
        }
    }
}

fn thumbnail_size(w: u32, h: u32) -> Option<(u32, u32)> {
    if w == 0 || h == 0 || w > 65_535 || h > 65_535 || w as u64 * h as u64 > 100_000_000 {
        return None;
    }
    let longest = w.max(h);
    Some(if longest <= EDGE {
        (w, h)
    } else {
        (((w as u64 * EDGE as u64 / longest as u64) as u32).max(1), ((h as u64 * EDGE as u64 / longest as u64) as u32).max(1))
    })
}

/// Decode the complete encoded image; keep only a bounded, opaque thumbnail.
/// GIF/TIFF/ICO use the first frame. No shell handlers, files or temporary caches.
pub fn decode(bytes: &[u8]) -> Option<Image> {
    if bytes.len() > IMAGE_LIMIT {
        return None;
    }
    let class = codec(bytes)?;
    // WIC can accept incomplete BMP headers; do not pass a known truncated file.
    if bytes.starts_with(b"BM") && (bytes.len() < 54 || u32::from_le_bytes(bytes[2..6].try_into().ok()?) as usize > bytes.len()) {
        return None;
    }
    unsafe {
        use windows_sys::Win32::System::Com::*;
        let initialized = CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED as u32);
        if initialized < 0 && initialized != 0x80010106u32 as i32 {
            return None;
        }
        let _apartment = Apartment(initialized >= 0);
        let p = windows_sys::Win32::UI::Shell::SHCreateMemStream(bytes.as_ptr(), bytes.len() as u32);
        if p.is_null() {
            return None;
        }
        let stream = Com(p);
        let factory = Com::create(
            &GUID::from_u128(0xcacaf262_9370_4615_a13b_9f5539da4c0a),
            &GUID::from_u128(0xec5ec8a9_c395_4314_9c77_54d7a935ff70),
        )?;
        let decoder = Com::create(&class, &GUID::from_u128(0x9edde9e7_8dee_47ea_99df_e6faf2ed44bf))?;
        let initialize: unsafe extern "system" fn(*mut c_void, *mut c_void, u32) -> i32 = decoder.method(4);
        if initialize(decoder.0, stream.0, 0) < 0 {
            return None;
        }
        let frame_call: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32 = decoder.method(13);
        let mut p = std::ptr::null_mut();
        if frame_call(decoder.0, 0, &mut p) < 0 || p.is_null() {
            return None;
        }
        let frame = Com(p);
        let size: unsafe extern "system" fn(*mut c_void, *mut u32, *mut u32) -> i32 = frame.method(3);
        let (mut w, mut h) = (0, 0);
        if size(frame.0, &mut w, &mut h) < 0 {
            return None;
        }
        let (width, height) = thumbnail_size(w, h)?;
        let scaler = if (width, height) != (w, h) {
            let scaler = factory.output(11)?;
            let scale: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, u32, u32) -> i32 = scaler.method(8);
            if scale(scaler.0, frame.0, width, height, 3) < 0 {
                return None;
            }
            Some(scaler)
        } else {
            None
        };
        let source = scaler.as_ref().unwrap_or(&frame);
        let converter = factory.output(10)?;
        let convert: unsafe extern "system" fn(*mut c_void, *mut c_void, *const GUID, u32, *mut c_void, f64, u32) -> i32 =
            converter.method(8);
        let format = GUID::from_u128(0x6fddc324_4e03_4bfe_b185_3d77768dc90f); // 32bppBGRA
        if convert(converter.0, source.0, &format, 0, std::ptr::null_mut(), 0.0, 0) < 0 {
            return None;
        }
        let mut pixels = vec![0u32; width as usize * height as usize];
        let copy: unsafe extern "system" fn(*mut c_void, *const c_void, u32, u32, *mut u8) -> i32 = converter.method(7);
        if copy(converter.0, std::ptr::null(), width * 4, (pixels.len() * 4) as u32, pixels.as_mut_ptr().cast()) < 0 {
            return None;
        }
        // Composite transparent pixels on a checkerboard once, outside the UI thread.
        for (i, p) in pixels.iter_mut().enumerate() {
            let alpha = *p >> 24;
            let bg = if ((i as u32 % width) / 8 + (i as u32 / width) / 8).is_multiple_of(2) { 238 } else { 214 };
            let mut rgb = 0;
            for shift in [0, 8, 16] {
                let c = (*p >> shift) & 255;
                rgb |= ((c * alpha + bg * (255 - alpha) + 127) / 255) << shift;
            }
            *p = rgb;
        }
        Some(Image { width, height, pixels })
    }
}

pub fn hex_rows(bytes: &[u8], lines: usize, bytes_per_row: usize) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    for (i, row) in bytes.chunks(bytes_per_row.clamp(1, 16)).take(lines).enumerate() {
        let _ = write!(s, "{:08X}  ", i * bytes_per_row.clamp(1, 16));
        for j in 0..bytes_per_row.clamp(1, 16) {
            if let Some(b) = row.get(j) {
                let _ = write!(s, "{b:02X} ");
            } else {
                s.push_str("   ");
            }
        }
        s.push(' ');
        for &b in row {
            s.push(if (32..127).contains(&b) { b as char } else { '.' });
        }
        s.push('\n');
    }
    s
}

fn text_content(bytes: &[u8]) -> String {
    let b = &bytes[..bytes.len().min(16 << 10)];
    if b.starts_with(&[0xff, 0xfe]) || b.starts_with(&[0xfe, 0xff]) {
        let le = b[0] == 0xff;
        let words: Vec<_> = b[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|x| if le { u16::from_le_bytes([x[0], x[1]]) } else { u16::from_be_bytes([x[0], x[1]]) })
            .collect();
        return String::from_utf16_lossy(&words).replace('\0', " ");
    }
    String::from_utf8_lossy(b).chars().map(|c| if c.is_control() && c != '\n' && c != '\r' && c != '\t' { '.' } else { c }).collect()
}

pub fn paint(dc: HDC, area: &windows_sys::Win32::Foundation::RECT) {
    buffered_paint(dc, area, |memory| paint_parts(memory, area));
}

fn paint_parts(dc: HDC, area: &windows_sys::Win32::Foundation::RECT) {
    app::with(|a| {
        fill(dc, area, a.pal.panel);
        unsafe {
            SelectObject(dc, a.font);
        }
        let pad = a.scale(12);
        let mut r = *area;
        r.left += pad;
        r.top += pad;
        r.right -= pad;
        r.bottom -= pad;
        let Some(i) = a.preview_entry else {
            text(dc, t(S::NoPreview), &r, a.pal.dim, 0);
            return;
        };
        let Some(cat) = a.cat() else {
            return;
        };
        let e = &cat.entries[i as usize];
        unsafe {
            SelectObject(dc, a.bold);
        }
        let line = a.scale(23);
        text(dc, &crate::lang::entry_name(cat, i), &rect(r.left, r.top, r.right, r.top + line), a.pal.text, 0);
        r.top += line + pad;
        unsafe {
            SelectObject(dc, a.font);
        }
        let details = format!(
            "{}: {}\n{}: {}\n{}: {}\n\n{}",
            t(S::Size),
            crate::lang::size(e.size),
            t(S::Modified),
            filetime(e.modified),
            t(S::StateLabel),
            crate::lang::state_name(e.state),
            crate::lang::entry_desc(e)
        );
        let height = text_block(dc, &details, &r, a.pal.text, 0);
        r.top += height + pad;
        if cat.path(i).starts_with(fr_core::model::LOST_DIR) {
            r.top += text_block(dc, t(S::LostPath), &r, a.pal.dim, 0) + pad;
        }
        if let Some(notice) = a.preview_notice {
            r.top += text_block(dc, t(notice), &r, a.pal.warn, 0) + pad;
        }
        if e.size > 0 && a.preview_bytes.is_empty() && a.preview_error.is_none() {
            text(dc, t(S::Loading), &r, a.pal.dim, 0);
            return;
        }
        if let Some(error) = &a.preview_error {
            text_block(dc, &crate::lang::error(error), &r, a.pal.bad, 0);
            return;
        }
        let show_image = a.preview_mode == Mode::Image && a.preview_image.is_some();
        if !show_image {
            let mut foot = r;
            unsafe {
                SelectObject(dc, a.small);
            }
            let mut measure = foot;
            measure.top = 0;
            let height = text_block_height(dc, t(S::PreviewLimit), &measure, 0);
            foot.top = (r.bottom - height).max(r.top);
            text_block(dc, t(S::PreviewLimit), &foot, a.pal.dim, 0);
            r.bottom = foot.top - pad;
        }
        if r.bottom <= r.top {
            return;
        }
        if a.preview_mode == Mode::Image
            && let Some(image) = &a.preview_image
        {
            let factor = ((r.right - r.left) as f64 / image.width as f64).min((r.bottom - r.top) as f64 / image.height as f64).min(1.0);
            let (w, h) = ((image.width as f64 * factor) as i32, (image.height as f64 * factor) as i32);
            unsafe {
                let mut bi: BITMAPINFO = std::mem::zeroed();
                bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
                bi.bmiHeader.biWidth = image.width as i32;
                bi.bmiHeader.biHeight = -(image.height as i32);
                bi.bmiHeader.biPlanes = 1;
                bi.bmiHeader.biBitCount = 32;
                bi.bmiHeader.biCompression = BI_RGB;
                SetStretchBltMode(dc, HALFTONE);
                StretchDIBits(
                    dc,
                    r.left + (r.right - r.left - w) / 2,
                    r.top,
                    w,
                    h,
                    0,
                    0,
                    image.width as i32,
                    image.height as i32,
                    image.pixels.as_ptr().cast(),
                    &bi,
                    DIB_RGB_COLORS,
                    SRCCOPY,
                );
            }
        } else {
            unsafe {
                SelectObject(dc, a.mono);
            }
            let mut metrics: TEXTMETRICW = unsafe { std::mem::zeroed() };
            unsafe {
                GetTextMetricsW(dc, &mut metrics);
            }
            let columns = ((r.right - r.left) / metrics.tmAveCharWidth.max(1)).max(1) as usize;
            let rows = ((r.bottom - r.top) / metrics.tmHeight.max(1)).max(1) as usize;
            let content = if a.preview_mode == Mode::Text {
                text_content(&a.preview_bytes)
            } else {
                hex_rows(&a.preview_bytes, rows, columns.saturating_sub(11).checked_div(4).unwrap_or(1).clamp(1, 16))
            };
            unsafe {
                SelectObject(dc, a.mono);
            }
            text_block(dc, &content, &r, a.pal.text, 0);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bmp(w: u32, h: u32) -> Vec<u8> {
        let stride = (w as usize * 3).div_ceil(4) * 4;
        let mut b = vec![0u8; 54 + stride * h as usize];
        b[..2].copy_from_slice(b"BM");
        let len = b.len() as u32;
        b[2..6].copy_from_slice(&len.to_le_bytes());
        b[10..14].copy_from_slice(&54u32.to_le_bytes());
        b[14..18].copy_from_slice(&40u32.to_le_bytes());
        b[18..22].copy_from_slice(&w.to_le_bytes());
        b[22..26].copy_from_slice(&h.to_le_bytes());
        b[26..28].copy_from_slice(&1u16.to_le_bytes());
        b[28..30].copy_from_slice(&24u16.to_le_bytes());
        b[54..].fill(180);
        b
    }

    #[test]
    fn large_photo_decodes_to_bounded_thumbnail() {
        let image = decode(&bmp(2100, 2000)).expect("ordinary photos above four megapixels need a preview");
        assert!(image.width <= 1024 && image.height <= 1024);
        assert!(image.pixels.last().unwrap() & 0x00ffffff != 0);
    }

    #[test]
    fn truncated_image_is_not_presented_as_complete() {
        let mut b = bmp(600, 800);
        b.truncate(1 << 20);
        assert!(decode(&b).is_none(), "a prefix must not turn the missing image rows black");
    }

    #[test]
    fn file_preview_reads_beyond_one_megabyte_and_keeps_only_a_thumbnail() {
        use fr_core::model::{Builder, Entry, FsInfo, NONE, Run, ef};
        let bytes = bmp(600, 800);
        let size = bytes.len() as u64;
        let mut disk = bytes;
        disk.resize(disk.len().div_ceil(512) * 512, 0);
        let path = std::env::temp_dir().join(format!("fr-preview-{}.img", std::process::id()));
        std::fs::write(&path, &disk).unwrap();
        let mut builder = Builder::default();
        builder.add(
            Entry { id: 10, flags: ef::DELETED | ef::ORPHAN, parent: NONE, size, valid: size, ..Default::default() },
            "large.bmp",
            &[Run { lcn: 0, len: disk.len() as u64 / 512 }],
        );
        let cat = builder.finish(FsInfo {
            kind: fr_core::part::FsKind::Ntfs,
            offset: 0,
            size: disk.len() as u64,
            cluster_size: 512,
            record_size: 1024,
            mft: vec![],
            serial: 0,
            root: 5,
            data_offset: 0,
        });
        let src = fr_core::Source::open(path.to_str().unwrap()).unwrap();
        let result = load(&src, &cat, 0);
        drop(src);
        std::fs::remove_file(&path).unwrap();
        let content = result.unwrap();
        assert_eq!(content.bytes.len(), BYTE_LIMIT);
        assert!(content.notice.is_none());
        let image = content.image.expect("the full bitmap is needed, not its 1 MB prefix");
        assert_eq!((image.width, image.height), (600, 800));
        assert_eq!(*image.pixels.last().unwrap(), 0x00b4b4b4);
        assert_eq!(cat.entries[0].state, fr_core::State::Good);
        assert!(cat.path(0).starts_with(fr_core::model::LOST_DIR));
    }
    #[test]
    fn preview_is_bounded_and_preserves_offsets() {
        assert_eq!(hex_rows(&[0, 65, 255], 1, 16), "00000000  00 41 FF                                         .A.\n");
        assert_eq!(hex_rows(&[1; 100], 2, 16).lines().count(), 2);
        assert_eq!(text_content(&[255, 254, 65, 0, 66, 0]), "AB");
    }
    #[test]
    fn rejects_unbounded_images_before_decoding() {
        let mut b = vec![0; 24];
        b[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        b[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
        b[20..24].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(decode(&b).is_none());
        assert!(decode(&[255, 216, 255, 224, 0, 0]).is_none());
    }
}
