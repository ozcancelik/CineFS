#![cfg_attr(windows, windows_subsystem = "windows")]

//! Native Windows Studio Desktop Application, System Tray & All-in-One Installer for CineFS.
//!
//! Key capabilities:
//! - Modern Dark Cinema UI rendered on a double-buffered 32-bit BGRA DIB canvas with
//!   sub-pixel anti-aliased SDF rounded cards, badges, vector icons, and ClearType typography.
//! - Per-Monitor High-DPI scaling (`DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2`) and
//!   Windows 11/10 Dark Titlebar (`DWMWA_USE_IMMERSIVE_DARK_MODE`).
//! - Bundled WinFsp 2.0 MSI (`include_bytes!`) with automatic detection and 1-click
//!   elevated auto-installation inside `CineFS.exe` (no separate WinFsp setup needed).
//! - Built-in Setup Wizard mode when launched as `CineFS-Setup.exe` (or `--setup`),
//!   installing `CineFS.exe`, bundled WinFsp, Desktop/Start Menu shortcuts, and
//!   Windows Explorer right-click `"Mount with CineFS"` context menus.
//! - Crash-free `"Open in Explorer"` via `ShellExecuteW` and background-safe
//!   DaVinci Resolve Media Pool import via `fuscript.exe` (`CREATE_NO_WINDOW`).

mod model;
#[allow(dead_code)]
mod settings;
#[cfg(windows)]
mod shell;

#[allow(unused_imports)]
use cinefs_core::DngPayloadMode;
#[allow(unused_imports)]
use std::path::PathBuf;
#[allow(unused_imports)]
use std::sync::Arc;
#[allow(unused_imports)]
use std::time::Instant;

pub use model::*;

#[cfg(not(windows))]
fn main() {
    println!(
        "cinefs-win is the native Windows Studio GUI + System Tray + Installer for CineFS.\n\
         On macOS, use CineFS.app or `cinefs-cli`."
    );
}

#[cfg(windows)]
mod win_gui {
    use super::*;
    use cinefs_core::winfsp::is_winfsp_available;
    use std::ffi::c_void;
    use std::fs;
    use std::mem::{size_of, zeroed};
    use std::os::windows::process::CommandExt;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::process::Command;
    use std::ptr;
    use std::sync::Mutex;
    use crate::shell::*;

    mod player_win;

    // Embedded assets so `CineFS.exe` / `CineFS-Setup.exe` works as a 100% self-contained single binary!
    static APP_ICON_ICO: &[u8] = include_bytes!("../../../windows/AppIcon.ico");
    static WINFSP_MSI_BYTES: &[u8] = include_bytes!("../../../windows/deps/winfsp.msi");

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    // Window Messages
    const WM_CREATE: u32 = 0x0001;
    const WM_DESTROY: u32 = 0x0002;
    const WM_SIZE: u32 = 0x0005;
    const WM_PAINT: u32 = 0x000F;
    const WM_CLOSE: u32 = 0x0010;
    const WM_ERASEBKGND: u32 = 0x0014;
    const WM_GETMINMAXINFO: u32 = 0x0024;
    const WM_SETCURSOR: u32 = 0x0020;
    const WM_SETICON: u32 = 0x0080;
    const WM_COMMAND: u32 = 0x0111;
    const WM_MOUSEMOVE: u32 = 0x0200;
    const WM_LBUTTONDOWN: u32 = 0x0201;
    const WM_LBUTTONUP: u32 = 0x0202;
    const WM_LBUTTONDBLCLK: u32 = 0x0203;
    const WM_RBUTTONUP: u32 = 0x0205;
    const WM_MOUSEWHEEL: u32 = 0x020A;
    const WM_MOUSELEAVE: u32 = 0x02A3;
    const WM_DROPFILES: u32 = 0x0233;
    const WM_DPICHANGED: u32 = 0x02E0;
    const WM_USER: u32 = 0x0400;
    const WM_TRAYICON: u32 = WM_USER + 42;

    // Window Styles
    const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
    const WS_VISIBLE: u32 = 0x1000_0000;
    const WS_CLIPCHILDREN: u32 = 0x0200_0000;

    const SW_HIDE: i32 = 0;
    const SW_SHOWNORMAL: i32 = 1;
    const SW_SHOW: i32 = 5;
    const SW_RESTORE: i32 = 9;

    // NotifyIcon
    const NIF_MESSAGE: u32 = 0x0000_0001;
    const NIF_ICON: u32 = 0x0000_0002;
    const NIF_TIP: u32 = 0x0000_0004;
    const NIM_ADD: u32 = 0x0000_0000;
    const NIM_DELETE: u32 = 0x0000_0002;

    // Popup Menu
    const TPM_BOTTOMALIGN: u32 = 0x0020;
    const TPM_TOPALIGN: u32 = 0x0000;
    const TPM_LEFTALIGN: u32 = 0x0000;
    const MF_STRING: u32 = 0x0000_0000;
    const MF_CHECKED: u32 = 0x0000_0008;
    const MF_SEPARATOR: u32 = 0x0000_0800;

    const IDI_APPLICATION: *const u16 = 32512 as *const u16;
    const IDC_ARROW: *const u16 = 32512 as *const u16;
    const IDC_HAND: *const u16 = 32649 as *const u16;
    const ICON_SMALL: usize = 0;
    const ICON_BIG: usize = 1;

    // GDI constants
    const BI_RGB: u32 = 0;
    const DIB_RGB_COLORS: u32 = 0;
    const SRCCOPY: u32 = 0x00CC_0020;
    const TRANSPARENT: i32 = 1;
    const CLEARTYPE_QUALITY: u32 = 5;
    const DT_LEFT: u32 = 0x0000;
    const DT_CENTER: u32 = 0x0001;
    const DT_RIGHT: u32 = 0x0002;
    const DT_VCENTER: u32 = 0x0004;
    const DT_SINGLELINE: u32 = 0x0020;
    const DT_WORDBREAK: u32 = 0x0010;
    const DT_NOPREFIX: u32 = 0x0800;
    const DT_END_ELLIPSIS: u32 = 0x8000;

    // Tray & Mode Menu IDs
    const ID_TRAY_SHOW: usize = 201;
    const ID_TRAY_ADD_FILE: usize = 202;
    const ID_TRAY_ADD_FOLDER: usize = 203;
    const ID_TRAY_RESOLVE_ALL: usize = 204;
    const ID_TRAY_UNMOUNT_ALL: usize = 205;
    const ID_TRAY_EXIT: usize = 206;
    const ID_TRAY_OPEN_LOG: usize = 207;
    const ID_TRAY_STRETCH: usize = 208;
    const ID_TRAY_BAKE: usize = 209;
    const ID_TRAY_FOCUS: usize = 210;

    /// 0 = Auto (from RAWC), 1 = off, 2 = horizontal x3 (EOS M crop_rec 3x1), 3 = vertical x1.67, 4 = vertical x3
    static STRETCH_MODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

    fn settings_path() -> PathBuf {
        data_dir().join("settings.ini")
    }

    fn stretch_override_for(mode: u8) -> Option<f32> {
        match mode {
            1 => Some(1.0),
            2 => Some(1.0 / 3.0),
            3 => Some(5.0 / 3.0),
            4 => Some(3.0),
            _ => None,
        }
    }

    /// Loads `settings.ini` and configures the core (stretch / focus-pixel defaults) from it.
    fn load_and_apply_settings() -> crate::settings::Settings {
        let s = crate::settings::Settings::load(&settings_path());
        cinefs_core::dng::set_stretch_bake(s.stretch_bake);
        cinefs_core::focus_fix::set_focus_fix(s.focus_fix);
        STRETCH_MODE.store(s.stretch_mode, std::sync::atomic::Ordering::Relaxed);
        cinefs_core::mlv::set_stretch_override(stretch_override_for(s.stretch_mode));
        s
    }

    /// Writes the current preferences. Must be called without holding `APP_STATE`.
    fn persist_settings() {
        let (mode, auto_open_explorer) = match APP_STATE.lock() {
            Ok(guard) => match guard.as_ref() {
                Some(st) => (st.selected_mode, st.auto_open_explorer),
                None => return,
            },
            Err(_) => return,
        };
        // Start from the file so fields owned elsewhere (player volume) are kept.
        let mut s = crate::settings::Settings::load(&settings_path());
        s.mode = mode;
        s.auto_open_explorer = auto_open_explorer;
        s.stretch_bake = cinefs_core::dng::stretch_bake();
        s.focus_fix = cinefs_core::focus_fix::focus_fix_enabled();
        s.stretch_mode = STRETCH_MODE.load(std::sync::atomic::Ordering::Relaxed);
        if let Err(e) = s.save(&settings_path()) {
            cinefs_core::log_warn!("Could not save settings: {e}");
        }
    }

    fn stretch_mode_label(m: u8) -> &'static str {
        match m {
            1 => "Off (no stretch)",
            2 => "Force horizontal x3",
            3 => "Force vertical x1.67",
            4 => "Force vertical x3",
            _ => "Auto (from file)",
        }
    }

    const ID_MODE_AUTO: usize = 300;
    const ID_MODE_PASSTHROUGH: usize = 301;
    const ID_MODE_NATIVE: usize = 302;
    const ID_MODE_UNPACKED16: usize = 303;

    #[repr(C)]
    struct WndClassExW {
        cb_size: u32,
        style: u32,
        lpfn_wnd_proc: unsafe extern "system" fn(*mut c_void, u32, usize, isize) -> isize,
        cb_cls_extra: i32,
        cb_wnd_extra: i32,
        h_instance: *mut c_void,
        h_icon: *mut c_void,
        h_cursor: *mut c_void,
        hbr_background: *mut c_void,
        lpsz_menu_name: *const u16,
        lpsz_class_name: *const u16,
        h_icon_sm: *mut c_void,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Point {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default, Debug)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    struct MinMaxInfo {
        pt_reserved: Point,
        pt_max_size: Point,
        pt_max_position: Point,
        pt_min_track_size: Point,
        pt_max_track_size: Point,
    }

    #[repr(C)]
    struct PaintStruct {
        hdc: *mut c_void,
        f_erase: i32,
        rc_paint: Rect,
        f_restore: i32,
        f_inc_update: i32,
        rgb_reserved: [u8; 32],
    }

    #[repr(C)]
    struct BitmapInfoHeader {
        bi_size: u32,
        bi_width: i32,
        bi_height: i32,
        bi_planes: u16,
        bi_bit_count: u16,
        bi_compression: u32,
        bi_size_image: u32,
        bi_x_pels_per_meter: i32,
        bi_y_pels_per_meter: i32,
        bi_clr_used: u32,
        bi_clr_important: u32,
    }

    #[repr(C)]
    struct BitmapInfo {
        bmi_header: BitmapInfoHeader,
        bmi_colors: [u32; 1],
    }

    #[repr(C)]
    struct TrackMouseEventStruct {
        cb_size: u32,
        dw_flags: u32,
        hwnd_track: *mut c_void,
        dw_hover_time: u32,
    }

    #[repr(C)]
    struct Msg {
        hwnd: *mut c_void,
        message: u32,
        w_param: usize,
        l_param: isize,
        time: u32,
        pt: Point,
    }

    #[repr(C)]
    struct NotifyIconDataW {
        cb_size: u32,
        h_wnd: *mut c_void,
        u_id: u32,
        u_flags: u32,
        u_callback_message: u32,
        h_icon: *mut c_void,
        sz_tip: [u16; 128],
    }

    #[repr(C)]
    struct OpenFileNameW {
        l_struct_size: u32,
        hwnd_owner: *mut c_void,
        h_instance: *mut c_void,
        lpstr_filter: *const u16,
        lpstr_custom_filter: *mut u16,
        n_max_cust_filter: u32,
        n_filter_index: u32,
        lpstr_file: *mut u16,
        n_max_file: u32,
        lpstr_file_title: *mut u16,
        n_max_file_title: u32,
        lpstr_initial_dir: *const u16,
        lpstr_title: *const u16,
        flags: u32,
        n_file_offset: u16,
        n_file_extension: u16,
        lpstr_def_ext: *const u16,
        l_cust_data: isize,
        lpfn_hook: *const c_void,
        lp_template_name: *const u16,
        pv_reserved: *mut c_void,
        dw_reserved: u32,
        flags_ex: u32,
    }

    #[repr(C)]
    struct BrowseInfoW {
        hwnd_owner: *mut c_void,
        pidl_root: *const c_void,
        psz_display_name: *mut u16,
        lpsz_title: *const u16,
        ul_flags: u32,
        lpfn: *const c_void,
        l_param: isize,
        i_image: i32,
    }

    #[repr(C)]
    struct ShellExecuteInfoW {
        cb_size: u32,
        f_mask: u32,
        hwnd: *mut c_void,
        lp_verb: *const u16,
        lp_file: *const u16,
        lp_parameters: *const u16,
        lp_directory: *const u16,
        n_show: i32,
        h_inst_app: *mut c_void,
        lp_id_list: *mut c_void,
        lp_class: *const u16,
        hkey_class: *mut c_void,
        dw_hot_key: u32,
        h_icon_or_monitor: *mut c_void,
        h_process: *mut c_void,
    }

    #[link(name = "kernel32", kind = "raw-dylib")]
    extern "system" {
        fn GetModuleHandleW(lpModuleName: *const u16) -> *mut c_void;
        fn GetLogicalDrives() -> u32;
        fn WaitForSingleObject(hHandle: *mut c_void, dwMilliseconds: u32) -> u32;
        fn CloseHandle(hObject: *mut c_void) -> i32;
    }

    #[link(name = "user32", kind = "raw-dylib")]
    extern "system" {
        fn RegisterClassExW(lpwcx: *const WndClassExW) -> u16;
        fn CreateWindowExW(
            dwExStyle: u32,
            lpClassName: *const u16,
            lpWindowName: *const u16,
            dwStyle: u32,
            X: i32,
            Y: i32,
            nWidth: i32,
            nHeight: i32,
            hWndParent: *mut c_void,
            hMenu: *mut c_void,
            hInstance: *mut c_void,
            lpParam: *mut c_void,
        ) -> *mut c_void;
        fn DefWindowProcW(
            hWnd: *mut c_void,
            Msg: u32,
            wParam: usize,
            lParam: isize,
        ) -> isize;
        fn ShowWindow(hWnd: *mut c_void, nCmdShow: i32) -> i32;
        fn SetForegroundWindow(hWnd: *mut c_void) -> i32;
        fn GetMessageW(
            lpMsg: *mut Msg,
            hWnd: *mut c_void,
            wMsgFilterMin: u32,
            wMsgFilterMax: u32,
        ) -> i32;
        fn TranslateMessage(lpMsg: *const Msg) -> i32;
        fn DispatchMessageW(lpMsg: *const Msg) -> isize;
        fn PostQuitMessage(nExitCode: i32);
        fn DestroyWindow(hWnd: *mut c_void) -> i32;
        fn SendMessageW(hWnd: *mut c_void, Msg: u32, wParam: usize, lParam: isize) -> isize;
        fn LoadIconW(hInstance: *mut c_void, lpIconName: *const u16) -> *mut c_void;
        fn LoadCursorW(hInstance: *mut c_void, lpCursorName: *const u16) -> *mut c_void;
        fn SetCursor(hCursor: *mut c_void) -> *mut c_void;
        fn CreateIconFromResourceEx(
            presbits: *const u8,
            dwResSize: u32,
            fIcon: i32,
            dwVer: u32,
            cxDesired: i32,
            cyDesired: i32,
            Flags: u32,
        ) -> *mut c_void;
        fn DrawIconEx(
            hdc: *mut c_void,
            xLeft: i32,
            yTop: i32,
            hIcon: *mut c_void,
            cxWidth: i32,
            cyWidth: i32,
            istepIfAniCur: u32,
            hbrFlickerFreeDraw: *mut c_void,
            diFlags: u32,
        ) -> i32;
        fn CreatePopupMenu() -> *mut c_void;
        fn AppendMenuW(
            hMenu: *mut c_void,
            uFlags: u32,
            uIDNewItem: usize,
            lpNewItem: *const u16,
        ) -> i32;
        fn TrackPopupMenu(
            hMenu: *mut c_void,
            uFlags: u32,
            x: i32,
            y: i32,
            nReserved: i32,
            hWnd: *mut c_void,
            prcRect: *const c_void,
        ) -> i32;
        fn DestroyMenu(hMenu: *mut c_void) -> i32;
        fn GetCursorPos(lpPoint: *mut Point) -> i32;
        fn ClientToScreen(hWnd: *mut c_void, lpPoint: *mut Point) -> i32;
        fn GetClientRect(hWnd: *mut c_void, lpRect: *mut Rect) -> i32;
        fn InvalidateRect(hWnd: *mut c_void, lpRect: *const Rect, bErase: i32) -> i32;
        fn BeginPaint(hWnd: *mut c_void, lpPaint: *mut PaintStruct) -> *mut c_void;
        fn EndPaint(hWnd: *mut c_void, lpPaint: *const PaintStruct) -> i32;
        fn DrawTextW(
            hdc: *mut c_void,
            lpchText: *const u16,
            cchText: i32,
            lprc: *mut Rect,
            format: u32,
        ) -> i32;
        fn TrackMouseEvent(lpEventTrack: *mut TrackMouseEventStruct) -> i32;
        fn SetProcessDPIAware() -> i32;
        fn SetWindowPos(
            hWnd: *mut c_void,
            hWndInsertAfter: *mut c_void,
            X: i32,
            Y: i32,
            cx: i32,
            cy: i32,
            uFlags: u32,
        ) -> i32;
    }

    #[link(name = "gdi32", kind = "raw-dylib")]
    extern "system" {
        fn CreateCompatibleDC(hdc: *mut c_void) -> *mut c_void;
        fn DeleteDC(hdc: *mut c_void) -> i32;
        fn CreateDIBSection(
            hdc: *mut c_void,
            pbmi: *const BitmapInfo,
            usage: u32,
            ppvBits: *mut *mut u32,
            hSection: *mut c_void,
            offset: u32,
        ) -> *mut c_void;
        fn SelectObject(hdc: *mut c_void, h: *mut c_void) -> *mut c_void;
        fn DeleteObject(ho: *mut c_void) -> i32;
        fn BitBlt(
            hdc: *mut c_void,
            x: i32,
            y: i32,
            cx: i32,
            cy: i32,
            hdcSrc: *mut c_void,
            x1: i32,
            y1: i32,
            rop: u32,
        ) -> i32;
        fn GdiFlush() -> i32;
        fn SetBkMode(hdc: *mut c_void, mode: i32) -> i32;
        fn SetTextColor(hdc: *mut c_void, color: u32) -> u32;
        fn CreateFontW(
            cHeight: i32,
            cWidth: i32,
            cEscapement: i32,
            cOrientation: i32,
            cWeight: i32,
            bItalic: u32,
            bUnderline: u32,
            bStrikeOut: u32,
            iCharSet: u32,
            iOutPrecision: u32,
            iClipPrecision: u32,
            iQuality: u32,
            iPitchAndFamily: u32,
            pszFaceName: *const u16,
        ) -> *mut c_void;
    }

    #[link(name = "shell32", kind = "raw-dylib")]
    extern "system" {
        fn DragAcceptFiles(hWnd: *mut c_void, fAccept: i32);
        fn DragQueryFileW(
            hDrop: *mut c_void,
            iFile: u32,
            lpszFile: *mut u16,
            cch: u32,
        ) -> u32;
        fn DragFinish(hDrop: *mut c_void);
        fn Shell_NotifyIconW(dwMessage: u32, lpData: *const NotifyIconDataW) -> i32;
        fn SHBrowseForFolderW(lpbi: *const BrowseInfoW) -> *mut c_void;
        fn SHGetPathFromIDListW(pidl: *const c_void, pszPath: *mut u16) -> i32;
        fn ShellExecuteW(
            hwnd: *mut c_void,
            lpOperation: *const u16,
            lpFile: *const u16,
            lpParameters: *const u16,
            lpDirectory: *const u16,
            nShowCmd: i32,
        ) -> isize;
        fn ShellExecuteExW(pExecInfo: *mut ShellExecuteInfoW) -> i32;
        fn IsUserAnAdmin() -> i32;
    }

    #[link(name = "comdlg32", kind = "raw-dylib")]
    extern "system" {
        fn GetOpenFileNameW(lpofn: *mut OpenFileNameW) -> i32;
    }

    #[link(name = "ole32", kind = "raw-dylib")]
    extern "system" {
        fn CoTaskMemFree(pv: *const c_void);
    }

    // =========================================================================
    // Studio Dark Cinema Color Palette (0x00RRGGBB for DIB, converted for GDI)
    // =========================================================================
    const COL_BG: u32 = 0x121316;
    const COL_HEADER_BG: u32 = 0x17181B;
    const COL_FOOTER_BG: u32 = 0x17181B;
    const COL_DIVIDER: u32 = 0x26282D;

    const COL_CARD_BG: u32 = 0x1B1C20;
    const COL_CARD_BORDER: u32 = 0x2B2D33;
    const COL_CARD_ROW_BG: u32 = 0x202126;

    const COL_DROP_BG: u32 = 0x16171A;
    const COL_DROP_BORDER: u32 = 0x34363D;

    const COL_ACCENT: u32 = 0xF59E0B;
    const COL_ACCENT_HOVER: u32 = 0xFBBF24;
    const COL_ACCENT_DARK: u32 = 0xD97706;

    const COL_BTN_SEC: u32 = 0x24262B;
    const COL_BTN_SEC_HOVER: u32 = 0x2E3036;
    const COL_BTN_SEC_BORDER: u32 = 0x383B42;

    const COL_BTN_DANGER_BG: u32 = 0x2B1B22;
    const COL_BTN_DANGER_HOVER: u32 = 0x3D222C;
    const COL_BTN_DANGER_BORDER: u32 = 0x5C2B39;
    const COL_BTN_DANGER_TEXT: u32 = 0xFCA5A5;

    const COL_EMERALD_BG: u32 = 0x14231E;
    const COL_EMERALD_BORDER: u32 = 0x25624A;
    const COL_EMERALD_TEXT: u32 = 0x5FC8A0;


    const COL_AMBER_BG: u32 = 0x2A2113;
    const COL_AMBER_BORDER: u32 = 0x6A4F1C;
    const COL_AMBER_TEXT: u32 = 0xE8B84A;

    const COL_ERR_BG: u32 = 0x2A1719;
    const COL_ERR_BORDER: u32 = 0x6B2A2E;
    const COL_ERR_TEXT: u32 = 0xE59A9A;

    const COL_TEXT_PRIMARY: u32 = 0xEDEDEF;
    const COL_TEXT_SECONDARY: u32 = 0x9A9CA3;
    const COL_TEXT_MUTED: u32 = 0x6A6C73;
    // Neutral badge (default): colour is reserved for meaning (green = zero-decode, amber = stretch)
    const COL_BADGE_BG: u32 = 0x25272C;
    const COL_BADGE_BORDER: u32 = 0x363840;
    const COL_BADGE_TEXT: u32 = 0xA6A8AF;
    const COL_TEXT_DARK: u32 = 0x121316;

    #[inline]
    const fn rgb_to_colorref(rgb: u32) -> u32 {
        let r = (rgb >> 16) & 0xFF;
        let g = (rgb >> 8) & 0xFF;
        let b = rgb & 0xFF;
        (b << 16) | (g << 8) | r
    }

    // =========================================================================
    // Anti-Aliased 32-bit DIB Software Canvas
    // =========================================================================
    struct Canvas<'a> {
        pixels: &'a mut [u32],
        width: i32,
        height: i32,
    }

    impl<'a> Canvas<'a> {
        fn clear(&mut self, color: u32) {
            self.pixels.fill(color);
        }

        #[inline]
        fn blend_pixel(&mut self, x: i32, y: i32, color: u32, alpha: u32) {
            if x < 0 || y < 0 || x >= self.width || y >= self.height || alpha == 0 {
                return;
            }
            let idx = (y * self.width + x) as usize;
            if alpha >= 255 {
                self.pixels[idx] = color;
                return;
            }
            let dst = self.pixels[idx];
            let inv = 255 - alpha;
            let r = (((color >> 16) & 0xFF) * alpha + ((dst >> 16) & 0xFF) * inv) >> 8;
            let g = (((color >> 8) & 0xFF) * alpha + ((dst >> 8) & 0xFF) * inv) >> 8;
            let b = ((color & 0xFF) * alpha + (dst & 0xFF) * inv) >> 8;
            self.pixels[idx] = (r << 16) | (g << 8) | b;
        }

        fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
            let x0 = x.max(0);
            let y0 = y.max(0);
            let x1 = (x + w).min(self.width);
            let y1 = (y + h).min(self.height);
            if x0 >= x1 || y0 >= y1 {
                return;
            }
            for py in y0..y1 {
                let row_start = (py * self.width + x0) as usize;
                let row_end = (py * self.width + x1) as usize;
                self.pixels[row_start..row_end].fill(color);
            }
        }

        /// Draws a sub-pixel anti-aliased rounded rectangle with optional 1px border.
        fn draw_rounded_rect(
            &mut self,
            x: i32,
            y: i32,
            w: i32,
            h: i32,
            radius: f32,
            fill_color: u32,
            fill_alpha: u32,
            border_color: Option<(u32, f32)>,
        ) {
            if w <= 0 || h <= 0 {
                return;
            }
            let x0 = (x - 1).max(0);
            let y0 = (y - 1).max(0);
            let x1 = (x + w + 1).min(self.width);
            let y1 = (y + h + 1).min(self.height);

            let cx = x as f32 + w as f32 * 0.5;
            let cy = y as f32 + h as f32 * 0.5;
            let hx = w as f32 * 0.5;
            let hy = h as f32 * 0.5;
            let r = radius.min(hx).min(hy);

            for py in y0..y1 {
                let dy = (py as f32 + 0.5 - cy).abs() - (hy - r);
                for px in x0..x1 {
                    let dx = (px as f32 + 0.5 - cx).abs() - (hx - r);
                    let dist = if dx > 0.0 && dy > 0.0 {
                        (dx * dx + dy * dy).sqrt() - r
                    } else {
                        dx.max(dy) - r
                    };

                    if fill_alpha > 0 && dist < 0.5 {
                        let cov = (0.5 - dist).clamp(0.0, 1.0);
                        let a = ((fill_alpha as f32) * cov) as u32;
                        self.blend_pixel(px, py, fill_color, a);
                    }

                    if let Some((bcol, bwidth)) = border_color {
                        let half_bw = bwidth * 0.5;
                        let bdist = (dist + half_bw).abs() - half_bw;
                        if bdist < 0.7 {
                            let bcov = (0.7 - bdist).clamp(0.0, 1.0);
                            let ba = (255.0 * bcov) as u32;
                            self.blend_pixel(px, py, bcol, ba);
                        }
                    }
                }
            }
        }

        /// Draws a dashed rounded rectangle border (used for the drag-and-drop zone).
        fn draw_dashed_rounded_rect(
            &mut self,
            x: i32,
            y: i32,
            w: i32,
            h: i32,
            radius: f32,
            fill_color: u32,
            border_color: u32,
            dash_period: i32,
        ) {
            self.draw_rounded_rect(x, y, w, h, radius, fill_color, 255, None);
            let x0 = (x - 1).max(0);
            let y0 = (y - 1).max(0);
            let x1 = (x + w + 1).min(self.width);
            let y1 = (y + h + 1).min(self.height);

            let cx = x as f32 + w as f32 * 0.5;
            let cy = y as f32 + h as f32 * 0.5;
            let hx = w as f32 * 0.5;
            let hy = h as f32 * 0.5;
            let r = radius.min(hx).min(hy);
            let period = dash_period.max(6);

            for py in y0..y1 {
                let dy = (py as f32 + 0.5 - cy).abs() - (hy - r);
                for px in x0..x1 {
                    let dx = (px as f32 + 0.5 - cx).abs() - (hx - r);
                    let dist = if dx > 0.0 && dy > 0.0 {
                        (dx * dx + dy * dy).sqrt() - r
                    } else {
                        dx.max(dy) - r
                    };
                    let bdist = (dist + 0.75).abs() - 0.75;
                    if bdist < 0.7 {
                        let phase = (px + py).rem_euclid(period);
                        if phase < (period * 3 / 5) {
                            let bcov = (0.7 - bdist).clamp(0.0, 1.0);
                            self.blend_pixel(px, py, border_color, (255.0 * bcov) as u32);
                        }
                    }
                }
            }
        }

        /// Draws a smooth anti-aliased line segment of thickness `thickness`.
        fn draw_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, thickness: f32, color: u32) {
            let min_x = (x0.min(x1) - thickness - 2.0).floor() as i32;
            let max_x = (x0.max(x1) + thickness + 2.0).ceil() as i32;
            let min_y = (y0.min(y1) - thickness - 2.0).floor() as i32;
            let max_y = (y0.max(y1) + thickness + 2.0).ceil() as i32;

            let vx = x1 - x0;
            let vy = y1 - y0;
            let len_sq = (vx * vx + vy * vy).max(0.0001);
            let half_t = thickness * 0.5;

            for py in min_y..=max_y {
                for px in min_x..=max_x {
                    let wx = px as f32 + 0.5 - x0;
                    let wy = py as f32 + 0.5 - y0;
                    let t = ((wx * vx + wy * vy) / len_sq).clamp(0.0, 1.0);
                    let dx = wx - t * vx;
                    let dy = wy - t * vy;
                    let dist = (dx * dx + dy * dy).sqrt() - half_t;
                    if dist < 0.7 {
                        let cov = (0.7 - dist).clamp(0.0, 1.0);
                        self.blend_pixel(px, py, color, (255.0 * cov) as u32);
                    }
                }
            }
        }

        /// Draws an anti-aliased circle.
        fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, color: u32, alpha: u32) {
            let x0 = (cx - r - 1.0).floor() as i32;
            let x1 = (cx + r + 1.0).ceil() as i32;
            let y0 = (cy - r - 1.0).floor() as i32;
            let y1 = (cy + r + 1.0).ceil() as i32;
            for py in y0..=y1 {
                for px in x0..=x1 {
                    let dx = px as f32 + 0.5 - cx;
                    let dy = py as f32 + 0.5 - cy;
                    let dist = (dx * dx + dy * dy).sqrt() - r;
                    if dist < 0.5 {
                        let cov = (0.5 - dist).clamp(0.0, 1.0);
                        self.blend_pixel(px, py, color, ((alpha as f32) * cov) as u32);
                    }
                }
            }
        }
    }

    // =========================================================================
    // Interactive Hit Regions
    // =========================================================================
    #[derive(Clone, Debug, PartialEq, Eq)]
    enum HitAction {
        AddFile,
        AddFolder,
        SelectModeMenu,
        InstallWinFsp,
        DismissBanner,
        ToggleAutoExplorer,
        LocateResolve,
        UnmountAll,
        CardExplorer(usize),
        CardResolve(usize),
        CardUnmount(usize),
        ClipExplorer(usize, usize),
        ClipPlay(usize, usize),
        // Setup Wizard mode actions
        SetupToggleDesktopIcon,
        SetupToggleContextMenu,
        SetupToggleDefender,
        SetupRunInstall,
        SetupLaunchPortable,
    }

    #[derive(Clone, Debug)]
    struct HitBox {
        rect: Rect,
        action: HitAction,
    }

    impl HitBox {
        fn contains(&self, x: i32, y: i32) -> bool {
            x >= self.rect.left && x < self.rect.right && y >= self.rect.top && y < self.rect.bottom
        }
    }

    struct AppState {
        dpi: i32,
        h_icon_big: *mut c_void,
        selected_mode: DngPayloadMode,
        auto_open_explorer: bool,
        winfsp_installed: bool,
        resolve_fuscript_path: Option<PathBuf>,
        status_message: Option<(String, bool)>, // (message, is_error)
        mounts: Vec<WinMountEntry>,
        scroll_y: i32,
        max_scroll_y: i32,
        hit_boxes: Vec<HitBox>,
        hovered_action: Option<HitAction>,
        pressed_action: Option<HitAction>,
        tracking_mouse: bool,
        // Setup Wizard Mode state
        is_setup_mode: bool,
        setup_desktop_icon: bool,
        setup_context_menu: bool,
        setup_defender_exclusion: bool,
    }

    unsafe impl Send for AppState {}

    static APP_STATE: Mutex<Option<AppState>> = Mutex::new(None);

    fn wstr(s: &str) -> Vec<u16> {
        let mut v: Vec<u16> = s.encode_utf16().collect();
        v.push(0);
        v
    }

    #[inline]
    fn scale_dpi(px: i32, dpi: i32) -> i32 {
        (px * dpi + 48) / 96
    }

    /// Parses `windows/AppIcon.ico` and creates an `HICON` for the requested size (`32` or `256`).
    unsafe fn load_embedded_icon(desired_px: i32) -> *mut c_void {
        if APP_ICON_ICO.len() < 22 {
            return LoadIconW(ptr::null_mut(), IDI_APPLICATION);
        }
        let count = u16::from_le_bytes([APP_ICON_ICO[4], APP_ICON_ICO[5]]) as usize;
        let mut best_off = 0usize;
        let mut best_sz = 0usize;
        let mut best_diff = i32::MAX;

        for i in 0..count {
            let entry_pos = 6 + i * 16;
            if entry_pos + 16 > APP_ICON_ICO.len() {
                break;
            }
            let w_raw = APP_ICON_ICO[entry_pos] as i32;
            let w = if w_raw == 0 { 256 } else { w_raw };
            let sz = u32::from_le_bytes(
                APP_ICON_ICO[entry_pos + 8..entry_pos + 12]
                    .try_into()
                    .unwrap(),
            ) as usize;
            let off = u32::from_le_bytes(
                APP_ICON_ICO[entry_pos + 12..entry_pos + 16]
                    .try_into()
                    .unwrap(),
            ) as usize;
            if off + sz <= APP_ICON_ICO.len() {
                let diff = (w - desired_px).abs();
                if diff < best_diff {
                    best_diff = diff;
                    best_off = off;
                    best_sz = sz;
                }
            }
        }

        if best_sz > 0 {
            let h = CreateIconFromResourceEx(
                APP_ICON_ICO.as_ptr().add(best_off),
                best_sz as u32,
                1,
                0x0003_0000,
                desired_px,
                desired_px,
                0,
            );
            if !h.is_null() {
                return h;
            }
        }
        LoadIconW(ptr::null_mut(), IDI_APPLICATION)
    }

    /// EnablesPer-Monitor V2 High-DPI awareness and queries window DPI dynamically.
    unsafe fn enable_high_dpi() {
        #[link(name = "kernel32", kind = "raw-dylib")]
        extern "system" {
            fn LoadLibraryW(lpLibFileName: *const u16) -> *mut c_void;
            fn GetProcAddress(hModule: *mut c_void, lpProcName: *const u8) -> *const c_void;
        }
        let u32_w = wstr("user32.dll");
        let h_u32 = LoadLibraryW(u32_w.as_ptr());
        if !h_u32.is_null() {
            let p_set_ctx = GetProcAddress(h_u32, b"SetProcessDpiAwarenessContext\0".as_ptr());
            if !p_set_ctx.is_null() {
                type FnSetDpiCtx = unsafe extern "system" fn(isize) -> i32;
                let f: FnSetDpiCtx = std::mem::transmute(p_set_ctx);
                // DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 = -4
                if f(-4) != 0 {
                    return;
                }
            }
        }
        let _ = SetProcessDPIAware();
    }

    unsafe fn get_window_dpi(hwnd: *mut c_void) -> i32 {
        #[link(name = "kernel32", kind = "raw-dylib")]
        extern "system" {
            fn LoadLibraryW(lpLibFileName: *const u16) -> *mut c_void;
            fn GetProcAddress(hModule: *mut c_void, lpProcName: *const u8) -> *const c_void;
        }
        let u32_w = wstr("user32.dll");
        let h_u32 = LoadLibraryW(u32_w.as_ptr());
        if !h_u32.is_null() {
            let p_get_dpi = GetProcAddress(h_u32, b"GetDpiForWindow\0".as_ptr());
            if !p_get_dpi.is_null() {
                type FnGetDpi = unsafe extern "system" fn(*mut c_void) -> u32;
                let f: FnGetDpi = std::mem::transmute(p_get_dpi);
                let d = f(hwnd) as i32;
                if d >= 72 && d <= 480 {
                    return d;
                }
            }
        }
        96
    }

    /// Applies Windows 11/10 Dark Mode Titlebar and dark caption color via `dwmapi.dll`.
    unsafe fn apply_dark_titlebar(hwnd: *mut c_void) {
        #[link(name = "kernel32", kind = "raw-dylib")]
        extern "system" {
            fn LoadLibraryW(lpLibFileName: *const u16) -> *mut c_void;
            fn GetProcAddress(hModule: *mut c_void, lpProcName: *const u8) -> *const c_void;
        }
        let dwm_w = wstr("dwmapi.dll");
        let h_dwm = LoadLibraryW(dwm_w.as_ptr());
        if !h_dwm.is_null() {
            let p_set_attr = GetProcAddress(h_dwm, b"DwmSetWindowAttribute\0".as_ptr());
            if !p_set_attr.is_null() {
                type FnDwmSetWindowAttribute =
                    unsafe extern "system" fn(*mut c_void, u32, *const c_void, u32) -> i32;
                let f: FnDwmSetWindowAttribute = std::mem::transmute(p_set_attr);
                let dark: i32 = 1;
                // DWMWA_USE_IMMERSIVE_DARK_MODE = 20 (and 19 on early Win10 builds)
                let _ = f(hwnd, 20, (&dark as *const i32).cast::<c_void>(), 4);
                let _ = f(hwnd, 19, (&dark as *const i32).cast::<c_void>(), 4);
                // DWMWA_CAPTION_COLOR = 35
                let caption_colorref: u32 = rgb_to_colorref(COL_HEADER_BG);
                let _ = f(hwnd, 35, (&caption_colorref as *const u32).cast::<c_void>(), 4);
            }
        }
    }

    /// Extracts the bundled `winfsp.msi` to `%TEMP%\CineFS_WinFsp_2.0.msi` and installs it via `msiexec.exe`.
    unsafe fn install_bundled_winfsp(hwnd: *mut c_void, silent: bool) -> bool {
        if is_winfsp_available() {
            return true;
        }
        let msi_path = std::env::temp_dir().join("CineFS_WinFsp_2.0.msi");
        if fs::write(&msi_path, WINFSP_MSI_BYTES).is_err() {
            return false;
        }

        let verb = wstr("runas");
        let file = wstr("msiexec.exe");
        let params = if silent {
            wstr(&format!("/i \"{}\" /qn /norestart", msi_path.display()))
        } else {
            wstr(&format!(
                "/i \"{}\" /passive /norestart",
                msi_path.display()
            ))
        };

        let mut sei: ShellExecuteInfoW = zeroed();
        sei.cb_size = size_of::<ShellExecuteInfoW>() as u32;
        sei.f_mask = 0x0000_0040; // SEE_MASK_NOCLOSEPROCESS
        sei.hwnd = hwnd;
        sei.lp_verb = verb.as_ptr();
        sei.lp_file = file.as_ptr();
        sei.lp_parameters = params.as_ptr();
        sei.n_show = if silent { SW_HIDE } else { SW_SHOWNORMAL };

        if ShellExecuteExW(&mut sei) != 0 && !sei.h_process.is_null() {
            WaitForSingleObject(sei.h_process, 120_000);
            CloseHandle(sei.h_process);
        }

        let ok = is_winfsp_available();
        if let Ok(mut guard) = APP_STATE.lock() {
            if let Some(st) = guard.as_mut() {
                st.winfsp_installed = ok;
                if ok {
                    st.status_message = Some((
                        String::from(
                            "WinFsp 2.0 Virtual Drive Driver installed and ready!",
                        ),
                        false,
                    ));
                } else {
                    st.status_message = Some((
                        String::from(
                            "WinFsp installation was cancelled or did not complete.",
                        ),
                        true,
                    ));
                }
            }
        }
        InvalidateRect(hwnd, ptr::null(), 0);
        ok
    }

    /// Runs the full one-click CineFS + WinFsp system installation.
    unsafe fn perform_full_setup(
        hwnd: *mut c_void,
        create_desktop_icon: bool,
        register_context_menu: bool,
        add_defender_exclusion: bool,
    ) {
        // If not running as Administrator, re-launch elevated with `--run-elevated-setup`
        if IsUserAnAdmin() == 0 {
            if let Ok(cur_exe) = std::env::current_exe() {
                let verb = wstr("runas");
                let exe_w = wstr(&cur_exe.to_string_lossy());
                let mut args = String::from("--run-elevated-setup");
                if !create_desktop_icon {
                    args.push_str(" --no-desktop");
                }
                if !register_context_menu {
                    args.push_str(" --no-context");
                }
                if !add_defender_exclusion {
                    args.push_str(" --no-defender");
                }
                let args_w = wstr(&args);
                let mut sei: ShellExecuteInfoW = zeroed();
                sei.cb_size = size_of::<ShellExecuteInfoW>() as u32;
                sei.f_mask = 0x0000_0040;
                sei.hwnd = hwnd;
                sei.lp_verb = verb.as_ptr();
                sei.lp_file = exe_w.as_ptr();
                sei.lp_parameters = args_w.as_ptr();
                sei.n_show = SW_SHOWNORMAL;
                if ShellExecuteExW(&mut sei) != 0 {
                    if !sei.h_process.is_null() {
                        WaitForSingleObject(sei.h_process, 180_000);
                        CloseHandle(sei.h_process);
                    }
                    DestroyWindow(hwnd);
                    return;
                }
            }
        }

        cinefs_core::log_info!(
            "perform_full_setup: desktop={create_desktop_icon} context_menu={register_context_menu} defender={add_defender_exclusion}"
        );
        // 1. Install bundled WinFsp if not already installed
        let winfsp_ok = install_bundled_winfsp(hwnd, true);
        cinefs_core::log_info!("WinFsp install result: available = {winfsp_ok}");

        // 2. Copy CineFS.exe and AppIcon.ico into `C:\Program Files\CineFS`
        let pf = std::env::var_os("ProgramFiles")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
        let install_dir = pf.join("CineFS");
        let _ = fs::create_dir_all(&install_dir);

        let target_exe = install_dir.join("CineFS.exe");
        let target_ico = install_dir.join("AppIcon.ico");
        if let Ok(cur_exe) = std::env::current_exe() {
            let _ = fs::copy(&cur_exe, &target_exe);
            // Also copy cinefs-cli.exe if located next to setup
            if let Some(parent) = cur_exe.parent() {
                let cli_src = parent.join("cinefs-cli.exe");
                if cli_src.is_file() {
                    let _ = fs::copy(&cli_src, install_dir.join("cinefs-cli.exe"));
                }
            }
        }
        let _ = fs::write(&target_ico, APP_ICON_ICO);

        // 3. Register Windows Explorer right-click "Mount with CineFS" + Start Menu & Desktop Shortcuts via PowerShell
        let mut ps_script = format!(
            "$ErrorActionPreference = 'SilentlyContinue';\n\
             $exe = '{}';\n\
             $ico = '{}';\n\
             $wsh = New-Object -ComObject WScript.Shell;\n\
             $smDir = Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'CineFS';\n\
             New-Item -ItemType Directory -Force -Path $smDir | Out-Null;\n\
             $lnk = $wsh.CreateShortcut((Join-Path $smDir 'CineFS.lnk'));\n\
             $lnk.TargetPath = $exe;\n\
             $lnk.IconLocation = $ico;\n\
             $lnk.Description = 'CineFS — Zero-Copy Virtual CinemaDNG Mount Engine';\n\
             $lnk.Save();\n",
            target_exe.display().to_string().replace('\'', "''"),
            target_ico.display().to_string().replace('\'', "''"),
        );

        if create_desktop_icon {
            ps_script.push_str(
                "$desk = [Environment]::GetFolderPath('CommonDesktopDirectory');\n\
                 $dlnk = $wsh.CreateShortcut((Join-Path $desk 'CineFS.lnk'));\n\
                 $dlnk.TargetPath = $exe;\n\
                 $dlnk.IconLocation = $ico;\n\
                 $dlnk.Description = 'CineFS — Zero-Copy Virtual CinemaDNG Mount Engine';\n\
                 $dlnk.Save();\n",
            );
        }

        if register_context_menu {
            ps_script.push_str(
                "New-Item -Path 'HKLM:\\SOFTWARE\\Classes\\.mlv\\OpenWithProgids' -Force | Out-Null;\n\
                 Set-ItemProperty -Path 'HKLM:\\SOFTWARE\\Classes\\.mlv\\OpenWithProgids' -Name 'CineFS.RAWClip' -Value '';\n\
                 New-Item -Path 'HKLM:\\SOFTWARE\\Classes\\.mcraw\\OpenWithProgids' -Force | Out-Null;\n\
                 Set-ItemProperty -Path 'HKLM:\\SOFTWARE\\Classes\\.mcraw\\OpenWithProgids' -Name 'CineFS.RAWClip' -Value '';\n\
                 New-Item -Path 'HKLM:\\SOFTWARE\\Classes\\.braw\\OpenWithProgids' -Force | Out-Null;\n\
                 Set-ItemProperty -Path 'HKLM:\\SOFTWARE\\Classes\\.braw\\OpenWithProgids' -Name 'CineFS.RAWClip' -Value '';\n\
                 New-Item -Path 'HKLM:\\SOFTWARE\\Classes\\CineFS.RAWClip\\DefaultIcon' -Force | Out-Null;\n\
                 Set-ItemProperty -Path 'HKLM:\\SOFTWARE\\Classes\\CineFS.RAWClip\\DefaultIcon' -Name '(default)' -Value \"`\"$ico`\",0\";\n\
                 New-Item -Path 'HKLM:\\SOFTWARE\\Classes\\CineFS.RAWClip\\shell\\open\\command' -Force | Out-Null;\n\
                 Set-ItemProperty -Path 'HKLM:\\SOFTWARE\\Classes\\CineFS.RAWClip\\shell\\open' -Name '(default)' -Value 'Mount with CineFS';\n\
                 Set-ItemProperty -Path 'HKLM:\\SOFTWARE\\Classes\\CineFS.RAWClip\\shell\\open' -Name 'Icon' -Value \"`\"$ico`\"\";\n\
                 Set-ItemProperty -Path 'HKLM:\\SOFTWARE\\Classes\\CineFS.RAWClip\\shell\\open\\command' -Name '(default)' -Value \"`\"$exe`\" `\"%1`\"\";\n\
                 New-Item -Path 'HKLM:\\SOFTWARE\\Classes\\Directory\\shell\\CineFS\\command' -Force | Out-Null;\n\
                 Set-ItemProperty -Path 'HKLM:\\SOFTWARE\\Classes\\Directory\\shell\\CineFS' -Name '(default)' -Value 'Mount with CineFS';\n\
                 Set-ItemProperty -Path 'HKLM:\\SOFTWARE\\Classes\\Directory\\shell\\CineFS' -Name 'Icon' -Value \"`\"$ico`\"\";\n\
                 Set-ItemProperty -Path 'HKLM:\\SOFTWARE\\Classes\\Directory\\shell\\CineFS\\command' -Name '(default)' -Value \"`\"$exe`\" `\"%V`\"\";\n",
            );
        }

        if add_defender_exclusion {
            ps_script.push_str(
                "Add-MpPreference -ExclusionPath (Join-Path $env:USERPROFILE 'CineFS_Mounts') -ErrorAction SilentlyContinue;\n",
            );
        }

        let _ = Command::new("powershell.exe")
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-WindowStyle")
            .arg("Hidden")
            .arg("-Command")
            .arg(&ps_script)
            .creation_flags(CREATE_NO_WINDOW)
            .status();

        // 4. Launch installed CineFS.exe (NOT elevated!) and close Setup
        cinefs_core::log_info!("Setup finished; launching '{}'", target_exe.display());
        if !launch_unelevated(&target_exe) {
            let open_w = wstr("open");
            let target_w = wstr(&target_exe.to_string_lossy());
            ShellExecuteW(
                ptr::null_mut(),
                open_w.as_ptr(),
                target_w.as_ptr(),
                ptr::null(),
                ptr::null(),
                SW_SHOWNORMAL,
            );
        }
        if !hwnd.is_null() {
            DestroyWindow(hwnd);
        }
    }

    fn set_status(hwnd: *mut c_void, msg: String, is_err: bool) {
        if is_err {
            cinefs_core::log_warn!("STATUS(error): {msg}");
        } else {
            cinefs_core::log_info!("STATUS: {msg}");
        }
        if let Ok(mut guard) = APP_STATE.lock() {
            if let Some(st) = guard.as_mut() {
                st.status_message = Some((msg, is_err));
            }
        }
        unsafe {
            InvalidateRect(hwnd, ptr::null(), 0);
        }
    }

    /// Mounts a source `.MLV`, `.mcraw`, or directory without holding the `APP_STATE` mutex across WinFsp or Win32 calls.
    unsafe fn mount_source_path(hwnd: *mut c_void, source: PathBuf) {
        mount_sources(hwnd, vec![source]);
    }

    /// Mounts one OR MANY sources (`.MLV` / `.mcraw` files and folders) into a SINGLE virtual volume.
    unsafe fn mount_sources(hwnd: *mut c_void, sources: Vec<PathBuf>) {
        if sources.is_empty() {
            return;
        }
        let source = sources[0].clone();
        // 1. Check if already mounted or gather current settings without holding lock
        let (already_mounted_mp, mode, used_mounts, auto_open, winfsp_ready) = {
            let Ok(guard) = APP_STATE.lock() else {
                return;
            };
            let Some(st) = guard.as_ref() else {
                return;
            };
            let existing = if sources.len() == 1 {
                st.mounts
                    .iter()
                    .find(|m| m.source_paths.contains(&source))
                    .map(|m| m.explorer_root_path())
            } else {
                None
            };
            let used: Vec<String> = st
                .mounts
                .iter()
                .map(|m| m.mount_point.to_string_lossy().to_string())
                .collect();
            (
                existing,
                st.selected_mode,
                used,
                st.auto_open_explorer,
                st.winfsp_installed,
            )
        };

        cinefs_core::log_info!(
            "mount_sources: {} source(s), first '{}' (mode={:?}, winfsp_ready={winfsp_ready}, elevated={})",
            sources.len(),
            source.display(),
            mode,
            is_elevated()
        );
        if let Some(mp) = already_mounted_mp {
            cinefs_core::log_info!("already mounted at '{}', just opening it", mp.display());
            open_path_in_explorer(&mp, "already-mounted");
            return;
        }

        // 2. If WinFsp is not installed yet, automatically install bundled WinFsp first!
        if !winfsp_ready && !is_winfsp_available() {
            if !install_bundled_winfsp(hwnd, false) {
                return;
            }
        }

        let mask = GetLogicalDrives();
        cinefs_core::log_info!("GetLogicalDrives before mount = 0x{mask:08X}");
        let mount_point = match pick_free_drive_letter(mask, &used_mounts) {
            Some(dl) => PathBuf::from(dl),
            None => {
                let home = std::env::var_os("USERPROFILE")
                    .map(PathBuf::from)
                    .unwrap_or_else(std::env::temp_dir);
                let stem = if sources.len() > 1 {
                    format!("CineFS_{}_clips", sources.len())
                } else {
                    source
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| String::from("Clip"))
                };
                home.join("CineFS_Mounts").join(stem)
            }
        };

        // 3. Perform mount outside of APP_STATE lock (because WinFsp broadcasts WM_DEVICECHANGE to hwnd!)
        cinefs_core::log_info!("mounting '{}' at '{}'", source.display(), mount_point.display());
        let mount_res = WinMountEntry::mount(sources.clone(), mount_point, mode);

        match mount_res {
            Ok(entry) => {
                let explorer_path = entry.explorer_root_path();
                cinefs_core::log_info!(
                    "mounted: {} clip(s), root='{}', GetLogicalDrives after = 0x{:08X}, clips={:?}",
                    entry.clips.len(),
                    explorer_path.display(),
                    GetLogicalDrives(),
                    entry.clip_folders
                );
                // Make sure Windows really exposes the volume before opening Explorer / Resolve.
                let visible = verify_mount_visible(&explorer_path, 6000);
                let status = format!(
                    "Mounted {} virtual CinemaDNG clip(s) at {} ({}, indexed in {:.1} ms)",
                    entry.clips.len(),
                    explorer_path.display(),
                    format_bytes(entry.total_bytes),
                    entry.index_time_ms
                );
                if let Ok(mut guard) = APP_STATE.lock() {
                    if let Some(st) = guard.as_mut() {
                        st.winfsp_installed = true;
                        st.status_message = Some((status, false));
                        st.mounts.insert(0, entry);
                    }
                }
                InvalidateRect(hwnd, ptr::null(), 0);
                match visible {
                    Ok(_) => {
                        if auto_open {
                            open_path_in_explorer(&explorer_path, "auto-open");
                        }
                    }
                    Err(e) => {
                        let hint = if is_elevated() {
                            " CineFS is running as Administrator — close it and start it normally (not 'Run as administrator')."
                        } else {
                            ""
                        };
                        set_status(
                            hwnd,
                            format!(
                                "Mounted at {} but Windows cannot read it ({e}).{hint} See the log (tray menu > Open Log).",
                                explorer_path.display()
                            ),
                            true,
                        );
                    }
                }
            }
            Err(e) => {
                cinefs_core::log_error!("Mount failed for '{}': {e}", source.display());
                let err_msg = format!("Mount failed for {}: {e}  (see log: tray menu > Open Log)", source.display());
                if let Ok(mut guard) = APP_STATE.lock() {
                    if let Some(st) = guard.as_mut() {
                        st.status_message = Some((err_msg, true));
                    }
                }
                InvalidateRect(hwnd, ptr::null(), 0);
            }
        }
    }

    unsafe fn open_file_dialog(hwnd: *mut c_void) {
        let mut file_buf = vec![0u16; 8192];
        let mut filter: Vec<u16> = Vec::new();
        filter.extend("RAW Video Clips (*.MLV;*.mcraw;*.braw)".encode_utf16());
        filter.push(0);
        filter.extend("*.mlv;*.MLV;*.mcraw;*.MCRAW;*.braw;*.BRAW".encode_utf16());
        filter.push(0);
        filter.extend("All Files (*.*)".encode_utf16());
        filter.push(0);
        filter.extend("*.*".encode_utf16());
        filter.push(0);
        filter.push(0);

        let title = wstr("Select .MLV, .mcraw or .braw Clip(s) to Mount");
        let mut ofn: OpenFileNameW = zeroed();
        ofn.l_struct_size = size_of::<OpenFileNameW>() as u32;
        ofn.hwnd_owner = hwnd;
        ofn.lpstr_filter = filter.as_ptr();
        ofn.lpstr_file = file_buf.as_mut_ptr();
        ofn.n_max_file = file_buf.len() as u32;
        ofn.lpstr_title = title.as_ptr();
        // OFN_EXPLORER (0x80000) | OFN_FILEMUSTEXIST (0x1000) | OFN_PATHMUSTEXIST (0x800) | OFN_ALLOWMULTISELECT (0x200)
        ofn.flags = 0x0008_0000 | 0x0000_1000 | 0x0000_0800 | 0x0000_0200;

        if GetOpenFileNameW(&mut ofn) != 0 {
            // Parse single or multi-select null-separated strings
            let mut parts: Vec<String> = Vec::new();
            let mut start = 0usize;
            while start < file_buf.len() && file_buf[start] != 0 {
                let mut end = start;
                while end < file_buf.len() && file_buf[end] != 0 {
                    end += 1;
                }
                parts.push(String::from_utf16_lossy(&file_buf[start..end]));
                start = end + 1;
            }
            if parts.len() == 1 {
                mount_sources(hwnd, vec![PathBuf::from(&parts[0])]);
            } else if parts.len() > 1 {
                let dir = PathBuf::from(&parts[0]);
                let all: Vec<PathBuf> = parts[1..].iter().map(|n| dir.join(n)).collect();
                mount_sources(hwnd, all);
            }
        }
    }

    unsafe fn open_folder_dialog(hwnd: *mut c_void) {
        let mut display_buf = [0u16; 260];
        let title = wstr("Select Folder Containing .MLV, .mcraw or .braw Clips");
        let bi = BrowseInfoW {
            hwnd_owner: hwnd,
            pidl_root: ptr::null(),
            psz_display_name: display_buf.as_mut_ptr(),
            lpsz_title: title.as_ptr(),
            ul_flags: 0x0000_0001 | 0x0000_0040, // BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE
            lpfn: ptr::null(),
            l_param: 0,
            i_image: 0,
        };
        let pidl = SHBrowseForFolderW(&bi);
        if !pidl.is_null() {
            let mut path_buf = [0u16; 1024];
            if SHGetPathFromIDListW(pidl, path_buf.as_mut_ptr()) != 0 {
                let len = path_buf.iter().position(|&c| c == 0).unwrap_or(0);
                if len > 0 {
                    let path_str = String::from_utf16_lossy(&path_buf[..len]);
                    mount_source_path(hwnd, PathBuf::from(path_str));
                }
            }
            CoTaskMemFree(pidl);
        }
    }

    unsafe fn prompt_locate_resolve(hwnd: *mut c_void) {
        let mut file_buf = [0u16; 1024];
        let mut filter: Vec<u16> = Vec::new();
        filter.extend("DaVinci Resolve (Resolve.exe;fuscript.exe)".encode_utf16());
        filter.push(0);
        filter.extend("Resolve.exe;fuscript.exe;*.exe".encode_utf16());
        filter.push(0);
        filter.push(0);

        let title = wstr("Locate DaVinci Resolve (Resolve.exe or fuscript.exe)");
        let init_dir = wstr(r"C:\Program Files\Blackmagic Design\DaVinci Resolve");
        let mut ofn: OpenFileNameW = zeroed();
        ofn.l_struct_size = size_of::<OpenFileNameW>() as u32;
        ofn.hwnd_owner = hwnd;
        ofn.lpstr_filter = filter.as_ptr();
        ofn.lpstr_file = file_buf.as_mut_ptr();
        ofn.n_max_file = file_buf.len() as u32;
        ofn.lpstr_initial_dir = init_dir.as_ptr();
        ofn.lpstr_title = title.as_ptr();
        ofn.flags = 0x0008_0000 | 0x0000_1000 | 0x0000_0800;

        if GetOpenFileNameW(&mut ofn) != 0 {
            let len = file_buf.iter().position(|&c| c == 0).unwrap_or(0);
            if len > 0 {
                let picked = PathBuf::from(String::from_utf16_lossy(&file_buf[..len]));
                let fuscript = if picked
                    .file_name()
                    .map(|n| n.to_string_lossy().eq_ignore_ascii_case("Resolve.exe"))
                    .unwrap_or(false)
                {
                    let candidate = picked.with_file_name("fuscript.exe");
                    if candidate.is_file() {
                        candidate
                    } else {
                        picked
                    }
                } else {
                    picked
                };

                if let Ok(mut guard) = APP_STATE.lock() {
                    if let Some(st) = guard.as_mut() {
                        st.resolve_fuscript_path = Some(fuscript.clone());
                        st.status_message = Some((
                            format!("DaVinci Resolve linked: {}", fuscript.display()),
                            false,
                        ));
                    }
                }
                InvalidateRect(hwnd, ptr::null(), 0);
            }
        }
    }

    use cinefs_core::resolve::ResolveOutcome;

    fn run_resolve_import(
        fuscript: Option<PathBuf>,
        clips: &[PathBuf],
        pars: &[cinefs_core::resolve::ClipPar],
    ) -> ResolveOutcome {
        match fuscript {
            Some(f) => cinefs_core::resolve::run_import_with_par(&f, clips, pars, std::time::Duration::from_secs(45)),
            None => {
                cinefs_core::log_warn!("Resolve import: fuscript.exe not found");
                ResolveOutcome::NoFuscript
            }
        }
    }

    /// Imports the mounted clip folders into DaVinci Resolve's Media Pool on a worker thread
    /// (so a slow / hung Resolve never freezes the UI) and reports the *real* outcome.
    unsafe fn import_to_resolve(
        hwnd: *mut c_void,
        mount_idx: Option<usize>,
        specific_clip_idx: Option<usize>,
    ) {
        let (clip_paths, fallback_explorer_path, fuscript_opt, pars) = {
            let Ok(guard) = APP_STATE.lock() else {
                return;
            };
            let Some(st) = guard.as_ref() else {
                return;
            };
            if st.mounts.is_empty() {
                return;
            }
            let mut paths = Vec::new();
            let mut fallback = st.mounts[0].explorer_root_path();
            match mount_idx {
                Some(m_idx) if m_idx < st.mounts.len() => {
                    let m = &st.mounts[m_idx];
                    fallback = m.explorer_root_path();
                    let all_clips = m.full_clip_paths();
                    if let Some(c_idx) = specific_clip_idx {
                        if let Some(cp) = all_clips.get(c_idx) {
                            paths.push(cp.clone());
                        }
                    } else {
                        paths.extend(all_clips);
                    }
                }
                _ => {
                    for m in &st.mounts {
                        paths.extend(m.full_clip_paths());
                    }
                }
            }
            // Clips that need a pixel-aspect fix (crop_rec anamorphic): PAR = pixel width / pixel height
            let pars: Vec<cinefs_core::resolve::ClipPar> = st
                .mounts
                .iter()
                .flat_map(|m| m.clips.iter())
                .filter(|c| (c.stretch - 1.0).abs() > 0.01 && !c.stretch_baked)
                .map(|c| cinefs_core::resolve::ClipPar {
                    folder: c.folder_name.clone(),
                    par: if c.stretch_axis == "horizontal" { c.stretch } else { 1.0 / c.stretch },
                })
                .collect();
            (
                paths,
                fallback,
                st.resolve_fuscript_path
                    .clone()
                    .or_else(locate_resolve_fuscript),
                pars,
            )
        };

        if clip_paths.is_empty() {
            return;
        }
        for p in &clip_paths {
            cinefs_core::log_info!(
                "Resolve import candidate '{}': exists (seen by CineFS) = {}",
                p.display(),
                p.exists()
            );
        }
        set_status(
            hwnd,
            format!("Sending {} clip(s) to DaVinci Resolve...", clip_paths.len()),
            false,
        );

        let hwnd_val = hwnd as usize;
        std::thread::spawn(move || {
            let hwnd = hwnd_val as *mut c_void;
            let outcome = run_resolve_import(fuscript_opt, &clip_paths, &pars);
            let explorer_hint = "The virtual drive was opened in Explorer — drag the clip folder into Resolve's Media Pool.";
            match outcome {
                ResolveOutcome::Imported(n) => set_status(
                    hwnd,
                    format!("Imported {n} item(s) into the DaVinci Resolve Media Pool."),
                    false,
                ),
                ResolveOutcome::ImportedPar { count, par_applied, par_total } => set_status(
                    hwnd,
                    if par_applied == par_total {
                        format!("Imported {count} item(s) into Resolve; pixel aspect ratio set on {par_applied} stretched clip(s).")
                    } else {
                        format!("Imported {count} item(s) into Resolve, but Resolve refused the pixel aspect change on {} of {par_total} stretched clip(s) — see the log (tray > Open Log).", par_total - par_applied)
                    },
                    par_applied != par_total,
                ),
                ResolveOutcome::ImportedNothing => {
                    unsafe { open_path_in_explorer(&fallback_explorer_path, "resolve-fallback") };
                    set_status(
                        hwnd,
                        format!("Resolve imported 0 items — it could not read the virtual drive (is Resolve running as a different user / elevated?). {explorer_hint}"),
                        true,
                    );
                }
                ResolveOutcome::NoProject => {
                    unsafe { open_path_in_explorer(&fallback_explorer_path, "resolve-fallback") };
                    set_status(
                        hwnd,
                        String::from("Open a project in DaVinci Resolve first (virtual drive opened in Explorer)."),
                        true,
                    );
                }
                ResolveOutcome::NoResolve => {
                    unsafe { open_path_in_explorer(&fallback_explorer_path, "resolve-fallback") };
                    set_status(
                        hwnd,
                        format!("Cannot reach DaVinci Resolve. Start Resolve and enable Preferences > System > General > External scripting using: Local (needs Resolve Studio). {explorer_hint}"),
                        true,
                    );
                }
                ResolveOutcome::NoFuscript => {
                    unsafe { open_path_in_explorer(&fallback_explorer_path, "resolve-fallback") };
                    set_status(
                        hwnd,
                        format!("fuscript.exe not found — use 'Locate Resolve'. {explorer_hint}"),
                        true,
                    );
                }
                ResolveOutcome::Failed(why) => {
                    unsafe { open_path_in_explorer(&fallback_explorer_path, "resolve-fallback") };
                    set_status(
                        hwnd,
                        format!("Resolve import failed: {why}. {explorer_hint}"),
                        true,
                    );
                }
            }
        });
    }

    unsafe fn unmount_at_index(hwnd: *mut c_void, idx: usize) {
        let removed_opt = {
            let Ok(mut guard) = APP_STATE.lock() else {
                return;
            };
            let Some(st) = guard.as_mut() else {
                return;
            };
            if idx < st.mounts.len() {
                Some(st.mounts.remove(idx))
            } else {
                None
            }
        };

        if let Some(mut removed) = removed_opt {
            player_win::close_for(&removed.vfs);
            let mp_display = removed.mount_point.display().to_string();
            cinefs_core::log_info!("unmount '{mp_display}'");
            let _ = removed.volume.unmount();
            if let Ok(mut guard) = APP_STATE.lock() {
                if let Some(st) = guard.as_mut() {
                    st.status_message = Some((format!("Unmounted {mp_display}."), false));
                }
            }
            InvalidateRect(hwnd, ptr::null(), 0);
        }
    }

    unsafe fn unmount_all(hwnd: *mut c_void) {
        let drained: Vec<WinMountEntry> = {
            let Ok(mut guard) = APP_STATE.lock() else {
                return;
            };
            let Some(st) = guard.as_mut() else {
                return;
            };
            st.mounts.drain(..).collect()
        };

        player_win::close_all();
        for mut m in drained {
            cinefs_core::log_info!("unmount '{}'", m.mount_point.display());
            let _ = m.volume.unmount();
        }

        if let Ok(mut guard) = APP_STATE.lock() {
            if let Some(st) = guard.as_mut() {
                st.scroll_y = 0;
                st.status_message =
                    Some((String::from("All virtual volumes unmounted."), false));
            }
        }
        InvalidateRect(hwnd, ptr::null(), 0);
    }

    unsafe fn setup_tray_icon(hwnd: *mut c_void, icon: *mut c_void, add: bool) {
        let mut nid: NotifyIconDataW = zeroed();
        nid.cb_size = size_of::<NotifyIconDataW>() as u32;
        nid.h_wnd = hwnd;
        nid.u_id = 1;
        if add {
            nid.u_flags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
            nid.u_callback_message = WM_TRAYICON;
            nid.h_icon = if icon.is_null() {
                LoadIconW(ptr::null_mut(), IDI_APPLICATION)
            } else {
                icon
            };
            for (i, ch) in "CineFS — Virtual CinemaDNG Mount Engine"
                .encode_utf16()
                .take(127)
                .enumerate()
            {
                nid.sz_tip[i] = ch;
            }
            Shell_NotifyIconW(NIM_ADD, &nid);
        } else {
            Shell_NotifyIconW(NIM_DELETE, &nid);
        }
    }

    unsafe fn show_tray_menu(hwnd: *mut c_void) {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        let s_show = wstr("Open CineFS Studio Window");
        let s_add = wstr("Mount .MLV / .mcraw / .braw Clip(s)...");
        let s_dir = wstr("Mount Folder...");
        let s_res = wstr("Import All to DaVinci Resolve");
        let s_unm = wstr("Unmount All Drives");
        let s_log = wstr("Open Log File (diagnostics)");
        let s_str = wstr(&format!(
            "Pixel stretch for next mounts: {}  (click to change)",
            stretch_mode_label(STRETCH_MODE.load(std::sync::atomic::Ordering::Relaxed))
        ));
        let s_exit = wstr("Unmount All && Quit CineFS");

        AppendMenuW(menu, MF_STRING, ID_TRAY_SHOW, s_show.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(menu, MF_STRING, ID_TRAY_ADD_FILE, s_add.as_ptr());
        AppendMenuW(menu, MF_STRING, ID_TRAY_ADD_FOLDER, s_dir.as_ptr());
        AppendMenuW(menu, MF_STRING, ID_TRAY_RESOLVE_ALL, s_res.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(menu, MF_STRING, ID_TRAY_UNMOUNT_ALL, s_unm.as_ptr());
        let s_bake = wstr(if cinefs_core::dng::stretch_bake() {
            "Stretch method: Baked into DNG pixels (universal)  (click to change)"
        } else {
            "Stretch method: Metadata only (fast)  (click to change)"
        });
        AppendMenuW(menu, MF_STRING, ID_TRAY_BAKE, s_bake.as_ptr());
        AppendMenuW(menu, MF_STRING, ID_TRAY_STRETCH, s_str.as_ptr());
        let s_fp = wstr(if cinefs_core::focus_fix::focus_fix_enabled() {
            "Focus pixels: Fix PDAF dots (EOS M / 650D / 700D / 100D)  (click to change)"
        } else {
            "Focus pixels: Off  (click to change)"
        });
        AppendMenuW(menu, MF_STRING, ID_TRAY_FOCUS, s_fp.as_ptr());

        AppendMenuW(menu, MF_STRING, ID_TRAY_OPEN_LOG, s_log.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(menu, MF_STRING, ID_TRAY_EXIT, s_exit.as_ptr());

        let mut pt = Point { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        SetForegroundWindow(hwnd);
        TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_BOTTOMALIGN,
            pt.x,
            pt.y,
            0,
            hwnd,
            ptr::null(),
        );
        DestroyMenu(menu);
    }

    unsafe fn show_mode_popup(hwnd: *mut c_void, btn_rect: Rect, cur_mode: DngPayloadMode) {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        let modes = [
            (
                ID_MODE_AUTO,
                DngPayloadMode::Auto,
                "Auto (Fastest — Zero-Decode LJ92 / Native)",
            ),
            (
                ID_MODE_PASSTHROUGH,
                DngPayloadMode::Lj92Passthrough,
                "LJ92 Passthrough (Compression = 7)",
            ),
            (
                ID_MODE_NATIVE,
                DngPayloadMode::NativePacked,
                "Native Bit-Depth Packed (10/12/14-bit)",
            ),
            (
                ID_MODE_UNPACKED16,
                DngPayloadMode::Unpacked16,
                "Unpacked 16-bit Linear (Adobe Premiere)",
            ),
        ];
        for (id, m, label) in modes {
            let lw = wstr(label);
            let flags = if m == cur_mode {
                MF_STRING | MF_CHECKED
            } else {
                MF_STRING
            };
            AppendMenuW(menu, flags, id, lw.as_ptr());
        }

        let mut pt = Point {
            x: btn_rect.left,
            y: btn_rect.bottom + 4,
        };
        ClientToScreen(hwnd, &mut pt);
        TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_TOPALIGN,
            pt.x,
            pt.y,
            0,
            hwnd,
            ptr::null(),
        );
        DestroyMenu(menu);
    }

    // =========================================================================
    // Complete Studio Dark UI Renderer (`WM_PAINT`)
    // =========================================================================
    struct TextDrawCmd {
        text: String,
        rect: Rect,
        color: u32,
        font_kind: FontKind,
        flags: u32,
    }

    #[derive(Clone, Copy)]
    enum FontKind {
        TitleBold,
        Subtitle,
        SectionTitle,
        BodySemibold,
        BodyRegular,
        MonoClip,
        BadgeSmall,
    }

    /// Table column rectangles (left, right) laid out from the right edge:
    /// `[mode, audio, stretch, frames, fps, bit, resolution]`.
    fn table_cols(right: i32, dpi: i32) -> [(i32, i32); 7] {
        let widths = [116, 44, 76, 58, 54, 36, 96];
        let gap = scale_dpi(10, dpi);
        let mut x = right;
        let mut out = [(0, 0); 7];
        for (i, w) in widths.iter().enumerate() {
            let w = scale_dpi(*w, dpi);
            out[i] = (x - w, x);
            x = x - w - gap;
        }
        out
    }

    unsafe fn render_window(_hwnd: *mut c_void, hdc_win: *mut c_void, client_w: i32, client_h: i32) {
        if client_w <= 0 || client_h <= 0 {
            return;
        }

        let hdc_mem = CreateCompatibleDC(hdc_win);
        if hdc_mem.is_null() {
            return;
        }

        let bmi = BitmapInfo {
            bmi_header: BitmapInfoHeader {
                bi_size: size_of::<BitmapInfoHeader>() as u32,
                bi_width: client_w,
                bi_height: -client_h, // top-down DIB
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

        let mut bits_ptr: *mut u32 = ptr::null_mut();
        let h_bm = CreateDIBSection(
            hdc_mem,
            &bmi,
            DIB_RGB_COLORS,
            &mut bits_ptr,
            ptr::null_mut(),
            0,
        );
        if h_bm.is_null() || bits_ptr.is_null() {
            DeleteDC(hdc_mem);
            return;
        }
        let old_bm = SelectObject(hdc_mem, h_bm);
        let pixel_count = (client_w * client_h) as usize;
        let pixels = std::slice::from_raw_parts_mut(bits_ptr, pixel_count);
        let mut canvas = Canvas {
            pixels,
            width: client_w,
            height: client_h,
        };
        canvas.clear(COL_BG);

        let mut text_cmds: Vec<TextDrawCmd> = Vec::with_capacity(64);
        let mut dpi = 96;
        let mut icon_handle: *mut c_void = ptr::null_mut();
        let mut icon_rect = (0, 0, 0);

        if let Ok(mut guard) = APP_STATE.lock() {
            if let Some(st) = guard.as_mut() {
                dpi = st.dpi;
                icon_handle = st.h_icon_big;
                st.hit_boxes.clear();

                if st.is_setup_mode {
                    render_setup_view(
                        st,
                        &mut canvas,
                        &mut text_cmds,
                        &mut icon_rect,
                        client_w,
                        client_h,
                    );
                } else {
                    render_main_studio_view(
                        st,
                        &mut canvas,
                        &mut text_cmds,
                        &mut icon_rect,
                        client_w,
                        client_h,
                    );
                }
            }
        }

        GdiFlush();

        // Draw embedded high-res app icon in header
        if !icon_handle.is_null() && icon_rect.2 > 0 {
            DrawIconEx(
                hdc_mem,
                icon_rect.0,
                icon_rect.1,
                icon_handle,
                icon_rect.2,
                icon_rect.2,
                0,
                ptr::null_mut(),
                0x0003, // DI_NORMAL
            );
        }

        // Create DPI-scaled ClearType fonts and draw all text commands
        let mk_font = |height_px: i32, weight: i32, face: &str| -> *mut c_void {
            let f_w = wstr(face);
            CreateFontW(
                -scale_dpi(height_px, dpi),
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                1,
                0,
                0,
                CLEARTYPE_QUALITY,
                0,
                f_w.as_ptr(),
            )
        };

        let f_title = mk_font(18, 600, "Segoe UI Semibold");
        let f_sub = mk_font(12, 400, "Segoe UI");
        let f_sec = mk_font(16, 600, "Segoe UI Semibold");
        let f_semi = mk_font(13, 600, "Segoe UI Semibold");
        let f_reg = mk_font(12, 400, "Segoe UI");
        let f_mono = mk_font(12, 400, "Consolas");
        let f_badge = mk_font(11, 600, "Segoe UI Semibold");

        SetBkMode(hdc_mem, TRANSPARENT);
        for cmd in &text_cmds {
            let hfont = match cmd.font_kind {
                FontKind::TitleBold => f_title,
                FontKind::Subtitle => f_sub,
                FontKind::SectionTitle => f_sec,
                FontKind::BodySemibold => f_semi,
                FontKind::BodyRegular => f_reg,
                FontKind::MonoClip => f_mono,
                FontKind::BadgeSmall => f_badge,
            };
            let prev_f = SelectObject(hdc_mem, hfont);
            SetTextColor(hdc_mem, rgb_to_colorref(cmd.color));
            let w_str = wstr(&cmd.text);
            let mut rc = cmd.rect;
            DrawTextW(
                hdc_mem,
                w_str.as_ptr(),
                (w_str.len() - 1) as i32,
                &mut rc,
                cmd.flags | DT_NOPREFIX,
            );
            SelectObject(hdc_mem, prev_f);
        }

        for hf in [f_title, f_sub, f_sec, f_semi, f_reg, f_mono, f_badge] {
            if !hf.is_null() {
                DeleteObject(hf);
            }
        }

        BitBlt(hdc_win, 0, 0, client_w, client_h, hdc_mem, 0, 0, SRCCOPY);
        SelectObject(hdc_mem, old_bm);
        DeleteObject(h_bm);
        DeleteDC(hdc_mem);
    }

    fn render_button(
        st: &mut AppState,
        canvas: &mut Canvas<'_>,
        text_cmds: &mut Vec<TextDrawCmd>,
        rect: Rect,
        label: &str,
        action: HitAction,
        style: BtnStyle,
        dpi: i32,
    ) {
        let is_hover = st.hovered_action.as_ref() == Some(&action);
        let is_press = st.pressed_action.as_ref() == Some(&action);
        let r = scale_dpi(7, dpi) as f32;

        let (bg, border, text_col) = match style {
            BtnStyle::PrimaryAmber => {
                let fill = if is_press {
                    COL_ACCENT_DARK
                } else if is_hover {
                    COL_ACCENT_HOVER
                } else {
                    COL_ACCENT
                };
                (fill, None, COL_TEXT_DARK)
            }
            BtnStyle::Secondary => {
                let fill = if is_hover {
                    COL_BTN_SEC_HOVER
                } else {
                    COL_BTN_SEC
                };
                (fill, Some((COL_BTN_SEC_BORDER, 1.0)), COL_TEXT_PRIMARY)
            }
            BtnStyle::Danger => {
                let fill = if is_hover {
                    COL_BTN_DANGER_HOVER
                } else {
                    COL_BTN_DANGER_BG
                };
                (
                    fill,
                    Some((COL_BTN_DANGER_BORDER, 1.0)),
                    COL_BTN_DANGER_TEXT,
                )
            }
            BtnStyle::Emerald => {
                let fill = if is_hover { 0x154035 } else { COL_EMERALD_BG };
                (fill, Some((COL_EMERALD_BORDER, 1.0)), COL_EMERALD_TEXT)
            }
        };

        canvas.draw_rounded_rect(
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            r,
            bg,
            255,
            border,
        );
        text_cmds.push(TextDrawCmd {
            text: label.to_string(),
            rect,
            color: text_col,
            font_kind: FontKind::BodySemibold,
            flags: DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        });
        st.hit_boxes.push(HitBox { rect, action });
    }

    #[derive(Clone, Copy)]
    enum BtnStyle {
        PrimaryAmber,
        Secondary,
        Danger,
        Emerald,
    }

    fn render_setup_view(
        st: &mut AppState,
        canvas: &mut Canvas<'_>,
        text_cmds: &mut Vec<TextDrawCmd>,
        icon_rect: &mut (i32, i32, i32),
        w: i32,
        h: i32,
    ) {
        let dpi = st.dpi;
        let pad = scale_dpi(28, dpi);
        let card_w = (w - pad * 2).min(scale_dpi(640, dpi));
        let card_h = (h - pad * 2).min(scale_dpi(420, dpi));
        let cx = (w - card_w) / 2;
        let cy = (h - card_h) / 2;

        canvas.draw_rounded_rect(
            cx,
            cy,
            card_w,
            card_h,
            scale_dpi(12, dpi) as f32,
            COL_CARD_BG,
            255,
            Some((COL_CARD_BORDER, 1.2)),
        );

        let ico_sz = scale_dpi(48, dpi);
        *icon_rect = (cx + scale_dpi(28, dpi), cy + scale_dpi(24, dpi), ico_sz);

        text_cmds.push(TextDrawCmd {
            text: String::from("CineFS Studio Setup for Windows"),
            rect: Rect {
                left: cx + scale_dpi(88, dpi),
                top: cy + scale_dpi(24, dpi),
                right: cx + card_w - scale_dpi(24, dpi),
                bottom: cy + scale_dpi(50, dpi),
            },
            color: COL_TEXT_PRIMARY,
            font_kind: FontKind::TitleBold,
            flags: DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        });
        text_cmds.push(TextDrawCmd {
            text: String::from(
                "One-click installer: bundles CineFS Desktop, CLI & WinFsp 2.0 Virtual Drive Driver",
            ),
            rect: Rect {
                left: cx + scale_dpi(88, dpi),
                top: cy + scale_dpi(50, dpi),
                right: cx + card_w - scale_dpi(24, dpi),
                bottom: cy + scale_dpi(72, dpi),
            },
            color: COL_TEXT_SECONDARY,
            font_kind: FontKind::Subtitle,
            flags: DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        });

        canvas.fill_rect(
            cx + scale_dpi(24, dpi),
            cy + scale_dpi(86, dpi),
            card_w - scale_dpi(48, dpi),
            1,
            COL_DIVIDER,
        );

        let opts = [
            (
                st.setup_desktop_icon,
                HitAction::SetupToggleDesktopIcon,
                "Create Desktop & Start Menu shortcuts for CineFS",
            ),
            (
                st.setup_context_menu,
                HitAction::SetupToggleContextMenu,
                "Add 'Mount with CineFS' to Windows Explorer right-click menu (.MLV, .mcraw, .braw & folders)",
            ),
            (
                st.setup_defender_exclusion,
                HitAction::SetupToggleDefender,
                "Configure Windows Defender exclusion for zero-latency 4K RAW playback",
            ),
        ];

        let mut oy = cy + scale_dpi(108, dpi);
        for (checked, act, label) in opts {
            let box_sz = scale_dpi(20, dpi);
            let bx = cx + scale_dpi(28, dpi);
            let by = oy;
            canvas.draw_rounded_rect(
                bx,
                by,
                box_sz,
                box_sz,
                scale_dpi(5, dpi) as f32,
                if checked { COL_ACCENT } else { COL_BTN_SEC },
                255,
                Some((
                    if checked {
                        COL_ACCENT
                    } else {
                        COL_BTN_SEC_BORDER
                    },
                    1.0,
                )),
            );
            if checked {
                canvas.draw_line(
                    (bx + scale_dpi(5, dpi)) as f32,
                    (by + scale_dpi(10, dpi)) as f32,
                    (bx + scale_dpi(9, dpi)) as f32,
                    (by + scale_dpi(14, dpi)) as f32,
                    scale_dpi(2, dpi) as f32,
                    COL_TEXT_DARK,
                );
                canvas.draw_line(
                    (bx + scale_dpi(9, dpi)) as f32,
                    (by + scale_dpi(14, dpi)) as f32,
                    (bx + scale_dpi(15, dpi)) as f32,
                    (by + scale_dpi(6, dpi)) as f32,
                    scale_dpi(2, dpi) as f32,
                    COL_TEXT_DARK,
                );
            }
            let row_rc = Rect {
                left: bx,
                top: by - scale_dpi(4, dpi),
                right: cx + card_w - scale_dpi(28, dpi),
                bottom: by + box_sz + scale_dpi(4, dpi),
            };
            text_cmds.push(TextDrawCmd {
                text: String::from(label),
                rect: Rect {
                    left: bx + box_sz + scale_dpi(12, dpi),
                    ..row_rc
                },
                color: COL_TEXT_PRIMARY,
                font_kind: FontKind::BodyRegular,
                flags: DT_LEFT | DT_VCENTER | DT_SINGLELINE,
            });
            st.hit_boxes.push(HitBox {
                rect: row_rc,
                action: act,
            });
            oy += scale_dpi(38, dpi);
        }

        // Bundled WinFsp status pill
        let info_rc = Rect {
            left: cx + scale_dpi(28, dpi),
            top: oy + scale_dpi(8, dpi),
            right: cx + card_w - scale_dpi(28, dpi),
            bottom: oy + scale_dpi(52, dpi),
        };
        canvas.draw_rounded_rect(
            info_rc.left,
            info_rc.top,
            info_rc.right - info_rc.left,
            info_rc.bottom - info_rc.top,
            scale_dpi(8, dpi) as f32,
            COL_EMERALD_BG,
            255,
            Some((COL_EMERALD_BORDER, 1.0)),
        );
        let wf_text = if st.winfsp_installed {
            "✓ WinFsp 2.0 Driver is already installed on this PC — ready for instant mounting."
        } else {
            "✓ WinFsp 2.0 Driver is bundled inside this installer and will be installed automatically."
        };
        text_cmds.push(TextDrawCmd {
            text: String::from(wf_text),
            rect: Rect {
                left: info_rc.left + scale_dpi(14, dpi),
                ..info_rc
            },
            color: COL_EMERALD_TEXT,
            font_kind: FontKind::BodySemibold,
            flags: DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        });

        let btn_y = cy + card_h - scale_dpi(58, dpi);
        let btn_h = scale_dpi(38, dpi);
        render_button(
            st,
            canvas,
            text_cmds,
            Rect {
                left: cx + scale_dpi(28, dpi),
                top: btn_y,
                right: cx + scale_dpi(210, dpi),
                bottom: btn_y + btn_h,
            },
            "Run Portable Without Setup",
            HitAction::SetupLaunchPortable,
            BtnStyle::Secondary,
            dpi,
        );
        render_button(
            st,
            canvas,
            text_cmds,
            Rect {
                left: cx + card_w - scale_dpi(270, dpi),
                top: btn_y,
                right: cx + card_w - scale_dpi(28, dpi),
                bottom: btn_y + btn_h,
            },
            "Install CineFS + WinFsp Now",
            HitAction::SetupRunInstall,
            BtnStyle::PrimaryAmber,
            dpi,
        );
    }

    fn render_main_studio_view(
        st: &mut AppState,
        canvas: &mut Canvas<'_>,
        text_cmds: &mut Vec<TextDrawCmd>,
        icon_rect: &mut (i32, i32, i32),
        w: i32,
        h: i32,
    ) {
        let dpi = st.dpi;
        let header_h = scale_dpi(68, dpi);
        let footer_h = scale_dpi(46, dpi);
        let pad_x = scale_dpi(18, dpi);

        // 1. Header Bar
        canvas.fill_rect(0, 0, w, header_h, COL_HEADER_BG);
        canvas.fill_rect(0, header_h - 1, w, 1, COL_DIVIDER);

        let ico_sz = scale_dpi(38, dpi);
        *icon_rect = (pad_x, (header_h - ico_sz) / 2, ico_sz);

        let title_x = pad_x + ico_sz + scale_dpi(12, dpi);
        text_cmds.push(TextDrawCmd {
            text: String::from("CineFS"),
            rect: Rect {
                left: title_x,
                top: scale_dpi(12, dpi),
                right: title_x + scale_dpi(260, dpi),
                bottom: scale_dpi(36, dpi),
            },
            color: COL_TEXT_PRIMARY,
            font_kind: FontKind::TitleBold,
            flags: DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        });
        text_cmds.push(TextDrawCmd {
            text: String::from("Zero-Copy Virtual CinemaDNG Mount Engine"),
            rect: Rect {
                left: title_x,
                top: scale_dpi(36, dpi),
                right: title_x + scale_dpi(300, dpi),
                bottom: scale_dpi(56, dpi),
            },
            color: COL_TEXT_SECONDARY,
            font_kind: FontKind::Subtitle,
            flags: DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        });

        // Header Right Controls: Mode Picker, + Add Folder, + Add Clip
        let btn_h = scale_dpi(34, dpi);
        let btn_y = (header_h - btn_h) / 2;

        let add_clip_w = scale_dpi(164, dpi);
        let add_clip_rc = Rect {
            left: w - pad_x - add_clip_w,
            top: btn_y,
            right: w - pad_x,
            bottom: btn_y + btn_h,
        };
        render_button(
            st,
            canvas,
            text_cmds,
            add_clip_rc,
            "+ Add .MLV / .mcraw / .braw",
            HitAction::AddFile,
            BtnStyle::PrimaryAmber,
            dpi,
        );

        let add_dir_w = scale_dpi(118, dpi);
        let add_dir_rc = Rect {
            left: add_clip_rc.left - scale_dpi(8, dpi) - add_dir_w,
            top: btn_y,
            right: add_clip_rc.left - scale_dpi(8, dpi),
            bottom: btn_y + btn_h,
        };
        render_button(
            st,
            canvas,
            text_cmds,
            add_dir_rc,
            "+ Add Folder",
            HitAction::AddFolder,
            BtnStyle::Secondary,
            dpi,
        );

        let mode_w = scale_dpi(188, dpi);
        let mode_rc = Rect {
            left: add_dir_rc.left - scale_dpi(8, dpi) - mode_w,
            top: btn_y,
            right: add_dir_rc.left - scale_dpi(8, dpi),
            bottom: btn_y + btn_h,
        };
        let mode_label = match st.selected_mode {
            DngPayloadMode::Auto => "Mode: Auto (Fastest) ▾",
            DngPayloadMode::Lj92Passthrough => "Mode: LJ92 Passthrough ▾",
            DngPayloadMode::NativePacked => "Mode: Native Packed ▾",
            DngPayloadMode::Unpacked16 => "Mode: 16-bit Unpacked ▾",
        };
        render_button(
            st,
            canvas,
            text_cmds,
            mode_rc,
            mode_label,
            HitAction::SelectModeMenu,
            BtnStyle::Secondary,
            dpi,
        );

        // 2. Optional Top Banner (WinFsp Missing OR Status/Error Banner)
        let mut content_top = header_h;
        let content_bottom = h - footer_h;

        if !st.winfsp_installed {
            let ban_h = scale_dpi(44, dpi);
            canvas.fill_rect(0, content_top, w, ban_h, COL_AMBER_BG);
            canvas.fill_rect(0, content_top + ban_h - 1, w, 1, COL_AMBER_BORDER);
            let inst_btn_w = scale_dpi(210, dpi);
            let inst_btn_h = scale_dpi(28, dpi);
            let inst_rc = Rect {
                left: w - pad_x - inst_btn_w,
                top: content_top + (ban_h - inst_btn_h) / 2,
                right: w - pad_x,
                bottom: content_top + (ban_h - inst_btn_h) / 2 + inst_btn_h,
            };
            render_button(
                st,
                canvas,
                text_cmds,
                inst_rc,
                "Install Bundled WinFsp Driver",
                HitAction::InstallWinFsp,
                BtnStyle::PrimaryAmber,
                dpi,
            );
            text_cmds.push(TextDrawCmd {
                text: String::from(
                    "⚠ WinFsp virtual drive driver is not installed yet — click to install the bundled driver in 1 click.",
                ),
                rect: Rect {
                    left: pad_x,
                    top: content_top,
                    right: inst_rc.left - scale_dpi(12, dpi),
                    bottom: content_top + ban_h,
                },
                color: COL_AMBER_TEXT,
                font_kind: FontKind::BodySemibold,
                flags: DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
            });
            content_top += ban_h;
        } else if let Some((ref msg, is_err)) = st.status_message.clone() {
            let ban_h = scale_dpi(36, dpi);
            let (bg, border, fg) = if is_err {
                (COL_ERR_BG, COL_ERR_BORDER, COL_ERR_TEXT)
            } else {
                (COL_EMERALD_BG, COL_EMERALD_BORDER, COL_EMERALD_TEXT)
            };
            canvas.fill_rect(0, content_top, w, ban_h, bg);
            canvas.fill_rect(0, content_top + ban_h - 1, w, 1, border);
            let prefix = if is_err { "⚠  " } else { "✓  " };
            let dis_w = scale_dpi(68, dpi);
            let dis_h = scale_dpi(24, dpi);
            let dis_rc = Rect {
                left: w - pad_x - dis_w,
                top: content_top + (ban_h - dis_h) / 2,
                right: w - pad_x,
                bottom: content_top + (ban_h - dis_h) / 2 + dis_h,
            };
            render_button(
                st,
                canvas,
                text_cmds,
                dis_rc,
                "Dismiss",
                HitAction::DismissBanner,
                BtnStyle::Secondary,
                dpi,
            );
            text_cmds.push(TextDrawCmd {
                text: format!("{prefix}{msg}"),
                rect: Rect {
                    left: pad_x,
                    top: content_top,
                    right: dis_rc.left - scale_dpi(12, dpi),
                    bottom: content_top + ban_h,
                },
                color: fg,
                font_kind: FontKind::BodyRegular,
                flags: DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
            });
            content_top += ban_h;
        }

        // 3. Main Content Area (Empty Drop Zone vs Scrollable Mount Cards)
        if st.mounts.is_empty() {
            st.max_scroll_y = 0;
            st.scroll_y = 0;
            let margin = scale_dpi(18, dpi);
            let dz_x = margin;
            let dz_y = content_top + margin;
            let dz_w = w - margin * 2;
            let dz_h = (content_bottom - content_top) - margin * 2;

            canvas.draw_dashed_rounded_rect(
                dz_x,
                dz_y,
                dz_w,
                dz_h,
                scale_dpi(12, dpi) as f32,
                COL_DROP_BG,
                COL_DROP_BORDER,
                scale_dpi(14, dpi),
            );

            let mid_x = dz_x + dz_w / 2;
            let mid_y = dz_y + dz_h / 2 - scale_dpi(24, dpi);

            // Vector Cinema Drop Circle + Arrow Icon
            let circ_r = scale_dpi(30, dpi) as f32;
            canvas.fill_circle(mid_x as f32, (mid_y - scale_dpi(28, dpi)) as f32, circ_r, 0x22293A, 255);
            let acx = mid_x as f32;
            let acy = (mid_y - scale_dpi(28, dpi)) as f32;
            let s = scale_dpi(10, dpi) as f32;
            let lw = scale_dpi(2, dpi).max(2) as f32;
            canvas.draw_line(acx, acy - s, acx, acy + s * 0.7, lw, COL_ACCENT);
            canvas.draw_line(acx - s * 0.7, acy + s * 0.1, acx, acy + s * 0.8, lw, COL_ACCENT);
            canvas.draw_line(acx + s * 0.7, acy + s * 0.1, acx, acy + s * 0.8, lw, COL_ACCENT);
            canvas.draw_line(acx - s, acy + s * 1.15, acx + s, acy + s * 1.15, lw, COL_ACCENT);

            text_cmds.push(TextDrawCmd {
                text: String::from("Drop .MLV / .mcraw / .braw Clips or Folders Here"),
                rect: Rect {
                    left: dz_x + scale_dpi(24, dpi),
                    top: mid_y + scale_dpi(10, dpi),
                    right: dz_x + dz_w - scale_dpi(24, dpi),
                    bottom: mid_y + scale_dpi(38, dpi),
                },
                color: COL_TEXT_PRIMARY,
                font_kind: FontKind::SectionTitle,
                flags: DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            });

            text_cmds.push(TextDrawCmd {
                text: String::from(
                    "Instantly mounts virtual CinemaDNG frame sequences + Broadcast WAV as a native Windows drive (Z:, Y:, ...)\nwith zero disk duplication and zero-decode LJ92 passthrough for DaVinci Resolve, Premiere Pro & After Effects.",
                ),
                rect: Rect {
                    left: mid_x - scale_dpi(310, dpi),
                    top: mid_y + scale_dpi(42, dpi),
                    right: mid_x + scale_dpi(310, dpi),
                    bottom: mid_y + scale_dpi(84, dpi),
                },
                color: COL_TEXT_SECONDARY,
                font_kind: FontKind::BodyRegular,
                flags: DT_CENTER | DT_WORDBREAK,
            });

            let b_w1 = scale_dpi(190, dpi);
            let b_w2 = scale_dpi(145, dpi);
            let gap = scale_dpi(10, dpi);
            let total_bw = b_w1 + gap + b_w2;
            let bx0 = mid_x - total_bw / 2;
            let by0 = mid_y + scale_dpi(94, dpi);
            let bh0 = scale_dpi(34, dpi);

            render_button(
                st,
                canvas,
                text_cmds,
                Rect {
                    left: bx0,
                    top: by0,
                    right: bx0 + b_w1,
                    bottom: by0 + bh0,
                },
                "Browse .MLV / .mcraw / .braw Files...",
                HitAction::AddFile,
                BtnStyle::PrimaryAmber,
                dpi,
            );
            render_button(
                st,
                canvas,
                text_cmds,
                Rect {
                    left: bx0 + b_w1 + gap,
                    top: by0,
                    right: bx0 + total_bw,
                    bottom: by0 + bh0,
                },
                "Browse Folder...",
                HitAction::AddFolder,
                BtnStyle::Secondary,
                dpi,
            );
        } else {
            // Render scrollable list of Mount Cards
            let card_gap = scale_dpi(12, dpi);
            let card_hdr_h = scale_dpi(56, dpi);
            let clip_row_h = scale_dpi(34, dpi);
            let table_hdr_h = scale_dpi(26, dpi);
            let card_pad = scale_dpi(12, dpi);

            let mut total_h = scale_dpi(16, dpi);
            for m in &st.mounts {
                let ch = card_hdr_h + card_pad + table_hdr_h + (m.clips.len() as i32) * (clip_row_h + scale_dpi(4, dpi));
                total_h += ch + card_gap;
            }
            let view_h = (content_bottom - content_top).max(1);
            st.max_scroll_y = (total_h - view_h).max(0);
            st.scroll_y = st.scroll_y.clamp(0, st.max_scroll_y);

            let mut cur_y = content_top + scale_dpi(14, dpi) - st.scroll_y;
            let num_mounts = st.mounts.len();

            for m_idx in 0..num_mounts {
                let clip_count = st.mounts[m_idx].clips.len();
                let card_h =
                    card_hdr_h + card_pad + table_hdr_h + (clip_count as i32) * (clip_row_h + scale_dpi(4, dpi));
                let card_x = pad_x;
                let card_w = w - pad_x * 2;

                if cur_y + card_h >= content_top && cur_y <= content_bottom {
                    canvas.draw_rounded_rect(
                        card_x,
                        cur_y,
                        card_w,
                        card_h,
                        scale_dpi(10, dpi) as f32,
                        COL_CARD_BG,
                        255,
                        Some((COL_CARD_BORDER, 1.0)),
                    );

                    // Drive Letter Emerald Badge (e.g. `Z:\`)
                    let drv_w = scale_dpi(46, dpi);
                    let drv_h = scale_dpi(32, dpi);
                    let drv_x = card_x + scale_dpi(14, dpi);
                    let drv_y = cur_y + scale_dpi(12, dpi);
                    canvas.draw_rounded_rect(
                        drv_x,
                        drv_y,
                        drv_w,
                        drv_h,
                        scale_dpi(6, dpi) as f32,
                        COL_EMERALD_BG,
                        255,
                        Some((COL_EMERALD_BORDER, 1.0)),
                    );
                    let drv_badge = st.mounts[m_idx].drive_badge.clone();
                    text_cmds.push(TextDrawCmd {
                        text: drv_badge,
                        rect: Rect {
                            left: drv_x,
                            top: drv_y,
                            right: drv_x + drv_w,
                            bottom: drv_y + drv_h,
                        },
                        color: COL_EMERALD_TEXT,
                        font_kind: FontKind::BodySemibold,
                        flags: DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                    });

                    // Card Action Buttons: `[ Open in Explorer ]`, `[ Import to Resolve ]`, `[ Unmount ]`
                    let act_h = scale_dpi(28, dpi);
                    let act_y = cur_y + scale_dpi(14, dpi);
                    let unm_w = scale_dpi(84, dpi);
                    let res_w = scale_dpi(140, dpi);
                    let exp_w = scale_dpi(124, dpi);
                    let b_gap = scale_dpi(6, dpi);

                    let unm_rc = Rect {
                        left: card_x + card_w - scale_dpi(14, dpi) - unm_w,
                        top: act_y,
                        right: card_x + card_w - scale_dpi(14, dpi),
                        bottom: act_y + act_h,
                    };
                    let res_rc = Rect {
                        left: unm_rc.left - b_gap - res_w,
                        top: act_y,
                        right: unm_rc.left - b_gap,
                        bottom: act_y + act_h,
                    };
                    let exp_rc = Rect {
                        left: res_rc.left - b_gap - exp_w,
                        top: act_y,
                        right: res_rc.left - b_gap,
                        bottom: act_y + act_h,
                    };

                    render_button(
                        st,
                        canvas,
                        text_cmds,
                        exp_rc,
                        "Open in Explorer",
                        HitAction::CardExplorer(m_idx),
                        BtnStyle::Secondary,
                        dpi,
                    );
                    render_button(
                        st,
                        canvas,
                        text_cmds,
                        res_rc,
                        "Import to Resolve",
                        HitAction::CardResolve(m_idx),
                        BtnStyle::PrimaryAmber,
                        dpi,
                    );
                    render_button(
                        st,
                        canvas,
                        text_cmds,
                        unm_rc,
                        "Unmount",
                        HitAction::CardUnmount(m_idx),
                        BtnStyle::Danger,
                        dpi,
                    );

                    // Source Title & Metadata Subtitle
                    let src_name = st.mounts[m_idx].source_name.clone();
                    let sub_info = format!(
                        "{}  •  {} clip{}  •  {} virtual DNG  •  Indexed in {:.1} ms",
                        st.mounts[m_idx].explorer_root_path().display(),
                        clip_count,
                        if clip_count == 1 { "" } else { "s" },
                        format_bytes(st.mounts[m_idx].total_bytes),
                        st.mounts[m_idx].index_time_ms
                    );
                    let text_left = drv_x + drv_w + scale_dpi(12, dpi);
                    let text_right = exp_rc.left - scale_dpi(10, dpi);

                    text_cmds.push(TextDrawCmd {
                        text: src_name,
                        rect: Rect {
                            left: text_left,
                            top: cur_y + scale_dpi(9, dpi),
                            right: text_right,
                            bottom: cur_y + scale_dpi(30, dpi),
                        },
                        color: COL_TEXT_PRIMARY,
                        font_kind: FontKind::BodySemibold,
                        flags: DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
                    });
                    text_cmds.push(TextDrawCmd {
                        text: sub_info,
                        rect: Rect {
                            left: text_left,
                            top: cur_y + scale_dpi(29, dpi),
                            right: text_right,
                            bottom: cur_y + scale_dpi(48, dpi),
                        },
                        color: COL_TEXT_SECONDARY,
                        font_kind: FontKind::Subtitle,
                        flags: DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
                    });

                    canvas.fill_rect(
                        card_x + scale_dpi(14, dpi),
                        cur_y + card_hdr_h - 1,
                        card_w - scale_dpi(28, dpi),
                        1,
                        COL_DIVIDER,
                    );

                    // Clip rows inside Mount Card
                    let mut ry = cur_y + card_hdr_h + scale_dpi(6, dpi);
                    {
                        let hx = card_x + scale_dpi(14, dpi);
                        let hw = card_w - scale_dpi(28, dpi);
                        let cols = table_cols(hx + hw - scale_dpi(12, dpi), dpi);
                        let labels: [(&str, u32); 7] = [
                            ("MODE", DT_LEFT),
                            ("AUDIO", DT_CENTER),
                            ("STRETCH", DT_RIGHT),
                            ("FRAMES", DT_RIGHT),
                            ("FPS", DT_RIGHT),
                            ("BIT", DT_RIGHT),
                            ("RESOLUTION", DT_RIGHT),
                        ];
                        for (i, (t, al)) in labels.iter().enumerate() {
                            text_cmds.push(TextDrawCmd {
                                text: t.to_string(),
                                rect: Rect { left: cols[i].0, top: ry, right: cols[i].1, bottom: ry + table_hdr_h },
                                color: COL_TEXT_MUTED,
                                font_kind: FontKind::BadgeSmall,
                                flags: al | DT_VCENTER | DT_SINGLELINE,
                            });
                        }
                        text_cmds.push(TextDrawCmd {
                            text: String::from("CLIP"),
                            rect: Rect {
                                left: hx + scale_dpi(26, dpi),
                                top: ry,
                                right: cols[6].0 - scale_dpi(10, dpi),
                                bottom: ry + table_hdr_h,
                            },
                            color: COL_TEXT_MUTED,
                            font_kind: FontKind::BadgeSmall,
                            flags: DT_LEFT | DT_VCENTER | DT_SINGLELINE,
                        });
                        canvas.fill_rect(hx, ry + table_hdr_h - 1, hw, 1, COL_DIVIDER);
                        ry += table_hdr_h;
                    }
                    for c_idx in 0..clip_count {
                        let clip = st.mounts[m_idx].clips[c_idx].clone();
                        let rx = card_x + scale_dpi(14, dpi);
                        let rw = card_w - scale_dpi(28, dpi);

                        if c_idx % 2 == 0 {
                            canvas.draw_rounded_rect(
                                rx,
                                ry,
                                rw,
                                clip_row_h,
                                scale_dpi(6, dpi) as f32,
                                COL_CARD_ROW_BG,
                                255,
                                None,
                            );
                        }

                        // Play button (opens the GPU player for this clip)
                        let play_action = HitAction::ClipPlay(m_idx, c_idx);
                        let play_hover = st.hovered_action.as_ref() == Some(&play_action);
                        let tri_cx = (rx + scale_dpi(14, dpi)) as f32;
                        let tri_cy = (ry + clip_row_h / 2) as f32;
                        let tri = scale_dpi(5, dpi) as f32;
                        let tri_col = if play_hover { COL_ACCENT } else { COL_TEXT_SECONDARY };
                        // Filled triangle drawn as stacked vertical strokes (the canvas has no polygon fill).
                        let steps = (tri * 2.0).ceil() as i32;
                        for k in 0..=steps {
                            let x = tri_cx - tri * 0.8 + k as f32 * (tri * 1.8 / steps as f32);
                            let half = tri * (1.0 - k as f32 / steps as f32);
                            canvas.draw_line(x, tri_cy - half, x, tri_cy + half, 1.2, tri_col);
                        }
                        st.hit_boxes.push(HitBox {
                            rect: Rect {
                                left: rx,
                                top: ry,
                                right: rx + scale_dpi(26, dpi),
                                bottom: ry + clip_row_h,
                            },
                            action: play_action,
                        });

                        // Table cells (same column geometry as the header row)
                        let cols = table_cols(rx + rw - scale_dpi(12, dpi), dpi);
                        let p_h = scale_dpi(20, dpi);
                        let p_y = ry + (clip_row_h - p_h) / 2;
                        let cell = |text: String, col: (i32, i32), color: u32, flags: u32| TextDrawCmd {
                            text,
                            rect: Rect { left: col.0, top: ry, right: col.1, bottom: ry + clip_row_h },
                            color,
                            font_kind: FontKind::Subtitle,
                            flags: flags | DT_VCENTER | DT_SINGLELINE,
                        };

                        // MODE (green only for zero-decode passthrough)
                        let (p_bg, p_bd, p_fg) = if clip.is_passthrough {
                            (COL_EMERALD_BG, COL_EMERALD_BORDER, COL_EMERALD_TEXT)
                        } else {
                            (COL_BADGE_BG, COL_BADGE_BORDER, COL_BADGE_TEXT)
                        };
                        canvas.draw_rounded_rect(
                            cols[0].0,
                            p_y,
                            cols[0].1 - cols[0].0,
                            p_h,
                            scale_dpi(4, dpi) as f32,
                            p_bg,
                            255,
                            Some((p_bd, 1.0)),
                        );
                        text_cmds.push(TextDrawCmd {
                            text: if clip.focus_pixels > 0 {
                                if clip.payload_badge.starts_with("Native") { "Native + FP" } else { "16-bit + FP" }.to_string()
                            } else {
                                clip.payload_badge.to_string()
                            },
                            rect: Rect { left: cols[0].0, top: p_y, right: cols[0].1, bottom: p_y + p_h },
                            color: p_fg,
                            font_kind: FontKind::BadgeSmall,
                            flags: DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                        });

                        // AUDIO
                        text_cmds.push(cell(
                            String::from(if clip.has_audio { "WAV" } else { "–" }),
                            cols[1],
                            if clip.has_audio { COL_TEXT_SECONDARY } else { COL_TEXT_MUTED },
                            DT_CENTER,
                        ));

                        // STRETCH (amber pill only when a stretch applies)
                        if (clip.stretch - 1.0).abs() > 0.01 {
                            canvas.draw_rounded_rect(
                                cols[2].0,
                                p_y,
                                cols[2].1 - cols[2].0,
                                p_h,
                                scale_dpi(4, dpi) as f32,
                                COL_AMBER_BG,
                                255,
                                Some((COL_AMBER_BORDER, 1.0)),
                            );
                            text_cmds.push(TextDrawCmd {
                                text: format!(
                                    "{} ×{:.2}",
                                    if clip.stretch_axis == "horizontal" { "H" } else { "V" },
                                    clip.stretch
                                )
                                .replace(".00", ""),
                                rect: Rect { left: cols[2].0, top: p_y, right: cols[2].1, bottom: p_y + p_h },
                                color: COL_AMBER_TEXT,
                                font_kind: FontKind::BadgeSmall,
                                flags: DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                            });
                        } else {
                            text_cmds.push(cell(String::from("–"), cols[2], COL_TEXT_MUTED, DT_RIGHT));
                        }

                        text_cmds.push(cell(clip.frame_count.to_string(), cols[3], COL_TEXT_SECONDARY, DT_RIGHT));
                        text_cmds.push(cell(format!("{:.2}", clip.fps), cols[4], COL_TEXT_SECONDARY, DT_RIGHT));
                        text_cmds.push(cell(clip.bits_per_pixel.to_string(), cols[5], COL_TEXT_SECONDARY, DT_RIGHT));
                        text_cmds.push(cell(
                            format!("{}×{}", clip.width, clip.height),
                            cols[6],
                            COL_TEXT_SECONDARY,
                            DT_RIGHT,
                        ));

                        // Clip Folder Name (clickable to open clip folder directly in Explorer)
                        let folder_rc = Rect {
                            left: rx + scale_dpi(26, dpi),
                            top: ry,
                            right: cols[6].0 - scale_dpi(10, dpi),
                            bottom: ry + clip_row_h,
                        };
                        text_cmds.push(TextDrawCmd {
                            text: clip.folder_name,
                            rect: folder_rc,
                            color: COL_TEXT_PRIMARY,
                            font_kind: FontKind::MonoClip,
                            flags: DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
                        });
                        st.hit_boxes.push(HitBox {
                            rect: folder_rc,
                            action: HitAction::ClipExplorer(m_idx, c_idx),
                        });

                        ry += clip_row_h + scale_dpi(4, dpi);
                    }
                }

                cur_y += card_h + card_gap;
            }
        }

        // 4. Footer Bar (drawn on top so scrolled cards clip cleanly underneath)
        let fy = h - footer_h;
        canvas.fill_rect(0, fy, w, footer_h, COL_FOOTER_BG);
        canvas.fill_rect(0, fy, w, 1, COL_DIVIDER);

        // Checkbox: "Open in Explorer on mount"
        let cb_sz = scale_dpi(18, dpi);
        let cb_x = pad_x;
        let cb_y = fy + (footer_h - cb_sz) / 2;
        canvas.draw_rounded_rect(
            cb_x,
            cb_y,
            cb_sz,
            cb_sz,
            scale_dpi(4, dpi) as f32,
            if st.auto_open_explorer {
                COL_ACCENT
            } else {
                COL_BTN_SEC
            },
            255,
            Some((
                if st.auto_open_explorer {
                    COL_ACCENT
                } else {
                    COL_BTN_SEC_BORDER
                },
                1.0,
            )),
        );
        if st.auto_open_explorer {
            canvas.draw_line(
                (cb_x + scale_dpi(4, dpi)) as f32,
                (cb_y + scale_dpi(9, dpi)) as f32,
                (cb_x + scale_dpi(8, dpi)) as f32,
                (cb_y + scale_dpi(13, dpi)) as f32,
                scale_dpi(2, dpi) as f32,
                COL_TEXT_DARK,
            );
            canvas.draw_line(
                (cb_x + scale_dpi(8, dpi)) as f32,
                (cb_y + scale_dpi(13, dpi)) as f32,
                (cb_x + scale_dpi(14, dpi)) as f32,
                (cb_y + scale_dpi(5, dpi)) as f32,
                scale_dpi(2, dpi) as f32,
                COL_TEXT_DARK,
            );
        }
        let cb_hit = Rect {
            left: cb_x,
            top: fy + scale_dpi(6, dpi),
            right: cb_x + scale_dpi(185, dpi),
            bottom: h - scale_dpi(6, dpi),
        };
        text_cmds.push(TextDrawCmd {
            text: String::from("Open in Explorer on mount"),
            rect: Rect {
                left: cb_x + cb_sz + scale_dpi(8, dpi),
                ..cb_hit
            },
            color: COL_TEXT_SECONDARY,
            font_kind: FontKind::Subtitle,
            flags: DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        });
        st.hit_boxes.push(HitBox {
            rect: cb_hit,
            action: HitAction::ToggleAutoExplorer,
        });

        // Locate Resolve Button
        let res_btn_w = scale_dpi(136, dpi);
        let res_btn_h = scale_dpi(26, dpi);
        let res_btn_rc = Rect {
            left: cb_hit.right + scale_dpi(8, dpi),
            top: fy + (footer_h - res_btn_h) / 2,
            right: cb_hit.right + scale_dpi(8, dpi) + res_btn_w,
            bottom: fy + (footer_h - res_btn_h) / 2 + res_btn_h,
        };
        let has_resolve =
            st.resolve_fuscript_path.is_some() || locate_resolve_fuscript().is_some();
        render_button(
            st,
            canvas,
            text_cmds,
            res_btn_rc,
            if has_resolve {
                "Resolve: Linked ✓"
            } else {
                "Locate Resolve..."
            },
            HitAction::LocateResolve,
            BtnStyle::Secondary,
            dpi,
        );

        // WinFsp Driver Status Pill
        let wf_btn_w = scale_dpi(128, dpi);
        let wf_btn_rc = Rect {
            left: res_btn_rc.right + scale_dpi(8, dpi),
            top: res_btn_rc.top,
            right: res_btn_rc.right + scale_dpi(8, dpi) + wf_btn_w,
            bottom: res_btn_rc.bottom,
        };
        render_button(
            st,
            canvas,
            text_cmds,
            wf_btn_rc,
            if st.winfsp_installed {
                "WinFsp: Ready ✓"
            } else {
                "Install WinFsp..."
            },
            HitAction::InstallWinFsp,
            if st.winfsp_installed {
                BtnStyle::Emerald
            } else {
                BtnStyle::PrimaryAmber
            },
            dpi,
        );

        // Right footer: Active mount count + Unmount All
        let mut right_limit = w - pad_x;
        if !st.mounts.is_empty() {
            let ua_w = scale_dpi(102, dpi);
            let ua_rc = Rect {
                left: right_limit - ua_w,
                top: res_btn_rc.top,
                right: right_limit,
                bottom: res_btn_rc.bottom,
            };
            render_button(
                st,
                canvas,
                text_cmds,
                ua_rc,
                "Unmount All",
                HitAction::UnmountAll,
                BtnStyle::Danger,
                dpi,
            );
            right_limit = ua_rc.left - scale_dpi(12, dpi);
        }

        let summary_txt = format!(
            "{} active mount{} • WinFsp Zero-Copy",
            st.mounts.len(),
            if st.mounts.len() == 1 { "" } else { "s" }
        );
        text_cmds.push(TextDrawCmd {
            text: summary_txt,
            rect: Rect {
                left: wf_btn_rc.right + scale_dpi(8, dpi),
                top: fy,
                right: right_limit,
                bottom: h,
            },
            color: COL_TEXT_MUTED,
            font_kind: FontKind::Subtitle,
            flags: DT_RIGHT | DT_VCENTER | DT_SINGLELINE,
        });
    }

    unsafe fn dispatch_hit_action(hwnd: *mut c_void, action: HitAction) {
        match action {
            HitAction::AddFile => open_file_dialog(hwnd),
            HitAction::AddFolder => open_folder_dialog(hwnd),
            HitAction::SelectModeMenu => {
                let (rect_opt, cur_mode) = {
                    let Ok(guard) = APP_STATE.lock() else {
                        return;
                    };
                    let Some(st) = guard.as_ref() else {
                        return;
                    };
                    let r = st
                        .hit_boxes
                        .iter()
                        .find(|h| h.action == HitAction::SelectModeMenu)
                        .map(|h| h.rect);
                    (r, st.selected_mode)
                };
                if let Some(rc) = rect_opt {
                    show_mode_popup(hwnd, rc, cur_mode);
                }
            }
            HitAction::InstallWinFsp => {
                let already = is_winfsp_available();
                if already {
                    if let Ok(mut guard) = APP_STATE.lock() {
                        if let Some(st) = guard.as_mut() {
                            st.winfsp_installed = true;
                            st.status_message = Some((
                                String::from(
                                    "WinFsp 2.0 Virtual Filesystem Driver is installed and active.",
                                ),
                                false,
                            ));
                        }
                    }
                    InvalidateRect(hwnd, ptr::null(), 0);
                } else {
                    let _ = install_bundled_winfsp(hwnd, false);
                }
            }
            HitAction::DismissBanner => {
                if let Ok(mut guard) = APP_STATE.lock() {
                    if let Some(st) = guard.as_mut() {
                        st.status_message = None;
                    }
                }
                InvalidateRect(hwnd, ptr::null(), 0);
            }
            HitAction::ToggleAutoExplorer => {
                if let Ok(mut guard) = APP_STATE.lock() {
                    if let Some(st) = guard.as_mut() {
                        st.auto_open_explorer = !st.auto_open_explorer;
                    }
                }
                persist_settings();
                InvalidateRect(hwnd, ptr::null(), 0);
            }
            HitAction::LocateResolve => prompt_locate_resolve(hwnd),
            HitAction::UnmountAll => unmount_all(hwnd),
            HitAction::CardExplorer(m_idx) => {
                let path_opt = {
                    let Ok(guard) = APP_STATE.lock() else {
                        return;
                    };
                    guard
                        .as_ref()
                        .and_then(|st| st.mounts.get(m_idx))
                        .map(|m| m.explorer_root_path())
                };
                if let Some(p) = path_opt {
                    open_path_in_explorer(&p, "card-button");
                }
            }
            HitAction::CardResolve(m_idx) => import_to_resolve(hwnd, Some(m_idx), None),
            HitAction::CardUnmount(m_idx) => unmount_at_index(hwnd, m_idx),
            HitAction::ClipExplorer(m_idx, c_idx) => {
                let path_opt = {
                    let Ok(guard) = APP_STATE.lock() else {
                        return;
                    };
                    guard
                        .as_ref()
                        .and_then(|st| st.mounts.get(m_idx))
                        .and_then(|m| m.full_clip_paths().get(c_idx).cloned())
                };
                if let Some(p) = path_opt {
                    open_path_in_explorer(&p, "clip-button");
                }
            }
            HitAction::ClipPlay(m_idx, c_idx) => {
                let target = {
                    let Ok(guard) = APP_STATE.lock() else {
                        return;
                    };
                    guard.as_ref().and_then(|st| st.mounts.get(m_idx)).map(|m| {
                        let title = m
                            .clip_folders
                            .get(c_idx)
                            .cloned()
                            .unwrap_or_else(|| format!("Clip {}", c_idx + 1));
                        (m.vfs.clone(), title)
                    })
                };
                if let Some((vfs, title)) = target {
                    if let Err(e) = player_win::open(vfs, c_idx, &title) {
                        cinefs_core::log_warn!("player: {e}");
                        set_status(hwnd, format!("Could not open the player: {e}"), true);
                    }
                }
            }
            HitAction::SetupToggleDesktopIcon => {
                if let Ok(mut guard) = APP_STATE.lock() {
                    if let Some(st) = guard.as_mut() {
                        st.setup_desktop_icon = !st.setup_desktop_icon;
                    }
                }
                InvalidateRect(hwnd, ptr::null(), 0);
            }
            HitAction::SetupToggleContextMenu => {
                if let Ok(mut guard) = APP_STATE.lock() {
                    if let Some(st) = guard.as_mut() {
                        st.setup_context_menu = !st.setup_context_menu;
                    }
                }
                InvalidateRect(hwnd, ptr::null(), 0);
            }
            HitAction::SetupToggleDefender => {
                if let Ok(mut guard) = APP_STATE.lock() {
                    if let Some(st) = guard.as_mut() {
                        st.setup_defender_exclusion = !st.setup_defender_exclusion;
                    }
                }
                InvalidateRect(hwnd, ptr::null(), 0);
            }
            HitAction::SetupRunInstall => {
                let (desk, ctx, def) = {
                    let Ok(guard) = APP_STATE.lock() else {
                        return;
                    };
                    let Some(st) = guard.as_ref() else {
                        return;
                    };
                    (
                        st.setup_desktop_icon,
                        st.setup_context_menu,
                        st.setup_defender_exclusion,
                    )
                };
                perform_full_setup(hwnd, desk, ctx, def);
            }
            HitAction::SetupLaunchPortable => {
                if let Ok(mut guard) = APP_STATE.lock() {
                    if let Some(st) = guard.as_mut() {
                        st.is_setup_mode = false;
                    }
                }
                InvalidateRect(hwnd, ptr::null(), 0);
            }
        }
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: *mut c_void,
        msg: u32,
        w_param: usize,
        l_param: isize,
    ) -> isize {
        catch_unwind(AssertUnwindSafe(|| match msg {
            WM_CREATE => {
                apply_dark_titlebar(hwnd);
                let dpi = get_window_dpi(hwnd);
                let h_icon_big = load_embedded_icon(256);
                let h_icon_sm = load_embedded_icon(32);
                SendMessageW(hwnd, WM_SETICON, ICON_BIG, h_icon_big as isize);
                SendMessageW(hwnd, WM_SETICON, ICON_SMALL, h_icon_sm as isize);

                DragAcceptFiles(hwnd, 1);
                setup_tray_icon(hwnd, h_icon_sm, true);

                let exe_is_setup = is_setup_exe();
                let prefs = load_and_apply_settings();

                *APP_STATE.lock().unwrap() = Some(AppState {
                    dpi,
                    h_icon_big,
                    selected_mode: prefs.mode,
                    auto_open_explorer: prefs.auto_open_explorer,
                    winfsp_installed: is_winfsp_available(),
                    resolve_fuscript_path: locate_resolve_fuscript(),
                    status_message: None,
                    mounts: Vec::new(),
                    scroll_y: 0,
                    max_scroll_y: 0,
                    hit_boxes: Vec::new(),
                    hovered_action: None,
                    pressed_action: None,
                    tracking_mouse: false,
                    is_setup_mode: exe_is_setup,
                    setup_desktop_icon: true,
                    setup_context_menu: true,
                    setup_defender_exclusion: true,
                });
                0
            }

            WM_ERASEBKGND => 1,

            WM_GETMINMAXINFO => {
                let mmi = l_param as *mut MinMaxInfo;
                if !mmi.is_null() {
                    let dpi = get_window_dpi(hwnd);
                    (*mmi).pt_min_track_size.x = scale_dpi(720, dpi);
                    (*mmi).pt_min_track_size.y = scale_dpi(460, dpi);
                }
                0
            }

            WM_DPICHANGED => {
                let new_dpi = (w_param & 0xFFFF) as i32;
                if let Ok(mut guard) = APP_STATE.lock() {
                    if let Some(st) = guard.as_mut() {
                        st.dpi = new_dpi;
                    }
                }
                let rc = l_param as *const Rect;
                if !rc.is_null() {
                    SetWindowPos(
                        hwnd,
                        ptr::null_mut(),
                        (*rc).left,
                        (*rc).top,
                        (*rc).right - (*rc).left,
                        (*rc).bottom - (*rc).top,
                        0x0004 | 0x0010, // SWP_NOZORDER | SWP_NOACTIVATE
                    );
                }
                InvalidateRect(hwnd, ptr::null(), 0);
                0
            }

            WM_SIZE => {
                InvalidateRect(hwnd, ptr::null(), 0);
                0
            }

            WM_PAINT => {
                let mut ps: PaintStruct = zeroed();
                let hdc = BeginPaint(hwnd, &mut ps);
                if !hdc.is_null() {
                    let mut rc = Rect::default();
                    GetClientRect(hwnd, &mut rc);
                    render_window(hwnd, hdc, rc.right - rc.left, rc.bottom - rc.top);
                }
                EndPaint(hwnd, &ps);
                0
            }

            WM_SETCURSOR => {
                if (l_param & 0xFFFF) == 1 {
                    let is_hand = APP_STATE
                        .lock()
                        .ok()
                        .and_then(|g| g.as_ref().map(|s| s.hovered_action.is_some()))
                        .unwrap_or(false);
                    let cur = LoadCursorW(
                        ptr::null_mut(),
                        if is_hand { IDC_HAND } else { IDC_ARROW },
                    );
                    SetCursor(cur);
                    return 1;
                }
                DefWindowProcW(hwnd, msg, w_param, l_param)
            }

            WM_MOUSEMOVE => {
                let mx = (l_param as i16) as i32;
                let my = ((l_param >> 16) as i16) as i32;
                let mut needs_repaint = false;
                let mut start_track = false;

                if let Ok(mut guard) = APP_STATE.lock() {
                    if let Some(st) = guard.as_mut() {
                        if !st.tracking_mouse {
                            st.tracking_mouse = true;
                            start_track = true;
                        }
                        let found = st
                            .hit_boxes
                            .iter()
                            .rev()
                            .find(|h| h.contains(mx, my))
                            .map(|h| h.action.clone());
                        if found != st.hovered_action {
                            st.hovered_action = found;
                            needs_repaint = true;
                        }
                    }
                }
                if start_track {
                    let mut tme = TrackMouseEventStruct {
                        cb_size: size_of::<TrackMouseEventStruct>() as u32,
                        dw_flags: 0x0000_0002, // TME_LEAVE
                        hwnd_track: hwnd,
                        dw_hover_time: 0,
                    };
                    TrackMouseEvent(&mut tme);
                }
                if needs_repaint {
                    InvalidateRect(hwnd, ptr::null(), 0);
                }
                0
            }

            WM_MOUSELEAVE => {
                let mut needs_repaint = false;
                if let Ok(mut guard) = APP_STATE.lock() {
                    if let Some(st) = guard.as_mut() {
                        st.tracking_mouse = false;
                        if st.hovered_action.is_some() || st.pressed_action.is_some() {
                            st.hovered_action = None;
                            st.pressed_action = None;
                            needs_repaint = true;
                        }
                    }
                }
                if needs_repaint {
                    InvalidateRect(hwnd, ptr::null(), 0);
                }
                0
            }

            WM_LBUTTONDOWN => {
                let mx = (l_param as i16) as i32;
                let my = ((l_param >> 16) as i16) as i32;
                let mut needs_repaint = false;
                if let Ok(mut guard) = APP_STATE.lock() {
                    if let Some(st) = guard.as_mut() {
                        let found = st
                            .hit_boxes
                            .iter()
                            .rev()
                            .find(|h| h.contains(mx, my))
                            .map(|h| h.action.clone());
                        if st.pressed_action != found {
                            st.pressed_action = found;
                            needs_repaint = true;
                        }
                    }
                }
                if needs_repaint {
                    InvalidateRect(hwnd, ptr::null(), 0);
                }
                0
            }

            WM_LBUTTONUP => {
                let mx = (l_param as i16) as i32;
                let my = ((l_param >> 16) as i16) as i32;
                let mut triggered: Option<HitAction> = None;
                if let Ok(mut guard) = APP_STATE.lock() {
                    if let Some(st) = guard.as_mut() {
                        let up_hit = st
                            .hit_boxes
                            .iter()
                            .rev()
                            .find(|h| h.contains(mx, my))
                            .map(|h| h.action.clone());
                        if up_hit.is_some() && up_hit == st.pressed_action {
                            triggered = up_hit;
                        }
                        st.pressed_action = None;
                    }
                }
                InvalidateRect(hwnd, ptr::null(), 0);
                if let Some(act) = triggered {
                    dispatch_hit_action(hwnd, act);
                }
                0
            }

            WM_MOUSEWHEEL => {
                let delta = ((w_param >> 16) as i16) as i32;
                let mut needs_repaint = false;
                if let Ok(mut guard) = APP_STATE.lock() {
                    if let Some(st) = guard.as_mut() {
                        if st.max_scroll_y > 0 {
                            let step = scale_dpi(48, st.dpi);
                            let new_scroll =
                                (st.scroll_y - (delta * step) / 120).clamp(0, st.max_scroll_y);
                            if new_scroll != st.scroll_y {
                                st.scroll_y = new_scroll;
                                needs_repaint = true;
                            }
                        }
                    }
                }
                if needs_repaint {
                    InvalidateRect(hwnd, ptr::null(), 0);
                }
                0
            }

            WM_DROPFILES => {
                let h_drop = w_param as *mut c_void;
                let count = DragQueryFileW(h_drop, 0xFFFF_FFFF, ptr::null_mut(), 0);
                let mut paths = Vec::new();
                for i in 0..count {
                    let len = DragQueryFileW(h_drop, i, ptr::null_mut(), 0) as usize;
                    if len > 0 {
                        let mut buf = vec![0u16; len + 1];
                        DragQueryFileW(h_drop, i, buf.as_mut_ptr(), (len + 1) as u32);
                        let p = String::from_utf16_lossy(&buf[..len]);
                        paths.push(PathBuf::from(p));
                    }
                }
                DragFinish(h_drop);
                if let Ok(mut guard) = APP_STATE.lock() {
                    if let Some(st) = guard.as_mut() {
                        st.is_setup_mode = false;
                    }
                }
                mount_sources(hwnd, paths);
                0
            }

            WM_COMMAND => {
                let id = w_param & 0xFFFF;
                match id {
                    ID_TRAY_SHOW => {
                        ShowWindow(hwnd, SW_SHOW);
                        ShowWindow(hwnd, SW_RESTORE);
                        SetForegroundWindow(hwnd);
                    }
                    ID_TRAY_ADD_FILE => {
                        ShowWindow(hwnd, SW_SHOW);
                        open_file_dialog(hwnd);
                    }
                    ID_TRAY_ADD_FOLDER => {
                        ShowWindow(hwnd, SW_SHOW);
                        open_folder_dialog(hwnd);
                    }
                    ID_TRAY_RESOLVE_ALL => import_to_resolve(hwnd, None, None),
                    ID_TRAY_UNMOUNT_ALL => unmount_all(hwnd),
                    ID_TRAY_BAKE => {
                        let on = !cinefs_core::dng::stretch_bake();
                        cinefs_core::dng::set_stretch_bake(on);
                        persist_settings();
                        set_status(
                            hwnd,
                            format!(
                                "Stretch method: {} — applies to clips mounted from now on.",
                                if on { "baked into the DNG pixels" } else { "metadata (DefaultScale) only" }
                            ),
                            false,
                        );
                    }
                    ID_TRAY_FOCUS => {
                        let on = !cinefs_core::focus_fix::focus_fix_enabled();
                        cinefs_core::focus_fix::set_focus_fix(on);
                        persist_settings();
                        set_status(
                            hwnd,
                            format!(
                                "Focus pixels: {} — applies to clips mounted from now on.",
                                if on { "PDAF dots are corrected (pixels rewritten, no LJ92 passthrough)" } else { "off" }
                            ),
                            false,
                        );
                    }
                    ID_TRAY_STRETCH => {
                        let next = (STRETCH_MODE.load(std::sync::atomic::Ordering::Relaxed) + 1) % 5;
                        STRETCH_MODE.store(next, std::sync::atomic::Ordering::Relaxed);
                        cinefs_core::mlv::set_stretch_override(stretch_override_for(next));
                        persist_settings();
                        set_status(
                            hwnd,
                            format!(
                                "Pixel stretch: {} — applies to clips mounted from now on (unmount + mount again to change existing ones).",
                                stretch_mode_label(next)
                            ),
                            false,
                        );
                    }
                    ID_TRAY_OPEN_LOG => {
                        let lp = cinefs_core::log::active_path()
                            .unwrap_or_else(cinefs_core::log::log_file_path);
                        let _ = Command::new("notepad.exe").arg(&lp).spawn();
                    }
                    ID_TRAY_EXIT => {
                        DestroyWindow(hwnd);
                    }
                    ID_MODE_AUTO | ID_MODE_PASSTHROUGH | ID_MODE_NATIVE | ID_MODE_UNPACKED16 => {
                        let new_mode = match id {
                            ID_MODE_PASSTHROUGH => DngPayloadMode::Lj92Passthrough,
                            ID_MODE_NATIVE => DngPayloadMode::NativePacked,
                            ID_MODE_UNPACKED16 => DngPayloadMode::Unpacked16,
                            _ => DngPayloadMode::Auto,
                        };
                        if let Ok(mut guard) = APP_STATE.lock() {
                            if let Some(st) = guard.as_mut() {
                                st.selected_mode = new_mode;
                            }
                        }
                        persist_settings();
                        InvalidateRect(hwnd, ptr::null(), 0);
                    }
                    _ => {}
                }
                0
            }

            WM_TRAYICON => {
                let mouse_msg = (l_param as u32) & 0xFFFF;
                if mouse_msg == WM_LBUTTONDBLCLK || mouse_msg == WM_LBUTTONUP {
                    ShowWindow(hwnd, SW_SHOW);
                    ShowWindow(hwnd, SW_RESTORE);
                    SetForegroundWindow(hwnd);
                } else if mouse_msg == WM_RBUTTONUP {
                    show_tray_menu(hwnd);
                }
                0
            }

            WM_CLOSE => {
                let has_mounts = APP_STATE
                    .lock()
                    .ok()
                    .and_then(|g| g.as_ref().map(|s| !s.mounts.is_empty()))
                    .unwrap_or(false);
                if has_mounts {
                    ShowWindow(hwnd, SW_HIDE);
                    0
                } else {
                    DestroyWindow(hwnd);
                    0
                }
            }

            WM_DESTROY => {
                player_win::close_all();
                let drained: Vec<WinMountEntry> = APP_STATE
                    .lock()
                    .ok()
                    .and_then(|mut g| g.take())
                    .map(|mut s| s.mounts.drain(..).collect())
                    .unwrap_or_default();
                for mut m in drained {
                    let _ = m.volume.unmount();
                }
                setup_tray_icon(hwnd, ptr::null_mut(), false);
                PostQuitMessage(0);
                0
            }

            _ => DefWindowProcW(hwnd, msg, w_param, l_param),
        }))
        .unwrap_or(0)
    }

    pub fn run() {
        unsafe {
            // Check if launched as elevated setup worker (`--run-elevated-setup`)
            let args: Vec<String> = std::env::args().collect();
            cinefs_core::log::init(if is_setup_exe() { "CineFS-Setup" } else { "CineFS" });
            cinefs_core::log::install_panic_hook();
            log_startup_diagnostics();

            if args.iter().any(|a| a == "--run-elevated-setup") {
                let desk = !args.iter().any(|a| a == "--no-desktop");
                let ctx = !args.iter().any(|a| a == "--no-context");
                let def = !args.iter().any(|a| a == "--no-defender");
                perform_full_setup(ptr::null_mut(), desk, ctx, def);
                return;
            }

            // An elevated CineFS creates drive letters that normal Explorer / Resolve can't see.
            if !is_setup_exe() && maybe_relaunch_unelevated(&args) {
                return;
            }

            enable_high_dpi();

            let h_inst = GetModuleHandleW(ptr::null());
            let class_name = wstr("CineFSStudioWindowClass");
            let title = wstr("CineFS");
            let h_icon_big = load_embedded_icon(256);
            let h_icon_sm = load_embedded_icon(32);

            let wc = WndClassExW {
                cb_size: size_of::<WndClassExW>() as u32,
                style: 0,
                lpfn_wnd_proc: wnd_proc,
                cb_cls_extra: 0,
                cb_wnd_extra: 0,
                h_instance: h_inst,
                h_icon: h_icon_big,
                h_cursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
                hbr_background: ptr::null_mut(),
                lpsz_menu_name: ptr::null(),
                lpsz_class_name: class_name.as_ptr(),
                h_icon_sm,
            };
            RegisterClassExW(&wc);

            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE | WS_CLIPCHILDREN,
                160,
                120,
                880,
                560,
                ptr::null_mut(),
                ptr::null_mut(),
                h_inst,
                ptr::null_mut(),
            );
            if hwnd.is_null() {
                return;
            }

            // Auto-mount any paths passed via command line (Explorer Right-Click / Open With)
            // or handed over by an elevated instance that relaunched us un-elevated.
            let mut auto_paths: Vec<PathBuf> = take_pending_mounts();
            for arg in std::env::args_os().skip(1) {
                if arg.to_string_lossy().starts_with("--") {
                    continue;
                }
                auto_paths.push(PathBuf::from(&arg));
            }
            auto_paths.retain(|p| p.exists());
            if !auto_paths.is_empty() {
                if let Ok(mut guard) = APP_STATE.lock() {
                    if let Some(st) = guard.as_mut() {
                        st.is_setup_mode = false;
                    }
                }
                mount_sources(hwnd, auto_paths);
            }

            let mut msg: Msg = zeroed();
            while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

#[cfg(windows)]
fn main() {
    win_gui::run();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes_human_readable() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024 * 1024 * 100), "100.0 MB");
    }
}
