//! Read-only access to what files are recovered from: a mounted volume (`C:`), a whole
//! physical disk (`disk0`) or an image file. Handles are opened with `GENERIC_READ` only,
//! so nothing here can change the source.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::fs::File;
use std::io;
use std::ops::{Deref, DerefMut};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_IO_PENDING, GENERIC_READ, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, DeviceIoControl, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED};
use windows_sys::Win32::System::Ioctl::{
    DISK_GEOMETRY, FSCTL_ALLOW_EXTENDED_DASD_IO, GET_LENGTH_INFORMATION, IOCTL_DISK_GET_DRIVE_GEOMETRY, IOCTL_DISK_GET_LENGTH_INFO,
    IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR, STORAGE_PROPERTY_QUERY,
    StorageAccessAlignmentProperty,
};
use windows_sys::Win32::System::Threading::CreateEventW;

/// Default buffer alignment when the driver does not report physical sectors.
pub const IO_ALIGN: usize = 4096;

#[derive(Default)]
pub struct IoStats {
    calls: AtomicU64,
    bytes: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct IoSnapshot {
    pub calls: u64,
    pub bytes: u64,
}

impl IoSnapshot {
    pub fn since(self, before: Self) -> Self {
        Self { calls: self.calls.saturating_sub(before.calls), bytes: self.bytes.saturating_sub(before.bytes) }
    }
}

/// Enumerate existing physical-drive aliases instead of probing an arbitrary range.
pub fn physical_disks() -> io::Result<Vec<u32>> {
    use windows_sys::Win32::Storage::FileSystem::QueryDosDeviceW;
    let mut names = vec![0u16; 4096];
    loop {
        // SAFETY: a null name requests the MULTI_SZ list in our writable buffer.
        let n = unsafe { QueryDosDeviceW(ptr::null(), names.as_mut_ptr(), names.len() as u32) };
        if n != 0 {
            let mut disks = names[..n as usize]
                .split(|&c| c == 0)
                .filter_map(|name| {
                    let name = String::from_utf16_lossy(name);
                    name.get(..13).filter(|p| p.eq_ignore_ascii_case("PhysicalDrive"))?;
                    name.get(13..)?.parse::<u32>().ok()
                })
                .collect::<Vec<_>>();
            disks.sort_unstable();
            disks.dedup();
            return Ok(disks);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER as i32) || names.len() >= 1 << 20 {
            return Err(error);
        }
        names.resize(names.len() * 2, 0);
    }
}

/// Heap buffer aligned to [`IO_ALIGN`], zeroed on creation.
pub struct AlignedBuf {
    ptr: NonNull<u8>,
    len: usize,
    alignment: usize,
}

// SAFETY: plain owned heap memory.
unsafe impl Send for AlignedBuf {}
unsafe impl Sync for AlignedBuf {}

impl AlignedBuf {
    pub fn new(len: usize) -> Self {
        Self::with_alignment(len, IO_ALIGN)
    }

    fn with_alignment(len: usize, alignment: usize) -> Self {
        let layout = Layout::from_size_align(len.max(1), alignment).expect("buffer layout");
        // SAFETY: non-zero size.
        let p = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(p).unwrap_or_else(|| std::alloc::handle_alloc_error(layout));
        AlignedBuf { ptr, len, alignment }
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: ptr is valid for len bytes for the lifetime of self.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and we have unique access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.len.max(1), self.alignment).unwrap();
        // SAFETY: allocated in `new` with the same layout.
        unsafe { dealloc(self.ptr.as_ptr(), layout) }
    }
}

/// One reusable overlapped request: OVERLAPPED block (heap-pinned) and an event.
pub struct ReadSlot {
    ov: Box<OVERLAPPED>,
    event: HANDLE,
    expected: u32,
}

// SAFETY: the event handle and the OVERLAPPED block are only touched by the owning thread.
unsafe impl Send for ReadSlot {}

impl ReadSlot {
    pub fn new() -> io::Result<Self> {
        // SAFETY: plain Win32 call; null name, default security.
        let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: all-zero is the documented initial state of OVERLAPPED.
        let ov = Box::new(unsafe { std::mem::zeroed::<OVERLAPPED>() });
        Ok(ReadSlot { ov, event, expected: 0 })
    }
}

impl Drop for ReadSlot {
    fn drop(&mut self) {
        // SAFETY: we own the event handle.
        unsafe { CloseHandle(self.event) };
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// Mounted volume by drive letter.
    Volume(char),
    /// Physical disk number (`\\.\PhysicalDriveN`).
    Disk(u32),
    /// Raw image file (also a fixed VHD).
    Image(PathBuf),
    /// Volume shadow copy number (`\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopyN`).
    Shadow(u32),
}

/// Something to recover from. Read-only by construction.
pub struct Source {
    handle: HANDLE,
    pub kind: SourceKind,
    /// `C:`, `disk0` or the image file name.
    pub name: String,
    /// Readable bytes.
    pub size: u64,
    /// Logical sector size: device reads must be aligned to it.
    pub sector: u32,
    buffer_alignment: usize,
    control: Option<std::sync::Arc<crate::control::Control>>,
    stats: Option<Arc<IoStats>>,
}

// SAFETY: a Win32 file handle may be used from any thread; all reads are overlapped with
// caller-owned OVERLAPPED blocks.
unsafe impl Send for Source {}
unsafe impl Sync for Source {}

/// Accepts `C`, `C:`, `C:\`, `\\.\C:`.
pub fn parse_drive_letter(spec: &str) -> Option<char> {
    let s = spec.trim().trim_start_matches(r"\\.\");
    let c = s.chars().next()?;
    let rest = &s[c.len_utf8()..];
    if c.is_ascii_alphabetic() && (rest.is_empty() || rest == ":" || rest == r":\" || rest == ":/") {
        Some(c.to_ascii_uppercase())
    } else {
        None
    }
}

/// Accepts `shadow3` or the full `\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy3`.
pub fn parse_shadow_number(spec: &str) -> Option<u32> {
    let s = spec.trim().to_ascii_lowercase();
    let n = s.strip_prefix("shadow").or_else(|| s.strip_prefix(r"\\?\globalroot\device\harddiskvolumeshadowcopy"))?;
    n.parse().ok()
}

/// Accepts `disk0`, `PhysicalDrive0`, `\\.\PhysicalDrive0`.
pub fn parse_disk_number(spec: &str) -> Option<u32> {
    let s = spec.trim().trim_start_matches(r"\\.\").to_ascii_lowercase();
    let n = s.strip_prefix("physicaldrive").or_else(|| s.strip_prefix("disk"))?;
    n.parse().ok()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn unsupported(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, crate::error::Error::new(msg))
}

impl Source {
    /// Duplicate the retained kernel object rather than reopening its pathname.
    pub fn try_clone(&self) -> io::Result<Self> {
        use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut handle = ptr::null_mut();
        unsafe {
            let process = GetCurrentProcess();
            if DuplicateHandle(process, self.handle, process, &mut handle, 0, 0, DUPLICATE_SAME_ACCESS) == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self {
            handle,
            kind: self.kind.clone(),
            name: self.name.clone(),
            size: self.size,
            sector: self.sector,
            buffer_alignment: self.buffer_alignment,
            control: self.control.clone(),
            stats: self.stats.clone(),
        })
    }
    pub(crate) fn same_file(&self, file: &File) -> io::Result<bool> {
        Ok(file_key(self.handle)? == file_key(file.as_raw_handle())?)
    }

    pub(crate) fn identity(&self) -> io::Result<String> {
        self.checkpoint()?;
        let mut identity = format!("{:?}:{}", self.kind, self.size);
        if matches!(self.kind, SourceKind::Image(_)) {
            let mut info: windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            if unsafe { windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle(self.handle, &mut info) } == 0 {
                return Err(io::Error::last_os_error());
            }
            identity +=
                &format!(":{:?}:{}:{}", file_key(self.handle)?, info.ftLastWriteTime.dwHighDateTime, info.ftLastWriteTime.dwLowDateTime);
        }
        let sample = self.read_bytes(0, self.size.min(512) as usize);
        if sample.as_ref().is_err_and(|e| e.kind() == io::ErrorKind::Interrupted) {
            return Err(sample.unwrap_err());
        }
        if let Ok(bytes) = sample {
            let hash = bytes.iter().fold(0xcbf29ce484222325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3));
            identity += &format!(":{hash:016x}");
        } else {
            identity.push_str(":unreadable-header");
        }
        Ok(identity)
    }
    /// Open `C:`, `disk0` or an image file.
    pub fn open(spec: &str) -> io::Result<Source> {
        let (kind, name, path) = if let Some(l) = parse_drive_letter(spec) {
            (SourceKind::Volume(l), format!("{l}:"), format!(r"\\.\{l}:"))
        } else if let Some(n) = parse_disk_number(spec) {
            (SourceKind::Disk(n), format!("disk{n}"), format!(r"\\.\PhysicalDrive{n}"))
        } else if let Some(n) = parse_shadow_number(spec) {
            (SourceKind::Shadow(n), format!("shadow{n}"), format!(r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy{n}"))
        } else if Path::new(spec).is_file() {
            let p = PathBuf::from(spec);
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| spec.to_string());
            (SourceKind::Image(p), name, spec.to_string())
        } else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                crate::error::Error::new("not a drive letter, disk number or image file").context(spec),
            ));
        };
        let w = wide(&path);
        // SAFETY: valid NUL-terminated path; GENERIC_READ only.
        let handle = unsafe {
            CreateFileW(
                w.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut src = Source { handle, kind, name, size: 0, sector: 512, buffer_alignment: IO_ALIGN, control: None, stats: None };
        match &src.kind {
            SourceKind::Image(p) => {
                let len = std::fs::metadata(p)?.len();
                src.size = len;
                src.check_image(len)?;
            }
            _ => {
                let mut slot = ReadSlot::new()?;
                if matches!(src.kind, SourceKind::Volume(_)) {
                    // Access the partition tail as well as filesystem-managed clusters.
                    // The handle remains read-only; start_read still enforces src.size.
                    if let Err(error) = src.ioctl(&mut slot, FSCTL_ALLOW_EXTENDED_DASD_IO, &[], &mut []) {
                        use windows_sys::Win32::Foundation::{ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED};
                        if !matches!(error.raw_os_error(), Some(n) if n == ERROR_INVALID_FUNCTION as i32 || n == ERROR_NOT_SUPPORTED as i32)
                        {
                            return Err(error);
                        }
                    }
                }
                let mut out = [0u8; size_of::<GET_LENGTH_INFORMATION>()];
                match src.ioctl(&mut slot, IOCTL_DISK_GET_LENGTH_INFO, &[], &mut out) {
                    Ok(_) => src.size = i64::from_le_bytes(out[..8].try_into().unwrap()).max(0) as u64,
                    // Shadow copies may not answer; their size is in the boot sector.
                    Err(_) if matches!(src.kind, SourceKind::Shadow(_)) => src.size = u64::MAX,
                    Err(e) => return Err(e),
                }
                let mut g = [0u8; size_of::<DISK_GEOMETRY>()];
                if src.ioctl(&mut slot, IOCTL_DISK_GET_DRIVE_GEOMETRY, &[], &mut g).is_ok() {
                    // SAFETY: the buffer holds a DISK_GEOMETRY written by the driver.
                    let geo: DISK_GEOMETRY = unsafe { ptr::read_unaligned(g.as_ptr().cast()) };
                    if geo.BytesPerSector.is_power_of_two() && (512..=4096).contains(&geo.BytesPerSector) {
                        src.sector = geo.BytesPerSector;
                    }
                }
                let mut query = [0u8; size_of::<STORAGE_PROPERTY_QUERY>()];
                query[..4].copy_from_slice(&StorageAccessAlignmentProperty.to_le_bytes());
                query[4..8].copy_from_slice(&PropertyStandardQuery.to_le_bytes());
                let mut descriptor = [0u8; size_of::<STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR>()];
                if let Ok(n) = src.ioctl(&mut slot, IOCTL_STORAGE_QUERY_PROPERTY, &query, &mut descriptor)
                    && let Some((logical, alignment)) = access_alignment(&descriptor[..n.min(descriptor.len() as u32) as usize])
                {
                    src.sector = logical;
                    src.buffer_alignment = alignment;
                }
            }
        }
        if src.size == u64::MAX && matches!(src.kind, SourceKind::Shadow(_)) {
            let boot = src.read_bytes(0, 4096)?;
            let (kind, size) = crate::part::boot_size(&boot)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, crate::error::Error::new("shadow copy size is unavailable")))?;
            let sector = u16::from_le_bytes([boot[11], boot[12]]) as u32;
            if kind != crate::part::FsKind::Ntfs || !sector.is_power_of_two() || !(512..=4096).contains(&sector) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, crate::error::Error::new("invalid shadow copy boot sector")));
            }
            src.size = size;
            src.sector = sector;
        }
        Ok(src)
    }

    /// Fixed VHD: raw disk plus a 512-byte footer. Dynamic VHD and VHDX keep the disk in
    /// blocks with their own tables and are refused rather than read wrongly.
    fn check_image(&mut self, len: u64) -> io::Result<()> {
        if len < 512 {
            return Ok(());
        }
        let head = self.read_bytes(0, 8)?;
        if &head[..] == b"vhdxfile" {
            return Err(unsupported("VHDX images are not supported; attach it in Disk Management and scan its drive letter"));
        }
        if &head[..] == b"conectix" {
            return Err(unsupported("dynamic VHD images are not supported; attach it in Disk Management and scan its drive letter"));
        }
        let tail = self.read_bytes(len - 512, 8)?;
        if &tail[..] == b"conectix" {
            self.size = len - 512;
        }
        Ok(())
    }

    pub fn is_device(&self) -> bool {
        !matches!(self.kind, SourceKind::Image(_))
    }

    pub fn with_control(mut self, control: std::sync::Arc<crate::control::Control>) -> Self {
        self.control = Some(control);
        self
    }

    pub fn with_stats(mut self, stats: Arc<IoStats>) -> Self {
        self.stats = Some(stats);
        self
    }

    pub fn io_snapshot(&self) -> IoSnapshot {
        self.stats.as_ref().map_or_else(IoSnapshot::default, |s| IoSnapshot {
            calls: s.calls.load(Ordering::Relaxed),
            bytes: s.bytes.load(Ordering::Relaxed),
        })
    }

    pub fn clear_control(&mut self) {
        self.control = None;
    }

    pub(crate) fn cancel_operation(&self) {
        if let Some(control) = &self.control {
            control.cancel();
        }
    }

    #[inline]
    pub fn checkpoint(&self) -> io::Result<()> {
        match &self.control {
            Some(c) => c.checkpoint(),
            None => Ok(()),
        }
    }

    /// Start an overlapped read of `len` bytes at `off` into `dst`.
    ///
    /// # Safety
    /// `dst..dst+len` must stay valid and unaliased until [`Source::finish_read`] returns
    /// for this slot.
    pub unsafe fn start_read(&self, off: u64, dst: *mut u8, len: u32, slot: &mut ReadSlot) -> io::Result<()> {
        self.checkpoint()?;
        if off.checked_add(len as u64).is_none_or(|end| end > self.size) {
            return Err(crate::error::Error::new("read range is outside the source").into_io(io::ErrorKind::UnexpectedEof));
        }
        // SAFETY: zeroed OVERLAPPED is valid; fields set below.
        *slot.ov = unsafe { std::mem::zeroed() };
        slot.ov.hEvent = slot.event;
        slot.ov.Anonymous.Anonymous.Offset = off as u32;
        slot.ov.Anonymous.Anonymous.OffsetHigh = (off >> 32) as u32;
        slot.expected = len;
        if let Some(stats) = &self.stats {
            stats.calls.fetch_add(1, Ordering::Relaxed);
            stats.bytes.fetch_add(len as u64, Ordering::Relaxed);
        }
        // SAFETY: caller guarantees the destination; OVERLAPPED is heap-pinned in the slot.
        let ok = unsafe { ReadFile(self.handle, dst, len, ptr::null_mut(), &mut *slot.ov) };
        if ok == 0 {
            let e = unsafe { GetLastError() };
            if e != ERROR_IO_PENDING {
                return Err(io::Error::from_raw_os_error(e as i32));
            }
        }
        Ok(())
    }

    /// Wait for a read started with [`Source::start_read`]. Short reads are errors.
    pub fn finish_read(&self, slot: &mut ReadSlot) -> io::Result<u32> {
        let n = self.wait(slot)?;
        if n != slot.expected {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                crate::error::Error::Message("short read: {} of {} bytes", vec![n.to_string(), slot.expected.to_string()]),
            ));
        }
        Ok(n)
    }

    fn wait(&self, slot: &ReadSlot) -> io::Result<u32> {
        let mut n = 0;
        loop {
            // SAFETY: the request and its destination outlive this wait, including cancellation.
            let ok = unsafe {
                if let Some(control) = &self.control {
                    if control.is_cancelled() {
                        CancelIoEx(self.handle, &*slot.ov);
                        GetOverlappedResult(self.handle, &*slot.ov, &mut n, 1)
                    } else {
                        GetOverlappedResultEx(self.handle, &*slot.ov, &mut n, 100, 0)
                    }
                } else {
                    GetOverlappedResult(self.handle, &*slot.ov, &mut n, 1)
                }
            };
            if ok != 0 {
                return Ok(n);
            }
            let error = io::Error::last_os_error();
            if self.control.is_none() || error.raw_os_error() != Some(windows_sys::Win32::Foundation::WAIT_TIMEOUT as i32) {
                return Err(error);
            }
        }
    }

    pub(crate) fn cancel_read(&self, slot: &mut ReadSlot) {
        // SAFETY: the caller retains both the request and its destination through this wait.
        unsafe {
            CancelIoEx(self.handle, &*slot.ov);
            let mut n = 0;
            GetOverlappedResult(self.handle, &*slot.ov, &mut n, 1);
        }
    }

    /// Synchronous `DeviceIoControl` on the source handle. Returns the output length.
    pub fn ioctl(&self, slot: &mut ReadSlot, code: u32, input: &[u8], output: &mut [u8]) -> io::Result<u32> {
        self.checkpoint()?;
        // SAFETY: both buffers outlive the request, which has completed before returning.
        unsafe {
            *slot.ov = std::mem::zeroed();
            slot.ov.hEvent = slot.event;
            let ok = DeviceIoControl(
                self.handle,
                code,
                if input.is_empty() { ptr::null() } else { input.as_ptr().cast() },
                input.len() as u32,
                if output.is_empty() { ptr::null_mut() } else { output.as_mut_ptr().cast() },
                output.len() as u32,
                ptr::null_mut(),
                &mut *slot.ov,
            );
            if ok == 0 {
                let e = GetLastError();
                if e != ERROR_IO_PENDING {
                    return Err(io::Error::from_raw_os_error(e as i32));
                }
            }
            self.wait(slot)
        }
    }

    /// Synchronous read. For a device `off` and `buf.len()` must be multiples of the sector
    /// size and `buf` should come from an [`AlignedBuf`].
    pub fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut slot = ReadSlot::new()?;
        self.read_with(&mut slot, off, buf)
    }

    /// [`Source::read_at`] with a caller-provided slot (no event per call).
    pub fn read_with(&self, slot: &mut ReadSlot, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let len = (buf.len() - done).min(64 << 20) as u32;
            // SAFETY: buf outlives the wait right below.
            unsafe { self.start_read(off + done as u64, buf.as_mut_ptr().add(done), len, slot)? };
            self.finish_read(slot)?;
            done += len as usize;
        }
        Ok(())
    }

    /// Read that survives bad sectors: a failing range is retried in smaller pieces down to
    /// single sectors, and sectors that still fail are zero-filled. Returns the number of
    /// bytes that could not be read. Ranges past the end of the source count as unreadable.
    pub fn read_robust(&self, slot: &mut ReadSlot, off: u64, buf: &mut [u8]) -> u64 {
        self.read_robust_impl(slot, off, buf, false)
    }

    /// Continue sector recovery after an already completed, failed whole-buffer request.
    pub(crate) fn retry_read(&self, slot: &mut ReadSlot, off: u64, buf: &mut [u8]) -> u64 {
        self.read_robust_impl(slot, off, buf, true)
    }

    fn read_robust_impl(&self, slot: &mut ReadSlot, off: u64, buf: &mut [u8], failed: bool) -> u64 {
        if self.checkpoint().is_err() {
            buf.fill(0);
            return buf.len() as u64;
        }
        let end = self.size.saturating_sub(off).min(buf.len() as u64) as usize;
        let mut bad = (buf.len() - end) as u64;
        buf[end..].fill(0);
        if end == 0 || !failed && self.read_with(slot, off, &mut buf[..end]).is_ok() {
            return bad;
        }
        let sector = self.sector as usize;
        let block = (64usize << 10).max(sector);
        for (i, piece) in buf[..end].chunks_mut(block).enumerate() {
            if self.checkpoint().is_err() {
                return buf.len() as u64;
            }
            let at = off + (i * block) as u64;
            if self.read_with(slot, at, piece).is_ok() {
                continue;
            }
            for (j, s) in piece.chunks_mut(sector).enumerate() {
                if self.read_with(slot, at + (j * sector) as u64, s).is_err() {
                    s.fill(0);
                    bad += s.len() as u64;
                }
            }
        }
        bad
    }

    /// Read `len` bytes at any offset (aligned internally).
    pub fn read_bytes(&self, off: u64, len: usize) -> io::Result<Vec<u8>> {
        if off.checked_add(len as u64).is_none_or(|end| end > self.size) {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, crate::error::Error::new("read range is outside the source")));
        }
        if len == 0 {
            return Ok(Vec::new());
        }
        if !self.is_device() {
            let mut v = vec![0u8; len];
            self.read_at(off, &mut v)?;
            return Ok(v);
        }
        let a = self.sector as u64;
        let start = off / a * a;
        let end = (off + len as u64).div_ceil(a) * a;
        let mut buf = self.buffer((end - start) as usize);
        self.read_at(start, &mut buf)?;
        let s = (off - start) as usize;
        Ok(buf[s..s + len].to_vec())
    }

    /// Allocate a reusable buffer with the source's reported physical alignment.
    /// Transfer offsets and lengths still use logical sectors, including retries.
    pub fn buffer(&self, len: usize) -> AlignedBuf {
        AlignedBuf::with_alignment(len, self.buffer_alignment)
    }
}

fn access_alignment(bytes: &[u8]) -> Option<(u32, usize)> {
    let required = size_of::<STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR>();
    if bytes.len() < required {
        return None;
    }
    let field = |at| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    if field(0) < required as u32 || field(4) < required as u32 {
        return None;
    }
    let logical = field(16);
    let physical = field(20);
    if !(512..=4096).contains(&logical) || !logical.is_power_of_two() {
        return None;
    }
    let alignment =
        if physical.is_power_of_two() && (logical..=1 << 20).contains(&physical) { (physical as usize).max(IO_ALIGN) } else { IO_ALIGN };
    Some((logical, alignment))
}

pub(crate) fn file_key(handle: HANDLE) -> io::Result<(u32, u64)> {
    let mut info: windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle(handle, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((info.dwVolumeSerialNumber, ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64))
}

impl Drop for Source {
    fn drop(&mut self) {
        // SAFETY: we own the handle.
        unsafe { CloseHandle(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_alignment_keeps_logical_and_physical_sectors_separate() {
        let mut descriptor = [0u8; 28];
        descriptor[..4].copy_from_slice(&28u32.to_le_bytes());
        descriptor[4..8].copy_from_slice(&28u32.to_le_bytes());
        descriptor[16..20].copy_from_slice(&512u32.to_le_bytes());
        descriptor[20..24].copy_from_slice(&65536u32.to_le_bytes());
        assert_eq!(access_alignment(&descriptor), Some((512, 65536)));
        let buffer = AlignedBuf::with_alignment(513, 65536);
        assert_eq!(buffer.as_ptr() as usize % 65536, 0);
        assert!(buffer.iter().all(|&b| b == 0));
        assert_eq!(access_alignment(&descriptor[..24]), None);
        descriptor[20..24].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(access_alignment(&descriptor), Some((512, IO_ALIGN)));
        descriptor[16..20].copy_from_slice(&513u32.to_le_bytes());
        assert_eq!(access_alignment(&descriptor), None);
    }

    #[test]
    fn cloned_source_keeps_the_original_object_after_path_replacement() {
        let dir = std::env::temp_dir().join(format!("fr-clone-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("source.img");
        let moved = dir.join("moved.img");
        std::fs::write(&path, [1u8; 512]).unwrap();
        let source = Source::open(path.to_str().unwrap()).unwrap();
        std::fs::rename(&path, &moved).unwrap();
        std::fs::write(&path, [2u8; 512]).unwrap();
        let cloned = source.try_clone().unwrap();
        assert_eq!(cloned.read_bytes(0, 512).unwrap(), [1u8; 512]);
        assert_eq!(std::fs::read(&path).unwrap(), [2u8; 512]);
        drop(source);
        drop(cloned);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn spec_forms() {
        assert_eq!(parse_drive_letter("c"), Some('C'));
        assert_eq!(parse_drive_letter(r"\\.\D:"), Some('D'));
        assert_eq!(parse_drive_letter(r"C:\Windows"), None);
        assert_eq!(parse_disk_number("disk2"), Some(2));
        assert_eq!(parse_disk_number(r"\\.\PhysicalDrive0"), Some(0));
        assert_eq!(parse_disk_number("diskette"), None);
        assert_eq!(parse_shadow_number("shadow12"), Some(12));
        assert_eq!(parse_shadow_number(r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy3"), Some(3));
    }

    #[test]
    fn robust_read_of_an_image() {
        let dir = std::env::temp_dir().join(format!("fr-src-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.img");
        let data: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
        std::fs::write(&p, &data).unwrap();
        let src = Source::open(p.to_str().unwrap()).unwrap();
        assert_eq!(src.size, 10_000);
        assert_eq!(src.read_bytes(1000, 300).unwrap(), data[1000..1300]);
        let mut slot = ReadSlot::new().unwrap();
        let mut buf = vec![0u8; 4096];
        // 9728..13824: the last 3824 bytes lie past the end.
        let bad = src.read_robust(&mut slot, 9728, &mut buf);
        assert_eq!(bad, 4096 - 272);
        assert_eq!(&buf[..272], &data[9728..]);
        drop(src);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
