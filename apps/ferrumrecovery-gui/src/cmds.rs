use crate::app::{self, Busy, STATE_CHOICES, SourceKind, StateChoice, Status};
use crate::engine::{self, Event};
use crate::lang::{self, Lang, S, t};
use crate::settings::ThemePref;
use crate::util::*;
use std::path::PathBuf;
use std::sync::{Arc, atomic::AtomicU64};
use std::time::Instant;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::{ClientToScreen, InvalidateRect};
use windows_sys::Win32::UI::Controls::Dialogs::*;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub const OPEN: u16 = 300;
pub const OPEN_SAVED: u16 = 301;
pub const SAVE: u16 = 302;
pub const CSV: u16 = 303;
pub const RECOVER: u16 = 304;
pub const EXIT: u16 = 305;
pub const SCAN: u16 = 306;
pub const STOP: u16 = 307;
pub const CHECK_SELECTED: u16 = 310;
pub const UNCHECK_SELECTED: u16 = 311;
pub const CHECK_ALL: u16 = 312;
pub const UNCHECK_ALL: u16 = 313;
pub const EXISTING: u16 = 320;
pub const SYSTEM: u16 = 321;
pub const EMPTY: u16 = 322;
pub const TRACES: u16 = 323;
pub const STREAMS: u16 = 324;
pub const THEME: u16 = 330;
pub const LANGUAGE: u16 = 340;
pub const ABOUT: u16 = 350;
pub const PAUSE: u16 = 351;
pub const PRIORITY: u16 = 352;
pub const PREVIEW: u16 = 360;
pub const FILTERS: u16 = 370;
pub const IMAGE: u16 = 371;
pub const SCAN_MODE: u16 = 375;
pub const TIMER_PROGRESS: usize = 1;
pub const TIMER_FILTER: usize = 2;
pub const TIMER_PREVIEW: usize = 3;

pub fn message(text: &str, flags: u32) -> i32 {
    let hwnd = app::with(|a| a.w.main).unwrap_or(std::ptr::null_mut());
    unsafe { MessageBoxW(hwnd, wide(text).as_ptr(), wide("FerrumRecovery").as_ptr(), flags) }
}

pub fn dialog(save: bool, sessions: bool, csv: bool) -> Option<PathBuf> {
    let h = app::with(|a| a.w.main)?;
    let filter = if csv {
        "CSV (*.csv)\0*.csv\0\0".to_owned()
    } else if sessions {
        "FerrumRecovery (*.frs)\0*.frs\0\0".to_owned()
    } else {
        format!("{} (*.img;*.dd;*.raw;*.vhd)\0*.img;*.dd;*.raw;*.vhd\0{}\0*.*\0\0", t(S::DiskImages), t(S::AllFiles))
    };
    let filter: Vec<u16> = filter.encode_utf16().collect();
    let mut buf = vec![0u16; 32768];
    let ext = wide(if csv {
        "csv"
    } else if sessions {
        "frs"
    } else {
        "img"
    });
    unsafe {
        let mut o: OPENFILENAMEW = std::mem::zeroed();
        o.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
        o.hwndOwner = h;
        o.lpstrFilter = filter.as_ptr();
        o.lpstrFile = buf.as_mut_ptr();
        o.nMaxFile = buf.len() as u32;
        o.lpstrDefExt = ext.as_ptr();
        o.Flags = OFN_EXPLORER | OFN_NOCHANGEDIR | OFN_PATHMUSTEXIST | if save { 0 } else { OFN_FILEMUSTEXIST };
        let ok = if save { GetSaveFileNameW(&mut o) } else { GetOpenFileNameW(&mut o) };
        if ok == 0 {
            let code = CommDlgExtendedError();
            if code != 0 {
                message(&lang::error(&fr_core::error::Error::value("file dialog failed (code {})", code)), MB_OK | MB_ICONERROR);
            }
            return None;
        }
    }
    let n = buf.iter().position(|&x| x == 0).unwrap_or(buf.len());
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..n])))
}

fn folder() -> Option<PathBuf> {
    let h = app::with(|a| a.w.main)?;
    let title = wide(t(S::ChooseTarget));
    let mut name = [0u16; 260];
    unsafe {
        let mut b: BROWSEINFOW = std::mem::zeroed();
        b.hwndOwner = h;
        b.pszDisplayName = name.as_mut_ptr();
        b.lpszTitle = title.as_ptr();
        b.ulFlags = BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE;
        let pidl = SHBrowseForFolderW(&b);
        if pidl.is_null() {
            return None;
        }
        let mut path = vec![0u16; 32768];
        let ok = SHGetPathFromIDListEx(pidl, path.as_mut_ptr(), path.len() as u32, 0);
        windows_sys::Win32::System::Com::CoTaskMemFree(pidl.cast());
        if ok == 0 {
            return None;
        }
        let n = path.iter().position(|&x| x == 0).unwrap_or(0);
        Some(PathBuf::from(String::from_utf16_lossy(&path[..n])))
    }
}

pub fn start(spec: String, part: Option<fr_core::Part>) {
    let job = app::with(|a| {
        if a.busy.is_some() {
            return None;
        }
        a.selected_source = spec.clone();
        if !a.sources.iter().any(|s| s.spec == spec) {
            a.sources.push(app::SourceItem {
                spec: spec.clone(),
                label: spec.clone(),
                size: None,
                kind: if spec.to_ascii_lowercase().ends_with(".frs") { SourceKind::Saved } else { SourceKind::Image },
            });
        }
        let control = Arc::new(fr_core::control::Control::new(a.low_priority));
        let progress = fr_scan::Progress::default();
        let req = fr_scan::Request {
            existing: a.settings.existing,
            traces: a.settings.traces,
            streams: a.settings.streams,
            deep: a.settings.deep,
            full: a.settings.full,
            threads: if a.low_priority { Some(1) } else { None },
            ..Default::default()
        };
        a.busy = Some(Busy::Scan { progress: progress.clone(), started: Instant::now(), spec: spec.clone() });
        a.control = Some(control.clone());
        Some((a.w.main, a.sender.clone(), a.low_priority, req, a.settings.check, progress, control))
    })
    .flatten();
    if let Some((h, tx, low, req, check, progress, control)) = job {
        if let Some(combo) = app::with(|a| a.w.source) {
            fill_sources(combo);
        }
        crate::ui::controls();
        unsafe {
            SetTimer(h, TIMER_PROGRESS, 125, None);
        }
        engine::worker(h, tx, low, Event::Error, move || engine::scan(spec, part, req, check, progress, control));
    }
}

pub fn scan_choice() {
    if app::with(|a| a.busy.is_some()).unwrap_or(false) {
        stop();
        return;
    }
    let choice = app::with(|a| unsafe {
        let i = send(a.w.source, CB_GETCURSEL, 0, 0);
        a.sources.get(i as usize).cloned()
    })
    .flatten();
    let Some(choice) = choice else {
        return;
    };
    match choice.kind {
        SourceKind::OpenImage => open(false),
        SourceKind::OpenSaved => open(true),
        _ => start(choice.spec, None),
    }
}

fn open(saved: bool) {
    if let Some(p) = dialog(false, saved, false) {
        start(p.to_string_lossy().into_owned(), None);
    } else if let Some(h) = app::with(|a| a.w.source) {
        fill_sources(h);
    }
}

pub fn stop() {
    app::with(|a| {
        if let Some(c) = &a.control {
            c.cancel();
        }
    });
}

pub fn select_preview(row: usize) {
    if let Some((h, preview)) = app::with(|a| {
        a.preview_entry = a.rows.get(row).copied();
        a.preview_bytes.clear();
        a.preview_image = None;
        a.preview_error = None;
        a.preview_notice = None;
        if let Some(c) = &a.preview_cancel {
            c.cancel();
        }
        a.preview_generation = a.preview_generation.wrapping_add(1);
        (a.w.main, a.w.preview)
    }) {
        unsafe {
            InvalidateRect(preview, std::ptr::null(), 0);
            windows_sys::Win32::Graphics::Gdi::UpdateWindow(preview);
            SetTimer(h, TIMER_PREVIEW, 40, None);
        }
    }
}

pub fn load_preview() {
    let job = app::with(|a| {
        if a.preview_running {
            return None;
        }
        let (Some(data), Some(i)) = (a.data.clone(), a.preview_entry) else {
            return None;
        };
        a.preview_running = true;
        let control = Arc::new(fr_core::control::Control::new(true));
        a.preview_cancel = Some(control.clone());
        Some((a.w.main, a.sender.clone(), data, i, a.preview_generation, control))
    })
    .flatten();
    if let Some((h, tx, data, i, generation, control)) = job {
        engine::worker(h, tx, true, move |e| Event::Preview(generation, i, Err(e)), move || engine::preview(data, i, generation, control));
    }
}

pub fn check_selected(value: Option<bool>) {
    let selected = crate::list::selected();
    app::with(|a| {
        for r in &selected {
            if let Some(&i) = a.rows.get(*r) {
                a.checked[i as usize] = value.unwrap_or(!a.checked[i as usize]);
            }
        }
        a.recount_checked();
    });
    crate::list::refresh();
}

fn recover() {
    let selected = crate::list::selected();
    let job = app::with(|a| {
        if a.busy.is_some() {
            return None;
        }
        let data = a.data.clone()?;
        let ids = a.chosen(&selected);
        Some((data, ids))
    })
    .flatten();
    let Some((data, ids)) = job else {
        return;
    };
    if ids.is_empty() {
        message(t(S::NothingChosen), MB_OK | MB_ICONINFORMATION);
        return;
    }
    let Some(path) = folder() else {
        return;
    };
    let target = match fr_core::recover::check_target(&data.src, data.part.offset, data.part.size, &path) {
        Ok(t) => t,
        Err(e) => {
            message(&lang::error(&e), MB_OK | MB_ICONERROR);
            return;
        }
    };
    if target.same_disk && message(t(S::SameDisk), MB_YESNO | MB_ICONWARNING) != IDYES {
        return;
    }
    let control = Arc::new(fr_core::control::Control::new(app::with(|a| a.low_priority).unwrap_or(false)));
    let done = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    if let Some((h, tx, low)) = app::with(|a| {
        a.busy = Some(Busy::Recover { done: done.clone(), total: ids.len() as u64, bytes: bytes.clone(), started: Instant::now() });
        a.control = Some(control.clone());
        (a.w.main, a.sender.clone(), a.low_priority)
    }) {
        crate::ui::controls();
        unsafe {
            SetTimer(h, TIMER_PROGRESS, 125, None);
        }
        engine::worker(h, tx, low, Event::Error, move || engine::recover_files(data, ids, path, control, done, bytes));
    }
}

pub fn on_events() {
    let events = app::with(|a| a.events.try_iter().collect::<Vec<_>>()).unwrap_or_default();
    for e in events {
        match e {
            Event::Sources(sources, errors) => {
                let Some(h) = app::with(|a| {
                    let extra: Vec<_> = a
                        .sources
                        .iter()
                        .filter(|s| matches!(s.kind, SourceKind::Image | SourceKind::Saved) && !sources.iter().any(|x| x.spec == s.spec))
                        .cloned()
                        .collect();
                    a.sources = sources;
                    a.sources.extend(extra);
                    a.w.source
                }) else {
                    continue;
                };
                fill_sources(h);
                if !errors.is_empty() {
                    message(&errors.iter().map(lang::error).collect::<Vec<_>>().join("\n"), MB_OK | MB_ICONERROR);
                }
            }
            Event::Preview(g, i, r) => {
                let stale = app::with(|a| {
                    a.preview_running = false;
                    a.preview_cancel = None;
                    if a.preview_generation != g || a.preview_entry != Some(i) {
                        return true;
                    }
                    match r {
                        Ok(content) => {
                            a.preview_bytes = content.bytes;
                            a.preview_image = content.image;
                            a.preview_notice = content.notice;
                        }
                        Err(e) => a.preview_error = Some(e),
                    }
                    unsafe {
                        InvalidateRect(a.w.preview, std::ptr::null(), 0);
                    }
                    false
                })
                .unwrap_or(false);
                if stale {
                    load_preview();
                }
            }
            other => {
                let closing = app::with(|a| {
                    a.busy = None;
                    a.control = None;
                    unsafe {
                        KillTimer(a.w.main, TIMER_PROGRESS);
                    }
                    a.closing
                })
                .unwrap_or(true);
                if closing {
                    if let Some(h) = app::with(|a| a.w.main) {
                        unsafe {
                            DestroyWindow(h);
                        }
                    }
                    return;
                }
                match other {
                    Event::Parts(spec, parts, size) => choose_part(spec, parts, size),
                    Event::Loaded(d) => {
                        let warnings = d.scanned.warnings();
                        crate::list::clear_selection();
                        app::with(|a| {
                            a.preview_generation = a.preview_generation.wrapping_add(1);
                            a.preview_entry = None;
                            a.preview_bytes.clear();
                            a.preview_image = None;
                            a.checked = vec![false; d.cat().entries.len()];
                            a.checked_count = 0;
                            a.checked_bytes = 0;
                            a.folder = app::ALL;
                            a.expanded.clear();
                            a.expanded.insert(app::ALL);
                            a.status = Status::Scanned(d.scanned.took);
                            a.data = Some(Arc::from(d));
                            a.refilter();
                        });
                        crate::list::refresh();
                        if !warnings.is_empty() {
                            message(&warnings.iter().map(lang::error).collect::<Vec<_>>().join("\n"), MB_OK | MB_ICONWARNING);
                        }
                    }
                    Event::Recovery { counts, errors, metadata_errors, stopped } => {
                        let details = format!(
                            "{}{}",
                            if metadata_errors == 0 { String::new() } else { format!("\n{}: {}", t(S::MetadataWarnings), metadata_errors) },
                            if errors.is_empty() {
                                String::new()
                            } else {
                                format!("\n\n{}", errors.iter().map(lang::error).collect::<Vec<_>>().join("\n"))
                            }
                        );
                        let text = format!(
                            "{}\n\n{}: {}\n{}: {}\n{}: {}\n{}: {}\n{}: {}{}",
                            t(if stopped { S::Stopped } else { S::RecoverDone }),
                            t(S::RecoveredOk),
                            counts[0],
                            t(S::RecoveredPartly),
                            counts[1],
                            t(S::RecoveredErased),
                            counts[2],
                            t(S::RecoveredSkipped),
                            counts[3],
                            t(S::RecoveredFailed),
                            counts[4],
                            details
                        );
                        app::with(|a| a.status = Status::Message(if stopped { S::Stopped } else { S::RecoverDone }));
                        message(&text, MB_OK | MB_ICONINFORMATION);
                    }
                    Event::Saved => {
                        app::with(|a| a.status = Status::Message(S::Saved));
                    }
                    Event::Imaged { copied, bad, remaining, stopped } => {
                        let text = format!(
                            "{}\n\n{}: {}\n{}: {}\n{}: {}",
                            t(if stopped { S::Stopped } else { S::ImageDone }),
                            t(S::Size),
                            lang::size(copied),
                            t(S::Unreadable),
                            lang::size(bad),
                            t(S::Remaining),
                            lang::size(remaining)
                        );
                        app::with(|a| a.status = Status::Message(if stopped { S::Stopped } else { S::ImageDone }));
                        message(&text, MB_OK | MB_ICONINFORMATION);
                    }
                    Event::Stopped => {
                        app::with(|a| a.status = Status::Message(S::Stopped));
                    }
                    Event::Error(e) => {
                        message(&lang::error(&e), MB_OK | MB_ICONERROR);
                        app::with(|a| a.status = Status::Error(e));
                    }
                    _ => {}
                }
                crate::ui::controls();
            }
        }
    }
}

fn choose_part(spec: String, parts: Vec<fr_core::Part>, size: u64) {
    if parts.is_empty() {
        if message(t(S::RawWarning), MB_YESNO | MB_ICONQUESTION) == IDYES {
            start(spec, Some(fr_core::Part { index: 0, offset: 0, size, fs: fr_core::FsKind::Unknown, label: String::new() }));
        }
        return;
    }
    let h = app::with(|a| a.w.main).unwrap_or(std::ptr::null_mut());
    unsafe {
        let menu = CreatePopupMenu();
        let full = app::with(|a| a.settings.full).unwrap_or(false);
        if full {
            AppendMenuW(menu, MF_STRING, 1, wide(t(S::FullRange)).as_ptr());
        }
        for (i, p) in parts.iter().enumerate() {
            AppendMenuW(
                menu,
                MF_STRING,
                1000 + i,
                wide(&format!("{} {}: {} {} {}", t(S::Partition), p.index, p.fs.name(), lang::size(p.size), p.label)).as_ptr(),
            );
        }
        let mut point = POINT { x: 0, y: 80 };
        ClientToScreen(h, &mut point);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY, point.x, point.y, 0, h, std::ptr::null());
        DestroyMenu(menu);
        if cmd == 1 && full {
            start(spec, Some(fr_core::Part { index: 0, offset: 0, size, fs: fr_core::FsKind::Unknown, label: String::new() }));
            return;
        }
        if cmd >= 1000
            && let Some(part) = parts.get(cmd as usize - 1000)
        {
            start(spec, Some(part.clone()));
        }
    }
}

pub fn fill_sources(h: HWND) {
    let (labels, selected) = app::with(|a| {
        (
            a.sources.iter().map(|s| s.display()).collect::<Vec<_>>(),
            a.sources.iter().position(|s| !s.spec.is_empty() && s.spec == a.selected_source).unwrap_or(0),
        )
    })
    .unwrap_or_default();
    unsafe {
        send(h, CB_RESETCONTENT, 0, 0);
        for label in labels {
            send(h, CB_ADDSTRING, 0, wide(&label).as_ptr() as isize);
        }
        send(h, CB_SETCURSEL, selected, 0);
    }
}

pub fn filters() {
    crate::list::clear_selection();
    let Some(w) = app::with(|a| a.w) else {
        return;
    };
    let mut b = vec![0u16; 4096];
    let n = unsafe { GetWindowTextW(w.search, b.as_mut_ptr(), b.len() as i32) };
    let ty = unsafe { send(w.types, CB_GETCURSEL, 0, 0) };
    let st = unsafe { send(w.states, CB_GETCURSEL, 0, 0) };
    app::with(|a| {
        a.search = String::from_utf16_lossy(&b[..n as usize]);
        a.group = fr_core::types::Group::ALL.get(ty.saturating_sub(1) as usize).copied().filter(|_| ty > 0);
        a.state_choice = STATE_CHOICES.get(st as usize).copied().unwrap_or(StateChoice::Any);
        a.refilter();
    });
    crate::list::refresh();
}

pub fn language() {
    let Some((w, group, state)) = app::with(|a| (a.w, a.group, a.state_choice)) else {
        return;
    };
    let ty = group.and_then(|g| fr_core::types::Group::ALL.iter().position(|&v| v == g)).map_or(0, |i| i + 1);
    let st = STATE_CHOICES.iter().position(|&v| v == state).unwrap_or(0);
    unsafe {
        send(w.types, CB_RESETCONTENT, 0, 0);
        send(w.types, CB_ADDSTRING, 0, wide(t(S::TypeAll)).as_ptr() as isize);
        for g in fr_core::types::Group::ALL {
            send(w.types, CB_ADDSTRING, 0, wide(lang::group_name(g)).as_ptr() as isize);
        }
        send(w.types, CB_SETCURSEL, ty, 0);
        send(w.states, CB_RESETCONTENT, 0, 0);
        for st in STATE_CHOICES {
            let text = match st {
                StateChoice::Any => t(S::StateAny),
                StateChoice::Recoverable => t(S::StateRecoverable),
                StateChoice::One(s) => lang::state_name(s),
            };
            send(w.states, CB_ADDSTRING, 0, wide(text).as_ptr() as isize);
        }
        send(w.states, CB_SETCURSEL, st, 0);
        send(w.search, EM_SETCUEBANNER, 1, wide(t(S::SearchCue)).as_ptr() as isize);
        InvalidateRect(w.preview, std::ptr::null(), 0);
    }
    fill_sources(w.source);
    crate::list::columns(w.files);
    crate::list::folder_columns(w.folders);
    crate::ui::measure();
    crate::ui::layout();
    crate::ui::controls();
    filters();
}

unsafe fn item(menu: HMENU, id: u16, s: S, checked: bool) {
    unsafe {
        AppendMenuW(menu, MF_STRING | if checked { MF_CHECKED } else { 0 }, id as usize, wide(t(s)).as_ptr());
    }
}

pub fn menu(which: usize, x: i32, y: i32) {
    let Some((h, settings, paused, low)) =
        app::with(|a| (a.w.main, a.settings.clone(), a.control.as_ref().is_some_and(|c| c.is_paused()), a.low_priority))
    else {
        return;
    };
    unsafe {
        let m = CreatePopupMenu();
        match which {
            0 => {
                for (id, s) in [
                    (OPEN, S::OpenImage),
                    (OPEN_SAVED, S::OpenSaved),
                    (SAVE, S::SaveScan),
                    (CSV, S::ExportCsv),
                    (IMAGE, S::CreateImage),
                    (RECOVER, S::RecoverChecked),
                    (EXIT, S::Exit),
                ] {
                    item(m, id, s, false);
                }
            }
            1 => {
                item(m, FILTERS, S::Filters, false);
                item(m, PAUSE, if paused { S::Resume } else { S::Pause }, false);
                item(m, PRIORITY, S::LowPriority, low);
                let scan_mode = CreatePopupMenu();
                for (i, label) in [S::Quick, S::Deep, S::Full].into_iter().enumerate() {
                    let selected = if settings.full {
                        2
                    } else if settings.deep {
                        1
                    } else {
                        0
                    };
                    item(scan_mode, SCAN_MODE + i as u16, label, i == selected);
                    if app::with(|a| a.busy.is_some()).unwrap_or(false) {
                        EnableMenuItem(scan_mode, SCAN_MODE as u32 + i as u32, MF_BYCOMMAND | MF_GRAYED);
                    }
                }
                AppendMenuW(m, MF_POPUP, scan_mode as usize, wide(t(S::ScanMode)).as_ptr());
                for (id, s, b) in [
                    (EXISTING, S::ShowExisting, settings.existing),
                    (SYSTEM, S::ShowSystem, settings.system),
                    (EMPTY, S::ShowEmpty, settings.empty),
                    (TRACES, S::FindTraces, settings.traces),
                    (STREAMS, S::FindStreams, settings.streams),
                ] {
                    item(m, id, s, b);
                }
                let theme = CreatePopupMenu();
                for (i, s) in [S::ThemeSystem, S::ThemeLight, S::ThemeDark].into_iter().enumerate() {
                    item(
                        theme,
                        THEME + i as u16,
                        s,
                        i == match settings.theme {
                            ThemePref::System => 0,
                            ThemePref::Light => 1,
                            ThemePref::Dark => 2,
                        },
                    );
                }
                AppendMenuW(m, MF_POPUP, theme as usize, wide(t(S::ThemeMenu)).as_ptr());
                let languages = CreatePopupMenu();
                item(languages, LANGUAGE, S::LangSystem, settings.lang.is_none());
                for (i, l) in [Lang::En, Lang::Ru].into_iter().enumerate() {
                    AppendMenuW(
                        languages,
                        MF_STRING | if settings.lang == Some(l) { MF_CHECKED } else { 0 },
                        (LANGUAGE + 1 + i as u16) as usize,
                        wide(lang::native_name(l)).as_ptr(),
                    );
                }
                AppendMenuW(m, MF_POPUP, languages as usize, wide(t(S::LangMenu)).as_ptr());
                let preview = CreatePopupMenu();
                let mode = app::with(|a| a.preview_mode).unwrap_or(crate::preview::Mode::Image);
                for (i, (label, choice)) in [S::Image, S::Text, S::Hex]
                    .into_iter()
                    .zip([crate::preview::Mode::Image, crate::preview::Mode::Text, crate::preview::Mode::Hex])
                    .enumerate()
                {
                    item(preview, PREVIEW + i as u16, label, mode == choice);
                }
                AppendMenuW(m, MF_POPUP, preview as usize, wide(t(S::Preview)).as_ptr());
            }
            2 => item(m, ABOUT, S::About, false),
            _ => {
                for (id, s) in [
                    (CHECK_SELECTED, S::CheckSel),
                    (UNCHECK_SELECTED, S::UncheckSel),
                    (CHECK_ALL, S::CheckAll),
                    (UNCHECK_ALL, S::UncheckAll),
                    (RECOVER, S::RecoverSel),
                ] {
                    item(m, id, s, false);
                }
            }
        }
        if let Some((busy, cancel, loaded)) = app::with(|a| (a.busy.is_some(), a.control.is_some(), a.data.is_some())) {
            for id in [SAVE, CSV, RECOVER] {
                if busy || !loaded {
                    EnableMenuItem(m, id as u32, MF_BYCOMMAND | MF_GRAYED);
                }
            }
            if !cancel {
                EnableMenuItem(m, PAUSE as u32, MF_BYCOMMAND | MF_GRAYED);
            }
            if busy {
                for id in [
                    OPEN,
                    OPEN_SAVED,
                    IMAGE,
                    FILTERS,
                    PRIORITY,
                    EXISTING,
                    SYSTEM,
                    EMPTY,
                    TRACES,
                    STREAMS,
                    CHECK_SELECTED,
                    UNCHECK_SELECTED,
                    CHECK_ALL,
                    UNCHECK_ALL,
                ] {
                    EnableMenuItem(m, id as u32, MF_BYCOMMAND | MF_GRAYED);
                }
                if which == 1 {
                    let count = GetMenuItemCount(m);
                    for pos in count - 3..count {
                        EnableMenuItem(m, pos as u32, MF_BYPOSITION | MF_GRAYED);
                    }
                }
            }
        }
        let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_NONOTIFY, x, y, 0, h, std::ptr::null());
        DestroyMenu(m);
        if cmd != 0 {
            run(cmd as u16);
        }
    }
}

pub fn run(cmd: u16) {
    let busy = app::with(|a| a.busy.is_some()).unwrap_or(false);
    if busy && !matches!(cmd, STOP | SCAN | PAUSE | EXIT | ABOUT) {
        return;
    }
    if matches!(cmd, SAVE | CSV | RECOVER) && !app::with(|a| a.data.is_some()).unwrap_or(false) {
        return;
    }
    match cmd {
        SCAN_MODE..=377 => {
            app::with(|a| {
                a.settings.full = cmd == SCAN_MODE + 2;
                a.settings.deep = cmd != SCAN_MODE;
            });
            save_settings();
            crate::ui::controls();
        }
        IMAGE => {
            let spec = app::with(|a| unsafe {
                a.sources
                    .get(send(a.w.source, CB_GETCURSEL, 0, 0) as usize)
                    .filter(|s| !s.spec.is_empty() && s.kind != SourceKind::Saved)
                    .map(|s| s.spec.clone())
            })
            .flatten();
            let Some(spec) = spec else {
                message(t(S::ImagePickSource), MB_OK | MB_ICONINFORMATION);
                return;
            };
            let source = match fr_scan::open_source(&spec) {
                Ok(s) => s,
                Err(e) => {
                    message(&lang::error(&e), MB_OK | MB_ICONERROR);
                    return;
                }
            };
            let Some(path) = dialog(true, false, false) else {
                return;
            };
            let target = match fr_core::recover::check_target(&source, 0, source.size, path.parent().unwrap_or(std::path::Path::new("."))) {
                Ok(t) => t,
                Err(e) => {
                    message(&lang::error(&e), MB_OK | MB_ICONERROR);
                    return;
                }
            };
            if target.same_disk && message(t(S::SameDisk), MB_YESNO | MB_ICONWARNING) != IDYES {
                return;
            }
            let control = Arc::new(fr_core::control::Control::new(app::with(|a| a.low_priority).unwrap_or(false)));
            let progress = Arc::new(std::sync::Mutex::new(fr_core::image::Progress { total: source.size, ..Default::default() }));
            if let Some((h, tx, low)) = app::with(|a| {
                a.control = Some(control.clone());
                a.busy = Some(Busy::Image { progress: progress.clone() });
                (a.w.main, a.sender.clone(), a.low_priority)
            }) {
                crate::ui::controls();
                unsafe {
                    SetTimer(h, TIMER_PROGRESS, 125, None);
                }
                engine::worker(h, tx, low, Event::Error, move || engine::image(source, path, control, progress));
            }
        }
        FILTERS => {
            if let Some(current) = app::with(|a| a.advanced.clone())
                && let Some(next) = crate::filters::show(current)
            {
                crate::list::clear_selection();
                app::with(|a| {
                    a.advanced = next;
                    a.refilter();
                });
                crate::list::refresh();
            }
        }
        OPEN => open(false),
        OPEN_SAVED => open(true),
        SCAN => scan_choice(),
        STOP => stop(),
        RECOVER => recover(),
        EXIT => {
            if let Some(h) = app::with(|a| a.w.main) {
                unsafe {
                    PostMessageW(h, WM_CLOSE, 0, 0);
                }
            }
        }
        SAVE | CSV => {
            let Some(path) = dialog(true, cmd == SAVE, cmd == CSV) else {
                return;
            };
            if let Some((h, tx, data, ids)) =
                app::with(|a| Some((a.w.main, a.sender.clone(), a.data.clone()?, if cmd == CSV { Some(a.rows.clone()) } else { None })))
                    .flatten()
            {
                app::with(|a| a.busy = Some(Busy::Save));
                crate::ui::controls();
                engine::worker(h, tx, true, Event::Error, move || engine::save(data, path, ids));
            }
        }
        CHECK_SELECTED => check_selected(Some(true)),
        UNCHECK_SELECTED => check_selected(Some(false)),
        CHECK_ALL | UNCHECK_ALL => {
            app::with(|a| {
                if cmd == UNCHECK_ALL {
                    a.checked.fill(false);
                } else {
                    for &i in &a.rows {
                        a.checked[i as usize] = true;
                    }
                }
                a.recount_checked();
            });
            crate::list::refresh();
        }
        PAUSE => {
            app::with(|a| {
                if let Some(c) = &a.control {
                    c.set_paused(!c.is_paused());
                }
            });
        }
        PRIORITY => {
            app::with(|a| a.low_priority ^= true);
        }
        EXISTING | SYSTEM | EMPTY | TRACES | STREAMS => {
            crate::list::clear_selection();
            app::with(|a| {
                let v = match cmd {
                    EXISTING => &mut a.settings.existing,
                    SYSTEM => &mut a.settings.system,
                    EMPTY => &mut a.settings.empty,
                    TRACES => &mut a.settings.traces,
                    _ => &mut a.settings.streams,
                };
                *v ^= true;

                a.refilter();
            });
            save_settings();
            crate::list::refresh();
        }
        n if (THEME..THEME + 3).contains(&n) => {
            app::with(|a| {
                a.settings.theme = [ThemePref::System, ThemePref::Light, ThemePref::Dark][(n - THEME) as usize];
            });
            save_settings();
            crate::ui::theme();
        }
        n if (LANGUAGE..LANGUAGE + 3).contains(&n) => {
            app::with(|a| {
                a.settings.lang = [None, Some(Lang::En), Some(Lang::Ru)][(n - LANGUAGE) as usize];
                lang::set_lang(a.settings.lang);
            });
            language();
            save_settings();
        }
        n if (PREVIEW..PREVIEW + 3).contains(&n) => {
            app::with(|a| {
                a.preview_mode =
                    [crate::preview::Mode::Image, crate::preview::Mode::Text, crate::preview::Mode::Hex][(n - PREVIEW) as usize];
                unsafe {
                    InvalidateRect(a.w.preview, std::ptr::null(), 0);
                }
            });
        }
        ABOUT => {
            message(
                &format!("FerrumRecovery {}\n\n{}\n\n{}", env!("CARGO_PKG_VERSION"), t(S::AboutText), t(S::License)),
                MB_OK | MB_ICONINFORMATION,
            );
        }
        _ => {}
    }
}

pub fn save_settings() {
    let settings = app::with(|a| a.busy.is_none().then(|| a.settings.clone())).flatten();
    if let Some(settings) = settings
        && let Err(e) = settings.save()
    {
        message(&lang::error(&e), MB_OK | MB_ICONERROR);
    }
}
