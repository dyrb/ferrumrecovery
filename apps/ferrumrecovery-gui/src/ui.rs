//! Layout and drawing for the native window.
use crate::app::{self, Busy, Layout};
use crate::lang::{self, S, t};
use crate::settings::ThemePref;
use crate::util::*;
use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub fn palette(pref: ThemePref) -> crate::theme::Palette {
    let dark = match pref {
        ThemePref::System => crate::theme::system_dark(),
        ThemePref::Dark => true,
        ThemePref::Light => false,
    };
    if dark { crate::theme::DARK } else { crate::theme::LIGHT }
}

pub fn font(dpi: u32, px: i32, weight: i32, mono: bool) -> HFONT {
    let face = wide(if mono { "Consolas" } else { "Segoe UI" });
    unsafe {
        CreateFontW(
            -px * dpi as i32 / 96,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            DEFAULT_PITCH as u32,
            face.as_ptr(),
        )
    }
}

/// FerrumTree's message font, with the same weight and small-text variants.
pub fn make_fonts(dpi: u32) -> [HFONT; 4] {
    unsafe {
        let mut m: NONCLIENTMETRICSW = std::mem::zeroed();
        m.cbSize = size_of::<NONCLIENTMETRICSW>() as u32;
        if windows_sys::Win32::UI::HiDpi::SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS,
            m.cbSize,
            (&mut m as *mut NONCLIENTMETRICSW).cast(),
            0,
            dpi,
        ) == 0
        {
            return [font(dpi, 12, 400, false), font(dpi, 12, 600, false), font(dpi, 11, 400, false), font(dpi, 12, 400, true)];
        }
        let mut lf = m.lfMessageFont;
        let normal = CreateFontIndirectW(&lf);
        lf.lfWeight = 600;
        let bold = CreateFontIndirectW(&lf);
        lf.lfWeight = 400;
        lf.lfHeight = lf.lfHeight * 9 / 10;
        [normal, bold, CreateFontIndirectW(&lf), font(dpi, 12, 400, true)]
    }
}

/// Geometry follows FerrumTree; recovery keeps its own three-pane content.
pub fn compute(dpi: u32, w: i32, h: i32, tree_share: f32, preview_share: f32, menus: [i32; 3]) -> Layout {
    let s = |v: i32| v * dpi as i32 / 96;
    let mut l = Layout::default();
    let mut x = s(6);
    for (i, width) in menus.into_iter().enumerate() {
        l.menus[i] = (rect(x, 0, x + width + s(20), s(26)), i);
        x += width + s(20);
    }
    l.top = rect(0, s(26), w, s(114));
    l.brand = rect(x + s(12), 0, w - s(14), s(26));
    l.source = rect(s(96), s(36), s(358), s(66));
    l.scan = rect(s(368), s(35), s(478), s(65));
    l.recover = rect(w - s(156), s(35), w - s(12), s(65));
    l.deep = rect(w - s(430), s(70), w - s(235), s(96));
    l.check = rect(w - s(225), s(70), w - s(12), s(96));
    l.status_line = rect(s(12), s(70), (w - s(450)).min(s(700)), s(94));
    l.progress = rect(s(12), s(102), w - s(12), s(106));
    l.filter_row = rect(s(12), s(122), w - s(12), s(150));
    let types_x = w - s(408);
    l.search = rect(s(12), s(122), types_x - s(10), s(150));
    l.types = rect(types_x, s(122), w - s(240), s(150));
    l.states = rect(w - s(230), s(122), w - s(12), s(150));
    l.statusbar = rect(0, h - s(24), w, h);
    let (top, bottom) = (s(186), l.statusbar.top - s(6));
    // Both splitters share these limits, so dragging cannot collapse the file pane.
    let available = (w - s(16 + 12)).max(0);
    let tree = (w as f32 * tree_share).round() as i32;
    let tree = tree.clamp(s(180), (available - s(320 + 200)).max(s(180)));
    let preview = (w as f32 * preview_share).round() as i32;
    let preview = preview.clamp(s(200), (available - tree - s(320)).max(s(200)));
    let right = w - s(8);
    let tx = s(8) + tree;
    let px = right - preview;
    l.folders = rect(s(8), top, tx, bottom);
    l.split1 = rect(tx, top, tx + s(6), bottom);
    l.files = rect(tx + s(6), top, px - s(6), bottom);
    l.split2 = rect(px - s(6), top, px, bottom);
    l.preview = rect(px, top, right, bottom);
    l.headings = [l.folders, l.files, l.preview].map(|r| rect(r.left, s(158), r.right, top));
    l
}

/// Menu labels are measured only when the font or language changes.
pub fn measure() {
    let Some((w, font)) = app::with(|a| (a.w, a.font)) else { return };
    let menus = unsafe {
        let dc = GetDC(w.main);
        let old = SelectObject(dc, font);
        let widths = [S::MenuFile, S::MenuView, S::MenuHelp].map(|s| text_width(dc, t(s)));
        SelectObject(dc, old);
        ReleaseDC(w.main, dc);
        widths
    };
    app::with(|a| a.menu_widths = menus);
}

pub fn control_metrics() {
    if let Some((w, dpi)) = app::with(|a| (a.w, a.dpi)) {
        unsafe {
            for (h, height) in [(w.source, 24), (w.types, 22), (w.states, 22)] {
                send(h, CB_SETITEMHEIGHT, usize::MAX, (height * dpi as i32 / 96) as isize);
            }
            send(w.search, EM_SETMARGINS, (EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize, 0);
            send(w.search, EM_SETLIMITTEXT, 4096, 0);
        }
    }
}

pub fn drag_splitter(dpi: u32, w: i32, l: &Layout, drag: app::Drag, x: i32) -> Option<(f32, f32)> {
    let s = |v| v * dpi as i32 / 96;
    let tree = l.folders.right - l.folders.left;
    let preview = l.preview.right - l.preview.left;
    let available = w - s(28);
    let (next_tree, next_preview) = match drag {
        app::Drag::Tree { offset } => ((x - offset - s(8)).clamp(s(180), (available - preview - s(320)).max(s(180))), preview),
        app::Drag::Preview { offset } => (tree, (w - s(8) - x + offset).clamp(s(200), (available - tree - s(320)).max(s(200)))),
    };
    if (tree, preview) == (next_tree, next_preview) {
        return None;
    }
    Some((next_tree as f32 / w as f32, next_preview as f32 / w as f32))
}

pub fn layout() {
    let Some((w, dpi, tree, preview, menus)) = app::with(|a| (a.w, a.dpi, a.settings.split_tree, a.settings.split_preview, a.menu_widths))
    else {
        return;
    };
    let c = client(w.main);
    let l = compute(dpi, c.right, c.bottom, tree, preview, menus);
    let inset = |r: RECT| rect(r.left + 1, r.top + 1, r.right - 1, r.bottom - 1);
    let Some(old) = app::with(|a| std::mem::replace(&mut a.layout, l)) else { return };
    let same = |a: RECT, b: RECT| (a.left, a.top, a.right, a.bottom) == (b.left, b.top, b.right, b.bottom);
    // The edit remains a native single-line control at its font's natural height.
    // Its surrounding field uses the same grid as the neighbouring combo boxes.
    let edit = |r: RECT| {
        let inset = 6 * dpi as i32 / 96;
        rect(r.left + inset, r.top + inset, r.right - inset, r.bottom - inset)
    };
    let moves = [
        (w.source, l.source, old.source),
        (w.scan, l.scan, old.scan),
        (w.recover, l.recover, old.recover),
        (w.deep, l.deep, old.deep),
        (w.check, l.check, old.check),
        (w.search, edit(l.search), edit(old.search)),
        (w.types, l.types, old.types),
        (w.states, l.states, old.states),
        (w.folders, inset(l.folders), inset(old.folders)),
        (w.files, inset(l.files), inset(old.files)),
        (w.preview, inset(l.preview), inset(old.preview)),
    ];
    if !moves.iter().any(|&(_, r, old)| !same(r, old)) {
        return;
    }
    unsafe {
        let mut batch = BeginDeferWindowPos(moves.len() as i32);
        for &(h, r, old) in &moves {
            if same(r, old) {
                continue;
            }
            let extra = if [w.source, w.types, w.states].contains(&h) { 300 * dpi as i32 / 96 } else { 0 };
            let flags = SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOCOPYBITS;
            if !batch.is_null() {
                batch = DeferWindowPos(
                    batch,
                    h,
                    std::ptr::null_mut(),
                    r.left,
                    r.top,
                    (r.right - r.left).max(0),
                    (r.bottom - r.top).max(0) + extra,
                    flags,
                );
            }
        }
        let moved = !batch.is_null() && EndDeferWindowPos(batch) != 0;
        if !moved {
            for &(h, r, old) in &moves {
                if same(r, old) {
                    continue;
                }
                let extra = if [w.source, w.types, w.states].contains(&h) { 300 * dpi as i32 / 96 } else { 0 };
                SetWindowPos(
                    h,
                    std::ptr::null_mut(),
                    r.left,
                    r.top,
                    (r.right - r.left).max(0),
                    (r.bottom - r.top).max(0) + extra,
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOCOPYBITS,
                );
            }
        }
        crate::list::size_folders();
        if !same(l.source, old.source) {
            send(w.source, CB_SETDROPPEDWIDTH, (600 * dpi as i32 / 96) as usize, 0);
        }
        InvalidateRect(w.main, std::ptr::null(), 0);
        UpdateWindow(w.main);
        for &(h, r, old) in &moves {
            if !same(r, old) {
                InvalidateRect(h, std::ptr::null(), 0);
                UpdateWindow(h);
            }
        }
    }
}

pub fn theme() {
    if let Some((w, p)) = app::with(|a| {
        a.pal = palette(a.settings.theme);
        unsafe {
            DeleteObject(a.brush_input);
            a.brush_input = CreateSolidBrush(a.pal.input);
        }
        (a.w, a.pal)
    }) {
        crate::theme::set_app_dark(p.dark);
        crate::theme::set_title_dark(w.main, p.dark);
        for h in [w.source, w.search, w.types, w.states] {
            crate::theme::style_control(h, p.dark, crate::theme::Kind::Cfd);
        }
        crate::list::theme();
        unsafe {
            RedrawWindow(w.main, std::ptr::null(), std::ptr::null_mut(), RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_ERASE);
        }
    }
}

pub fn controls() {
    if let Some((w, busy, cancel, loaded, full)) =
        app::with(|a| (a.w, a.busy.is_some(), a.control.is_some(), a.data.is_some(), a.settings.full))
    {
        unsafe {
            SetWindowTextW(w.scan, wide(t(if cancel { S::Stop } else { S::Scan })).as_ptr());
            SetWindowTextW(w.recover, wide(t(S::Recover)).as_ptr());
            SetWindowTextW(w.deep, wide(t(if full { S::Full } else { S::Deep })).as_ptr());
            SetWindowTextW(w.check, wide(t(S::Check)).as_ptr());
            EnableWindow(w.scan, (!busy || cancel) as i32);
            EnableWindow(w.recover, (!busy && loaded) as i32);
            for h in [w.source, w.deep, w.check, w.search, w.types, w.states] {
                EnableWindow(h, (!busy) as i32);
            }
            InvalidateRect(w.main, std::ptr::null(), 0);
        }
    }
}

pub fn paint(dc: HDC) {
    let Some(h) = app::with(|a| a.w.main) else { return };
    let c = client(h);
    if c.right <= 0 || c.bottom <= 0 {
        return;
    }
    buffered_paint(dc, &c, paint_parts);
}

fn paint_parts(dc: HDC) {
    app::with(|a| {
        let c = client(a.w.main);
        let s = |v| a.scale(v);
        fill(dc, &c, a.pal.bg);
        fill(dc, &a.layout.top, a.pal.panel);
        unsafe {
            SelectObject(dc, a.font);
        }
        for (r, i) in &a.layout.menus {
            text(dc, t([S::MenuFile, S::MenuView, S::MenuHelp][*i]), r, a.pal.text, DT_CENTER);
        }
        unsafe {
            SelectObject(dc, a.small);
        }
        text(dc, concat!("FerrumRecovery ", env!("CARGO_PKG_VERSION")), &a.layout.brand, a.pal.dim, DT_RIGHT);
        unsafe {
            SelectObject(dc, a.font);
        }
        text(dc, t(S::Source), &rect(s(12), a.layout.source.top, s(92), a.layout.source.bottom), a.pal.dim, 0);
        fill(dc, &a.layout.search, a.pal.input);
        frame(dc, &a.layout.search, a.pal.border);
        for (r, title) in a.layout.headings.iter().zip([S::Folders, S::Files, S::Preview]) {
            let label = rect(r.left + s(6), r.top, r.right - s(6), r.bottom);
            text(dc, t(title), &label, a.pal.text, 0);
            fill(dc, &rect(r.left, r.bottom - 1, r.right, r.bottom), a.pal.border);
        }
        let mut fraction = None;
        let status = match &a.busy {
            Some(Busy::Save) => t(S::Saving).to_owned(),
            Some(Busy::Scan { progress, started, spec }) => {
                let (phase, f) = progress.fraction();
                fraction = f;
                let label = t(match phase {
                    1 => {
                        if a.settings.full {
                            S::ReadingFull
                        } else {
                            S::ReadingFree
                        }
                    }
                    2 => S::Checking,
                    3 => S::ReadingLayouts,
                    4 => S::ReadingFormats,
                    5 => S::BuildingResults,
                    _ => S::Reading,
                });
                let percent = f.map(lang::percent).unwrap_or_default();
                let count = if phase == 4 {
                    use std::sync::atomic::Ordering::Relaxed;
                    format!(
                        "{} / {}",
                        lang::count(progress.analysis.done.load(Relaxed)),
                        lang::count(progress.analysis.total.load(Relaxed))
                    )
                } else {
                    lang::files(progress.found())
                };
                let mut status = format!("{label} {spec} {percent} ({count})");
                if let Some(f) = f
                    && phase <= 1
                    && f > 0.02
                    && f < 0.995
                    && started.elapsed().as_secs() > 3
                    && !a.control.as_ref().is_some_and(|c| c.is_paused())
                {
                    let left = started.elapsed().mul_f64((1.0 - f) / f);
                    status += &format!("    {}: {}", t(S::Eta), lang::seconds(left));
                }
                status
            }
            Some(Busy::Recover { done, total, bytes, started, .. }) => {
                use std::sync::atomic::Ordering::Relaxed;
                let count = done.load(Relaxed);
                fraction = Some(count as f64 / (*total).max(1) as f64);
                format!(
                    "{}: {} / {} ({})    {}",
                    t(S::Recovering),
                    count,
                    total,
                    lang::size(bytes.load(Relaxed)),
                    lang::seconds(started.elapsed())
                )
            }
            Some(Busy::Image { progress }) => {
                let p = *progress.lock().unwrap_or_else(|e| e.into_inner());
                fraction = Some(p.done as f64 / p.total.max(1) as f64);
                format!(
                    "{}: {} / {}    {}: {}",
                    t(S::CreateImage),
                    lang::size(p.copied),
                    lang::size(p.total),
                    t(S::Unreadable),
                    lang::size(p.bad)
                )
            }
            None => a.status.text(),
        };
        let status = if a.control.as_ref().is_some_and(|c| c.is_cancelled()) {
            t(S::Stopping).to_string()
        } else if a.control.as_ref().is_some_and(|c| c.is_paused()) {
            format!("{}: {status}", t(S::Paused))
        } else {
            status
        };
        text(dc, &status, &a.layout.status_line, a.pal.text, 0);
        fill(dc, &a.layout.progress, a.pal.border);
        if let Some(f) = fraction {
            let mut r = a.layout.progress;
            r.right = r.left + ((r.right - r.left) as f64 * f.clamp(0.0, 1.0)) as i32;
            fill(dc, &r, a.pal.accent);
        }
        let (n, b) = a.checked_summary();
        let foot =
            format!("{}: {}    {}: {} ({})", t(S::InList), lang::count(a.rows.len() as u64), t(S::Checked), lang::count(n), lang::size(b));
        unsafe {
            SelectObject(dc, a.small);
        }
        let sb = a.layout.statusbar;
        text(dc, &foot, &rect(sb.left + s(10), sb.top, sb.right - s(10), sb.bottom), a.pal.dim, 0);
        fill(dc, &rect(sb.left, sb.top, sb.right, sb.top + 1), a.pal.border);
        for gap in [a.layout.split1, a.layout.split2] {
            let x = (gap.left + gap.right) / 2;
            let y = (gap.top + gap.bottom) / 2;
            fill(dc, &rect(x, y - s(12), x + 1, y + s(12)), a.pal.border);
        }
        frame(dc, &a.layout.folders, a.pal.border);
        frame(dc, &a.layout.files, a.pal.border);
        frame(dc, &a.layout.preview, a.pal.border);
    });
}

pub unsafe fn button(p: isize) -> bool {
    unsafe {
        let d = &*(p as *const DRAWITEMSTRUCT);
        if d.CtlType != ODT_BUTTON {
            return false;
        }
        let saved = SaveDC(d.hDC);
        let drawn = app::with(|a| {
            let checked = if d.hwndItem == a.w.deep {
                Some(a.settings.deep)
            } else if d.hwndItem == a.w.check {
                Some(a.settings.check)
            } else {
                None
            };
            let disabled = d.itemState & ODS_DISABLED != 0;
            fill(
                d.hDC,
                &d.rcItem,
                if checked.is_some() {
                    a.pal.panel
                } else if d.itemState & ODS_SELECTED != 0 {
                    a.pal.sel
                } else {
                    a.pal.hover
                },
            );
            let mut r = d.rcItem;
            if let Some(checked) = checked {
                let h = a.scale(14);
                let top = r.top + (r.bottom - r.top - h) / 2;
                let mut boxr = rect(r.left + 3, top, r.left + 3 + h, top + h);
                DrawFrameControl(d.hDC, &mut boxr, DFC_BUTTON, DFCS_BUTTONCHECK | if checked { DFCS_CHECKED } else { 0 });
                r.left = boxr.right + a.scale(8);
            } else {
                let primary = d.hwndItem == a.w.scan && !disabled;
                let pressed = d.itemState & ODS_SELECTED != 0;
                let bg = if primary {
                    if pressed { 0x9A5A00 } else { a.pal.accent }
                } else if pressed {
                    a.pal.hover
                } else {
                    a.pal.input
                };
                fill(d.hDC, &r, a.pal.panel);
                let pen = CreatePen(PS_SOLID, 1, if primary { bg } else { a.pal.border });
                SetDCBrushColor(d.hDC, bg);
                let brush = SelectObject(d.hDC, GetStockObject(DC_BRUSH));
                let old_pen = SelectObject(d.hDC, pen);
                RoundRect(d.hDC, r.left, r.top, r.right, r.bottom, a.scale(8), a.scale(8));
                SelectObject(d.hDC, old_pen);
                SelectObject(d.hDC, brush);
                DeleteObject(pen);
            }
            let mut b = [0u16; 256];
            let n = GetWindowTextW(d.hwndItem, b.as_mut_ptr(), b.len() as i32);
            let primary = d.hwndItem == a.w.scan && !disabled;
            SelectObject(d.hDC, if primary { a.bold } else { a.font });
            text(
                d.hDC,
                &String::from_utf16_lossy(&b[..n as usize]),
                &r,
                if disabled {
                    a.pal.dim
                } else if primary {
                    0xFFFFFF
                } else {
                    a.pal.text
                },
                if checked.is_some() { 0 } else { DT_CENTER },
            );
            if d.itemState & ODS_FOCUS != 0 {
                r.left += 3;
                r.right -= 3;
                r.top += 3;
                r.bottom -= 3;
                DrawFocusRect(d.hDC, &r);
            }
        })
        .is_some();
        RestoreDC(d.hDC, saved);
        drawn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitters_keep_the_anchor_and_the_other_pane_width() {
        for dpi in [96, 120, 144, 192] {
            let s = |v| v * dpi as i32 / 96;
            let l = compute(dpi, s(1360), s(900), 0.2, 0.26, [0; 3]);
            for (drag, anchor) in
                [(app::Drag::Tree { offset: s(3) }, l.folders.right + s(3)), (app::Drag::Preview { offset: -s(3) }, l.preview.left - s(3))]
            {
                assert!(drag_splitter(dpi, s(1360), &l, drag, anchor).is_none(), "mousedown must not jump");
                let (tree, preview) = drag_splitter(dpi, s(1360), &l, drag, anchor + s(20)).unwrap();
                let moved = compute(dpi, s(1360), s(900), tree, preview, [0; 3]);
                match drag {
                    app::Drag::Tree { .. } => {
                        assert_eq!(moved.folders.right, l.folders.right + s(20));
                        assert_eq!(moved.preview.left, l.preview.left);
                    }
                    app::Drag::Preview { .. } => {
                        assert_eq!(moved.preview.left, l.preview.left + s(20));
                        assert_eq!(moved.folders.right, l.folders.right);
                    }
                }
            }
        }
    }

    #[test]
    fn panes_and_controls_fit_at_supported_sizes() {
        for dpi in [96, 120, 144, 192] {
            let s = |v| v * dpi as i32 / 96;
            for (w, h) in [(884, 521), (1360, 900), (1920, 1080)] {
                for (tree, preview) in [(0.2, 0.26), (0.6, 0.6), (0.08, 0.08)] {
                    let l = compute(dpi, s(w), s(h), tree, preview, [s(35), s(38), s(65)]);
                    for r in [l.source, l.scan, l.recover, l.deep, l.check, l.search, l.types, l.states, l.folders, l.files, l.preview] {
                        assert!(r.left >= 0 && r.top >= 0 && r.right <= s(w) && r.bottom <= s(h));
                        assert!(r.right > r.left && r.bottom > r.top);
                    }
                    assert_eq!(l.top.bottom, s(114));
                    assert_eq!(l.statusbar.bottom - l.statusbar.top, s(24));
                    assert_eq!(l.files.left - l.folders.right, s(6));
                    assert_eq!(l.preview.left - l.files.right, s(6));
                    assert!(l.files.right - l.files.left >= s(320));
                    assert!(l.scan.right < l.recover.left);
                    assert!(l.status_line.right < l.deep.left);
                    assert!(l.deep.right < l.check.left);
                    assert!(l.search.right < l.types.left && l.types.right < l.states.left);
                }
            }
        }
    }
}
