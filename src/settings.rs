//! The settings window: a page in WebView2, which comes with Windows 11 and with Edge on Windows
//! 10. Only this window uses it, so none of it stays in memory once the window closes.

use crate::config::{self, Hotkey};
use crate::engine::{self, Device};
use crate::i18n::t;
use crate::{models, request_load, request_unload, try_hotkeys};
use serde_json::{Value, json};
use crate::translate::{HISTORY, HISTORY_ADDED};
use std::cell::{Cell, RefCell, OnceCell};
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::atomic::Ordering;
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{CoTaskMemPWSTR, CoreWebView2EnvironmentOptions, CreateCoreWebView2ControllerCompletedHandler, CreateCoreWebView2EnvironmentCompletedHandler, WebMessageReceivedEventHandler};
use windows::Win32::Foundation::{HWND as ComHwnd, RECT as ComRect};
use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

const BIG_ICON: i32 = 48;
const TIMER: usize = 1;
const TIMER_MS: u32 = 250;
const PICK_FOLDER: u32 = WM_APP + 1; // posted by the page: a dialog must not run inside WebView2's own callback
const WIDTH: i32 = 800;
const HEIGHT: i32 = 620;
const MIN_WIDTH: i32 = 640;
const MIN_HEIGHT: i32 = 480;
const PAGE: &str = include_str!("settings.html");
const RUN_KEY: *const u16 = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: *const u16 = w!("Sonoba");
const OLD_RUN_VALUE: *const u16 = w!("screen-translate");
const THEME_KEY: *const u16 = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
const USE_DARK_TITLE_BAR: u32 = 20; // DWMWA_USE_IMMERSIVE_DARK_MODE

thread_local! {
    static WINDOW: Cell<HWND> = const { Cell::new(null_mut()) };
    static CONTROLLER: RefCell<Option<ICoreWebView2Controller>> = const { RefCell::new(None) };
    static WEBVIEW: RefCell<Option<ICoreWebView2>> = const { RefCell::new(None) };
    static SENT: RefCell<String> = const { RefCell::new(String::new()) };
    static MESSAGE: RefCell<String> = const { RefCell::new(String::new()) };
    static FONTS: OnceCell<Vec<String>> = const { OnceCell::new() };
    static CHOOSING_KEY: Cell<bool> = const { Cell::new(false) };
    static HISTORY_SENT: Cell<usize> = const { Cell::new(0) }; // HISTORY_ADDED as the page last heard it
    static GO_HISTORY: Cell<bool> = const { Cell::new(false) }; // show the history once the page is up
}

pub fn window() -> HWND {
    WINDOW.get()
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

fn run_value_exists(value: *const u16) -> bool {
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, value, RRF_RT_REG_SZ, null_mut(), null_mut(), null_mut()) == 0 }
}

fn autostart() -> bool {
    run_value_exists(RUN_VALUE)
}

/// An autostart from before the rename to Sonoba would start the old exe, so it moves to this one.
pub fn carry_over_autostart() {
    if run_value_exists(OLD_RUN_VALUE) {
        set_autostart(true);
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, OLD_RUN_VALUE) };
    }
}

fn set_autostart(on: bool) {
    unsafe {
        if !on {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
            return;
        }
        let Ok(exe) = std::env::current_exe() else { return };
        let command = wide(&format!("\"{}\"", exe.display()));
        RegSetKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, REG_SZ, command.as_ptr() as *const _, command.len() as u32 * 2);
    }
}

fn light_theme() -> bool {
    let (mut value, mut size) = (1u32, 4u32);
    unsafe { RegGetValueW(HKEY_CURRENT_USER, THEME_KEY, w!("AppsUseLightTheme"), RRF_RT_REG_DWORD, null_mut(), &mut value as *mut _ as *mut _, &mut size) };
    value != 0
}

unsafe extern "system" fn add_font(font: *const LOGFONTW, _: *const TEXTMETRICW, _: u32, list: LPARAM) -> i32 {
    unsafe {
        let name = &(*font).lfFaceName;
        let name = String::from_utf16_lossy(&name[..name.iter().position(|&c| c == 0).unwrap_or(name.len())]);
        if !name.starts_with('@') {
            (*(list as *mut Vec<String>)).push(name); // the @ ones are for vertical text
        }
        1
    }
}

fn fonts() -> Vec<String> {
    FONTS.with(|fonts| {
        fonts
            .get_or_init(|| unsafe {
                let mut list: Vec<String> = Vec::new();
                let mut wanted: LOGFONTW = std::mem::zeroed();
                wanted.lfCharSet = DEFAULT_CHARSET as u8;
                let hdc = GetDC(null_mut());
                EnumFontFamiliesExW(hdc, &wanted, Some(add_font), &mut list as *mut _ as LPARAM, 0);
                ReleaseDC(null_mut(), hdc);
                list.sort();
                list.dedup();
                list
            })
            .clone()
    })
}

fn data_dir() -> PathBuf {
    models::dir().parent().map(PathBuf::from).unwrap_or_default()
}

/// Everything the page shows.
fn state() -> Value {
    let mut config = config::get();
    let entries = models::list();
    if !entries.iter().any(|e| e.file == config.model) {
        // The file has gone, so the choice goes with it.
        config::update(|c| c.model = models::DEFAULT.to_string());
        config = config::get();
    }
    if let Some(error) = models::LAST_ERROR.lock().unwrap().take() {
        MESSAGE.set(t(&error));
    }
    let list: Vec<Value> = entries
        .iter()
        .map(|e| {
            let (present, partial, download, license) = (models::is_present(&e.file), models::has_part(&e.file), models::can_download(&e.file), models::license_of(&e.file));
            json!({ "file": e.file, "family": e.family, "size": e.size, "tier": e.tier, "gb": e.gb, "memory_gb": e.memory_gb,
                    "present": present, "partial": partial, "download": download, "license": license })
        })
        .collect();
    let device = match config.device {
        Device::Auto => "auto",
        Device::Gpu => "gpu",
        Device::Cpu => "cpu",
    };
    let engine_state = match engine::STATE.load(Ordering::Relaxed) {
        engine::READY => "ready",
        engine::LOADING => "loading",
        _ => "unloaded",
    };
    let hotkeys: Vec<Value> = config.hotkeys.iter().map(|h| json!({ "mods": h.mods, "key": h.key })).collect();
    json!({
        "title": t("Sonoba の設定"),
        "version": env!("CARGO_PKG_VERSION"),
        "folder": data_dir().display().to_string(),
        "models": list,
        "model": config.model,
        "model_folder": models::folder().display().to_string(),
        "model_folder_chosen": !config.model_folder.is_empty(),
        "progress": models::PROGRESS.load(Ordering::Relaxed),
        "downloading": models::DOWNLOADING.lock().unwrap().clone(),
        "scripts": crate::ocr::SCRIPTS.iter().map(|s| json!({ "id": s.id, "present": models::is_present(&s.file()), "partial": models::has_part(&s.file()) })).collect::<Vec<_>>(),
        "engine": engine_state,
        "message": MESSAGE.with_borrow(Clone::clone),
        "device": device,
        "gpu_limit": config.gpu_limit_mib,
        "threads": config.cpu_threads,
        "cpu_auto": engine::performance_cores(),
        "cpu_max": std::thread::available_parallelism().map_or(1, |n| n.get()),
        "hotkeys": hotkeys,
        "default_hotkeys": config::Config::default().hotkeys.iter().map(|h| json!({ "mods": h.mods, "key": h.key })).collect::<Vec<_>>(),
        "show_frame": config.show_frame,
        "flash_frame": config.flash_frame,
        "shared": config.shared,
        "source": config.source,
        "target": config.target,
        "ui_lang": config.ui_lang,
        "taken": *crate::TAKEN_KEYS.lock().unwrap(),
        "form": config.form,
        "glossary": config.glossary,
        "font": config.font,
        "blur_background": config.blur_background,
        "fonts": fonts(),
        "bold_text": config.bold_text,
        "opacity": config.opacity,
        "keep_history": config.keep_history,
        "autostart": autostart(),
        "license_accepted": config.license_accepted,
        "intro": (models::needs_license(&config.model) && !config.license_accepted) || (!config.intro_done && !models::is_present(&config.model)),
        "intro_done": config.intro_done,
    })
}

fn post(text: &str) {
    WEBVIEW.with_borrow(|webview| {
        if let Some(webview) = webview {
            let _ = unsafe { webview.PostWebMessageAsJson(&HSTRING::from(text)) };
        }
    });
}

/// Sends the state if it changed since the last time, and the translations added since.
fn refresh() {
    let text = state().to_string();
    if SENT.with_borrow(|sent| *sent != text) {
        post(&text);
        SENT.set(text);
    }
    let new: Vec<(String, String)> = {
        let history = HISTORY.lock().unwrap();
        let added = HISTORY_ADDED.load(Ordering::Relaxed);
        history.iter().take(added - HISTORY_SENT.replace(added)).cloned().collect()
    };
    if !new.is_empty() {
        post(&json!({ "history_new": new }).to_string());
    }
}

fn hotkey_changed(index: usize, new: Hotkey) {
    let mut hotkeys = config::get().hotkeys;
    if index >= hotkeys.len() {
        return;
    }
    hotkeys[index] = new;
    let repeated = new != Hotkey::OFF && (0..hotkeys.len()).any(|i| i != index && hotkeys[i] == new);
    crate::pause_hotkeys(); // its own key must not look taken
    if repeated || (new != Hotkey::OFF && !try_hotkeys(new)) {
        MESSAGE.set(t("そのキーは使えません（別の操作や別のアプリが使っています）"));
    } else {
        config::update(|c| c.hotkeys = hotkeys);
    }
    CHOOSING_KEY.set(false);
    crate::resume_hotkeys();
}

/// The keys go back to the defaults, except that none may land on another action's key.
fn reset_hotkeys(index: Option<usize>) {
    let defaults = config::Config::default().hotkeys;
    match index {
        Some(index) if index < defaults.len() => hotkey_changed(index, defaults[index]),
        Some(_) => {}
        None => {
            config::update(|c| c.hotkeys = defaults);
            crate::resume_hotkeys();
        }
    }
}

/// A changed choice takes effect with the next load, or right away if a model is in memory.
fn choice_changed(change: impl FnOnce(&mut config::Config)) {
    config::update(change);
    if engine::STATE.load(Ordering::Relaxed) != engine::UNLOADED {
        request_unload();
        if models::is_present(&config::get().model) {
            request_load();
        }
    }
}

/// The folder the user picks in Windows' own dialog, or None when they cancel.
fn pick_folder() -> Option<PathBuf> {
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
    use windows::Win32::UI::Shell::{FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName, SIGDN_FILESYSPATH};
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        dialog.SetOptions(dialog.GetOptions().ok()? | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        let _ = dialog.SetTitle(&HSTRING::from(t("モデルの保存先")));
        if let Ok(now) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(models::folder().as_os_str()), None) {
            let _ = dialog.SetFolder(&now); // opens where the models are now
        }
        dialog.Show(Some(ComHwnd(window()))).ok()?; // an error when cancelled
        let name = dialog.GetResult().ok()?.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let folder = name.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(name.0 as *const _));
        folder
    }
}

/// The translation models are read from, and downloaded to, another folder from now on. Nothing is moved.
fn set_model_folder(folder: Option<PathBuf>) {
    if models::PROGRESS.load(Ordering::Relaxed) >= 0 {
        return; // a download is under way to the old folder
    }
    let chosen = folder.filter(|f| *f != models::dir()).map_or_else(String::new, |f| f.display().to_string());
    choice_changed(|c| c.model_folder = chosen);
}

fn delete_model() {
    let file = config::get().model;
    crate::unload_now();
    engine::wait_until_stopped(); // Windows cannot delete a file that is in use
    if let Err(e) = models::delete(&file) {
        MESSAGE.set(format!("{}{e}", t("削除できません: ")));
    }
}

fn open_link(url: &str) {
    if url.starts_with("https://") {
        unsafe { ShellExecuteW(null_mut(), w!("open"), wide(url).as_ptr(), null(), null(), SW_SHOWNORMAL) };
    }
}

fn on_message(text: &str) {
    let Ok(message) = serde_json::from_str::<Value>(text) else { return };
    let value = &message["value"];
    let flag = value.as_bool().unwrap_or(false);
    let text_value = value.as_str().unwrap_or("").to_string();
    MESSAGE.set(String::new());
    match message["cmd"].as_str().unwrap_or("") {
        "ready" => {
            SENT.set(String::new());
            if GO_HISTORY.replace(false) {
                post(r#"{"go":"history"}"#);
            }
        }
        "model" => choice_changed(|c| c.model = text_value),
        "model_folder_pick" => unsafe {
            PostMessageW(window(), PICK_FOLDER, 0, 0);
        },
        "model_folder_reset" => set_model_folder(None),
        "device" => choice_changed(|c| {
            c.device = match text_value.as_str() {
                "gpu" => Device::Gpu,
                "cpu" => Device::Cpu,
                _ => Device::Auto,
            }
        }),
        "gpu_limit" => choice_changed(|c| c.gpu_limit_mib = value.as_u64()),
        "threads" => choice_changed(|c| c.cpu_threads = value.as_u64().map(|n| n as usize)),
        "source" => config::update(|c| c.source = text_value),
        "target" => config::update(|c| c.target = text_value),
        "ui_lang" => {
            config::update(|c| c.ui_lang = text_value);
            crate::language_changed();
        }
        "show_frame" => {
            config::update(|c| c.show_frame = flag);
            crate::update_frame();
        }
        "flash_frame" => config::update(|c| c.flash_frame = flag),
        "shared" => {
            config::update(|c| c.shared = flag);
            crate::shared_changed();
        }
        "autostart" => set_autostart(flag),
        "form" => config::update(|c| c.form = text_value),
        "glossary" => {
            let pair = |t: &Value| Some((t[0].as_str()?.trim().to_string(), t[1].as_str()?.trim().to_string()));
            let terms = value.as_array().map_or_else(Vec::new, |terms| terms.iter().filter_map(pair).collect());
            config::update(|c| c.glossary = terms);
        }
        "font" => {
            config::update(|c| c.font = text_value);
            crate::look_changed();
        }
        "bold_text" => {
            config::update(|c| c.bold_text = flag);
            crate::look_changed();
        }
        "blur_background" => {
            config::update(|c| c.blur_background = flag);
            crate::look_changed();
        }
        "opacity" => {
            config::update(|c| c.opacity = value.as_u64().unwrap_or(100).clamp(30, 100) as u8);
            crate::look_changed();
        }
        "keep_history" => {
            config::update(|c| c.keep_history = flag);
            if !flag {
                HISTORY.lock().unwrap().clear();
            }
        }
        "history_clear" => HISTORY.lock().unwrap().clear(),
        "copy" => {
            crate::copy_to_clipboard(&text_value);
        }
        "history" => {
            let history: Vec<(String, String)> = {
                let history = HISTORY.lock().unwrap();
                HISTORY_SENT.set(HISTORY_ADDED.load(Ordering::Relaxed));
                history.iter().cloned().collect()
            };
            post(&json!({ "history": history }).to_string());
        }
        "intro_done" => config::update(|c| c.intro_done = true),
        "license_accept" => config::update(|c| c.license_accepted = true),
        "close" => unsafe {
            DestroyWindow(window());
        },
        // The PLaMo licence binds whoever uses a PLaMo model, so the page has them accept it first.
        "download" if !models::needs_license(&config::get().model) || config::get().license_accepted => models::start_download(config::get().model),
        "script_download" => {
            if let Some(script) = crate::ocr::SCRIPTS.iter().find(|s| s.id == text_value) {
                models::start_download(script.file());
            }
        }
        "script_delete" => {
            if let Some(Err(e)) = crate::ocr::SCRIPTS.iter().find(|s| s.id == text_value).map(|s| models::delete(&s.file())) {
                MESSAGE.set(format!("{}{e}", t("削除できません: ")));
            }
        }
        "cancel" => models::CANCEL.store(true, Ordering::Relaxed),
        "delete" => delete_model(),
        "load" => request_load(),
        "unload" => crate::unload_now(),
        "capture" => {
            CHOOSING_KEY.set(flag);
            if flag { crate::pause_hotkeys() } else { crate::resume_hotkeys() }
        }
        "hotkey_reset" => reset_hotkeys(value.as_u64().map(|i| i as usize)),
        "hotkey_off" => hotkey_changed(value.as_u64().unwrap_or(u64::MAX) as usize, Hotkey::OFF),
        "hotkey" => {
            if let (Some(index), Some(mods), Some(key)) = (message["index"].as_u64(), message["mods"].as_u64(), message["key"].as_u64()) {
                hotkey_changed(index as usize, Hotkey { mods: mods as u32, key: key as u32 });
            }
        }
        "open" => open_link(&text_value),
        _ => {}
    }
    refresh();
}

fn fit_to_window(hwnd: HWND) {
    let mut r: RECT = unsafe { std::mem::zeroed() };
    unsafe { GetClientRect(hwnd, &mut r) };
    CONTROLLER.with_borrow(|controller| {
        if let Some(controller) = controller {
            let _ = unsafe { controller.SetBounds(ComRect { left: 0, top: 0, right: r.right, bottom: r.bottom }) };
        }
    });
}

fn page_ready(hwnd: HWND, controller: ICoreWebView2Controller) -> windows::core::Result<()> {
    if hwnd != window() {
        return unsafe { controller.Close() }; // closed again before the page came up
    }
    unsafe {
        let webview = controller.CoreWebView2()?;
        let settings = webview.Settings()?;
        settings.SetAreDefaultContextMenusEnabled(false)?;
        settings.SetAreHostObjectsAllowed(false)?;
        settings.SetIsStatusBarEnabled(false)?;
        settings.SetIsZoomControlEnabled(false)?;
        settings.SetAreDevToolsEnabled(std::env::var_os("SONOBA_DEV").is_some())?;
        let mut token = 0;
        let received = WebMessageReceivedEventHandler::create(Box::new(|_, args| {
            if let Some(args) = args {
                let mut text = PWSTR::null();
                args.WebMessageAsJson(&mut text)?;
                on_message(&CoTaskMemPWSTR::from(text).to_string());
            }
            Ok(())
        }));
        webview.add_WebMessageReceived(&received, &mut token)?;
        controller.SetIsVisible(true)?;
        webview.NavigateToString(&HSTRING::from(PAGE))?;
        controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC)?;
        CONTROLLER.set(Some(controller));
        WEBVIEW.set(Some(webview));
    }
    fit_to_window(hwnd);
    Ok(())
}

fn start_page(hwnd: HWND) -> windows::core::Result<()> {
    let folder = HSTRING::from(data_dir().join("webview").as_os_str());
    let handler = CreateCoreWebView2EnvironmentCompletedHandler::create(Box::new(move |result, environment| {
        result?;
        let Some(environment) = environment else { return Ok(()) };
        let created = CreateCoreWebView2ControllerCompletedHandler::create(Box::new(move |result, controller| {
            result?;
            controller.map_or(Ok(()), |controller| page_ready(hwnd, controller))
        }));
        unsafe { environment.CreateCoreWebView2Controller(ComHwnd(hwnd), &created) }
    }));
    // The page is a plain form, and overlay tools such as RTSS crash the WebView2 GPU process (issue #1).
    let options = CoreWebView2EnvironmentOptions::default();
    unsafe { options.set_additional_browser_arguments("--disable-gpu".into()) };
    let options = ICoreWebView2EnvironmentOptions::from(options);
    unsafe { CreateCoreWebView2EnvironmentWithOptions(PCWSTR::null(), &folder, &options, &handler) }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER => {
                refresh();
                0
            }
            PICK_FOLDER => {
                if let Some(folder) = pick_folder() {
                    set_model_folder(Some(folder));
                    refresh();
                }
                0
            }
            WM_SIZE => {
                fit_to_window(hwnd);
                0
            }
            WM_MOVE => {
                CONTROLLER.with_borrow(|controller| {
                    if let Some(controller) = controller {
                        let _ = controller.NotifyParentWindowPositionChanged();
                    }
                });
                0
            }
            WM_DPICHANGED => {
                let r = &*(l as *const RECT);
                SetWindowPos(hwnd, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                0
            }
            WM_GETMINMAXINFO => {
                let dpi = GetDpiForWindow(hwnd) as i32;
                let info = &mut *(l as *mut MINMAXINFO);
                info.ptMinTrackSize = POINT { x: MIN_WIDTH * dpi / 96, y: MIN_HEIGHT * dpi / 96 };
                0
            }
            WM_ACTIVATE => {
                if w & 0xFFFF == WA_INACTIVE as usize || w >> 16 != 0 {
                    if CHOOSING_KEY.replace(false) {
                        crate::resume_hotkeys();
                    }
                } else {
                    CONTROLLER.with_borrow(|controller| {
                        if let Some(controller) = controller {
                            let _ = controller.MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC);
                        }
                    });
                }
                0
            }
            WM_DESTROY => {
                KillTimer(hwnd, TIMER);
                WEBVIEW.set(None);
                if let Some(controller) = CONTROLLER.take() {
                    let _ = controller.Close();
                }
                SENT.set(String::new());
                GO_HISTORY.set(false);
                WINDOW.set(null_mut());
                if CHOOSING_KEY.replace(false) {
                    crate::resume_hotkeys();
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
        class.lpszClassName = w!("SonobaSettings");
        class.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        class.hIcon = crate::app_icon(Some(BIG_ICON));
        class.hIconSm = crate::app_icon(None);
        class.hbrBackground = GetStockObject(if light_theme() { WHITE_BRUSH } else { BLACK_BRUSH }) as HBRUSH;
        RegisterClassExW(&class);
    }
}

pub fn open() {
    unsafe {
        if !window().is_null() {
            ShowWindow(window(), SW_RESTORE);
            SetForegroundWindow(window());
            return;
        }
        let title = wide(&t("Sonoba の設定"));
        let hwnd = CreateWindowExW(
            0,
            w!("SonobaSettings"),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            100,
            100,
            null_mut(),
            null_mut(),
            GetModuleHandleW(null()),
            null(),
        );
        WINDOW.set(hwnd);
        HISTORY_SENT.set(HISTORY_ADDED.load(Ordering::Relaxed));
        let dark = (!light_theme()) as i32;
        DwmSetWindowAttribute(hwnd, USE_DARK_TITLE_BAR, &dark as *const _ as *const _, 4);
        let dpi = GetDpiForWindow(hwnd) as i32;
        SetWindowPos(hwnd, null_mut(), 0, 0, WIDTH * dpi / 96, HEIGHT * dpi / 96, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        SetTimer(hwnd, TIMER, TIMER_MS, None);
        if start_page(hwnd).is_err() {
            DestroyWindow(hwnd);
            MessageBoxW(null_mut(), wide(&t("設定画面を開けません。WebView2 ランタイムが必要です。")).as_ptr(), w!("Sonoba"), MB_OK | MB_ICONWARNING);
        }
    }
}

/// The settings, opened on the history page.
pub fn open_history() {
    let up = WEBVIEW.with_borrow(Option::is_some);
    open();
    if up {
        post(r#"{"go":"history"}"#);
    } else {
        GO_HISTORY.set(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_old_autostart_value_moves_to_the_new_name() {
        // HKEY_CURRENT_USER points at a throwaway key for this process, so the real autostart entries stay untouched.
        unsafe {
            let sandbox = w!("Software\\Sonoba-test");
            let mut key = null_mut();
            assert_eq!(RegCreateKeyW(HKEY_CURRENT_USER, sandbox, &mut key), 0);
            assert_eq!(RegOverridePredefKey(HKEY_CURRENT_USER, key), 0);

            carry_over_autostart();
            assert!(!autostart(), "nothing to carry over");

            let old = wide("screen-translate.exe");
            assert_eq!(RegSetKeyValueW(HKEY_CURRENT_USER, RUN_KEY, OLD_RUN_VALUE, REG_SZ, old.as_ptr() as *const _, old.len() as u32 * 2), 0);
            carry_over_autostart();
            let (moved, gone) = (autostart(), !run_value_exists(OLD_RUN_VALUE));

            RegOverridePredefKey(HKEY_CURRENT_USER, null_mut());
            RegCloseKey(key);
            RegDeleteTreeW(HKEY_CURRENT_USER, sandbox);
            assert!(moved && gone);
        }
    }
}
