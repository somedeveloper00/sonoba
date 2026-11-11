//! Windows for sharing one window each in Discord, Meet or OBS: a translated window as DWM draws
//! it, with its translations on top, both as thumbnails, so they cost next to nothing. A share of
//! the original window leaves the overlay out, as it is a window of its own. With regions in a
//! game and in a comment viewer, each gets its own, named after its program. They sit off every
//! screen, out of the way, and stay after a job so that a share does not end with it.
//!
//! The translations come from a copy of the overlay, also off screen: on screen the overlay gives
//! way to windows in front and may hide from capture, while a share shows its window whole. Each
//! share shows only the regions in its own window, so the translations of a comment viewer lying
//! over a game stay out of the game's share.

use crate::Region;
use crate::config;
use crate::i18n::t;
use crate::patch::KEY_COLOR;
use std::cell::{Cell, RefCell};
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

const TIMER: usize = 1;
const TIMER_MS: u32 = 50; // the windows move and change size without telling these
const TITLE: &str = "Sonoba 共有用";

struct Share {
    hwnd: HWND,
    target: HWND,
    regions: Vec<Region>, // in the target, on the screen; none means all of the overlay
    thumbs: Vec<isize>,   // the target's, then the overlay copy's, one per region
    active: bool,         // the job running reads the target
    placed: Vec<i32>,
}

thread_local! {
    static SHARES: RefCell<Vec<Share>> = const { RefCell::new(Vec::new()) };
    static COPY: Cell<HWND> = const { Cell::new(null_mut()) }; // of the overlay
    static OVERLAY: Cell<HWND> = const { Cell::new(null_mut()) };
}

/// A share window for each window the job reads, with the regions in it, made the first time it
/// is needed. The others keep showing their windows, without translations.
pub fn show(sources: &[(HWND, Vec<Region>)], overlay: HWND) {
    OVERLAY.set(overlay);
    unsafe {
        let module = GetModuleHandleW(null());
        if !sources.is_empty() && COPY.get().is_null() {
            let style = WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT;
            let copy = CreateWindowExW(style, w!("SonobaOverlay"), null(), WS_POPUP, 0, 0, 1, 1, null_mut(), null_mut(), module, null());
            COPY.set(copy);
            repaint();
            ShowWindow(copy, SW_SHOWNOACTIVATE);
        }
        // Made outside the borrow: making a window runs its procedure.
        let new: Vec<HWND> = sources
            .iter()
            .filter(|(target, _)| !SHARES.with_borrow(|shares| shares.iter().any(|s| s.target == *target)))
            .map(|_| CreateWindowExW(WS_EX_APPWINDOW, w!("SonobaShare"), null(), WS_POPUP, 0, 0, 1, 1, null_mut(), null_mut(), module, null()))
            .collect();
        let mut new = new.into_iter();
        SHARES.with_borrow_mut(|shares| {
            for share in shares.iter_mut() {
                share.active = false;
            }
            for (target, regions) in sources {
                let share = match shares.iter().position(|s| s.target == *target) {
                    Some(index) => &mut shares[index],
                    None => {
                        let hwnd = new.next().unwrap_or(null_mut());
                        shares.push(Share { hwnd, target: *target, regions: Vec::new(), thumbs: Vec::new(), active: false, placed: Vec::new() });
                        shares.last_mut().unwrap()
                    }
                };
                share.active = true;
                if share.thumbs.is_empty() || share.regions != *regions {
                    share.regions = regions.clone();
                    register(share);
                }
                share.placed.clear();
            }
            // Named after the program, which stays the same, so that OBS finds it again next time.
            let mut names: Vec<String> = Vec::new();
            for share in shares.iter() {
                let name = format!("{} - {}", t(TITLE), program(share.target));
                let same = names.iter().filter(|n| **n == name).count();
                let title = if same == 0 { name.clone() } else { format!("{name} ({})", same + 1) };
                let units: Vec<u16> = title.encode_utf16().chain([0]).collect();
                SetWindowTextW(share.hwnd, units.as_ptr());
                names.push(name);
            }
        });
        let windows: Vec<HWND> = SHARES.with_borrow(|shares| shares.iter().map(|s| s.hwnd).collect());
        for hwnd in windows {
            tick(hwnd);
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            SetTimer(hwnd, TIMER, TIMER_MS, None);
        }
    }
}

/// The overlay has new translations, a new size or a new strength, and the copy follows.
pub fn repaint() {
    let copy = COPY.get();
    if copy.is_null() {
        return;
    }
    unsafe {
        let mut over: RECT = std::mem::zeroed();
        GetWindowRect(OVERLAY.get(), &mut over);
        let alpha = if crate::blur::active(copy) { 255 } else { (config::get().opacity as u32 * 255 / 100) as u8 };
        SetLayeredWindowAttributes(copy, KEY_COLOR, alpha, LWA_COLORKEY | LWA_ALPHA);
        let (x, y) = off_screen();
        SetWindowPos(copy, null_mut(), x, y, over.right - over.left, over.bottom - over.top, SWP_NOZORDER | SWP_NOACTIVATE);
        InvalidateRect(copy, null(), 0);
    }
}

/// The setting went off.
pub fn close() {
    for hwnd in SHARES.with_borrow(|shares| shares.iter().map(|s| s.hwnd).collect::<Vec<_>>()) {
        unsafe { DestroyWindow(hwnd) };
    }
}

/// The name of the program a window belongs to, without its folder and extension.
fn program(window: HWND) -> String {
    unsafe {
        let mut id = 0;
        GetWindowThreadProcessId(window, &mut id);
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, id);
        if process.is_null() {
            return String::new();
        }
        let mut path = [0u16; 1024];
        let mut length = path.len() as u32;
        let found = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, path.as_mut_ptr(), &mut length) != 0;
        CloseHandle(process);
        let path = if found { String::from_utf16_lossy(&path[..length as usize]) } else { String::new() };
        std::path::Path::new(&path).file_stem().map_or_else(String::new, |stem| stem.to_string_lossy().into_owned())
    }
}

/// The target's thumbnail, then one of the copy for each region: the later is drawn on top.
fn register(share: &mut Share) {
    unsafe {
        for thumb in share.thumbs.drain(..) {
            DwmUnregisterThumbnail(thumb);
        }
        let copies = share.regions.len().max(1);
        for source in std::iter::once(share.target).chain(std::iter::repeat_n(COPY.get(), copies)) {
            let mut thumb = 0;
            DwmRegisterThumbnail(share.hwnd, source, &mut thumb);
            share.thumbs.push(thumb);
        }
    }
}

/// Just past the right edge of all the screens, which can change.
fn off_screen() -> (i32, i32) {
    unsafe { (GetSystemMetrics(SM_XVIRTUALSCREEN) + GetSystemMetrics(SM_CXVIRTUALSCREEN) + 100, GetSystemMetrics(SM_YVIRTUALSCREEN)) }
}

/// Size and thumbnails after the windows; a share whose window has gone goes too.
fn tick(hwnd: HWND) {
    let gone = SHARES.with_borrow_mut(|shares| {
        let Some(share) = shares.iter_mut().find(|s| s.hwnd == hwnd) else { return false };
        if unsafe { IsWindow(share.target) } == 0 {
            return true;
        }
        unsafe { place(share) };
        false
    });
    if gone {
        unsafe { DestroyWindow(hwnd) };
    }
}

unsafe fn place(share: &mut Share) {
    unsafe {
        let overlay = OVERLAY.get();
        let Some(bounds) = crate::frame_bounds(share.target) else { return }; // for now not there
        let mut whole: RECT = std::mem::zeroed();
        GetWindowRect(share.target, &mut whole);
        let mut over: RECT = std::mem::zeroed();
        GetWindowRect(overlay, &mut over);
        let (w, h) = (bounds.right - bounds.left, bounds.bottom - bounds.top);
        let (x, y) = off_screen();
        let shown = (share.active && IsWindowVisible(overlay) != 0) as i32;
        let (sx, sy) = (bounds.left - whole.left, bounds.top - whole.top);
        let mut now = vec![x, y, w, h, sx, sy, over.left - bounds.left, over.top - bounds.top, over.right - over.left, over.bottom - over.top, shown];
        now.extend(share.regions.iter().flat_map(|r| [r.x, r.y, r.w, r.h]));
        if share.placed == now {
            return;
        }
        share.placed = now;
        SetWindowPos(share.hwnd, null_mut(), x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
        // The window without its invisible resize border, as a share of the window itself shows it.
        let props = DWM_THUMBNAIL_PROPERTIES {
            dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_RECTSOURCE | DWM_TNP_VISIBLE | DWM_TNP_OPACITY,
            rcDestination: RECT { left: 0, top: 0, right: w, bottom: h },
            rcSource: RECT { left: sx, top: sy, right: sx + w, bottom: sy + h },
            opacity: 255,
            fVisible: 1,
            fSourceClientAreaOnly: 0,
        };
        DwmUpdateThumbnailProperties(share.thumbs[0], &props);
        // Each region's translations, from where they are in the overlay to where they are in the window.
        let all = [Region { x: over.left, y: over.top, w: over.right - over.left, h: over.bottom - over.top }];
        let regions = if share.regions.is_empty() { &all[..] } else { &share.regions[..] };
        for (thumb, r) in share.thumbs[1..].iter().zip(regions) {
            let (fx, fy, tx, ty) = (r.x - over.left, r.y - over.top, r.x - bounds.left, r.y - bounds.top);
            let props = DWM_THUMBNAIL_PROPERTIES {
                rcSource: RECT { left: fx, top: fy, right: fx + r.w, bottom: fy + r.h },
                rcDestination: RECT { left: tx, top: ty, right: tx + r.w, bottom: ty + r.h },
                fVisible: shown,
                ..props
            };
            DwmUpdateThumbnailProperties(*thumb, &props);
        }
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER => {
                tick(hwnd);
                0
            }
            // A minimised window has nothing for a share to show.
            WM_SYSCOMMAND if w & 0xFFF0 == SC_MINIMIZE as usize => 0,
            WM_DESTROY => {
                let last = SHARES.with_borrow_mut(|shares| {
                    if let Some(index) = shares.iter().position(|s| s.hwnd == hwnd) {
                        for thumb in shares.remove(index).thumbs {
                            DwmUnregisterThumbnail(thumb);
                        }
                    }
                    shares.is_empty()
                });
                if last {
                    DestroyWindow(COPY.replace(null_mut()));
                }
                0
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}

pub fn register_class() {
    unsafe {
        let mut class: WNDCLASSEXW = std::mem::zeroed();
        class.cbSize = size_of::<WNDCLASSEXW>() as u32;
        class.lpfnWndProc = Some(proc);
        class.hInstance = GetModuleHandleW(null());
        class.lpszClassName = w!("SonobaShare");
        class.hbrBackground = GetStockObject(BLACK_BRUSH) as HBRUSH;
        class.hIcon = crate::app_icon(Some(48));
        class.hIconSm = crate::app_icon(None);
        RegisterClassExW(&class);
    }
}
