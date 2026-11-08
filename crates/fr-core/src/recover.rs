//! Writing found files to a target folder. The target must not lie on the scanned volume:
//! every byte written there could land on the clusters of a file still to be recovered.

use std::collections::HashSet;
use std::fs::{File, FileTimes};
use std::io::Write;
use std::os::windows::fs::FileTimesExt;
use std::os::windows::io::FromRawHandle;
use std::path::{Path, PathBuf};
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetDiskFreeSpaceExW, GetVolumeNameForVolumeMountPointW,
    GetVolumePathNameW, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::data::Reader;
use crate::error::Error;
use crate::model::{Catalog, State, ef};
use crate::source::{Source, SourceKind};

const FILE_ATTRIBUTE_READONLY: u32 = 1;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn from_wide(b: &[u16]) -> String {
    String::from_utf16_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())])
}

/// Mount point that holds `path` (`D:\`, or a folder for a volume mounted there).
fn mount_point(path: &Path) -> Option<String> {
    let w = wide(&path.to_string_lossy());
    let mut out = [0u16; 1024];
    // SAFETY: valid input string and output buffer with its length.
    if unsafe { GetVolumePathNameW(w.as_ptr(), out.as_mut_ptr(), out.len() as u32) } == 0 {
        return None;
    }
    Some(from_wide(&out))
}

/// `\\?\Volume{...}\` of a mount point.
fn volume_guid(mount: &str) -> Option<String> {
    let w = wide(mount);
    let mut out = [0u16; 128];
    // SAFETY: as above.
    if unsafe { GetVolumeNameForVolumeMountPointW(w.as_ptr(), out.as_mut_ptr(), out.len() as u32) } == 0 {
        return None;
    }
    Some(from_wide(&out))
}

/// (disk number, byte offset, length) of every piece of a volume.
fn disk_extents(volume_device: &str) -> Vec<(u32, u64, u64)> {
    let w = wide(volume_device);
    // SAFETY: no access rights requested; the handle only serves the query.
    let h = unsafe {
        CreateFileW(w.as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, ptr::null(), OPEN_EXISTING, 0, ptr::null_mut())
    };
    if h == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    // A spanned volume may have any number of extents. Windows reports the required
    // count even when the first query's buffer is too short.
    let mut out = vec![0u64; 4];
    let result = loop {
        let mut n = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                h,
                IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
                ptr::null(),
                0,
                out.as_mut_ptr().cast(),
                (out.len() * 8) as u32,
                &mut n,
                ptr::null_mut(),
            )
        };
        if ok != 0 {
            // SAFETY: the returned length is bounded by the allocation before slicing.
            if n as usize > out.len() * 8 {
                break Vec::new();
            }
            let bytes = unsafe { std::slice::from_raw_parts(out.as_ptr().cast::<u8>(), n as usize) };
            break parse_extents(bytes).unwrap_or_default();
        }
        if unsafe { windows_sys::Win32::Foundation::GetLastError() } != windows_sys::Win32::Foundation::ERROR_MORE_DATA {
            break Vec::new();
        }
        let count = (out[0] & u32::MAX as u64) as usize;
        // Bound allocations for a malformed device response, rather than truncating it.
        if count == 0 || count > 1 << 20 || count * 3 < out.len() {
            break Vec::new();
        }
        out.resize(1 + count * 3, 0);
    };
    unsafe { CloseHandle(h) };
    result
}

fn parse_extents(bytes: &[u8]) -> Option<Vec<(u32, u64, u64)>> {
    let count = u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?) as usize;
    if count == 0 {
        return None;
    }
    let entries = bytes.get(8..8usize.checked_add(count.checked_mul(24)?)?)?;
    entries
        .as_chunks::<24>()
        .0
        .iter()
        .map(|e| {
            let disk = u32::from_le_bytes(e[..4].try_into().ok()?);
            let start = i64::from_le_bytes(e[8..16].try_into().ok()?);
            let len = i64::from_le_bytes(e[16..24].try_into().ok()?);
            if start < 0 || len <= 0 || start.checked_add(len).is_none() {
                return None;
            }
            Some((disk, start as u64, len as u64))
        })
        .collect()
}

/// Target folder after the safety checks.
#[derive(Debug)]
pub struct Target {
    pub dir: PathBuf,
    /// Free bytes on the target volume.
    pub free: u64,
    /// The target is on the same physical disk as the source (allowed, but slower and
    /// riskier if the disk is failing).
    pub same_disk: bool,
    // No-delete-share handles keep validated ancestors from being replaced by junctions.
    _pins: Vec<File>,
}

/// Pin every directory component and reject reparse points, including junctions.
/// Missing directories are created only during recovery, after volume validation.
fn pin_directories(path: &Path, create: bool) -> std::io::Result<Vec<File>> {
    use std::path::Component;
    use windows_sys::Win32::Storage::FileSystem::*;
    let mut current = PathBuf::new();
    let mut pins = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) => {
                current.push(component);
                continue;
            }
            Component::CurDir => continue,
            Component::ParentDir => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    crate::error::Error::new("parent path components are not allowed"),
                ));
            }
            _ => current.push(component),
        }
        if create && matches!(std::fs::symlink_metadata(&current),Err(e) if e.kind()==std::io::ErrorKind::NotFound) {
            match std::fs::create_dir(&current) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(std::io::Error::new(e.kind(), Error::from(e).context(current.display()))),
            }
        }
        let name = wide(&current.to_string_lossy());
        // SAFETY: OPEN_REPARSE_POINT inspects the component itself. Previously pinned
        // parents cannot be renamed while the next component is opened.
        let h = unsafe {
            CreateFileW(
                name.as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            let e = std::io::Error::last_os_error();
            if !create && e.kind() == std::io::ErrorKind::NotFound {
                break;
            }
            return Err(e);
        }
        let file = unsafe { File::from_raw_handle(h) };
        let mut tag: FILE_ATTRIBUTE_TAG_INFO = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            GetFileInformationByHandleEx(
                h,
                FileAttributeTagInfo,
                (&mut tag as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
                std::mem::size_of_val(&tag) as u32,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 || tag.FileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                Error::new("destination must contain ordinary directories only (no junctions or symbolic links)")
                    .context(current.display()),
            ));
        }
        pins.push(file);
    }
    Ok(pins)
}

/// Check that `to` can receive files recovered from `src` (scanned range `offset..+size`).
pub fn check_target(src: &Source, offset: u64, size: u64, to: &Path) -> Result<Target, Error> {
    let abs = std::path::absolute(to).map_err(|e| Error::from(e).context(to.display()))?;
    let pins = pin_directories(&abs, false)?;
    let mut existing = abs.as_path();
    while !existing.exists() {
        existing = existing.parent().ok_or_else(|| Error::new("no such drive").context(abs.display()))?;
    }
    let mount = mount_point(existing).ok_or_else(|| Error::new("cannot find its volume").context(abs.display()))?;
    let guid = volume_guid(&mount);
    let target_extents = guid.as_deref().map(|g| disk_extents(g.trim_end_matches('\\'))).unwrap_or_default();

    // Disk ranges being recovered from.
    let source_extents: Vec<(u32, u64, u64)> = match &src.kind {
        SourceKind::Volume(l) => {
            let src_guid = volume_guid(&format!("{l}:\\"));
            if src_guid.is_some() && src_guid == guid {
                return Err(Error::value("destination is on the source volume {}; choose a folder on another drive", &src.name)
                    .context(abs.display()));
            }
            disk_extents(&format!(r"\\.\{l}:"))
        }
        SourceKind::Disk(n) => vec![(*n, offset, size)],
        SourceKind::Image(_) | SourceKind::Shadow(_) => Vec::new(),
    };
    if matches!(src.kind, SourceKind::Volume(_) | SourceKind::Disk(_)) && (source_extents.is_empty() || target_extents.is_empty()) {
        return Err("Cannot verify source and destination disk extents; choose a local destination on another drive".into());
    }
    let mut same_disk = false;
    for &(d, s, l) in &target_extents {
        for &(sd, ss, sl) in &source_extents {
            if d == sd {
                same_disk = true;
                if s < ss.saturating_add(sl) && ss < s.saturating_add(l) {
                    return Err(Error::new("destination lies inside the area being recovered from; choose a folder on another drive")
                        .context(abs.display()));
                }
            }
        }
    }
    let w = wide(&mount);
    let mut free = 0u64;
    // SAFETY: valid string, out pointers to locals.
    if unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut free, ptr::null_mut(), ptr::null_mut()) } == 0 {
        return Err(Error::from(std::io::Error::last_os_error()).context(mount));
    }
    Ok(Target { dir: abs, free, same_disk, _pins: pins })
}

/// A name that Windows accepts as a file name.
pub fn safe_name(name: &str) -> String {
    let mut s: String = name.chars().map(|c| if c < ' ' || r#"<>:"/\|?*"#.contains(c) { '_' } else { c }).collect();
    let trimmed = s.trim_end_matches(['.', ' ']).len();
    if trimmed < s.len() {
        let n = s.len() - trimmed;
        s.truncate(trimmed);
        s.extend(std::iter::repeat_n('_', n));
    }
    if s.is_empty() {
        s.push('_');
    }
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT")) && stem.len() == 4 && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        s.insert(0, '_');
    }
    s
}

/// `name (2).ext`, `name (3).ext`, ... until the path is free.
fn unique(dir: &Path, name: &str, taken: &mut HashSet<String>) -> PathBuf {
    let (stem, ext) = match name.rfind('.') {
        Some(d) if d > 0 => (&name[..d], &name[d..]),
        _ => (name, ""),
    };
    let mut k = 1;
    loop {
        let n = if k == 1 { name.to_string() } else { format!("{stem} ({k}){ext}") };
        let p = dir.join(&n);
        let key = p.to_string_lossy().to_lowercase();
        if !taken.contains(&key) && !p.exists() {
            taken.insert(key);
            return p;
        }
        k += 1;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    /// Written, but parts are zeros: unreadable, lost or (by the state) overwritten.
    Partial {
        bad: u64,
        missing: u64,
        overwritten: bool,
        corrupt_units: u32,
    },
    /// Saved, but every byte read was zero. This does not establish the cause.
    Zeroed,
    Skipped(&'static str),
    Failed(Error),
}

#[derive(Debug)]
pub struct FileResult {
    pub index: u32,
    pub path: PathBuf,
    pub bytes: u64,
    pub outcome: Outcome,
    /// Content can be saved even if the destination cannot preserve timestamps/attributes.
    pub metadata_error: Option<Error>,
}

#[derive(Clone, Debug)]
pub struct Options {
    /// Recreate the original folders under the target (otherwise all files side by side).
    pub structure: bool,
}

/// Recover entries `list` into `target`. `on_file` sees every result as it happens.
pub fn recover(src: &Source, cat: &Catalog, list: &[u32], target: &Target, opt: &Options, on_file: &mut dyn FnMut(&FileResult)) {
    let mut taken = HashSet::new();
    let mut reader = match Reader::new(src, cat) {
        Ok(r) => r,
        Err(e) => {
            let error = Error::from(e);
            for &i in list {
                on_file(&FileResult {
                    index: i,
                    path: PathBuf::new(),
                    bytes: 0,
                    outcome: Outcome::Failed(error.clone()),
                    metadata_error: None,
                });
            }
            return;
        }
    };
    for &i in list {
        if src.checkpoint().is_err() {
            break;
        }
        let e = &cat.entries[i as usize];
        let dir = if opt.structure { folder_of(cat, i, &target.dir) } else { target.dir.clone() };
        if e.is_dir() {
            let p = dir.join(safe_name(cat.name(i)));
            let outcome = match pin_directories(&p, true) {
                Ok(_) => Outcome::Ok,
                Err(err) => Outcome::Failed(err.into()),
            };
            on_file(&FileResult { index: i, path: p, bytes: 0, outcome, metadata_error: None });
            continue;
        }
        let skip = match e.state {
            _ if e.flags & ef::TRACE != 0 => Some("only the name survived"),
            State::Encrypted => Some("encrypted with EFS, cannot be decrypted"),
            State::NoData => Some("no data location survived"),
            _ => None,
        };
        if let Some(why) = skip {
            on_file(&FileResult { index: i, path: dir.join(cat.name(i)), bytes: 0, outcome: Outcome::Skipped(why), metadata_error: None });
            continue;
        }
        let _pins = match pin_directories(&dir, true) {
            Ok(p) => p,
            Err(err) => {
                on_file(&FileResult { index: i, path: dir, bytes: 0, outcome: Outcome::Failed(err.into()), metadata_error: None });
                continue;
            }
        };
        let path = unique(&dir, &safe_name(cat.name(i)), &mut taken);
        let mut metadata_error = None;
        let outcome = write_one(&mut reader, cat, i, &path, &mut metadata_error);
        let bytes = if matches!(outcome, Outcome::Ok | Outcome::Zeroed | Outcome::Partial { .. }) { e.size } else { 0 };
        on_file(&FileResult { index: i, path, bytes, outcome, metadata_error });
    }
}

/// Target folder for entry `i` with the original folders recreated (as shown by
/// [`Catalog::path`], so Recycle Bin items go back to where they were).
fn folder_of(cat: &Catalog, i: u32, root: &Path) -> PathBuf {
    let mut p = root.to_path_buf();
    let path = cat.path(i);
    let mut parts: Vec<&str> = path.split('\\').filter(|s| !s.is_empty()).collect();
    parts.pop();
    for part in parts {
        // Names in angle brackets are the program's own folders (`<Lost files>`).
        if part == crate::model::LOST_DIR {
            p.push("Lost files");
        } else {
            p.push(safe_name(part));
        }
    }
    p
}

fn write_one(reader: &mut Reader, cat: &Catalog, i: u32, path: &Path, metadata_error: &mut Option<Error>) -> Outcome {
    let e = cat.entries[i as usize];
    let mut f = match File::create_new(path) {
        Ok(f) => f,
        Err(err) => return Outcome::Failed(err.into()),
    };
    let st = match reader.read(i, &mut |b| f.write_all(b)) {
        Ok(st) => st,
        Err(err) => {
            drop(f);
            let _ = std::fs::remove_file(path);
            return Outcome::Failed(err.into());
        }
    };
    let mut times = FileTimes::new();
    if let Some(t) = crate::time::to_system(e.modified) {
        times = times.set_modified(t);
    }
    if let Some(t) = crate::time::to_system(e.accessed) {
        times = times.set_accessed(t);
    }
    if let Some(t) = crate::time::to_system(e.created) {
        times = times.set_created(t);
    }
    *metadata_error = f.set_times(times).err().map(Error::from);
    drop(f);
    if e.attrs & FILE_ATTRIBUTE_READONLY != 0 {
        let result = std::fs::metadata(path).and_then(|m| {
            let mut permissions = m.permissions();
            permissions.set_readonly(true);
            std::fs::set_permissions(path, permissions)
        });
        if metadata_error.is_none() {
            *metadata_error = result.err().map(Error::from);
        }
    }
    let uncertain = e.flags & (crate::ef::SIGNATURE | crate::ef::INCOMPLETE) == (crate::ef::SIGNATURE | crate::ef::INCOMPLETE);
    let overwritten = e.state == State::Overwritten || (e.state == State::Partial && !uncertain);
    if st.bad != 0 || st.missing != 0 || overwritten || st.corrupt_units != 0 || uncertain {
        Outcome::Partial { bad: st.bad, missing: st.missing, overwritten, corrupt_units: st.corrupt_units }
    } else if st.all_zero && e.size > 0 {
        Outcome::Zeroed
    } else {
        Outcome::Ok
    }
}

/// One-line description of an outcome.
pub fn describe(o: &Outcome) -> String {
    match o {
        Outcome::Ok => "ok".into(),
        Outcome::Partial { bad, missing, overwritten, corrupt_units } => {
            let mut v = Vec::new();
            if *overwritten {
                v.push("partly overwritten".to_string());
            }
            if *bad > 0 {
                v.push(format!("{} unreadable", crate::filter::format_size(*bad)));
            }
            if *missing > 0 {
                v.push(format!("{} lost", crate::filter::format_size(*missing)));
            }
            if *corrupt_units > 0 {
                v.push(format!("{corrupt_units} damaged compressed blocks"));
            }
            if v.is_empty() { "partial: original file boundaries unknown".into() } else { format!("partial: {}", v.join(", ")) }
        }
        Outcome::Zeroed => "saved; all bytes read were zero (cause unknown)".into(),
        Outcome::Skipped(why) => format!("skipped: {why}"),
        Outcome::Failed(e) => format!("failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_extents_are_complete_and_bounded() {
        let mut reply = vec![0u8; 8 + 24 * 19];
        reply[..4].copy_from_slice(&19u32.to_le_bytes());
        for i in 0..19 {
            let at = 8 + i * 24;
            reply[at..at + 4].copy_from_slice(&(i as u32).to_le_bytes());
            reply[at + 8..at + 16].copy_from_slice(&1024i64.to_le_bytes());
            reply[at + 16..at + 24].copy_from_slice(&2048i64.to_le_bytes());
        }
        assert_eq!(parse_extents(&reply).unwrap().len(), 19);
        assert!(parse_extents(&reply[..reply.len() - 1]).is_none());
        reply[16..24].copy_from_slice(&(-1i64).to_le_bytes());
        assert!(parse_extents(&reply).is_none());
        assert!(parse_extents(&[]).is_none());
    }

    #[test]
    fn recovery_keeps_valid_zero_filled_files() {
        use crate::model::{Data, Entry, FsInfo, NONE, Run};
        let dir = std::env::temp_dir().join(format!("fr-zero-recovery-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("source.img");
        let out = dir.join("zeros.bin");
        std::fs::write(&image, [0u8; 1024]).unwrap();
        let src = Source::open(image.to_str().unwrap()).unwrap();
        let fs = FsInfo {
            kind: crate::FsKind::Ntfs,
            offset: 0,
            size: 1024,
            cluster_size: 512,
            record_size: 1024,
            mft: vec![],
            serial: 0,
            root: 5,
            data_offset: 0,
        };
        let e = Entry { id: 42, parent: NONE, size: 1024, valid: 1024, data: Data::Runs { start: 0, count: 1 }, ..Default::default() };
        let cat = Catalog::new(fs, vec![e], vec![], vec![Run { lcn: 0, len: 2 }]);
        let mut reader = Reader::new(&src, &cat).unwrap();
        let mut metadata_error = None;
        assert_eq!(write_one(&mut reader, &cat, 0, &out, &mut metadata_error), Outcome::Zeroed);
        assert!(metadata_error.is_none());
        assert_eq!(std::fs::read(&out).unwrap(), [0u8; 1024]);
        drop(reader);
        drop(src);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn catalog_alias_cannot_redirect_output_to_an_absolute_path() {
        use crate::model::{Entry, FsInfo, NONE};
        let fs = FsInfo {
            kind: crate::FsKind::Ntfs,
            offset: 0,
            size: 1024,
            cluster_size: 512,
            record_size: 1024,
            mft: vec![],
            serial: 0,
            root: 5,
            data_offset: 0,
        };
        let mut cat = Catalog::new(fs, vec![Entry { parent: NONE, ..Default::default() }], vec![], vec![]);
        let root = Path::new(r"D:\recovered");
        for path in [r"\<C:/escape>\file.bin", r"\<../escape>\file.bin", r"\<C:>\file.bin"] {
            cat.alias.insert(0, path.into());
            assert!(folder_of(&cat, 0, root).starts_with(root), "{path}");
        }
    }

    #[test]
    fn names_windows_accepts() {
        assert_eq!(safe_name("a:b?.txt"), "a_b_.txt");
        assert_eq!(safe_name("trailing. "), "trailing__");
        assert_eq!(safe_name("con.txt"), "_con.txt");
        assert_eq!(safe_name("COM1"), "_COM1");
        assert_eq!(safe_name("COMMON.txt"), "COMMON.txt");
        assert_eq!(safe_name(""), "_");
    }

    #[test]
    fn unique_names() {
        let dir = std::env::temp_dir();
        let mut taken = HashSet::new();
        let a = unique(&dir, "fr-unique-test.txt", &mut taken);
        let b = unique(&dir, "fr-unique-test.txt", &mut taken);
        assert!(a.ends_with("fr-unique-test.txt"));
        assert!(b.ends_with("fr-unique-test (2).txt"));
    }
}
