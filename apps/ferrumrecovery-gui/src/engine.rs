//! Workers return owned results through a channel; window messages carry no pointers.
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, mpsc::Sender};

use fr_core::control::Control;
use fr_core::error::Error;
use fr_core::recover::{self, Outcome};
use fr_core::{Part, Source};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::app::{Loaded, SourceItem, SourceKind};

pub const WM_WORK: u32 = WM_APP + 1;

pub enum Event {
    Sources(Vec<SourceItem>, Vec<Error>),
    Parts(String, Vec<Part>, u64),
    Loaded(Box<Loaded>),
    Recovery { counts: [u64; 5], errors: Vec<Error>, metadata_errors: u64, stopped: bool },
    Preview(u64, u32, Result<crate::preview::Content, Error>),
    Saved,
    Imaged { copied: u64, bad: u64, remaining: u64, stopped: bool },
    Stopped,
    Error(Error),
}

pub fn worker(
    hwnd: HWND,
    tx: Sender<Event>,
    low: bool,
    failed: impl Fn(Error) -> Event + Clone + Send + 'static,
    f: impl FnOnce() -> Event + Send + 'static,
) {
    let h = hwnd as usize;
    let backup = tx.clone();
    let spawn_failed = failed.clone();
    let run = std::thread::Builder::new().name("recovery-worker".into()).spawn(move || {
        if low {
            // SAFETY: pseudo handle refers to this thread only.
            unsafe {
                use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
                SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
            }
        }
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|_| failed(Error::new("worker stopped unexpectedly")));
        if tx.send(result).is_ok() {
            // SAFETY: notification only; a closed window owns no worker allocation.
            unsafe {
                PostMessageW(h as HWND, WM_WORK, 0, 0);
            }
        }
    });
    if let Err(e) = run {
        let _ = backup.send(spawn_failed(e.into()));
        unsafe {
            PostMessageW(hwnd, WM_WORK, 0, 0);
        }
    }
}

pub fn sources(hwnd: HWND, tx: Sender<Event>) {
    worker(
        hwnd,
        tx,
        true,
        |e| Event::Sources(Vec::new(), vec![e]),
        || {
            use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
            let mask = unsafe { GetLogicalDrives() };
            let mut list = Vec::new();
            for k in 0..26 {
                if mask >> k & 1 == 0 {
                    continue;
                }
                let letter = (b'A' + k as u8) as char;
                let spec = format!("{letter}:");
                let root = crate::util::wide(&format!("{spec}\\"));
                if !matches!(unsafe { GetDriveTypeW(root.as_ptr()) }, 2 | 3 | 6) {
                    continue;
                }
                list.push(SourceItem { label: spec.clone(), size: None, spec, kind: SourceKind::Volume });
            }
            let mut errors = Vec::new();
            let disks = fr_core::source::physical_disks().unwrap_or_else(|e| {
                errors.push(e.into());
                Vec::new()
            });
            for n in disks {
                let spec = format!("disk{n}");
                if let Ok(src) = Source::open(&spec) {
                    list.push(SourceItem { label: spec.clone(), size: Some(src.size), spec, kind: SourceKind::Disk });
                }
            }
            let shadows = fr_core::devices::shadow_copies().unwrap_or_else(|e| {
                errors.push(e.into());
                Vec::new()
            });
            for n in shadows {
                let spec = format!("shadow{n}");
                if Source::open(&spec).is_ok() {
                    list.push(SourceItem { label: spec.clone(), size: None, spec, kind: SourceKind::Shadow });
                }
            }
            list.push(SourceItem { spec: String::new(), label: String::new(), size: None, kind: SourceKind::OpenImage });
            list.push(SourceItem { spec: String::new(), label: String::new(), size: None, kind: SourceKind::OpenSaved });
            Event::Sources(list, errors)
        },
    );
}

pub fn scan(
    spec: String,
    part: Option<Part>,
    mut req: fr_scan::Request,
    check: bool,
    progress: fr_scan::Progress,
    control: Arc<Control>,
) -> Event {
    let result = (|| -> Result<Event, Error> {
        if spec.to_ascii_lowercase().ends_with(".frs") {
            let (src, part, scanned) = fr_scan::load_session(std::path::Path::new(&spec))?;
            return Ok(Event::Loaded(Box::new(Loaded { spec: src_spec(&src), src, part, scanned })));
        }
        let mut src = fr_scan::open_source(&spec)?.with_control(control.clone());
        let part = if let Some(p) = part {
            p
        } else {
            let parts = fr_core::part::find(&src)?;
            let supported: Vec<_> = parts.into_iter().filter(|p| fr_scan::supported(p.fs)).collect();
            if supported.is_empty() && req.full {
                Part { index: 0, offset: 0, size: src.size, fs: fr_core::FsKind::Unknown, label: String::new() }
            } else {
                if supported.len() != 1 || req.full && supported[0].offset != 0 {
                    return Ok(Event::Parts(spec.clone(), supported, src.size));
                }
                supported[0].clone()
            }
        };
        let started = std::time::Instant::now();
        req.check = check && req.full;
        let mut scanned = fr_scan::scan(&src, &part, &req, &progress)?;
        if check && !scanned.checks_complete() {
            let all: Vec<u32> = (0..scanned.catalog.entries.len() as u32).collect();
            fr_core::check::check_progress(
                &src,
                &mut scanned.catalog,
                &all,
                if req.threads == Some(1) { 1 } else { 4 },
                Some(&progress.checks),
            );
        }
        control.checkpoint()?;
        scanned.took = started.elapsed();
        src.clear_control();
        Ok(Event::Loaded(Box::new(Loaded { spec, src, part, scanned })))
    })();
    if control.is_cancelled() { Event::Stopped } else { result.unwrap_or_else(Event::Error) }
}

fn src_spec(src: &Source) -> String {
    match &src.kind {
        fr_core::source::SourceKind::Image(p) => p.to_string_lossy().into_owned(),
        _ => src.name.clone(),
    }
}

pub fn recover_files(
    data: Arc<Loaded>,
    ids: Vec<u32>,
    path: PathBuf,
    control: Arc<Control>,
    done: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
) -> Event {
    let result = (|| -> Result<Event, Error> {
        let src = data.src.try_clone()?.with_control(control.clone());
        let target = recover::check_target(&src, data.part.offset, data.part.size, &path)?;
        let need = ids.iter().fold(0u64, |n, &i| n.saturating_add(data.cat().entries[i as usize].size));
        if need > target.free {
            return Err(Error::new("not enough free space on the target drive"));
        }
        let mut counts = [0; 5];
        let mut errors = Vec::new();
        let mut metadata_errors = 0;
        recover::recover(&src, data.cat(), &ids, &target, &recover::Options { structure: true }, &mut |r| {
            let k = match &r.outcome {
                Outcome::Ok => 0,
                Outcome::Partial { .. } => 1,
                Outcome::Zeroed => 2,
                Outcome::Skipped(_) => 3,
                Outcome::Failed(e) => {
                    if errors.len() < 8 {
                        errors.push(e.clone().context(data.cat().name(r.index)));
                    }
                    4
                }
            };
            if let Some(e) = &r.metadata_error {
                metadata_errors += 1;
                if errors.len() < 8 {
                    errors.push(
                        Error::cause("file data was saved but its metadata could not be restored", e.clone())
                            .context(data.cat().name(r.index)),
                    );
                }
            }
            counts[k] += 1;
            done.fetch_add(1, Relaxed);
            bytes.fetch_add(r.bytes, Relaxed);
        });
        Ok(Event::Recovery { counts, errors, metadata_errors, stopped: control.is_cancelled() })
    })();
    result.unwrap_or_else(Event::Error)
}

pub fn preview(data: Arc<Loaded>, i: u32, generation: u64, control: Arc<Control>) -> Event {
    let read = (|| {
        let src = data.src.try_clone()?.with_control(control);
        crate::preview::load(&src, data.cat(), i)
    })();
    Event::Preview(generation, i, read.map_err(Error::from))
}

pub fn save(data: Arc<Loaded>, path: PathBuf, csv: Option<Vec<u32>>) -> Event {
    let result = (|| -> Result<(), Error> {
        let parent = path.parent().ok_or("No destination folder")?;
        let _target = recover::check_target(&data.src, data.part.offset, data.part.size, parent)?;
        if let Some(ids) = csv {
            let file = std::fs::File::create_new(&path)?;
            let mut w = io::BufWriter::new(file);
            writeln!(w, "path,size,state,modified,deleted")?;
            for i in ids {
                let e = &data.cat().entries[i as usize];
                // Quote text and suppress spreadsheet formula interpretation.
                let p = data.cat().path(i).replace('"', "\"\"");
                writeln!(w, "\"'{}\",{},{},{},{}", p, e.size, e.state.name(), e.modified, e.deleted_at)?;
            }
            w.flush()?;
        } else {
            let boot = data.src.read_bytes(data.part.offset, 512)?;
            fr_core::session::save_catalog(&path, &data.spec, &data.part, &boot, &fr_scan::english(&data.scanned), data.cat())?;
        }
        Ok(())
    })();
    result.map(|()| Event::Saved).unwrap_or_else(|e| Event::Error(e.context(path.display())))
}

pub fn image(src: Source, path: PathBuf, control: Arc<Control>, progress: Arc<std::sync::Mutex<fr_core::image::Progress>>) -> Event {
    let len = src.size;
    let src = src.with_control(control.clone());
    let mut map_path = path.as_os_str().to_os_string();
    map_path.push(".map");
    let map_path = PathBuf::from(map_path);
    match fr_core::image::image(
        &src,
        0,
        len,
        &path,
        &map_path,
        &mut |p| {
            *progress.lock().unwrap_or_else(|e| e.into_inner()) = *p;
        },
        &|| control.is_cancelled(),
    ) {
        Ok(map) => {
            use fr_core::image::Status;
            Event::Imaged {
                copied: map.bytes(Status::Copied),
                bad: map.bytes(Status::Bad) + map.bytes(Status::Failed),
                remaining: map.bytes(Status::Untried),
                stopped: control.is_cancelled(),
            }
        }
        Err(_) if control.is_cancelled() => Event::Stopped,
        Err(e) => Event::Error(e.into()),
    }
}
