//! Player window for the Windows app: the shared GPU player (`cinefs-player`, wgpu on DX12) draws
//! into a child window, and a transport bar is painted underneath with the app's own canvas.
//!
//! Timing: a small pacer thread waits for each desktop composition (`DwmFlush`, i.e. the display
//! refresh) and posts one tick message to the window, so frames are drawn on the UI thread in step
//! with the screen instead of on the coarse `WM_TIMER` grid. The picture follows the audio clock
//! when the clip has sound (see `cinefs_player::audio`).
//!
//! Keys (same as the Mac player): Space play/pause, ←/→ one frame (Shift: 10), Home/End, L loop, M mute.

use super::*;
use cinefs_core::vfs::{CineFs, MountedClip};
use cinefs_player::player::Player;
use cinefs_player::raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, Win32WindowHandle, WindowsDisplayHandle,
};
use std::cell::RefCell;
use std::num::NonZeroIsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[link(name = "user32", kind = "raw-dylib")]
extern "system" {
    fn PostMessageW(hWnd: *mut c_void, Msg: u32, wParam: usize, lParam: isize) -> i32;
    fn SetWindowLongPtrW(hWnd: *mut c_void, nIndex: i32, dwNewLong: isize) -> isize;
    fn GetWindowLongPtrW(hWnd: *mut c_void, nIndex: i32) -> isize;
    fn GetParent(hWnd: *mut c_void) -> *mut c_void;
    fn SetCapture(hWnd: *mut c_void) -> *mut c_void;
    fn ReleaseCapture() -> i32;
    fn GetCapture() -> *mut c_void;
    fn SetFocus(hWnd: *mut c_void) -> *mut c_void;
    fn GetKeyState(nVirtKey: i32) -> i16;
    fn MoveWindow(hWnd: *mut c_void, X: i32, Y: i32, nWidth: i32, nHeight: i32, bRepaint: i32) -> i32;
    fn AdjustWindowRectEx(lpRect: *mut Rect, dwStyle: u32, bMenu: i32, dwExStyle: u32) -> i32;
    fn IsIconic(hWnd: *mut c_void) -> i32;
}

#[link(name = "dwmapi", kind = "raw-dylib")]
extern "system" {
    fn DwmFlush() -> i32;
}

const WM_ACTIVATE: u32 = 0x0006;
const WM_KEYDOWN: u32 = 0x0100;
const WM_CAPTURECHANGED: u32 = 0x0215;
const WM_APP_TICK: u32 = 0x8000 + 17;
const WS_CHILD: u32 = 0x4000_0000;
const WS_CLIPSIBLINGS: u32 = 0x0400_0000;
const GWLP_USERDATA: i32 = -21;
const VK_SHIFT: i32 = 0x10;

const PLAYER_CLASS: &str = "CineFSPlayerWindowClass";
const VIDEO_CLASS: &str = "CineFSPlayerVideoClass";

/// Transport bar height at 96 dpi.
const BAR_H: i32 = 44;

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    Seek,
    Volume,
}

#[derive(Clone, Copy, PartialEq)]
enum Part {
    Play,
    Seek,
    Mute,
    Volume,
    Loop,
}

struct PlayerWin {
    hwnd: *mut c_void,
    video: *mut c_void,
    player: Player,
    dpi: i32,
    has_audio: bool,
    volume: f32,
    muted: bool,
    drag: Option<Drag>,
    hover: Option<Part>,
    parts: Vec<(Part, Rect)>,
    ticks: u32,
    tick_pending: Arc<AtomicBool>,
    pacer_stop: Arc<AtomicBool>,
    pacer: Option<JoinHandle<()>>,
}

thread_local! {
    /// Open player windows (UI thread only): top-level HWND + volume + clip index.
    static OPEN: RefCell<Vec<(usize, Arc<CineFs>, usize)>> = const { RefCell::new(Vec::new()) };
}

unsafe fn win_state<'a>(hwnd: *mut c_void) -> Option<&'a mut PlayerWin> {
    (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PlayerWin).as_mut()
}

fn lparam_xy(l: isize) -> (i32, i32) {
    ((l & 0xFFFF) as i16 as i32, ((l >> 16) & 0xFFFF) as i16 as i32)
}

fn timecode(frame: f64, fps: f64) -> String {
    let f = frame.round().max(0.0) as i64;
    let r = (fps.round() as i64).max(1);
    let s = f / r;
    format!("{:02}:{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60, f % r)
}

unsafe fn register_classes() {
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::SeqCst) {
        return;
    }
    let h_inst = GetModuleHandleW(ptr::null());
    let cursor = LoadCursorW(ptr::null_mut(), IDC_ARROW);
    let player_cls = wstr(PLAYER_CLASS);
    let video_cls = wstr(VIDEO_CLASS);
    let icon_big = load_embedded_icon(256);
    let icon_sm = load_embedded_icon(32);
    for (name, proc_, big, sm) in [
        (&player_cls, player_proc as unsafe extern "system" fn(*mut c_void, u32, usize, isize) -> isize, icon_big, icon_sm),
        (&video_cls, video_proc as unsafe extern "system" fn(*mut c_void, u32, usize, isize) -> isize, ptr::null_mut(), ptr::null_mut()),
    ] {
        let wc = WndClassExW {
            cb_size: size_of::<WndClassExW>() as u32,
            style: 0,
            lpfn_wnd_proc: proc_,
            cb_cls_extra: 0,
            cb_wnd_extra: 0,
            h_instance: h_inst,
            h_icon: big,
            h_cursor: cursor,
            hbr_background: ptr::null_mut(),
            lpsz_menu_name: ptr::null(),
            lpsz_class_name: name.as_ptr(),
            h_icon_sm: sm,
        };
        RegisterClassExW(&wc);
    }
}

/// Opens a player for clip `clip_index` of `vfs`, starting at `start_frame` (the frame the clip
/// preview was scrubbed to). Errors are returned for the status banner.
pub(super) unsafe fn open(vfs: Arc<CineFs>, clip_index: usize, title: &str, start_frame: usize) -> Result<(), String> {
    if let Some(existing) = OPEN.with(|o| {
        o.borrow()
            .iter()
            .find(|(_, v, c)| Arc::ptr_eq(v, &vfs) && *c == clip_index)
            .map(|(h, _, _)| *h)
    }) {
        const SW_RESTORE: i32 = 9;
        let hwnd = existing as *mut c_void;
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        SetForegroundWindow(hwnd);
        SetFocus(hwnd);
        return Ok(());
    }
    let clip: Arc<MountedClip> = vfs.clips.get(clip_index).cloned().ok_or("clip index out of range")?;
    register_classes();
    let h_inst = GetModuleHandleW(ptr::null());

    // Window size from the clip's display aspect, capped to a comfortable default.
    let aspect = (clip.clip.width as f64 / (clip.clip.height as f64 * clip.clip.stretch_y.max(0.01))).clamp(0.3, 4.0);
    let style = WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN;
    let title_w = wstr(title);
    let class_w = wstr(PLAYER_CLASS);
    let hwnd = CreateWindowExW(
        0,
        class_w.as_ptr(),
        title_w.as_ptr(),
        style,
        200,
        140,
        960,
        600,
        ptr::null_mut(),
        ptr::null_mut(),
        h_inst,
        ptr::null_mut(),
    );
    if hwnd.is_null() {
        return Err("could not create the player window".into());
    }
    apply_dark_titlebar(hwnd);
    let dpi = get_window_dpi(hwnd);
    let bar_h = scale_dpi(BAR_H, dpi);
    let video_w = scale_dpi(960, dpi);
    let video_h = ((video_w as f64 / aspect).round() as i32).clamp(scale_dpi(240, dpi), scale_dpi(720, dpi));
    let mut rc = Rect { left: 0, top: 0, right: video_w, bottom: video_h + bar_h };
    AdjustWindowRectEx(&mut rc, style, 0, 0);
    SetWindowPos(hwnd, ptr::null_mut(), 0, 0, rc.right - rc.left, rc.bottom - rc.top, 0x0002 | 0x0004); // NOMOVE | NOZORDER

    let video_cls = wstr(VIDEO_CLASS);
    let video = CreateWindowExW(
        0,
        video_cls.as_ptr(),
        ptr::null(),
        WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
        0,
        0,
        video_w,
        video_h,
        hwnd,
        ptr::null_mut(),
        h_inst,
        ptr::null_mut(),
    );
    if video.is_null() {
        DestroyWindow(hwnd);
        return Err("could not create the video view".into());
    }

    let Some(handle) = NonZeroIsize::new(video as isize) else {
        DestroyWindow(hwnd);
        return Err("invalid video window".into());
    };
    let mut wh = Win32WindowHandle::new(handle);
    wh.hinstance = NonZeroIsize::new(h_inst as isize);
    let target = cinefs_player::wgpu::SurfaceTargetUnsafe::RawHandle {
        raw_display_handle: Some(RawDisplayHandle::Windows(WindowsDisplayHandle::new())),
        raw_window_handle: RawWindowHandle::Win32(wh),
    };
    let player = match Player::new(clip, target, video_w as u32, video_h as u32) {
        Ok(p) => p,
        Err(e) => {
            DestroyWindow(hwnd);
            return Err(format!("GPU player: {e}"));
        }
    };
    player.engine.set_looping(true);
    if start_frame > 0 {
        player.engine.seek(start_frame as f64);
    }

    let prefs = crate::settings::Settings::load(&settings_path());
    let has_audio = player.has_audio();
    let state = Box::new(PlayerWin {
        hwnd,
        video,
        player,
        dpi,
        has_audio,
        volume: prefs.player_volume,
        muted: prefs.player_muted,
        drag: None,
        hover: None,
        parts: Vec::new(),
        ticks: 0,
        tick_pending: Arc::new(AtomicBool::new(false)),
        pacer_stop: Arc::new(AtomicBool::new(false)),
        pacer: None,
    });
    let st = Box::into_raw(state);
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, st as isize);
    let st = &mut *st;
    apply_volume(st);
    st.player.engine.play();
    st.pacer = Some(spawn_pacer(hwnd, st.tick_pending.clone(), st.pacer_stop.clone()));

    OPEN.with(|o| o.borrow_mut().push((hwnd as usize, vfs, clip_index)));
    ShowWindow(hwnd, SW_SHOW);
    SetForegroundWindow(hwnd);
    SetFocus(hwnd);
    cinefs_core::log_info!("player window opened: '{title}' (audio: {has_audio})");
    Ok(())
}

/// Closes the players that read from `vfs` (before it is unmounted).
pub(super) unsafe fn close_for(vfs: &Arc<CineFs>) {
    let hwnds: Vec<usize> = OPEN.with(|o| {
        o.borrow().iter().filter(|(_, v, _)| Arc::ptr_eq(v, vfs)).map(|(h, _, _)| *h).collect()
    });
    for h in hwnds {
        DestroyWindow(h as *mut c_void);
    }
}

pub(super) unsafe fn close_all() {
    let hwnds: Vec<usize> = OPEN.with(|o| o.borrow().iter().map(|(h, _, _)| *h).collect());
    for h in hwnds {
        DestroyWindow(h as *mut c_void);
    }
}

/// Posts one tick per display refresh (never more than one queued).
fn spawn_pacer(hwnd: *mut c_void, pending: Arc<AtomicBool>, stop: Arc<AtomicBool>) -> JoinHandle<()> {
    let hwnd = hwnd as usize;
    std::thread::Builder::new()
        .name("cinefs-player-pacer".into())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let t = Instant::now();
                let hr = unsafe { DwmFlush() };
                // DwmFlush returns at once when composition is off / the window is minimised:
                // never spin, never tick faster than ~250 Hz.
                if hr != 0 || t.elapsed() < Duration::from_millis(2) {
                    std::thread::sleep(Duration::from_millis(4));
                }
                if !pending.swap(true, Ordering::AcqRel) {
                    unsafe { PostMessageW(hwnd as *mut c_void, WM_APP_TICK, 0, 0) };
                }
            }
        })
        .expect("spawn pacer thread")
}

fn apply_volume(st: &mut PlayerWin) {
    st.player.set_volume(if st.muted { 0.0 } else { st.volume });
}

fn save_volume(st: &PlayerWin) {
    let path = settings_path();
    let mut s = crate::settings::Settings::load(&path);
    s.player_volume = st.volume;
    s.player_muted = st.muted;
    if let Err(e) = s.save(&path) {
        cinefs_core::log_warn!("Could not save player volume: {e}");
    }
}

unsafe fn invalidate_bar(st: &PlayerWin) {
    let mut rc = Rect::default();
    GetClientRect(st.hwnd, &mut rc);
    rc.top = rc.bottom - scale_dpi(BAR_H, st.dpi);
    InvalidateRect(st.hwnd, &rc, 0);
}

unsafe fn layout_video(st: &mut PlayerWin) {
    let mut rc = Rect::default();
    GetClientRect(st.hwnd, &mut rc);
    let w = (rc.right - rc.left).max(1);
    let h = (rc.bottom - rc.top - scale_dpi(BAR_H, st.dpi)).max(1);
    MoveWindow(st.video, 0, 0, w, h, 0);
    st.player.resize(w as u32, h as u32);
    st.player.redraw();
}

fn step(st: &mut PlayerWin, delta: i64) {
    let e = &st.player.engine;
    e.pause();
    let n = e.frame_count().max(1) as i64;
    let cur = e.position().round() as i64;
    e.seek((cur + delta).clamp(0, n - 1) as f64);
}

fn part_at(st: &PlayerWin, x: i32, y: i32) -> Option<Part> {
    st.parts
        .iter()
        .find(|(_, r)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
        .map(|(p, _)| *p)
}

fn part_rect(st: &PlayerWin, part: Part) -> Option<Rect> {
    st.parts.iter().find(|(p, _)| *p == part).map(|(_, r)| *r)
}

/// Fraction 0..1 of `x` across a slider rectangle (with the thumb radius as inset).
fn slider_fraction(r: Rect, x: i32, inset: i32) -> f64 {
    let left = r.left + inset;
    let width = (r.right - r.left - 2 * inset).max(1);
    ((x - left) as f64 / width as f64).clamp(0.0, 1.0)
}

unsafe fn drag_to(st: &mut PlayerWin, drag: Drag, x: i32) {
    let inset = scale_dpi(6, st.dpi);
    match drag {
        Drag::Seek => {
            if let Some(r) = part_rect(st, Part::Seek) {
                let n = st.player.engine.frame_count().max(1);
                let f = slider_fraction(r, x, inset) * (n - 1) as f64;
                st.player.engine.seek(f.round());
            }
        }
        Drag::Volume => {
            if let Some(r) = part_rect(st, Part::Volume) {
                st.volume = slider_fraction(r, x, inset) as f32;
                if st.muted && st.volume > 0.0 {
                    st.muted = false;
                }
                apply_volume(st);
            }
        }
    }
    invalidate_bar(st);
}

unsafe fn handle_key(st: &mut PlayerWin, vk: usize) -> bool {
    let shift = GetKeyState(VK_SHIFT) < 0;
    match vk {
        0x20 => st.player.engine.toggle(),                    // Space
        0x25 => step(st, if shift { -10 } else { -1 }),       // Left
        0x27 => step(st, if shift { 10 } else { 1 }),         // Right
        0x24 => st.player.engine.seek(0.0),                   // Home
        0x23 => {
            let n = st.player.engine.frame_count().max(1);
            st.player.engine.seek((n - 1) as f64);            // End
        }
        0x4C => {
            let on = !st.player.engine.is_looping();          // L
            st.player.engine.set_looping(on);
        }
        0x4D if st.has_audio => {
            st.muted = !st.muted;                             // M
            apply_volume(st);
            save_volume(st);
        }
        _ => return false,
    }
    invalidate_bar(st);
    true
}

unsafe fn click(st: &mut PlayerWin, x: i32, y: i32) {
    match part_at(st, x, y) {
        Some(Part::Play) => st.player.engine.toggle(),
        Some(Part::Loop) => {
            let on = !st.player.engine.is_looping();
            st.player.engine.set_looping(on);
        }
        Some(Part::Mute) => {
            st.muted = !st.muted;
            apply_volume(st);
            save_volume(st);
        }
        Some(Part::Seek) => {
            st.drag = Some(Drag::Seek);
            SetCapture(st.hwnd);
            drag_to(st, Drag::Seek, x);
        }
        Some(Part::Volume) => {
            st.drag = Some(Drag::Volume);
            SetCapture(st.hwnd);
            drag_to(st, Drag::Volume, x);
        }
        None => {}
    }
    invalidate_bar(st);
}

/// Paints the transport bar (canvas shapes + GDI text) and records the hit regions.
unsafe fn paint_bar(st: &mut PlayerWin, hdc_win: *mut c_void, client_w: i32, client_h: i32) {
    let dpi = st.dpi;
    let bar_h = scale_dpi(BAR_H, dpi);
    let w = client_w.max(1);
    let y0 = client_h - bar_h;
    if bar_h <= 0 {
        return;
    }
    let hdc_mem = CreateCompatibleDC(hdc_win);
    if hdc_mem.is_null() {
        return;
    }
    let bmi = BitmapInfo {
        bmi_header: BitmapInfoHeader {
            bi_size: size_of::<BitmapInfoHeader>() as u32,
            bi_width: w,
            bi_height: -bar_h,
            bi_planes: 1,
            bi_bit_count: 32,
            bi_compression: BI_RGB,
            bi_size_image: 0,
            bi_x_pels_per_meter: 0,
            bi_y_pels_per_meter: 0,
            bi_clr_used: 0,
            bi_clr_important: 0,
        },
        bmi_colors: [0],
    };
    let mut bits: *mut u32 = ptr::null_mut();
    let h_bm = CreateDIBSection(hdc_mem, &bmi, DIB_RGB_COLORS, &mut bits, ptr::null_mut(), 0);
    if h_bm.is_null() || bits.is_null() {
        DeleteDC(hdc_mem);
        return;
    }
    let old_bm = SelectObject(hdc_mem, h_bm);
    let mut canvas = Canvas {
        pixels: std::slice::from_raw_parts_mut(bits, (w * bar_h) as usize),
        width: w,
        height: bar_h,
        clip_top: 0,
        clip_bottom: bar_h,
    };
    canvas.clear(COL_FOOTER_BG);
    canvas.fill_rect(0, 0, w, 1, COL_DIVIDER);

    // Text is drawn after the shapes; icon glyphs come from Segoe MDL2 Assets (Windows 10+).
    let mut texts: Vec<BarText> = Vec::new();
    let mut parts: Vec<(Part, Rect)> = Vec::new();

    let e = &st.player.engine;
    let n = e.frame_count().max(1);
    let pos = e.position();
    let fps = e.fps();
    let playing = e.is_playing();
    let looping = e.is_looping();
    let dropped = st.player.stats().dropped;

    let pad = scale_dpi(10, dpi);
    let btn = scale_dpi(30, dpi);
    let hover = st.hover;
    let button = |canvas: &mut Canvas,
                      texts: &mut Vec<BarText>,
                      parts: &mut Vec<(Part, Rect)>,
                      part: Part,
                      x: i32,
                      glyph: char,
                      active: bool|
     -> Rect {
        bar_button(canvas, texts, parts, BarButton { part, x, glyph, active, hover: hover == Some(part) }, btn, bar_h, dpi)
    };

    // Left: play / pause.
    let play_r = button(&mut canvas, &mut texts, &mut parts, Part::Play, pad, if playing { '\u{E769}' } else { '\u{E768}' }, false);

    // Right side, laid out from the right edge.
    let mut right = w - pad;
    let loop_r = button(&mut canvas, &mut texts, &mut parts, Part::Loop, right - btn, '\u{E8EE}', looping);
    right = loop_r.left - scale_dpi(8, dpi);
    if st.has_audio {
        let vol_w = scale_dpi(80, dpi);
        let vol_r = Rect { left: right - vol_w, top: 0, right, bottom: bar_h };
        draw_slider(&mut canvas, vol_r, if st.muted { 0.0 } else { st.volume as f64 }, st.muted, dpi);
        parts.push((Part::Volume, vol_r));
        right = vol_r.left - scale_dpi(6, dpi);
        let silent = st.muted || st.volume == 0.0;
        let mute_r = button(&mut canvas, &mut texts, &mut parts, Part::Mute, right - btn, if silent { '\u{E74F}' } else { '\u{E767}' }, false);
        right = mute_r.left - scale_dpi(10, dpi);
    }

    let info_w = scale_dpi(if dropped > 0 { 300 } else { 220 }, dpi);
    let info_r = Rect { left: (right - info_w).max(play_r.right + pad), top: 0, right, bottom: bar_h };
    let mut info = format!("{}  {}/{}  {:.2} fps", timecode(pos, fps), pos.round() as usize + 1, n, fps);
    if dropped > 0 {
        info.push_str(&format!("  dropped {dropped}"));
    }
    texts.push((info, info_r, if dropped > 0 { COL_AMBER_TEXT } else { COL_TEXT_SECONDARY }, false, DT_RIGHT | DT_VCENTER | DT_SINGLELINE));

    // Middle: seek slider.
    let seek_r = Rect { left: play_r.right + pad, top: 0, right: info_r.left - pad, bottom: bar_h };
    if seek_r.right - seek_r.left > scale_dpi(40, dpi) {
        let frac = if n > 1 { pos / (n - 1) as f64 } else { 0.0 };
        draw_slider(&mut canvas, seek_r, frac, false, dpi);
        parts.push((Part::Seek, seek_r));
    }
    st.parts = parts;

    GdiFlush();
    let mk_font = |px: i32, weight: i32, face: &str| {
        let f = wstr(face);
        CreateFontW(-scale_dpi(px, dpi), 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, CLEARTYPE_QUALITY, 0, f.as_ptr())
    };
    let f_text = mk_font(12, 400, "Consolas");
    let f_icon = mk_font(14, 400, "Segoe MDL2 Assets");
    SetBkMode(hdc_mem, TRANSPARENT);
    for (text, rect, color, icon, flags) in &texts {
        let prev = SelectObject(hdc_mem, if *icon { f_icon } else { f_text });
        SetTextColor(hdc_mem, rgb_to_colorref(*color));
        let ws = wstr(text);
        let mut rc = *rect;
        DrawTextW(hdc_mem, ws.as_ptr(), (ws.len() - 1) as i32, &mut rc, flags | DT_NOPREFIX);
        SelectObject(hdc_mem, prev);
    }
    for f in [f_text, f_icon] {
        if !f.is_null() {
            DeleteObject(f);
        }
    }
    BitBlt(hdc_win, 0, y0, w, bar_h, hdc_mem, 0, 0, SRCCOPY);
    SelectObject(hdc_mem, old_bm);
    DeleteObject(h_bm);
    DeleteDC(hdc_mem);
}

/// Text to draw on the bar: string, rectangle, colour, icon font?, DrawText flags.
type BarText = (String, Rect, u32, bool, u32);

struct BarButton {
    part: Part,
    x: i32,
    glyph: char,
    active: bool,
    hover: bool,
}

fn bar_button(
    canvas: &mut Canvas,
    texts: &mut Vec<BarText>,
    parts: &mut Vec<(Part, Rect)>,
    b: BarButton,
    size: i32,
    bar_h: i32,
    dpi: i32,
) -> Rect {
    let top = (bar_h - size) / 2;
    let r = Rect { left: b.x, top, right: b.x + size, bottom: top + size };
    let bg = if b.active { COL_AMBER_BG } else if b.hover { COL_BTN_SEC_HOVER } else { COL_BTN_SEC };
    let bd = if b.active { COL_AMBER_BORDER } else { COL_BTN_SEC_BORDER };
    canvas.draw_rounded_rect(r.left, r.top, size, size, scale_dpi(6, dpi) as f32, bg, 255, Some((bd, 1.0)));
    let fg = if b.active { COL_AMBER_TEXT } else { COL_TEXT_PRIMARY };
    texts.push((b.glyph.to_string(), r, fg, true, DT_CENTER | DT_VCENTER | DT_SINGLELINE));
    parts.push((b.part, Rect { top: 0, bottom: bar_h, ..r }));
    r
}

fn draw_slider(canvas: &mut Canvas, r: Rect, frac: f64, disabled: bool, dpi: i32) {
    let inset = scale_dpi(6, dpi);
    let track_h = scale_dpi(4, dpi).max(2);
    let cy = (r.top + r.bottom) / 2;
    let x0 = r.left + inset;
    let width = (r.right - r.left - 2 * inset).max(1);
    let filled = (width as f64 * frac.clamp(0.0, 1.0)).round() as i32;
    let radius = track_h as f32 / 2.0;
    canvas.draw_rounded_rect(x0, cy - track_h / 2, width, track_h, radius, COL_BTN_SEC_BORDER, 255, None);
    let accent = if disabled { COL_TEXT_MUTED } else { COL_ACCENT };
    if filled > 0 {
        canvas.draw_rounded_rect(x0, cy - track_h / 2, filled, track_h, radius, accent, 255, None);
    }
    canvas.fill_circle((x0 + filled) as f32, cy as f32, scale_dpi(6, dpi) as f32, if disabled { COL_TEXT_MUTED } else { COL_TEXT_PRIMARY }, 255);
}

unsafe fn destroy_state(hwnd: *mut c_void) {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PlayerWin;
    if p.is_null() {
        return;
    }
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
    let mut st = Box::from_raw(p);
    st.pacer_stop.store(true, Ordering::Relaxed);
    if let Some(h) = st.pacer.take() {
        let _ = h.join();
    }
    OPEN.with(|o| o.borrow_mut().retain(|(h, _, _)| *h != hwnd as usize));
    drop(st); // stops audio + decoder threads, releases the GPU surface (video child still alive)
}

unsafe extern "system" fn player_proc(hwnd: *mut c_void, msg: u32, w_param: usize, l_param: isize) -> isize {
    catch_unwind(AssertUnwindSafe(|| match msg {
        WM_APP_TICK => {
            if let Some(st) = win_state(hwnd) {
                st.tick_pending.store(false, Ordering::Release);
                if IsIconic(hwnd) == 0 {
                    st.player.tick();
                    st.ticks = st.ticks.wrapping_add(1);
                    // The bar shows position / state: refresh it ~15 times a second.
                    if st.ticks % 4 == 0 {
                        invalidate_bar(st);
                    }
                }
            }
            0
        }
        WM_SIZE => {
            if let Some(st) = win_state(hwnd) {
                layout_video(st);
                invalidate_bar(st);
            }
            0
        }
        WM_DPICHANGED => {
            if let Some(st) = win_state(hwnd) {
                st.dpi = ((w_param >> 16) & 0xFFFF) as i32;
                let r = &*(l_param as *const Rect);
                SetWindowPos(hwnd, ptr::null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, 0x0004);
            }
            0
        }
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut ps: PaintStruct = zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            if let Some(st) = win_state(hwnd) {
                let mut rc = Rect::default();
                GetClientRect(hwnd, &mut rc);
                paint_bar(st, hdc, rc.right - rc.left, rc.bottom - rc.top);
            }
            EndPaint(hwnd, &ps);
            0
        }
        WM_ACTIVATE => {
            SetFocus(hwnd);
            0
        }
        WM_KEYDOWN => {
            if let Some(st) = win_state(hwnd) {
                if handle_key(st, w_param) {
                    return 0;
                }
            }
            DefWindowProcW(hwnd, msg, w_param, l_param)
        }
        WM_LBUTTONDOWN => {
            if let Some(st) = win_state(hwnd) {
                let (x, y) = lparam_xy(l_param);
                let mut rc = Rect::default();
                GetClientRect(hwnd, &mut rc);
                click(st, x, y - (rc.bottom - scale_dpi(BAR_H, st.dpi)));
            }
            0
        }
        WM_MOUSEMOVE => {
            if let Some(st) = win_state(hwnd) {
                let (x, y) = lparam_xy(l_param);
                if let Some(d) = st.drag {
                    drag_to(st, d, x);
                } else {
                    let mut rc = Rect::default();
                    GetClientRect(hwnd, &mut rc);
                    let hover = part_at(st, x, y - (rc.bottom - scale_dpi(BAR_H, st.dpi)));
                    if hover != st.hover {
                        st.hover = hover;
                        invalidate_bar(st);
                    }
                }
            }
            0
        }
        WM_LBUTTONUP => {
            if let Some(st) = win_state(hwnd) {
                if st.drag.take() == Some(Drag::Volume) {
                    save_volume(st);
                }
                if GetCapture() == hwnd {
                    ReleaseCapture();
                }
            }
            0
        }
        WM_CAPTURECHANGED => {
            if let Some(st) = win_state(hwnd) {
                if st.drag.take() == Some(Drag::Volume) {
                    save_volume(st);
                }
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            destroy_state(hwnd);
            0
        }
        _ => DefWindowProcW(hwnd, msg, w_param, l_param),
    }))
    .unwrap_or(0)
}

/// The child the GPU draws into: it only forwards input to the player window.
unsafe extern "system" fn video_proc(hwnd: *mut c_void, msg: u32, w_param: usize, l_param: isize) -> isize {
    catch_unwind(AssertUnwindSafe(|| match msg {
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut ps: PaintStruct = zeroed();
            BeginPaint(hwnd, &mut ps);
            EndPaint(hwnd, &ps);
            if let Some(st) = win_state(GetParent(hwnd)) {
                st.player.redraw(); // uncovered: draw on the next tick
            }
            0
        }
        WM_LBUTTONDOWN => {
            SetFocus(GetParent(hwnd));
            0
        }
        WM_KEYDOWN => {
            SendMessageW(GetParent(hwnd), msg, w_param, l_param);
            0
        }
        _ => DefWindowProcW(hwnd, msg, w_param, l_param),
    }))
    .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timecode_matches_the_mac_player() {
        assert_eq!(timecode(0.0, 24.0), "00:00:00:00");
        assert_eq!(timecode(49.0, 23.976), "00:00:02:01");
        assert_eq!(timecode(3600.0 * 25.0, 25.0), "01:00:00:00");
    }

    #[test]
    fn slider_fraction_is_clamped_and_inset() {
        let r = Rect { left: 100, top: 0, right: 220, bottom: 40 };
        assert_eq!(slider_fraction(r, 110, 10), 0.0);
        assert_eq!(slider_fraction(r, 160, 10), 0.5);
        assert_eq!(slider_fraction(r, 500, 10), 1.0);
        assert_eq!(slider_fraction(r, 0, 10), 0.0);
    }
}
