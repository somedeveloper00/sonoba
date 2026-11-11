//! Screen OCR and English to Japanese translation, driven by global hotkeys.
//!
//! No window here ever takes the keyboard focus: the selection layer and the overlay are
//! WS_EX_NOACTIVATE and Esc is a hotkey, so the active window stays active throughout.
//! The overlay also lets the mouse through, and capture skips it, so OCR keeps seeing the
//! original text underneath the translation.
#![windows_subsystem = "windows"]

mod bar;
mod blur;
mod capture;
mod toast;
mod config;
mod engine;
mod i18n;
mod models;
mod ocr;
mod patch;
mod settings;
mod share;
mod translate;

use i18n::t;
use patch::{Backdrop, KEY_COLOR, Patch, WOBBLE_PERCENT};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

const POLL: Duration = Duration::from_millis(33); // a screen refresh or two
const WATCH: Duration = Duration::from_millis(100); // one-shot results, which take a capture trick
const PARTIAL_EVERY: Duration = Duration::from_millis(30); // repaints while a page fills in
const MOVE_SETTLE: Duration = Duration::from_millis(120); // no frames are judged until a drag pauses
const STALE_PERCENT: usize = 8; // of the pixels: a scrolled page, not a hover highlight or a caret
const SETTLE: Duration = Duration::from_millis(300);
const OCR_IDLE: Duration = Duration::from_secs(60); // free the OCR models this long after the last job
const ENGINE_IDLE: Duration = Duration::from_secs(15 * 60); // and the model after this long
const MAX_REST: Duration = Duration::from_millis(500); // the longest pause between frames that keep coming
const BACK_AFTER: Duration = Duration::from_secs(2); // off screen this long, a text back on it is history again
const READING_SLIP: usize = 2; // letters OCR may read differently from one frame to the next
const SAME_PLACE_PX: i32 = 3; // a patch this close to the last one with the same text is the same patch
const MIN_SELECTION: i32 = 8;
const MIN_TEXT_PX: i32 = 9; // below this Japanese stops being legible; the patch grows instead
const NARROWS: [i32; 3] = [100, 88, 76]; // letter widths to try at a size before going smaller; narrower reads poorly
const STATUS_PX: i32 = 16;
const DEFAULT_FONT: &str = "Yu Gothic UI";

const HK_ONCE: i32 = 1;
const HK_LIVE: i32 = 2;
const HK_STOP: i32 = 3;
const HK_ESC: i32 = 4;
const HK_PEEK: i32 = 6;
const HK_COPY: i32 = 7;
const ACTIONS: [i32; config::ACTIONS] = [HK_ONCE, HK_LIVE, HK_STOP, HK_PEEK, HK_COPY];
const ESSENTIAL_ACTIONS: usize = 3; // the app is no use without these
const WM_PATCHES: u32 = WM_APP + 1;
const WM_TRAY: u32 = WM_APP + 2;
const WM_JOB_OVER: u32 = WM_APP + 3;
const WM_PEEK_END: u32 = WM_APP + 9;
const WM_PARTS: u32 = WM_APP + 10;
const WM_TRANSLATED: u32 = WM_APP + 4;
const FLASH_TIMER: usize = 1;
const FLASH_MS: u32 = 450;
const PEEK_TIMER: usize = 2;
const PEEK_MS: u32 = 30;
const CLIP_TIMER: usize = 3;
const CLIP_MS: u32 = 50; // windows brought in front of the one being read
const CLIPBOARD_TRIES: usize = 25;
const CLIPBOARD_WAIT: Duration = Duration::from_millis(20);
const FRAME_PX: i32 = 2;
const FLASH_PX: i32 = 4;
const OUTLINE_EVERY_PX: i32 = 18; // of font size, for the outline of text on a picture
const FRAME_COLOR: u32 = 0x0071CC2E; // green, as COLORREF
const FLASH_COLOR: u32 = 0x0088FF39;
const MENU_EXIT: usize = 1;
const MENU_SETTINGS: usize = 2;
const MENU_PAUSE: usize = 3;
const MENU_HISTORY: usize = 4;
const HK_PROBE: i32 = 5;

const DIM_ALPHA: u8 = 110;
const HOVER_FRAME: i32 = 4;
const HOVER_COLOR: u32 = 0x00FFFFFF;

const NOT_FOUND: &str = "翻訳する文字が見つかりません";
const WORKING: &str = "翻訳中…";

#[derive(Clone, Copy, PartialEq)]
struct Region {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// One of several regions. Each keeps the mode it was selected in, as if it were alone.
#[derive(Clone, Copy, PartialEq)]
struct Part {
    region: Region,
    live: bool,
    window: usize, // the window it lies inside, which sharing reads it from; 0 for none
}

#[derive(Clone, Copy)]
struct Job {
    region: Region,
    live: bool,
    id: u64,
    window: usize, // the window the region belongs to, if any; for several regions, each has its own
}

/// How a region hangs on the window under it, so that it can go wherever the window goes.
#[derive(Clone, Copy)]
struct Anchor {
    window: HWND,
    whole: bool, // the region is the window itself; otherwise a part at (dx, dy) from its corner
    dx: i32,
    dy: i32,
}

static JOB: Mutex<Option<Job>> = Mutex::new(None);
static WAKE: Condvar = Condvar::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static WARM_UP: AtomicBool = AtomicBool::new(false);
static UNLOAD: AtomicBool = AtomicBool::new(false);
static MOVED_AT: Mutex<Option<Instant>> = Mutex::new(None); // the last time the anchor window moved
static PAUSED: AtomicBool = AtomicBool::new(false); // the anchor window is minimised
static SUSPENDED: AtomicBool = AtomicBool::new(false); // paused from the tray: no keys, no jobs
static PARTS: Mutex<Vec<Part>> = Mutex::new(Vec::new()); // the regions of a job with several
static MOUSE_KEYS: Mutex<Vec<(i32, config::Hotkey)>> = Mutex::new(Vec::new());
pub static TAKEN_KEYS: Mutex<Vec<usize>> = Mutex::new(Vec::new()); // actions another app holds the key of
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

pub fn request_load() {
    WARM_UP.store(true, Ordering::Relaxed);
    WAKE.notify_all();
}

pub fn request_unload() {
    UNLOAD.store(true, Ordering::Relaxed);
    WAKE.notify_all();
}

fn set_job(job: Option<Job>) {
    *JOB.lock().unwrap() = job;
    WAKE.notify_one();
}

fn current_job(id: u64) -> Option<Job> {
    JOB.lock().unwrap().filter(|job| job.id == id)
}

fn is_current(id: u64) -> bool {
    current_job(id).is_some()
}

// --- Worker: a watcher notices what changes on screen, however busy OCR is, and hands the newest
// frame over to the worker. So work never piles up, and stale text goes as soon as it is stale.

struct Frame {
    pixels: Vec<u8>,
    w: usize,
    h: usize,
}

struct Shared {
    frame: Mutex<Option<Frame>>, // the newest change, until the worker takes it
    fresh: Condvar,
    obsolete: AtomicBool, // the text being translated has left the screen
    done: AtomicBool,     // the current frame's translation is on show
    shown: Mutex<(Vec<u8>, Vec<Patch>)>, // what is on show, and the pixels it was made from
    working: Mutex<(Vec<u8>, Vec<Patch>)>, // the same for the text being translated
    read: Mutex<Vec<ReadPart>>, // of several regions, each as OCR last read it
    latest: Mutex<Vec<u8>>, // the newest picture, for work that finishes after the text has moved on
}

/// A region, its picture when OCR last read it, and the lines read then.
type ReadPart = (Region, Vec<u8>, Vec<ocr::Line>);

fn worker(overlay: usize) {
    let mut models: Option<ocr::Ocr> = None;
    let mut translator = translate::Translator::new();
    let mut idle_since = Instant::now();
    loop {
        let mut job = JOB.lock().unwrap();
        let requested = || WARM_UP.load(Ordering::Relaxed) || UNLOAD.load(Ordering::Relaxed) || engine::STATE.load(Ordering::Relaxed) == engine::LOADING;
        while job.is_none() && !requested() {
            let (guard, wait) = WAKE.wait_timeout(job, OCR_IDLE).unwrap();
            job = guard;
            if wait.timed_out() {
                free_when_idle(idle_since, &mut models, &mut translator);
            }
        }
        let current = *job;
        drop(job);
        match current {
            Some(job) => run(job, &mut models, &mut translator, overlay),
            None => serve_requests(&mut models, &mut translator, &|| JOB.lock().unwrap().is_none()),
        }
        idle_since = Instant::now();
    }
}

/// Loads the OCR models, or reloads them if the thread setting or the downloaded scripts have changed.
fn ensure_ocr(ocr_models: &mut Option<ocr::Ocr>) -> Result<(), String> {
    let threads = config::get().threads();
    let others: Vec<_> = ocr::SCRIPTS.iter().map(|s| (s, models::path(&s.file()))).filter(|(_, path)| path.exists()).collect();
    if !ocr_models.as_ref().is_some_and(|m| m.matches(threads, &others)) {
        *ocr_models = Some(ocr::Ocr::new(threads, &others)?);
    }
    Ok(())
}

/// Does what the settings window asked for. A load gives way once `keep_going` turns false, and
/// comes back here to finish, so that the state on show is never left at "loading".
fn serve_requests(models: &mut Option<ocr::Ocr>, translator: &mut translate::Translator, keep_going: &impl Fn() -> bool) {
    if UNLOAD.swap(false, Ordering::Relaxed) {
        translator.unload();
    }
    if WARM_UP.swap(false, Ordering::Relaxed) || engine::STATE.load(Ordering::Relaxed) == engine::LOADING {
        let _ = ensure_ocr(models);
        translator.warm_up(&|| keep_going() && !UNLOAD.load(Ordering::Relaxed));
    }
}

fn free_when_idle(since: Instant, models: &mut Option<ocr::Ocr>, translator: &mut translate::Translator) {
    translator.drop_dead_engine();
    if since.elapsed() >= OCR_IDLE {
        *models = None;
    }
    if since.elapsed() >= ENGINE_IDLE {
        translator.unload();
    }
}

fn new_shared() -> Shared {
    Shared {
        frame: Mutex::new(None),
        fresh: Condvar::new(),
        obsolete: AtomicBool::new(false),
        done: AtomicBool::new(false),
        shown: Mutex::new((Vec::new(), Vec::new())),
        working: Mutex::new((Vec::new(), Vec::new())),
        read: Mutex::new(Vec::new()),
        latest: Mutex::new(Vec::new()),
    }
}

fn run(job: Job, models: &mut Option<ocr::Ocr>, translator: &mut translate::Translator, overlay: usize) {
    let shared = new_shared();
    std::thread::scope(|scope| {
        scope.spawn(|| watch(job, overlay, &shared));
        translate_frames(job, models, translator, overlay, &shared);
    });
}

fn post_patches(overlay: usize, id: u64, patches: Vec<Patch>) {
    unsafe {
        PostMessageW(overlay as HWND, WM_PATCHES, id as usize, Box::into_raw(Box::new(patches)) as isize);
    }
}

fn translate_frames(job: Job, models: &mut Option<ocr::Ocr>, translator: &mut translate::Translator, overlay: usize, shared: &Shared) {
    let post = |patches: Vec<Patch>| post_patches(overlay, job.id, patches);
    let mut idle_since = Instant::now();
    let mut seen = HashMap::new(); // when each text was last on screen, for the history
    loop {
        let frame = loop {
            if !is_current(job.id) {
                return;
            }
            let (mut pending, _) = shared.fresh.wait_timeout_while(shared.frame.lock().unwrap(), POLL * 3, |frame| frame.is_none()).unwrap();
            if let Some(frame) = pending.take() {
                break frame;
            }
            drop(pending); // a load takes a while, and the watcher needs this
            serve_requests(models, translator, &|| is_current(job.id));
            free_when_idle(idle_since, models, translator); // a one-shot result can stay up for hours
        };
        shared.obsolete.store(false, Ordering::Relaxed);

        let start = Instant::now();
        // Not "has the screen changed": behind a subtitle, a video changes it all the time.
        let keep_going = || is_current(job.id) && !shared.obsolete.load(Ordering::Relaxed);
        // Lines that moved on while being translated, as a chat's do, go up where they are now.
        let post_now = |mut patches: Vec<Patch>| {
            catch_up(&frame.pixels, &mut patches, frame.w, shared);
            post(patches)
        };
        let result = translate_frame(&frame, job.live, models, translator, &keep_going, &post_now, shared);
        let mut patches = result.unwrap_or_else(|e| vec![Patch::status(&format!("エラー: {e}"), frame.w as i32)]);
        if keep_going() {
            let pixels = catch_up(&frame.pixels, &mut patches, frame.w, shared).unwrap_or(frame.pixels);
            let frame = Frame { pixels, w: frame.w, h: frame.h };
            let mut shown = shared.shown.lock().unwrap();
            hold_still(&mut patches, &shown, &frame);
            let changed = patches != shown.1;
            let new = newly_shown(&patches, &shown.1, &mut seen, Instant::now());
            *shown = (frame.pixels, patches.clone());
            drop(shown);
            if translator.keeps_history() {
                new.iter().for_each(|(source, text)| translate::remember(source, text));
            }
            shared.done.store(true, Ordering::Relaxed);
            post(patches);
            if changed {
                unsafe { PostMessageW(overlay as HWND, WM_TRANSLATED, job.id as usize, 0) };
            }
        }
        // A video in the region changes every frame: this keeps OCR to half of one's time,
        // until the text in it changes.
        let rest = start.elapsed().min(MAX_REST);
        if shared.frame.lock().unwrap().is_some() {
            let resting = Instant::now();
            while resting.elapsed() < rest && !shared.obsolete.load(Ordering::Relaxed) {
                std::thread::sleep(POLL / 3);
            }
        }
        idle_since = Instant::now();
    }
}

/// What has come on screen since the last look, in reading order, for the history: a line read
/// again later, as when a game goes back to it, counts each time. A text off the screen only for
/// a moment does not, as a menu item does while the mouse lights it up.
fn newly_shown(patches: &[Patch], before: &[Patch], seen: &mut HashMap<String, Instant>, now: Instant) -> Vec<(String, String)> {
    let shown = |p: &&Patch| !p.source.is_empty() && !p.text.is_empty();
    let back = |p: &&Patch| !before.iter().any(|b| b.source == p.source) && seen.get(&p.source).is_none_or(|&t| now.duration_since(t) >= BACK_AFTER);
    let new = patches.iter().filter(shown).filter(back).map(|p| (p.source.clone(), p.text.clone())).collect();
    for p in patches.iter().filter(shown) {
        seen.insert(p.source.clone(), now);
    }
    seen.retain(|_, &mut t| now.duration_since(t) < BACK_AFTER * 60);
    new
}

/// Two readings of one line: all but a letter or two around a part they share.
fn same_reading(a: &str, b: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let start = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let end = a.iter().rev().zip(b.iter().rev()).take_while(|(x, y)| x == y).count();
    start + end + READING_SLIP >= a.len().max(b.len())
}

/// OCR of a moving picture wobbles: a box moves by a pixel or two, a letter reads differently, a
/// line goes unread for a frame. Text that stayed keeps its old patch.
// shortcut: one letter changing in over fifty counts as wobble; a window into the text would tell.
fn hold_still(patches: &mut Vec<Patch>, (basis, shown): &(Vec<u8>, Vec<Patch>), frame: &Frame) {
    let unchanged = |old: &Patch| basis.len() == frame.pixels.len() && old.text_changed(&frame.pixels, frame.w) <= WOBBLE_PERCENT;
    // The picture behind kept text can have moved on; the copy painted under it is made again.
    let kept = |old: &Patch| match old.backdrop_behind(&frame.pixels, frame.w) {
        true => old.clone().keep_picture(&frame.pixels, frame.w, frame.h),
        false => old.clone(),
    };
    for patch in patches.iter_mut() {
        let near = |a: i32, b: i32| (a - b).abs() <= SAME_PLACE_PX;
        let same = shown.iter().find(|old| {
            let place = near(old.x, patch.x) && near(old.y, patch.y) && near(old.w, patch.w) && near(old.h, patch.h);
            (old.text == patch.text && place) || (old.overlaps(patch) && same_reading(&old.source, &patch.source) && unchanged(old))
        });
        if let Some(old) = same {
            *patch = kept(old);
        }
    }
    let unread = shown.iter().filter(|old| !old.source.is_empty() && unchanged(old) && !patches.iter().any(|p| p.overlaps(old)));
    let unread: Vec<Patch> = unread.map(kept).collect();
    patches.extend(unread);
    patches.sort_by_key(|p| (p.y, p.x)); // what comes and goes must not look like a change
}

/// Hands changed frames to the worker. Live, every change counts, and text whose pixels have
/// changed comes off straight away. One-shot, the job ends once most of the page has changed.
/// Several regions are watched as one picture of the area around them, blank between them: a live
/// region as it is now, a one-shot region as it first was, so that only live ones get translated
/// again. Returns the one-shot regions whose page has changed, which are done with.
fn keep_parts(frame: &[u8], out: &mut Vec<u8>, width: usize, (x, y): (i32, i32), parts: &[Part], still: &mut Vec<(Region, Vec<u8>)>) -> Vec<Region> {
    let height = frame.len() / 4 / width;
    let mut gone = Vec::new();
    out.clear();
    out.resize(frame.len(), 0);
    for part in parts {
        let region = part.region;
        let span = |from: i32, length: i32, at: i32, limit: usize| ((from - at).clamp(0, limit as i32) as usize, (from + length - at).clamp(0, limit as i32) as usize);
        let (left, right) = span(region.x, region.w, x, width);
        let (top, bottom) = span(region.y, region.h, y, height);
        let rows = || (top..bottom).map(|row| (row * width + left) * 4..(row * width + right) * 4);
        let now: Vec<u8> = rows().flat_map(|cells| frame[cells].iter().copied()).collect();
        let shown = if part.live {
            now
        } else {
            match still.iter().find(|(r, _)| *r == region) {
                Some((_, first)) if changed_pixels(&now, first) * 100 > first.len() / 4 * STALE_PERCENT => {
                    gone.push(region);
                    continue;
                }
                Some((_, first)) => first.clone(),
                None => {
                    still.push((region, now.clone()));
                    now
                }
            }
        };
        let line = (right - left) * 4;
        if line > 0 {
            for (cells, row) in rows().zip(shown.chunks_exact(line)) {
                out[cells].copy_from_slice(row);
            }
        }
    }
    gone
}

fn watch(job: Job, overlay: usize, shared: &Shared) {
    let post = |patches: Vec<Patch>| post_patches(overlay, job.id, patches);
    // Never OCR our own output. The last watcher may have just lifted this, and what was on show
    // takes a frame or two to leave the screen.
    unsafe { SetWindowDisplayAffinity(overlay as HWND, WDA_EXCLUDEFROMCAPTURE) };
    std::thread::sleep(POLL * 2);
    let finish = || unsafe {
        set_job(None);
        PostMessageW(overlay as HWND, WM_JOB_OVER, job.id as usize, 0);
    };
    let mut grabber: Option<capture::Grabber> = None;
    let mut size = (0, 0);
    let mut last: Vec<u8> = Vec::new();
    let mut parted: Vec<u8> = Vec::new();
    let mut still: Vec<(Region, Vec<u8>)> = Vec::new();
    let mut shown = false; // the overlay is in what screen capture sees
    let started = Instant::now();
    while let Some(current) = current_job(job.id) {
        let Region { x, y, w, h } = current.region;
        if w <= 0 || h <= 0 {
            std::thread::sleep(WATCH); // the window is off every screen for now
            continue;
        }
        if (w, h) != size {
            grabber = capture::Grabber::new(w, h);
            size = (w, h);
            last.clear();
            shared.done.store(false, Ordering::Relaxed);
            shared.obsolete.store(true, Ordering::Relaxed);
        }
        let Some(grabber) = &grabber else {
            post(vec![Patch::status("画面を取得できません", w)]);
            return finish();
        };
        if current.window != 0 && window_is_gone(current.window as HWND) {
            return finish();
        }
        let moving = MOVED_AT.lock().unwrap().is_some_and(|at| at.elapsed() < MOVE_SETTLE);
        let settled = last.is_empty() || job.live || shared.done.load(Ordering::Relaxed);
        if !moving && settled && !PAUSED.load(Ordering::Relaxed) {
            // For screen sharing each window is read as it draws itself, which leaves the overlay out.
            let parts = PARTS.lock().unwrap().clone();
            let from_windows = config::get().shared
                && if parts.is_empty() {
                    current.window != 0 && grabber.read_window(current.window as HWND, (x, y, w, h), (0, 0))
                } else {
                    parts.iter().all(|p| {
                        let Region { x: px, y: py, w: pw, h: ph } = p.region;
                        p.window != 0 && grabber.read_window(p.window as HWND, (px, py, pw, ph), (px - x, py - y))
                    })
                };
            let mut frame = match from_windows {
                true => {
                    if !shown {
                        unsafe { SetWindowDisplayAffinity(overlay as HWND, WDA_NONE) };
                        shown = true;
                    }
                    grabber.pixels()
                }
                false => {
                    // A one-shot result stays visible to screenshots, so it leaves the capture only here.
                    let trick = !job.live && !last.is_empty();
                    if trick || shown {
                        unsafe { SetWindowDisplayAffinity(overlay as HWND, WDA_EXCLUDEFROMCAPTURE) };
                    }
                    let frame = grabber.grab(x, y);
                    if trick {
                        unsafe { SetWindowDisplayAffinity(overlay as HWND, WDA_NONE) };
                    }
                    shown = trick;
                    frame
                }
            };
            if !parts.is_empty() {
                // The selection layer takes a moment to leave the screen, so the first looks count for nothing.
                if started.elapsed() < SETTLE {
                    still.clear();
                }
                let gone = keep_parts(frame, &mut parted, w as usize, (x, y), &parts, &mut still);
                frame = &parted;
                if !gone.is_empty() {
                    PARTS.lock().unwrap().retain(|p| !gone.contains(&p.region));
                    if gone.len() == parts.len() {
                        return finish();
                    }
                    unsafe { PostMessageW(overlay as HWND, WM_PARTS, job.id as usize, 0) };
                }
            }
            let changed = frame != last;
            let changed_for_blur = changed && (job.live || last.is_empty());
            if changed {
                let first = last.is_empty();
                if job.live || first {
                    if job.live {
                        drop_stale_patches(frame, w as usize, shared, &post);
                        give_up_on_stale_work(frame, w as usize, shared);
                    }
                    last.clear();
                    last.extend_from_slice(frame);
                    shared.latest.lock().unwrap().clone_from(&last);
                    *shared.frame.lock().unwrap() = Some(Frame { pixels: last.clone(), w: w as usize, h: h as usize });
                    shared.fresh.notify_one();
                } else if changed_pixels(frame, &last) * 100 > last.len() / 4 * STALE_PERCENT {
                    return finish();
                }
            }
            // The renderer uses the accepted image, including frozen one-shot regions. No new grab.
            blur::publish(overlay, job.id, current.region, &last, changed_for_blur);
        }
        let watching = job.live && (PARTS.lock().unwrap().is_empty() || PARTS.lock().unwrap().iter().any(|p| p.live));
        std::thread::sleep(if watching || last.is_empty() { POLL } else { WATCH });
    }
}

/// Closed, hidden, or on another virtual desktop, which is what a cloaked window is.
fn window_is_gone(window: HWND) -> bool {
    unsafe {
        let mut cloaked: u32 = 0;
        DwmGetWindowAttribute(window, DWMWA_CLOAKED as u32, &mut cloaked as *mut _ as *mut _, 4);
        IsWindow(window) == 0 || (IsWindowVisible(window) == 0 && IsIconic(window) == 0) || cloaked != 0
    }
}

fn changed_pixels(a: &[u8], b: &[u8]) -> usize {
    a.chunks_exact(4).zip(b.chunks_exact(4)).filter(|(p, q)| p != q).count()
}

fn drop_stale_patches(frame: &[u8], width: usize, shared: &Shared, post: &impl Fn(Vec<Patch>)) {
    let (basis, patches) = &mut *shared.shown.lock().unwrap();
    if basis.len() != frame.len() {
        return;
    }
    let stale: Vec<bool> = patches.iter().map(|p| p.is_gone(frame, width)).collect();
    // The picture behind text that stays can move on, as when a scene changes behind a game's
    // dialogue; the copy painted under the translation would then stand still in it.
    let behind: Vec<bool> = patches.iter().zip(&stale).map(|(p, &stale)| !stale && p.backdrop_behind(frame, width)).collect();
    if !stale.contains(&true) && !behind.contains(&true) {
        return;
    }
    // Text that has only moved, as a chat's lines do for a new one, takes its translation along.
    let moves = if stale.contains(&true) { scrolls(basis, frame, width) } else { Vec::new() };
    let height = frame.len() / 4 / width;
    let (mut followed, mut repainted) = (false, false);
    let kept: Vec<Patch> = patches
        .drain(..)
        .zip(stale.into_iter().zip(behind))
        .filter_map(|(p, (stale, behind))| {
            if behind {
                repainted = true;
                return Some(p.keep_picture(frame, width, height));
            }
            if !stale {
                return Some(p);
            }
            let dy = scrolled(&moves, &p, frame, basis, width)?;
            followed = true;
            Some(p.shifted(dy, frame, width))
        })
        .collect();
    *patches = kept;
    if followed || repainted {
        *basis = frame.to_vec(); // the text is where it now is, on what is now behind it
    }
    post(patches.clone());
}

/// Translating a page that has gone would only paint its text over the next one. Text that has
/// only moved is still worth finishing: it goes up where it has moved to.
fn give_up_on_stale_work(frame: &[u8], width: usize, shared: &Shared) {
    let (basis, patches) = &*shared.working.lock().unwrap();
    if basis.len() == frame.len() {
        let gone: Vec<&Patch> = patches.iter().filter(|p| p.is_stale(frame, basis, width)).collect();
        let moves = if gone.is_empty() { Vec::new() } else { scrolls(basis, frame, width) };
        let stale = gone.iter().filter(|p| scrolled(&moves, p, frame, basis, width).is_none()).count();
        if stale * 2 > patches.len() {
            shared.obsolete.store(true, Ordering::Relaxed);
        }
    }
}

type Rect = (usize, usize, usize, usize); // left, top, right, bottom

/// Each region's part of the picture whose content has scrolled since `basis`, and how far.
fn scrolls(basis: &[u8], frame: &[u8], width: usize) -> Vec<(Rect, i32)> {
    let height = frame.len() / 4 / width.max(1);
    let parts = PARTS.lock().unwrap().clone();
    let origin = JOB.lock().unwrap().map(|job| (job.region.x, job.region.y));
    let rects: Vec<Rect> = match origin.filter(|_| !parts.is_empty()) {
        None => vec![(0, 0, width, height)],
        Some((x, y)) => {
            let at = |v: i32, limit: usize| v.clamp(0, limit as i32) as usize;
            parts.iter().map(|p| (at(p.region.x - x, width), at(p.region.y - y, height), at(p.region.x + p.region.w - x, width), at(p.region.y + p.region.h - y, height))).collect()
        }
    };
    rects.into_iter().filter_map(|rect| patch::scroll(basis, frame, width, rect).map(|dy| (rect, dy))).collect()
}

/// How far a patch's text has scrolled with the part it is in, if it is found there.
fn scrolled(moves: &[(Rect, i32)], p: &Patch, frame: &[u8], basis: &[u8], width: usize) -> Option<i32> {
    let (cx, cy) = ((p.x + p.w / 2).max(0) as usize, (p.y + p.h / 2).max(0) as usize);
    let inside = |&&((left, top, right, bottom), _): &&(Rect, i32)| (left..right).contains(&cx) && (top..bottom).contains(&cy);
    moves.iter().find(inside).map(|&(_, dy)| dy).filter(|&dy| p.moved(frame, basis, width, dy))
}

/// Brings patches made from `pixels` up to the newest picture, if the screen has moved on since:
/// text that scrolled moves with it, text that has gone comes off. Returns that picture.
fn catch_up(pixels: &[u8], patches: &mut Vec<Patch>, width: usize, shared: &Shared) -> Option<Vec<u8>> {
    let latest = shared.latest.lock().unwrap().clone();
    if latest.len() != pixels.len() || latest == pixels {
        return None;
    }
    let moves = scrolls(pixels, &latest, width);
    *patches = std::mem::take(patches)
        .into_iter()
        .filter_map(|p| {
            if p.source.is_empty() || !p.is_stale(&latest, pixels, width) {
                return Some(p);
            }
            scrolled(&moves, &p, &latest, pixels, width).map(|dy| p.shifted(dy, &latest, width))
        })
        .collect();
    Some(latest)
}

/// OCR of a frame. Several regions are read one by one: the picture around regions far apart, a
/// game on one screen and comments on another, is large, and OCR scales a large picture down
/// until small text is lost.
fn read_lines(ocr: &ocr::Ocr, frame: &[u8], w: usize, h: usize, shared: &Shared) -> Result<Vec<ocr::Line>, String> {
    let parts = PARTS.lock().unwrap().clone();
    let Some(job) = (*JOB.lock().unwrap()).filter(|_| !parts.is_empty()) else { return ocr.read(frame, w, h) };
    let mut last = shared.read.lock().unwrap();
    read_parts(&parts, (job.region.x, job.region.y), (frame, w, h), &mut last, |pixels, pw, ph| ocr.read(pixels, pw, ph))
}

/// Each region read on its own, and only if its picture changed since it was last read: with a
/// comment viewer scrolling on, the game's subtitle is not read again with every new comment.
fn read_parts(
    parts: &[Part],
    (x, y): (i32, i32),
    (frame, w, h): (&[u8], usize, usize),
    last: &mut Vec<ReadPart>,
    mut read: impl FnMut(&[u8], usize, usize) -> Result<Vec<ocr::Line>, String>,
) -> Result<Vec<ocr::Line>, String> {
    let mut lines = Vec::new();
    let mut now = Vec::new();
    for part in parts {
        let r = part.region;
        let span = |from: i32, length: i32, at: i32, limit: usize| ((from - at).clamp(0, limit as i32) as usize, (from + length - at).clamp(0, limit as i32) as usize);
        let (left, right) = span(r.x, r.w, x, w);
        let (top, bottom) = span(r.y, r.h, y, h);
        if right <= left || bottom <= top {
            continue;
        }
        let pixels = crop(frame, w, (left, top, right, bottom));
        let found = match last.iter().position(|(region, before, _)| *region == r && *before == pixels) {
            Some(index) => last.swap_remove(index).2,
            None => {
                let mut read = read(&pixels, right - left, bottom - top)?;
                for line in &mut read {
                    line.glyphs.x += left as i32;
                    line.glyphs.y += top as i32;
                }
                read
            }
        };
        lines.extend(found.iter().cloned());
        now.push((r, pixels, found));
    }
    *last = now;
    Ok(lines)
}

/// The rectangle from left to right and top to bottom of a BGRA picture `w` wide.
fn crop(frame: &[u8], w: usize, (left, top, right, bottom): (usize, usize, usize, usize)) -> Vec<u8> {
    (top..bottom).flat_map(|row| &frame[(row * w + left) * 4..(row * w + right) * 4]).copied().collect()
}

fn translate_frame(
    frame: &Frame,
    live: bool,
    models: &mut Option<ocr::Ocr>,
    translator: &mut translate::Translator,
    keep_going: &(impl Fn() -> bool + Sync),
    post: &(impl Fn(Vec<Patch>) + Sync),
    shared: &Shared,
) -> Result<Vec<Patch>, String> {
    ensure_ocr(models)?;
    translator.follow_languages();
    let Frame { pixels, w, h } = frame;
    let (w, h) = (*w, *h);
    let frame = pixels.as_slice();
    let lines = read_lines(models.as_ref().unwrap(), frame, w, h, shared)?;
    let blocks = ocr::paragraphs(&lines);
    let blocks: Vec<&ocr::Block> = blocks.iter().filter(|b| !ocr::looks_like_code(&b.text)).collect();
    let everything = blocks.iter().map(|block| Patch::new(frame, w, h, block, String::new())).collect();
    *shared.working.lock().unwrap() = (frame.to_vec(), everything);
    // Numbers, code and the like come back unchanged: leave the original on show.
    // Over a picture the letters are painted out of a copy of it. Live, the copy is painted again
    // whenever the picture behind it moves on (`hold_still`, `drop_stale_patches`).
    let patch = |block: &ocr::Block, text: &str| {
        (text.to_lowercase() != block.text.to_lowercase()).then(|| {
            let patch = Patch::new(frame, w, h, block, text.to_string());
            if block.glyphs.picture { patch.keep_picture(frame, w, h) } else { patch }
        })
    };
    // Whatever is known already goes up with the first new line, so nothing flickers.
    let mut slots: Vec<Option<Patch>> = vec![None; blocks.len()];
    let mut pending: Vec<&str> = Vec::new();
    for (slot, block) in slots.iter_mut().zip(&blocks) {
        match translator.known(&block.text) {
            Some(text) => *slot = patch(block, &text),
            None if !pending.contains(&block.text.as_str()) => pending.push(&block.text),
            None => {}
        }
    }
    let shared = Mutex::new((slots, Instant::now()));
    let result = translator.translate_all(&pending, keep_going, &|source, translation| {
        let (slots, last_post) = &mut *shared.lock().unwrap();
        for (slot, block) in slots.iter_mut().zip(&blocks) {
            if block.text == source {
                *slot = patch(block, translation);
            }
        }
        if last_post.elapsed() > PARTIAL_EVERY {
            *last_post = Instant::now();
            post(slots.iter().flatten().cloned().collect());
        }
    });
    let mut out: Vec<Patch> = shared.into_inner().unwrap().0.into_iter().flatten().collect();
    // What did finish stays on show: one failed reply is no reason to lose the page.
    if let Err(e) = result {
        out.push(Patch::status(&format!("{}{}", t("エラー: "), t(&e)), w as i32));
    } else if out.is_empty() && !live {
        out.push(Patch::status(&t(NOT_FOUND), w as i32));
    }
    Ok(out)
}

// --- UI thread state. Cells only: window procedures re-enter while Win32 calls run.

struct Ui {
    main: Cell<HWND>,
    overlay: Cell<HWND>,
    frame: Cell<HWND>,
    line: Cell<HWND>, // around a drag
    selecting: Cell<Option<Region>>, // the drag so far, in screen pixels
    peeking: Cell<bool>, // the original is on show while a key is held
    shift_seen: Cell<bool>, // Shift was down during the drag in progress
    resume: Cell<Option<Region>>, // the live job to start again after a pause
    mouse_hook: Cell<HHOOK>,
    flashing: Cell<bool>,
    select: Cell<HWND>,
    drag_from: Cell<Option<(i32, i32)>>,
    drag_to: Cell<(i32, i32)>,
    hover: Cell<Option<(i32, i32, i32, i32)>>, // the window a click would select, in client pixels
    hover_window: Cell<HWND>,
    region: Cell<Region>,
    anchor: Cell<Option<Anchor>>,
    clip: Cell<HRGN>, // a copy of the overlay's region, where windows in front cut it
    hook: Cell<HWINEVENTHOOK>,
    job_id: Cell<u64>, // results from older jobs are dropped
    live: Cell<bool>,
    patches: RefCell<Vec<Patch>>,
}

thread_local! {
    static UI: Ui = Ui {
        main: Cell::new(null_mut()),
        overlay: Cell::new(null_mut()),
        frame: Cell::new(null_mut()),
        line: Cell::new(null_mut()),
        selecting: Cell::new(None),
        peeking: Cell::new(false),
        shift_seen: Cell::new(false),
        resume: Cell::new(None),
        mouse_hook: Cell::new(null_mut()),
        flashing: Cell::new(false),
        select: Cell::new(null_mut()),
        drag_from: Cell::new(None),
        drag_to: Cell::new((0, 0)),
        hover: Cell::new(None),
        hover_window: Cell::new(null_mut()),
        region: Cell::new(Region { x: 0, y: 0, w: 0, h: 0 }),
        anchor: Cell::new(None),
        clip: Cell::new(null_mut()),
        hook: Cell::new(null_mut()),
        job_id: Cell::new(0),
        live: Cell::new(false),
        patches: RefCell::new(Vec::new()),
    };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// For the calls that take no length and read up to a NUL.
fn wide_z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn lparam_xy(l: LPARAM) -> (i32, i32) {
    ((l & 0xFFFF) as u16 as i16 as i32, ((l >> 16) & 0xFFFF) as u16 as i16 as i32)
}

/// The model leaves memory, and a job that is running would only load it again.
pub fn unload_now() {
    stop();
    request_unload();
}

fn stop() {
    set_job(None);
    UI.with(|ui| unsafe {
        ui.live.set(false);
        ui.job_id.set(0);
        ui.flashing.set(false);
        ui.peeking.set(false);
        ui.patches.borrow_mut().clear();
        ShowWindow(ui.overlay.get(), SW_HIDE);
        ShowWindow(ui.frame.get(), SW_HIDE);
        KillTimer(ui.main.get(), CLIP_TIMER);
        unclip(ui);
        if !ui.hook.get().is_null() {
            UnhookWinEvent(ui.hook.replace(null_mut()));
        }
    });
}

/// Shows the frame around the region, or hides it, as the job and the settings have it.
pub fn update_frame() {
    UI.with(|ui| unsafe {
        let frame = ui.frame.get();
        let config = config::get();
        let flashing = ui.flashing.get();
        // While choosing regions the ones already there show, frame or no frame in the settings.
        let selecting = !ui.select.get().is_null();
        let wanted = ui.job_id.get() != 0 && !PAUSED.load(Ordering::Relaxed) && (config.show_frame || flashing || selecting);
        let region = ui.region.get();
        if !wanted || region.w <= 0 || region.h <= 0 {
            ShowWindow(frame, SW_HIDE);
            return;
        }
        let t = (if flashing { FLASH_PX } else { FRAME_PX }) * GetDpiForWindow(frame) as i32 / 96;
        let outer = RECT { left: region.x - t, top: region.y - t, right: region.x + region.w + t, bottom: region.y + region.h + t };
        let r = frame_rect(region, outer, on_its_monitor(&outer));
        SetWindowPos(frame, HWND_TOPMOST, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        InvalidateRect(frame, null(), 0);
        UpdateWindow(frame);
    });
}

/// The green line around a drag. The window covers every screen and only its shape follows the
/// drag: a window that grew on every move would show its new inside black until painted.
unsafe fn place_line(line: HWND, selecting: Option<Region>) {
    unsafe {
        let Some(region) = selecting else {
            ShowWindow(line, SW_HIDE);
            return;
        };
        let (mut dpi, mut unused) = (96, 96);
        let centre = POINT { x: region.x + region.w / 2, y: region.y + region.h / 2 };
        GetDpiForMonitor(MonitorFromPoint(centre, MONITOR_DEFAULTTONEAREST), MDT_EFFECTIVE_DPI, &mut dpi, &mut unused);
        let t = FRAME_PX * dpi as i32 / 96;
        let outer = RECT { left: region.x - t, top: region.y - t, right: region.x + region.w + t, bottom: region.y + region.h + t };
        let r = frame_rect(region, outer, on_its_monitor(&outer));
        let (vx, vy) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
        let ring = CreateRectRgn(r.left - vx, r.top - vy, r.right - vx, r.bottom - vy);
        let inside = CreateRectRgn(r.left - vx + t, r.top - vy + t, r.right - vx - t, r.bottom - vy - t);
        CombineRgn(ring, ring, inside, RGN_DIFF);
        DeleteObject(inside);
        SetWindowRgn(line, ring, 1); // the window owns the region from here; the class brush paints it
        if IsWindowVisible(line) == 0 {
            let (vw, vh) = (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN));
            SetWindowPos(line, HWND_TOPMOST, vx, vy, vw, vh, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        }
        UpdateWindow(line);
    }
}

/// `visible` is the part of `outer` its monitor shows. A maximised window has no outside, so the
/// frame comes in over its edge; a region across monitors keeps the whole frame.
fn frame_rect(region: Region, outer: RECT, visible: RECT) -> RECT {
    let fits = visible.left <= region.x && visible.top <= region.y && visible.right >= region.x + region.w && visible.bottom >= region.y + region.h;
    if fits { visible } else { outer }
}

unsafe fn paint_frame(hwnd: HWND) {
    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut r: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut r);
        let flashing = UI.with(|ui| ui.flashing.get());
        let t = (if flashing { FLASH_PX } else { FRAME_PX }) * GetDpiForWindow(hwnd) as i32 / 96;
        let color = if flashing { FLASH_COLOR } else { FRAME_COLOR };
        fill(hdc, &r, KEY_COLOR);
        let mut corner: RECT = std::mem::zeroed();
        GetWindowRect(hwnd, &mut corner);
        let parts = PARTS.lock().unwrap().clone();
        let around = |p: &Part| {
            let p = p.region;
            RECT { left: p.x - t - corner.left, top: p.y - t - corner.top, right: p.x + p.w + t - corner.left, bottom: p.y + p.h + t - corner.top }
        };
        let boxes = if parts.is_empty() { vec![r] } else { parts.iter().map(around).collect() };
        for b in boxes {
            fill(hdc, &RECT { bottom: b.top + t, ..b }, color);
            fill(hdc, &RECT { top: b.bottom - t, ..b }, color);
            fill(hdc, &RECT { right: b.left + t, ..b }, color);
            fill(hdc, &RECT { left: b.right - t, ..b }, color);
        }
        EndPaint(hwnd, &ps);
    }
}

unsafe extern "system" fn frame_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                paint_frame(hwnd);
                0
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}

fn frame_bounds(window: HWND) -> Option<RECT> {
    unsafe {
        let mut bounds: RECT = std::mem::zeroed();
        let size = size_of::<RECT>() as u32;
        let ok = DwmGetWindowAttribute(window, DWMWA_EXTENDED_FRAME_BOUNDS as u32, &mut bounds as *mut _ as *mut _, size) == 0;
        ok.then_some(bounds)
    }
}

/// `bounds` cut to the monitor it mostly sits on: a window can hang off the edge.
fn on_its_monitor(bounds: &RECT) -> RECT {
    unsafe {
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(MonitorFromRect(bounds, MONITOR_DEFAULTTONEAREST), &mut info);
        let mut visible: RECT = std::mem::zeroed();
        IntersectRect(&mut visible, bounds, &info.rcMonitor);
        visible
    }
}

fn anchored_region(anchor: Anchor, size: Region) -> Option<Region> {
    let bounds = frame_bounds(anchor.window)?;
    if anchor.whole {
        let r = on_its_monitor(&bounds);
        return Some(Region { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top });
    }
    Some(Region { x: bounds.left + anchor.dx, y: bounds.top + anchor.dy, ..size })
}

/// The window moved: the overlay goes with it at once, which needs no OCR. A new size means new
/// content, so the old text comes off and the watcher takes the job from there.
unsafe extern "system" fn on_location(_: HWINEVENTHOOK, _: u32, window: HWND, object: i32, child: i32, _: u32, _: u32) {
    if object != OBJID_WINDOW || child != 0 {
        return;
    }
    UI.with(|ui| unsafe {
        let Some(anchor) = ui.anchor.get().filter(|a| a.window == window) else { return };
        let overlay = ui.overlay.get();
        let minimised = IsIconic(window) != 0;
        PAUSED.store(minimised, Ordering::Relaxed);
        if minimised {
            ShowWindow(overlay, SW_HIDE);
            update_frame();
            return;
        }
        let old = ui.region.get();
        let Some(new) = anchored_region(anchor, old) else { return };
        *MOVED_AT.lock().unwrap() = Some(Instant::now());
        if new != old {
            ui.region.set(new);
            if let Some(job) = JOB.lock().unwrap().as_mut().filter(|job| job.id == ui.job_id.get()) {
                job.region = new;
            }
            let resized = (new.w, new.h) != (old.w, old.h);
            if resized {
                ShowWindow(overlay, SW_HIDE);
                ui.patches.borrow_mut().clear();
            } else {
                SetWindowPos(overlay, null_mut(), new.x, new.y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
            }
        }
        if !ui.patches.borrow().is_empty() {
            ShowWindow(overlay, SW_SHOWNOACTIVATE);
        }
        update_frame();
    });
}

// --- Selection layer

unsafe fn begin_select(live: bool) {
    let config = config::get();
    if models::needs_license(&config.model) && !config.license_accepted {
        return settings::open(); // which asks
    }
    UI.with(|ui| unsafe {
        if !ui.select.get().is_null() {
            // The hotkey again, instead of a drag: the same region as last time. For what is
            // already live that can only mean enough of it.
            let keep_live = live || bar::live();
            end_select();
            if live && ui.live.get() {
                return stop();
            }
            let region = ui.region.get();
            // The window may have gone since: what is left is the place on the screen.
            ui.anchor.set(ui.anchor.get().filter(|a| !window_is_gone(a.window) && IsIconic(a.window) == 0));
            let followed = ui.anchor.get().and_then(|anchor| anchored_region(anchor, region));
            if region.w > 0 {
                start_job(followed.unwrap_or(region), keep_live);
            }
            return;
        }
        request_load(); // ready by the time the drag ends
        let (vx, vy) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
        let (vw, vh) = (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN));
        let select = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
            w!("SonobaSelect"),
            null(),
            WS_POPUP,
            vx,
            vy,
            vw,
            vh,
            null_mut(),
            null_mut(),
            GetModuleHandleW(null()),
            null(),
        );
        SetLayeredWindowAttributes(select, KEY_COLOR, DIM_ALPHA, LWA_ALPHA | LWA_COLORKEY);
        SetWindowDisplayAffinity(select, WDA_EXCLUDEFROMCAPTURE); // the first frame may race its removal
        ui.select.set(select);
        ui.drag_from.set(None);
        ui.hover.set(None);
        RegisterHotKey(ui.main.get(), HK_ESC, 0, VK_ESCAPE as u32);
        ShowWindow(select, SW_SHOWNOACTIVATE);
        update_hover(select);
        bar::show(live, ui.job_id.get() != 0);
        update_frame(); // over the layer, where it is not dimmed
    });
}

/// The frontmost window at the point, below the selection layer, and its bounds.
unsafe fn window_at(select: HWND, at: POINT) -> Option<(HWND, RECT)> {
    unsafe {
        let mut hwnd = GetWindow(select, GW_HWNDNEXT);
        while !hwnd.is_null() {
            if is_solid(hwnd) {
                if let Some(bounds) = frame_bounds(hwnd).filter(|bounds| PtInRect(bounds, at) != 0) {
                    return Some((hwnd, bounds));
                }
            }
            hwnd = GetWindow(hwnd, GW_HWNDNEXT);
        }
        None
    }
}

/// Shown and taking clicks: not a see-through layer over the screen. Game overlays such as NVIDIA's
/// cover a whole monitor and let clicks through their clear pixels, without saying so in their style.
unsafe fn is_solid(hwnd: HWND) -> bool {
    unsafe {
        let style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        let mut cloaked: u32 = 0;
        DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED as u32, &mut cloaked as *mut _ as *mut _, 4);
        if IsWindowVisible(hwnd) == 0 || style & WS_EX_TRANSPARENT != 0 || cloaked != 0 {
            return false;
        }
        let layer = WS_EX_LAYERED | WS_EX_NOACTIVATE;
        let Some(bounds) = frame_bounds(hwnd).filter(|_| style & layer == layer) else { return true };
        let monitor = on_its_monitor(&bounds);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(MonitorFromRect(&bounds, MONITOR_DEFAULTTONEAREST), &mut info);
        let area = |r: &RECT| (r.right - r.left) as i64 * (r.bottom - r.top) as i64;
        area(&monitor) * 100 < area(&info.rcMonitor) * 95
    }
}

/// The window a region lies wholly inside, if any.
unsafe fn window_holding(select: HWND, r: Region) -> Option<HWND> {
    let centre = POINT { x: r.x + r.w / 2, y: r.y + r.h / 2 };
    let inside = |b: &RECT| r.x >= b.left && r.y >= b.top && r.x + r.w <= b.right && r.y + r.h <= b.bottom;
    unsafe { window_at(select, centre) }.filter(|(_, bounds)| inside(bounds)).map(|(window, _)| window)
}

/// A click with no drag selects the window under it, so that window gets a frame.
unsafe fn update_hover(select: HWND) {
    UI.with(|ui| unsafe {
        let (mut at, mut origin) = (POINT { x: 0, y: 0 }, POINT { x: 0, y: 0 });
        GetCursorPos(&mut at);
        ClientToScreen(select, &mut origin);
        let found = window_at(select, at);
        ui.hover_window.set(found.map_or(null_mut(), |(window, _)| window));
        let visible = found.map(|(_, bounds)| on_its_monitor(&bounds));
        let new = visible.map(|r| (r.left - origin.x, r.top - origin.y, r.right - origin.x, r.bottom - origin.y));
        let old = ui.hover.replace(new);
        if old != new {
            for edge in [old, new].into_iter().flatten().flat_map(hover_edges) {
                InvalidateRect(select, &edge, 0);
            }
        }
    });
}

/// The white frame around the window a click would select.
fn hover_edges((left, top, right, bottom): (i32, i32, i32, i32)) -> [RECT; 4] {
    let t = HOVER_FRAME;
    [
        RECT { left, top, right, bottom: top + t },
        RECT { left, top: bottom - t, right, bottom },
        RECT { left, top, right: left + t, bottom },
        RECT { left: right - t, top, right, bottom },
    ]
}

unsafe fn end_select() {
    UI.with(|ui| unsafe {
        UnregisterHotKey(ui.main.get(), HK_ESC);
        let select = ui.select.replace(null_mut());
        if !select.is_null() {
            ReleaseCapture();
            DestroyWindow(select);
            bar::hide();
            update_frame();
        }
        ui.selecting.set(None);
        place_line(ui.line.get(), None);
    });
}

/// A right-click inside a region takes that region away, and the layer stays for choosing more.
/// False when no region is there.
fn remove_region_at(x: i32, y: i32) -> bool {
    UI.with(|ui| {
        if ui.job_id.get() == 0 {
            return false;
        }
        let inside = |r: Region| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h;
        let mut parts = PARTS.lock().unwrap().clone();
        if parts.is_empty() {
            if !inside(ui.region.get()) {
                return false;
            }
            stop();
        } else {
            let Some(i) = parts.iter().rposition(|p| inside(p.region)) else { return false }; // the last added is on top
            parts.remove(i);
            ui.anchor.set(None);
            if let [last] = parts[..] {
                PARTS.lock().unwrap().clear();
                start_job(last.region, last.live);
            } else {
                let live = parts.iter().any(|p| p.live);
                let around = around_all(&parts);
                *PARTS.lock().unwrap() = parts;
                start_job(around, live);
            }
        }
        bar::set_running(ui.job_id.get() != 0);
        true
    })
}

fn selection_rect(from: (i32, i32), to: (i32, i32)) -> RECT {
    RECT { left: from.0.min(to.0), top: from.1.min(to.1), right: from.0.max(to.0), bottom: from.1.max(to.1) }
}

unsafe fn finish_select(hwnd: HWND) {
    UI.with(|ui| unsafe {
        let Some(from) = ui.drag_from.take() else { return };
        let mut rect = selection_rect(from, ui.drag_to.get());
        let mut origin = POINT { x: 0, y: 0 };
        ClientToScreen(hwnd, &mut origin);
        let live = bar::live();
        // Shift adds to what is running. A job with several regions stays where it is on screen.
        let adding = (ui.shift_seen.get() || shift_down()) && ui.job_id.get() != 0;
        let clicked = is_small(&rect);
        let mut anchor = None;
        if clicked {
            let Some((left, top, right, bottom)) = ui.hover.get() else { return end_select() };
            rect = RECT { left, top, right, bottom };
            anchor = Some(Anchor { window: ui.hover_window.get(), whole: true, dx: 0, dy: 0 });
        } else {
            // A part of one window moves with that window; a region across several stays put.
            let centre = POINT { x: (rect.left + rect.right) / 2 + origin.x, y: (rect.top + rect.bottom) / 2 + origin.y };
            if let Some((window, bounds)) = window_at(hwnd, centre) {
                let inside = rect.left + origin.x >= bounds.left
                    && rect.top + origin.y >= bounds.top
                    && rect.right + origin.x <= bounds.right
                    && rect.bottom + origin.y <= bounds.bottom;
                if inside {
                    anchor = Some(Anchor { window, whole: false, dx: rect.left + origin.x - bounds.left, dy: rect.top + origin.y - bounds.top });
                }
            }
        }
        let region = Region {
            x: rect.left + origin.x,
            y: rect.top + origin.y,
            w: rect.right - rect.left,
            h: rect.bottom - rect.top,
        };
        // Each of several regions is read from the window it lies in, when it lies in one.
        let window_of = |r: Region| window_holding(hwnd, r).filter(|&w| !is_desktop(w)).map_or(0, |w| w as usize);
        let mut parts = Vec::new();
        if adding {
            parts = PARTS.lock().unwrap().clone();
            if parts.is_empty() {
                let first = ui.region.get();
                parts.push(Part { region: first, live: ui.live.get(), window: window_of(first) });
            }
            parts.push(Part { region, live, window: window_of(region) });
        }
        end_select();
        if adding {
            let around = around_all(&parts);
            *PARTS.lock().unwrap() = parts;
            ui.anchor.set(None);
            start_job(around, live);
        } else {
            PARTS.lock().unwrap().clear();
            ui.anchor.set(anchor);
            start_job(region, live);
        }
    });
}

/// Shift counts if it was down at any point of the drag: fingers rarely leave it and the mouse
/// button at the same moment, and the state of the key is only ever the state right now.
fn shift_down() -> bool {
    unsafe { GetAsyncKeyState(VK_SHIFT as i32) < 0 }
}

fn around_all(parts: &[Part]) -> Region {
    let left = parts.iter().map(|p| p.region.x).min().unwrap_or(0);
    let top = parts.iter().map(|p| p.region.y).min().unwrap_or(0);
    let right = parts.iter().map(|p| p.region.x + p.region.w).max().unwrap_or(0);
    let bottom = parts.iter().map(|p| p.region.y + p.region.h).max().unwrap_or(0);
    Region { x: left, y: top, w: right - left, h: bottom - top }
}

fn is_small(rect: &RECT) -> bool {
    rect.right - rect.left < MIN_SELECTION || rect.bottom - rect.top < MIN_SELECTION
}

fn start_job(region: Region, live: bool) {
    stop();
    UI.with(|ui| unsafe {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let anchor = ui.anchor.get();
        ui.region.set(region);
        ui.job_id.set(id);
        // With several regions the job runs as live, and keep_parts holds the one-shot ones still.
        let parts = PARTS.lock().unwrap().clone();
        let live_now = if parts.is_empty() { live } else { parts.iter().any(|p| p.live) };
        let live = live || !parts.is_empty();
        ui.live.set(live_now);
        PAUSED.store(false, Ordering::Relaxed);
        if let Some(anchor) = anchor {
            let mut process = 0;
            let thread = GetWindowThreadProcessId(anchor.window, &mut process);
            ui.hook.set(SetWinEventHook(EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE, null_mut(), Some(on_location), process, thread, WINEVENT_OUTOFCONTEXT));
        }
        // The window to read, and to share. The desktop shows everything on it, so never that.
        let window = anchor.map(|a| a.window).filter(|&w| parts.is_empty() && !is_desktop(w));
        show_patches(vec![Patch::status(&t(WORKING), region.w)]);
        set_job(Some(Job { region, live, id, window: window.map_or(0, |w| w as usize) }));
        sources_changed(ui);
        update_frame();
    });
}

unsafe extern "system" fn select_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                let hdc = BeginPaint(hwnd, &mut ps);
                let (from, to, hover) = UI.with(|ui| (ui.drag_from.get(), ui.drag_to.get(), ui.hover.get()));
                let dragged = from.map(|from| selection_rect(from, to)).filter(|rect| !is_small(rect));
                let (shapes, color) = match (dragged, hover) {
                    (Some(rect), _) => (vec![rect], KEY_COLOR),
                    // A frame, not a clear area: clicks fall straight through the key colour.
                    (None, Some(hover)) => (hover_edges(hover).to_vec(), HOVER_COLOR),
                    (None, None) => (Vec::new(), 0),
                };
                // Each pixel once: black under the clear area would be on screen for a moment.
                for r in &shapes {
                    fill(hdc, r, color);
                    ExcludeClipRect(hdc, r.left, r.top, r.right, r.bottom);
                }
                FillRect(hdc, &ps.rcPaint, GetStockObject(BLACK_BRUSH));
                EndPaint(hwnd, &ps);
                0
            }
            WM_LBUTTONDOWN => {
                let at = lparam_xy(l);
                UI.with(|ui| {
                    ui.drag_from.set(Some(at));
                    ui.drag_to.set(at);
                    ui.shift_seen.set(shift_down());
                });
                SetCapture(hwnd);
                0
            }
            WM_MOUSEMOVE => {
                let at = lparam_xy(l);
                let dragging = UI.with(|ui| {
                    ui.drag_from.get().map(|from| {
                        ui.shift_seen.set(ui.shift_seen.get() || shift_down());
                        let before = ui.drag_to.replace(at);
                        if before == at {
                            return false; // the line moving under the cursor sends one of these too
                        }
                        // Only what changed: the strips the clear area gained or lost, and the hover
                        // frame as it gives way to the drag or comes back.
                        let shown = |r: RECT| (!is_small(&r)).then_some(r);
                        let old = shown(selection_rect(from, before));
                        let rect = selection_rect(from, at);
                        let new = shown(rect);
                        if old.is_some() != new.is_some() {
                            for edge in ui.hover.get().into_iter().flat_map(hover_edges) {
                                InvalidateRect(hwnd, &edge, 0);
                            }
                        }
                        let area = |r: Option<RECT>| r.map_or_else(|| CreateRectRgn(0, 0, 0, 0), |r| CreateRectRgnIndirect(&r));
                        let (changed, now) = (area(old), area(new));
                        CombineRgn(changed, changed, now, RGN_XOR);
                        InvalidateRgn(hwnd, changed, 0);
                        DeleteObject(changed);
                        DeleteObject(now);
                        let mut origin = POINT { x: 0, y: 0 };
                        ClientToScreen(hwnd, &mut origin);
                        ui.selecting.set((!is_small(&rect)).then(|| Region { x: rect.left + origin.x, y: rect.top + origin.y, w: rect.right - rect.left, h: rect.bottom - rect.top }));
                        true
                    })
                });
                match dragging {
                    Some(true) => UI.with(|ui| place_line(ui.line.get(), ui.selecting.get())),
                    Some(false) => {}
                    None => update_hover(hwnd),
                }
                0
            }
            WM_LBUTTONUP => {
                UI.with(|ui| ui.drag_to.set(lparam_xy(l)));
                finish_select(hwnd);
                0
            }
            // On the way up: the layer gone, the release would open the menu of the app below.
            WM_RBUTTONUP => {
                let (x, y) = lparam_xy(l);
                let mut origin = POINT { x: 0, y: 0 };
                ClientToScreen(hwnd, &mut origin);
                if !remove_region_at(x + origin.x, y + origin.y) {
                    end_select();
                }
                0
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}

// --- Overlay: everything is the see-through key colour except the patches

/// `narrow` is the letters' width in percent of the font's own.
unsafe fn text_font(px: i32, bold: bool, narrow: i32) -> HFONT {
    unsafe { text_font_quality(px, bold, narrow, CLEARTYPE_QUALITY as u32) }
}

unsafe fn text_font_quality(px: i32, bold: bool, narrow: i32, quality: u32) -> HFONT {
    let font = unsafe { font_of(px, 0, bold, quality) };
    if narrow >= 100 {
        return font;
    }
    unsafe {
        let dc = GetDC(null_mut());
        let old = SelectObject(dc, font);
        let mut metrics: TEXTMETRICW = std::mem::zeroed();
        GetTextMetricsW(dc, &mut metrics);
        SelectObject(dc, old);
        ReleaseDC(null_mut(), dc);
        DeleteObject(font);
        font_of(px, (metrics.tmAveCharWidth * narrow / 100).max(1), bold, quality)
    }
}

unsafe fn font_of(px: i32, width: i32, bold: bool, quality: u32) -> HFONT {
    let config = config::get();
    let face = wide_z(if config.font.is_empty() { DEFAULT_FONT } else { &config.font });
    unsafe {
        CreateFontW(
            -px,
            width,
            0,
            0,
            if bold || config.bold_text { FW_BOLD } else { FW_NORMAL } as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            quality,
            DEFAULT_PITCH as u32,
            face.as_ptr(),
        )
    }
}

const TEXT_FLAGS: DRAW_TEXT_FORMAT = DT_NOPREFIX | DT_EDITCONTROL; // the lines are broken by `wrapped`

/// The text cut where a line may break. Windows breaks Japanese between any two characters,
/// "プロ/ジェクト" included; this keeps a run of katakana, of kanji, or of Latin letters whole,
/// keeps the hiragana after a word with it (its okurigana and particles), and starts no line with
/// closing punctuation or a small kana.
fn pieces_to_break(text: &str) -> Vec<String> {
    #[derive(PartialEq, Clone, Copy)]
    enum Kind { Latin, Katakana, Kanji, Hiragana, Other }
    let kind = |c: char| match c {
        'ァ'..='ヺ' | 'ー' | 'ｦ'..='ﾟ' => Kind::Katakana,
        'ぁ'..='ゖ' => Kind::Hiragana,
        '一'..='鿿' | '々' | '〆' => Kind::Kanji,
        c if c.is_ascii_graphic() || c.is_alphanumeric() && !c.is_ascii() && (c as u32) < 0x2000 => Kind::Latin,
        _ => Kind::Other,
    };
    let no_start = |c: char| "、。，．・：；？！）」』】〕〉》｝］ー々ぁぃぅぇぉっゃゅょゎァィゥェォッャュョヮヵヶ…‥".contains(c);
    let no_end = |c: char| "（「『【〔〈《｛［".contains(c);
    let mut out: Vec<String> = Vec::new();
    let mut prev: Option<char> = None;
    for c in text.chars() {
        let starts = match prev {
            None => true,
            Some(p) if p == ' ' || p == '\n' => true,
            Some(_) if c == ' ' || no_start(c) => false,
            Some(p) if no_end(p) => false,
            Some(p) if kind(c) == Kind::Hiragana => "、。，．！？".contains(p),
            Some(p) => kind(p) != kind(c) || kind(c) == Kind::Other,
        };
        if starts {
            out.push(String::new());
        }
        out.last_mut().unwrap().push(c);
        prev = Some(c);
    }
    out
}

/// The text with a line break wherever the next piece would run past `width`, in the font on
/// `hdc`. A piece wider than a line is cut into lines of about equal length, which tends to land
/// between the words of a compound such as "プロジェクトロードマップ".
unsafe fn wrapped(hdc: HDC, text: &str, width: i32) -> Vec<u16> {
    let width_of = |s: &str| unsafe {
        let units = wide(s.trim_end());
        let mut size = SIZE { cx: 0, cy: 0 };
        GetTextExtentPoint32W(hdc, units.as_ptr(), units.len() as i32, &mut size);
        size.cx
    };
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for piece in pieces_to_break(text) {
        if piece.starts_with('\n') {
            lines.push(std::mem::take(&mut line));
            line = piece[1..].to_string();
            continue;
        }
        if width_of(&format!("{line}{piece}")) <= width || line.trim().is_empty() && width_of(&piece) <= width {
            line.push_str(&piece);
            continue;
        }
        if !line.trim().is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        if width_of(&piece) <= width {
            line = piece;
            continue;
        }
        let chars: Vec<char> = piece.chars().collect();
        let parts = (width_of(&piece) + width - 1) / width.max(1);
        let mut parts = parts.max(2) as usize;
        let cut = loop {
            let each = chars.len().div_ceil(parts);
            let cut: Vec<String> = chars.chunks(each.max(1)).map(|c| c.iter().collect()).collect();
            if parts >= chars.len() || cut.iter().all(|p| width_of(p) <= width) {
                break cut;
            }
            parts += 1;
        };
        let (last, rest) = cut.split_last().unwrap();
        lines.extend(rest.iter().cloned());
        line = last.clone();
    }
    lines.push(line);
    wide(&lines.iter().map(|l| l.trim_end()).collect::<Vec<_>>().join("\n"))
}

/// Keeps the original font size if the text fits, spreading one line over empty background
/// first, then narrowing the letters, and shrinking only after that: a translation as tall as
/// the labels around it reads better than a small one. A one-line original stays on one line
/// if narrowing can keep it there; a paragraph grows down over empty background before it
/// narrows. Overhanging leading is empty, so it does not count, and the patch grows otherwise
/// only when even MIN_TEXT_PX does not fit.
unsafe fn fit(hdc: HDC, patch: &mut Patch, status_px: i32) {
    let measure = |px: i32, narrow: i32, width: i32| unsafe {
        let font = text_font(px, patch.bold, narrow);
        let old = SelectObject(hdc, font);
        let text = wrapped(hdc, &patch.text, width);
        let mut rect = RECT { left: 0, top: 0, right: width, bottom: 0 };
        DrawTextW(hdc, text.as_ptr(), text.len() as i32, &mut rect, TEXT_FLAGS | DT_CALCRECT);
        SelectObject(hdc, old);
        DeleteObject(font);
        rect
    };
    let status = patch.h == 0;
    let mut px = if status { status_px } else { patch.em.max(1) };
    let smallest = MIN_TEXT_PX.min(px);
    let narrows: &[i32] = if status { &[100] } else { &NARROWS };
    let (size, width, narrow) = 'fit: loop {
        let room = patch.h + px / 2;
        // A label's glow can leave room for two lines; it narrows before it wraps.
        let line = if patch.one_line { measure(px, 100, 1 << 20).bottom.min(room) } else { room };
        let grown = patch.max_h.max(patch.h) + px / 2;
        let tries: Vec<(i32, i32)> = match patch.one_line {
            true => [line, room].iter().flat_map(|&limit| narrows.iter().map(move |&narrow| (limit, narrow))).collect(),
            false => narrows.iter().flat_map(|&narrow| [(room, narrow), (grown, narrow)]).collect(),
        };
        for (limit, narrow) in tries {
            let normal = measure(px, narrow, patch.w);
            if normal.bottom <= limit {
                if limit > room {
                    patch.h = patch.h.max(normal.bottom.min(patch.max_h)); // what it grew into is painted
                }
                break 'fit (normal, patch.w, narrow);
            }
            let spread = measure(px, narrow, patch.max_w);
            if spread.bottom <= limit {
                break 'fit (spread, patch.w.max(spread.right + 1), narrow);
            }
        }
        if status || px <= smallest {
            let normal = measure(px, 100, patch.w);
            patch.h = normal.bottom;
            break (normal, patch.w, 100);
        }
        px -= 1;
    };
    (patch.px, patch.narrow, patch.text_h) = (px, narrow, size.bottom);
    patch.w = if status { size.right } else { width };
}

/// Lays the patches out and shows them over the region. No patches hides the overlay.
fn show_patches(mut patches: Vec<Patch>) {
    UI.with(|ui| unsafe {
        let overlay = ui.overlay.get();
        if patches.is_empty() {
            ShowWindow(overlay, SW_HIDE);
            ui.patches.borrow_mut().clear();
            return;
        }
        let region = ui.region.get();
        SetWindowPos(overlay, null_mut(), region.x, region.y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        let status_px = STATUS_PX * GetDpiForWindow(overlay) as i32 / 96; // after the move: per monitor
        let hdc = GetDC(overlay);
        for patch in &mut patches {
            fit(hdc, patch, status_px);
        }
        ReleaseDC(overlay, hdc);

        let height = patches.iter().map(|p| p.y + p.h).max().unwrap_or(0).max(region.h);
        let text = patches.iter().map(|p| p.text.as_str()).collect::<Vec<_>>().join("\n");
        SetWindowTextW(overlay, wide_z(&text).as_ptr()); // keeps it readable by screen readers
        *ui.patches.borrow_mut() = patches;
        // Behind a minimised window the result waits for it to come back.
        let show = if PAUSED.load(Ordering::Relaxed) || ui.peeking.get() { 0 } else { SWP_SHOWWINDOW };
        SetWindowPos(overlay, HWND_TOPMOST, region.x, region.y, region.w, height, SWP_NOACTIVATE | show);
        InvalidateRect(overlay, null(), 0);
        share::repaint();
    });
}

/// Applies the font and strength from the settings to what is on show.
pub fn look_changed() {
    blur::configure();
    UI.with(|ui| unsafe {
        let alpha = if blur::active(ui.overlay.get()) { 255 } else { (config::get().opacity as u32 * 255 / 100) as u8 };
        SetLayeredWindowAttributes(ui.overlay.get(), KEY_COLOR, alpha, LWA_COLORKEY | LWA_ALPHA);
        let patches = ui.patches.borrow().clone();
        if !patches.is_empty() {
            show_patches(patches);
        }
    });
}

/// The original comes back for as long as the key is held.
fn peek_start() {
    UI.with(|ui| unsafe {
        if ui.job_id.get() == 0 {
            return;
        }
        ui.peeking.set(true);
        ShowWindow(ui.overlay.get(), SW_HIDE);
        if !config::get().hotkeys[3].is_mouse() {
            SetTimer(ui.main.get(), PEEK_TIMER, PEEK_MS, None);
        }
    });
}

fn peek_end() {
    UI.with(|ui| unsafe {
        KillTimer(ui.main.get(), PEEK_TIMER);
        if ui.peeking.replace(false) && !ui.patches.borrow().is_empty() && !PAUSED.load(Ordering::Relaxed) {
            ShowWindow(ui.overlay.get(), SW_SHOWNOACTIVATE);
        }
    });
}

pub fn copy_to_clipboard(text: &str) -> bool {
    let units: Vec<u16> = wide_z(text);
    UI.with(|ui| unsafe {
        // Other apps open the clipboard for a moment whenever it changes.
        let opened = (0..CLIPBOARD_TRIES).any(|_| {
            let opened = OpenClipboard(ui.main.get()) != 0;
            if !opened {
                std::thread::sleep(CLIPBOARD_WAIT);
            }
            opened
        });
        if !opened {
            return false;
        }
        EmptyClipboard();
        let memory = GlobalAlloc(GMEM_MOVEABLE, units.len() * 2);
        let target = GlobalLock(memory) as *mut u16;
        let copied = !target.is_null();
        if copied {
            std::ptr::copy_nonoverlapping(units.as_ptr(), target, units.len());
            GlobalUnlock(memory);
            SetClipboardData(CF_UNICODETEXT as u32, memory as HANDLE);
        }
        CloseClipboard();
        copied
    })
}

/// Says how it went beside the cursor: nothing else on screen changes.
fn copy_translation() {
    let text = UI.with(|ui| ui.patches.borrow().iter().filter(|p| !p.source.is_empty()).map(|p| p.text.clone()).collect::<Vec<_>>().join("\n"));
    let result = if text.is_empty() {
        "コピーする訳文がありません"
    } else if copy_to_clipboard(&text) {
        "訳文をコピーしました"
    } else {
        "クリップボードが使えません"
    };
    toast::show(&t(result));
}

/// Stops everything for a while, keys included, for when they clash with what is on screen. A
/// live job picks up again afterwards.
fn toggle_pause() {
    let pausing = !SUSPENDED.fetch_xor(true, Ordering::Relaxed);
    UI.with(|ui| unsafe {
        if pausing {
            end_select();
            ui.resume.set(ui.live.get().then(|| ui.region.get()));
            stop();
            pause_hotkeys();
        } else {
            register_hotkeys();
            if let Some(region) = ui.resume.take() {
                start_job(region, true);
            }
        }
        tray_icon(ui.main.get(), NIM_MODIFY);
    });
}

unsafe extern "system" fn mouse_hook(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        if code == HC_ACTION as i32 {
            let side = |data: &MSLLHOOKSTRUCT| if (data.mouseData >> 16) as u16 == XBUTTON1 { VK_XBUTTON1 } else { VK_XBUTTON2 };
            let data = &*(l as *const MSLLHOOKSTRUCT);
            let event = match w as u32 {
                WM_MBUTTONDOWN => Some((VK_MBUTTON, true)),
                WM_MBUTTONUP => Some((VK_MBUTTON, false)),
                WM_XBUTTONDOWN => Some((side(data), true)),
                WM_XBUTTONUP => Some((side(data), false)),
                _ => None,
            };
            if let Some((button, down)) = event {
                let held = |key: VIRTUAL_KEY, flag: u32| if GetAsyncKeyState(key as i32) < 0 { flag } else { 0 };
                let mods = held(VK_MENU, MOD_ALT) | held(VK_CONTROL, MOD_CONTROL) | held(VK_SHIFT, MOD_SHIFT);
                let keys = MOUSE_KEYS.lock().unwrap();
                // The release counts whatever the modifiers do meanwhile: it must not reach the app.
                let found = keys.iter().find(|(_, h)| h.key == button as u32 && (!down || h.mods == mods));
                if let Some((id, _)) = found {
                    let main = UI.with(|ui| ui.main.get());
                    if down {
                        PostMessageW(main, WM_HOTKEY, *id as usize, 0);
                    } else if *id == HK_PEEK {
                        PostMessageW(main, WM_PEEK_END, 0, 0);
                    }
                    return 1;
                }
            }
        }
        CallNextHookEx(null_mut(), code, w, l)
    }
}

/// The hook exists only while a mouse button is in use, and it only looks at those buttons.
fn set_mouse_keys(keys: Vec<(i32, config::Hotkey)>) {
    UI.with(|ui| unsafe {
        let wanted = !keys.is_empty();
        *MOUSE_KEYS.lock().unwrap() = keys;
        if wanted && ui.mouse_hook.get().is_null() {
            ui.mouse_hook.set(SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), GetModuleHandleW(null()), 0));
        } else if !wanted && !ui.mouse_hook.get().is_null() {
            UnhookWindowsHookEx(ui.mouse_hook.replace(null_mut()));
        }
    });
}

unsafe fn fill(hdc: HDC, rect: &RECT, color: u32) {
    unsafe {
        let brush = CreateSolidBrush(color);
        FillRect(hdc, rect, brush);
        DeleteObject(brush);
    }
}

/// Black around light text and white around dark, whatever the picture does behind it.
fn outline_color(fg: u32) -> u32 {
    let (r, g, b) = (fg & 0xFF, fg >> 8 & 0xFF, fg >> 16 & 0xFF);
    if r * 3 + g * 6 + b > 1275 { 0x00000000 } else { 0x00FFFFFF }
}

unsafe fn draw_backdrop(hdc: HDC, p: &Patch, b: &Backdrop) {
    unsafe {
        let mut info: BITMAPINFO = std::mem::zeroed();
        info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = b.w;
        info.bmiHeader.biHeight = -b.h; // top-down rows
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        SetDIBitsToDevice(hdc, p.x, p.y, b.w as u32, b.h as u32, 0, 0, 0, b.h as u32, b.bgra.as_ptr() as *const _, &info, DIB_RGB_COLORS);
    }
}

unsafe fn draw_patches(hdc: HDC, patches: &[Patch]) {
    unsafe {
        SetBkMode(hdc, TRANSPARENT as i32);
        for p in patches {
            match &p.backdrop {
                Some(b) => draw_backdrop(hdc, p, b),
                None => fill(hdc, &RECT { left: p.x, top: p.y, right: p.x + p.w, bottom: p.y + p.h }, p.bg),
            }
        }
        draw_patch_text(hdc, patches, None);
    }
}

/// Color and grayscale coverage for the GPU's text-only layer. Background blur stays on the GPU.
unsafe fn draw_text_layer(hdc: HDC, patches: &[Patch], mask: bool) {
    unsafe {
        for p in patches.iter().filter(|p| p.source.is_empty()) {
            fill(hdc, &RECT { left: p.x, top: p.y, right: p.x + p.w, bottom: p.y + p.h }, if mask { 0x00FFFFFF } else { p.bg });
        }
        draw_patch_text(hdc, patches, Some(mask));
    }
}

unsafe fn draw_patch_text(hdc: HDC, patches: &[Patch], mask: Option<bool>) {
    unsafe {
        SetBkMode(hdc, TRANSPARENT as i32);
        // A second pass, so no patch can paint over the text of the one before it. Text stays on
        // what its patch painted: the outline off its left edge would blend into the overlay's
        // see-through colour and show as pink.
        for p in patches {
            let (w, h) = p.backdrop.as_ref().map_or((p.w, p.h), |b| (b.w, b.h));
            let saved = SaveDC(hdc);
            IntersectClipRect(hdc, p.x, p.y, p.x + w, p.y + h);
            let quality = if mask.is_some() { ANTIALIASED_QUALITY } else { CLEARTYPE_QUALITY };
            let font = text_font_quality(p.px, p.bold, p.narrow, quality as u32);
            let old_font = SelectObject(hdc, font);
            let top = p.y + (p.h - p.text_h) / 2;
            // DT_EDITCONTROL drops a last line that overhangs, so the box must hold all of it.
            let area = RECT { left: p.x, top, right: p.x + p.w, bottom: top + p.text_h };
            let text = wrapped(hdc, &p.text, p.w);
            let write = |dx: i32, dy: i32, color: u32| {
                let mut at = RECT { left: area.left + dx, top: area.top + dy, right: area.right + dx, bottom: area.bottom + dy };
                SetTextColor(hdc, if mask == Some(true) { 0x00FFFFFF } else { color });
                DrawTextW(hdc, text.as_ptr(), text.len() as i32, &mut at, TEXT_FLAGS);
            };
            if p.backdrop.is_some() {
                let reach = (p.px / OUTLINE_EVERY_PX).max(1);
                let outline = outline_color(p.fg);
                for (dx, dy) in [(-1, -1), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)] {
                    write(dx * reach, dy * reach, outline);
                }
            }
            write(0, 0, p.fg);
            SelectObject(hdc, old_font);
            DeleteObject(font);
            RestoreDC(hdc, saved);
        }
    }
}

unsafe fn paint_overlay(hwnd: HWND) {
    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut client);
        let was_gpu = blur::active(hwnd);
        let gpu = UI.with(|ui| blur::paint(hwnd, ui.overlay.get(), ui.job_id.get(), ui.region.get(), (client.right, client.bottom), &ui.patches.borrow()));
        if gpu != was_gpu {
            let alpha = if gpu { 255 } else { (config::get().opacity as u32 * 255 / 100) as u8 };
            SetLayeredWindowAttributes(hwnd, KEY_COLOR, alpha, LWA_COLORKEY | LWA_ALPHA);
        }
        if gpu {
            fill(hdc, &client, KEY_COLOR);
            EndPaint(hwnd, &ps);
            if UI.with(|ui| ui.overlay.get()) == hwnd { share::repaint(); }
            return;
        }
        blur::release(hwnd);
        // Off screen first: drawing straight to the window would flash the original through.
        let mem = CreateCompatibleDC(hdc);
        let bitmap = CreateCompatibleBitmap(hdc, client.right, client.bottom);
        let old_bitmap = SelectObject(mem, bitmap);
        fill(mem, &client, KEY_COLOR);
        UI.with(|ui| draw_patches(mem, &ui.patches.borrow()));
        BitBlt(hdc, 0, 0, client.right, client.bottom, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old_bitmap);
        DeleteObject(bitmap);
        DeleteDC(mem);
        EndPaint(hwnd, &ps);
    }
}

unsafe extern "system" fn overlay_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_ERASEBKGND => 1,
            blur::WM_SOURCE => {
                blur::receive(UI.with(|ui| ui.job_id.get()));
                InvalidateRect(hwnd, null(), 0);
                share::repaint();
                0
            }
            WM_NCDESTROY => {
                blur::release(hwnd);
                DefWindowProcW(hwnd, msg, w, l)
            }
            WM_PAINT => {
                paint_overlay(hwnd);
                0
            }
            WM_JOB_OVER => {
                if UI.with(|ui| ui.job_id.get()) == w as u64 {
                    stop();
                }
                0
            }
            WM_PARTS => {
                if UI.with(|ui| ui.job_id.get()) == w as u64 {
                    let live = PARTS.lock().unwrap().iter().any(|p| p.live);
                    UI.with(|ui| {
                        ui.live.set(live);
                        InvalidateRect(ui.frame.get(), null(), 0);
                        sources_changed(ui);
                    });
                }
                0
            }
            WM_TRANSLATED => {
                if UI.with(|ui| ui.job_id.get()) == w as u64 && config::get().flash_frame {
                    UI.with(|ui| {
                        ui.flashing.set(true);
                        SetTimer(ui.main.get(), FLASH_TIMER, FLASH_MS, None);
                    });
                    update_frame();
                }
                0
            }
            WM_PATCHES => {
                let patches = Box::from_raw(l as *mut Vec<Patch>);
                if UI.with(|ui| ui.job_id.get()) == w as u64 {
                    show_patches(*patches);
                }
                0
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}

// --- Hidden main window: hotkeys and tray

unsafe fn tray_menu(hwnd: HWND) {
    unsafe {
        let menu = CreatePopupMenu();
        AppendMenuW(menu, MF_STRING, MENU_SETTINGS, wide_z(&t("設定")).as_ptr());
        AppendMenuW(menu, MF_STRING, MENU_HISTORY, wide_z(&t("履歴")).as_ptr());
        let paused = if SUSPENDED.load(Ordering::Relaxed) { MF_CHECKED } else { 0 };
        AppendMenuW(menu, MF_STRING | paused, MENU_PAUSE, wide_z(&t("一時停止")).as_ptr());
        AppendMenuW(menu, MF_STRING, MENU_EXIT, wide_z(&t("終了")).as_ptr());
        let mut at = POINT { x: 0, y: 0 };
        GetCursorPos(&mut at);
        SetForegroundWindow(hwnd); // Windows only dismisses a tray menu properly this way
        let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_NONOTIFY, at.x, at.y, 0, hwnd, null());
        DestroyMenu(menu);
        match chosen as usize {
            MENU_SETTINGS => settings::open(),
            MENU_HISTORY => settings::open_history(),
            MENU_PAUSE => toggle_pause(),
            MENU_EXIT => {
                log("quit from the tray menu");
                DestroyWindow(hwnd);
            }
            _ => {}
        }
    }
}

/// The icon in app.res, at the size given or Windows' size for a tray icon.
pub fn app_icon(size: Option<i32>) -> HICON {
    unsafe {
        let side = size.unwrap_or_else(|| GetSystemMetrics(SM_CXSMICON));
        LoadImageW(GetModuleHandleW(null()), 1 as *const u16, IMAGE_ICON, side, side, LR_DEFAULTCOLOR) as HICON
    }
}

/// The share windows come and go with the setting, showing the windows of the job running now.
pub fn shared_changed() {
    UI.with(|ui| {
        if config::get().shared {
            sources_changed(ui);
        } else {
            share::close();
            unsafe { KillTimer(ui.main.get(), CLIP_TIMER) };
            unsafe { unclip(ui) };
        }
    });
}

/// The windows the job reads, each with the regions in it (none: the whole overlay). Empty when
/// it reads the screen: one region across windows, or one of several regions in none.
fn sources() -> Vec<(HWND, Vec<Region>)> {
    let Some(job) = *JOB.lock().unwrap() else { return Vec::new() };
    let parts = PARTS.lock().unwrap().clone();
    if parts.is_empty() {
        return if job.window == 0 { Vec::new() } else { vec![(job.window as HWND, Vec::new())] };
    }
    if parts.iter().any(|p| p.window == 0) {
        return Vec::new();
    }
    let mut out: Vec<(HWND, Vec<Region>)> = Vec::new();
    for part in parts {
        match out.iter_mut().find(|(window, _)| *window == part.window as HWND) {
            Some((_, regions)) => regions.push(part.region),
            None => out.push((part.window as HWND, vec![part.region])),
        }
    }
    out
}

/// A job started, or lost regions: the share windows and the clipping follow its windows.
fn sources_changed(ui: &Ui) {
    if !config::get().shared {
        return;
    }
    let sources = sources();
    share::show(&sources, ui.overlay.get());
    if !sources.is_empty() && ui.job_id.get() != 0 {
        unsafe { SetTimer(ui.main.get(), CLIP_TIMER, CLIP_MS, None) };
    }
}

/// With sharing on, the window is read as it draws itself, whatever lies on top of it, so the
/// translations would stay over windows brought in front. On screen they give way to those
/// windows; the share window, with its own copy, still shows them all.
fn clip_overlay() {
    UI.with(|ui| unsafe {
        let sources = sources();
        if sources.is_empty() {
            return unclip(ui);
        }
        let overlay = ui.overlay.get();
        let mut at: RECT = std::mem::zeroed();
        GetWindowRect(overlay, &mut at);
        let local = |r: &RECT| CreateRectRgn(r.left - at.left, r.top - at.top, r.right - at.left, r.bottom - at.top);
        // What shows of each window read, together.
        let visible = CreateRectRgn(0, 0, 0, 0);
        for (window, _) in &sources {
            let Some(bounds) = frame_bounds(*window) else { continue };
            let shows = local(&bounds);
            let mut above = GetWindow(*window, GW_HWNDPREV);
            while !above.is_null() {
                let mut process = 0;
                GetWindowThreadProcessId(above, &mut process);
                if process != GetCurrentProcessId() && is_solid(above) {
                    if let Some(front) = frame_bounds(above) {
                        let cut = local(&front);
                        CombineRgn(shows, shows, cut, RGN_DIFF);
                        DeleteObject(cut);
                    }
                }
                above = GetWindow(above, GW_HWNDPREV);
            }
            CombineRgn(visible, visible, shows, RGN_OR);
            DeleteObject(shows);
        }
        let whole = CreateRectRgn(0, 0, at.right - at.left, at.bottom - at.top);
        CombineRgn(visible, visible, whole, RGN_AND);
        let covered = EqualRgn(visible, whole) == 0;
        DeleteObject(whole);
        if !covered {
            DeleteObject(visible);
            return unclip(ui);
        }
        let last = ui.clip.get();
        if !last.is_null() && EqualRgn(last, visible) != 0 {
            DeleteObject(visible);
            return;
        }
        let copy = CreateRectRgn(0, 0, 0, 0);
        CombineRgn(copy, visible, null_mut(), RGN_COPY);
        if !last.is_null() {
            DeleteObject(last);
        }
        ui.clip.set(copy);
        SetWindowRgn(overlay, visible, 1); // the window owns the region from here
    });
}

unsafe fn unclip(ui: &Ui) {
    let last = ui.clip.replace(null_mut());
    if !last.is_null() {
        unsafe {
            DeleteObject(last);
            SetWindowRgn(ui.overlay.get(), null_mut(), 1);
        }
    }
}

/// The desktop behind every window: never a window to read or share, as it shows the whole screen.
fn is_desktop(window: HWND) -> bool {
    let mut name = [0u16; 16];
    let n = unsafe { GetClassNameW(window, name.as_mut_ptr(), name.len() as i32) } as usize;
    matches!(String::from_utf16_lossy(&name[..n]).as_str(), "Progman" | "WorkerW")
}

/// The tray's text follows the language. The rest is written when next shown.
pub fn language_changed() {
    UI.with(|ui| unsafe { tray_icon(ui.main.get(), NIM_MODIFY) });
}

unsafe fn tray_icon(hwnd: HWND, action: u32) {
    unsafe {
        let mut data: NOTIFYICONDATAW = std::mem::zeroed();
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        data.uCallbackMessage = WM_TRAY;
        data.hIcon = app_icon(None);
        let state = if SUSPENDED.load(Ordering::Relaxed) { "一時停止中" } else { "ダブルクリックで設定を開きます" };
        let tip = wide(&format!("Sonoba\n{}", t(state)));
        data.szTip[..tip.len()].copy_from_slice(&tip);
        Shell_NotifyIconW(action, &data);
    }
}

unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_HOTKEY => {
                match w as i32 {
                    HK_ONCE => begin_select(false),
                    HK_LIVE => begin_select(true),
                    HK_STOP => {
                        end_select();
                        stop();
                    }
                    HK_ESC => end_select(),
                    HK_PEEK => peek_start(),
                    HK_COPY => copy_translation(),
                    _ => {}
                }
                0
            }
            WM_PEEK_END => {
                peek_end();
                0
            }
            WM_TIMER if w == PEEK_TIMER => {
                if GetAsyncKeyState(config::get().hotkeys[3].key as i32) >= 0 {
                    peek_end();
                }
                0
            }
            WM_TIMER if w == FLASH_TIMER => {
                KillTimer(hwnd, FLASH_TIMER);
                UI.with(|ui| ui.flashing.set(false));
                update_frame();
                0
            }
            WM_TIMER if w == CLIP_TIMER => {
                clip_overlay();
                0
            }
            WM_DISPLAYCHANGE => {
                stop(); // the region may no longer be where it was
                0
            }
            WM_TRAY => {
                match l as u32 {
                    WM_RBUTTONUP => tray_menu(hwnd),
                    WM_LBUTTONDBLCLK => settings::open(),
                    _ => {}
                }
                0
            }
            // The tray menu leaves this hidden window in front, so a later Alt+F4 would quit Sonoba unseen.
            WM_SYSCOMMAND if w & 0xFFF0 == SC_CLOSE as usize => 0,
            WM_CLOSE => {
                log("closed by another program");
                DefWindowProcW(hwnd, msg, w, l)
            }
            WM_ENDSESSION if w != 0 => {
                let flags = l as u32;
                log(if flags & ENDSESSION_CLOSEAPP != 0 {
                    "closed by Windows for an installer or update"
                } else if flags & ENDSESSION_LOGOFF != 0 {
                    "closed by Windows signing out"
                } else {
                    "closed by Windows shutting down or restarting"
                });
                0
            }
            WM_DESTROY => {
                tray_icon(hwnd, NIM_DELETE);
                PostQuitMessage(0);
                0
            }
            // Explorer restarted and lost every tray icon.
            _ if msg == TASKBAR_CREATED.load(Ordering::Relaxed) => {
                tray_icon(hwnd, NIM_ADD);
                0
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}

/// Registers the saved hotkeys. Returns false if another app holds one of them.
fn register_hotkeys() -> bool {
    if SUSPENDED.load(Ordering::Relaxed) {
        return true;
    }
    let main = UI.with(|ui| ui.main.get());
    let hotkeys = config::get().hotkeys;
    let mut mouse = Vec::new();
    let mut taken = Vec::new();
    for (index, (id, hotkey)) in ACTIONS.into_iter().zip(hotkeys).enumerate() {
        if hotkey == config::Hotkey::OFF {
            continue;
        }
        if hotkey.is_mouse() {
            mouse.push((id, hotkey));
        } else if unsafe { RegisterHotKey(main, id, hotkey.mods | MOD_NOREPEAT, hotkey.key) } == 0 {
            taken.push(index);
        }
    }
    set_mouse_keys(mouse);
    // The extras are conveniences: one that is taken shows in the settings and stops nothing.
    let all = !taken.iter().any(|&index| index < ESSENTIAL_ACTIONS);
    *TAKEN_KEYS.lock().unwrap() = taken;
    all
}

/// Choosing a key in the settings needs the keys free, so that they can be typed.
pub fn pause_hotkeys() {
    let main = UI.with(|ui| ui.main.get());
    for id in ACTIONS {
        unsafe { UnregisterHotKey(main, id) };
    }
    set_mouse_keys(Vec::new());
}

pub fn resume_hotkeys() {
    pause_hotkeys(); // registering a key twice would look like another app holding it
    register_hotkeys();
}

/// True if the key combination is free for the taking.
pub fn try_hotkeys(hotkey: config::Hotkey) -> bool {
    let main = UI.with(|ui| ui.main.get());
    hotkey.is_mouse() || unsafe {
        let free = RegisterHotKey(main, HK_PROBE, hotkey.mods, hotkey.key) != 0;
        UnregisterHotKey(main, HK_PROBE);
        free
    }
}

unsafe fn register_class(name: *const u16, proc: WNDPROC, cursor: *const u16) {
    unsafe {
        let mut class: WNDCLASSEXW = std::mem::zeroed();
        class.cbSize = size_of::<WNDCLASSEXW>() as u32;
        class.lpfnWndProc = proc;
        class.hInstance = GetModuleHandleW(null());
        class.lpszClassName = name;
        class.hCursor = LoadCursorW(null_mut(), cursor);
        RegisterClassExW(&class);
    }
}

static LOG: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();

/// Release builds abort on a panic and have no console, and a quiet end leaves no trace either,
/// so when Sonoba started, why it ended, and any panic go to data\sonoba.log.
fn log_panics() {
    LOG.get_or_init(|| models::dir().parent().map(|dir| dir.join("sonoba.log")));
    std::panic::set_hook(Box::new(|info| log(&info.to_string())));
}

fn log(text: &str) {
    let Some(Some(path)) = LOG.get() else { return };
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t) };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write;
        let (y, mo, d, h, mi, s) = (t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond);
        let _ = writeln!(file, "{y}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02} {text}");
    }
}

/// A second start opens the settings of the first, which is usually what was wanted.
fn already_running() -> bool {
    unsafe {
        CreateMutexW(null(), 0, w!("Sonoba.single-instance"));
        if GetLastError() != ERROR_ALREADY_EXISTS {
            return false;
        }
        let first = FindWindowW(w!("SonobaMain"), null());
        PostMessageW(first, WM_TRAY, 0, WM_LBUTTONDBLCLK as isize);
        true
    }
}

fn main() {
    log_panics();
    if already_running() {
        return;
    }
    log(concat!("started ", env!("CARGO_PKG_VERSION")));
    settings::carry_over_autostart();
    unsafe {
        CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32); // WebView2 asks for it
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // Windows slows down a process with no window in the taskbar, as this one sits in the tray,
        // and OCR took four times as long. It only ever works for what is on the screen.
        let full_speed = PROCESS_POWER_THROTTLING_STATE { Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION, ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED, StateMask: 0 };
        let size = size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32;
        SetProcessInformation(GetCurrentProcess(), ProcessPowerThrottling, &full_speed as *const _ as *const _, size);
        register_class(w!("SonobaMain"), Some(main_proc), IDC_ARROW);
        register_class(w!("SonobaSelect"), Some(select_proc), IDC_CROSS);
        register_class(w!("SonobaOverlay"), Some(overlay_proc), IDC_ARROW);
        register_class(w!("SonobaFrame"), Some(frame_proc), IDC_ARROW);
        register_class(w!("SonobaLine"), Some(DefWindowProcW), IDC_ARROW);
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);

        let module = GetModuleHandleW(null());
        let main = CreateWindowExW(0, w!("SonobaMain"), w!("Sonoba"), 0, 0, 0, 0, 0, null_mut(), null_mut(), module, null());
        let overlay = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
            w!("SonobaOverlay"),
            null(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            null_mut(),
            null_mut(),
            module,
            null(),
        );
        let frame = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
            w!("SonobaFrame"),
            null(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            null_mut(),
            null_mut(),
            module,
            null(),
        );
        SetLayeredWindowAttributes(frame, KEY_COLOR, 255, LWA_COLORKEY);
        SetWindowDisplayAffinity(frame, WDA_EXCLUDEFROMCAPTURE); // it can sit on the region's edge
        let line = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
            w!("SonobaLine"),
            null(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            null_mut(),
            null_mut(),
            module,
            null(),
        );
        SetClassLongPtrW(line, GCLP_HBRBACKGROUND, CreateSolidBrush(FRAME_COLOR) as isize);
        SetWindowDisplayAffinity(line, WDA_EXCLUDEFROMCAPTURE);
        UI.with(|ui| {
            ui.main.set(main);
            ui.overlay.set(overlay);
            ui.frame.set(frame);
            ui.line.set(line);
        });
        look_changed();

        tray_icon(main, NIM_ADD);
        settings::register_class();
        bar::register_class();
        toast::register_class();
        share::register_class();
        let overlay_handle = overlay as usize;
        std::thread::spawn(move || worker(overlay_handle));
        let hotkeys_ok = register_hotkeys();
        if !hotkeys_ok {
            let warning = wide_z(&t("ホットキーを登録できません。別のアプリが使っています。設定で変更してください。"));
            MessageBoxW(null_mut(), warning.as_ptr(), w!("Sonoba"), MB_OK | MB_ICONWARNING);
        }
        let config = config::get();
        if !hotkeys_ok || (models::needs_license(&config.model) && !config.license_accepted) || !models::is_present(&config.model) {
            settings::open();
        }

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_regions_whose_picture_changed_are_read_again() {
        use std::cell::Cell;
        // Two regions side by side in a picture 4 wide and 1 high; the stand-in OCR reads one line each.
        let part = |x| Part { region: Region { x, y: 0, w: 2, h: 1 }, live: true, window: 0 };
        let parts = [part(10), part(12)];
        let reads = Cell::new(0);
        let read = |_: &[u8], _: usize, _: usize| {
            reads.set(reads.get() + 1);
            let glyphs = patch::Glyphs { x: 0, y: 0, w: 1, h: 1, letters: 1, bg: [0; 3], fg: [0; 3], stem: 1, icon: false, picture: false };
            Ok(vec![ocr::Line { glyphs, text: String::new() }])
        };
        let mut frame = vec![0u8; 4 * 4];
        let mut last = Vec::new();
        let lines = read_parts(&parts, (10, 0), (&frame, 4, 1), &mut last, read).unwrap();
        assert_eq!((reads.get(), lines.iter().map(|l| l.glyphs.x).collect::<Vec<_>>()), (2, vec![0, 2]));
        read_parts(&parts, (10, 0), (&frame, 4, 1), &mut last, read).unwrap();
        assert_eq!(reads.get(), 2, "nothing changed, nothing read");
        frame[3 * 4] = 255; // a pixel of the second region
        let lines = read_parts(&parts, (10, 0), (&frame, 4, 1), &mut last, read).unwrap();
        assert_eq!((reads.get(), lines.len()), (3, 2), "only the second region read again, both still there");
    }

    #[test]
    fn a_crop_takes_the_rows_and_columns_asked_for() {
        // 3 wide, 2 high: each pixel's first byte is its index.
        let frame: Vec<u8> = (0..6u8).flat_map(|i| [i, 0, 0, 255]).collect();
        let part = crop(&frame, 3, (1, 0, 3, 2));
        assert_eq!(part.chunks(4).map(|px| px[0]).collect::<Vec<_>>(), vec![1, 2, 4, 5]);
    }

    #[test]
    fn only_the_parts_of_a_picture_are_kept() {
        let (width, height) = (10usize, 4usize);
        let frame = vec![9u8; width * height * 4];
        let mut out = Vec::new();
        let parts = [Part { region: Region { x: 102, y: 201, w: 2, h: 1 }, live: true, window: 0 }, Part { region: Region { x: 108, y: 203, w: 5, h: 5 }, live: false, window: 0 }];
        keep_parts(&frame, &mut out, width, (100, 200), &parts, &mut Vec::new());
        let kept: Vec<(usize, usize)> = (0..width * height).filter(|i| out[i * 4] == 9).map(|i| (i % width, i / width)).collect();
        assert_eq!(kept, [(2, 1), (3, 1), (8, 3), (9, 3)]);
    }

    #[test]
    fn a_one_shot_region_holds_still_beside_a_live_one_until_its_page_changes() {
        let width = 40usize;
        let region = |x, live| Part { region: Region { x, y: 0, w: 20, h: 4 }, live, window: 0 };
        let parts = [region(0, true), region(20, false)];
        let (mut out, mut still) = (Vec::new(), Vec::new());
        let mut frame = vec![1u8; width * 4 * 4];
        assert!(keep_parts(&frame, &mut out, width, (0, 0), &parts, &mut still).is_empty());

        // A pixel changes in each: the live region shows it, the one-shot one does not.
        for at in [0, 39] {
            frame[at * 4] = 200;
        }
        assert!(keep_parts(&frame, &mut out, width, (0, 0), &parts, &mut still).is_empty());
        assert_eq!((out[0], out[39 * 4]), (200, 1));

        // Most of the one-shot region changes, so it is done with.
        frame.iter_mut().for_each(|v| *v = 200);
        assert_eq!(keep_parts(&frame, &mut out, width, (0, 0), &parts, &mut still).len(), 1);
    }

    #[test]
    fn the_box_around_regions_holds_them_all() {
        let part = |x, y, w, h| Part { region: Region { x, y, w, h }, live: false, window: 0 };
        let parts = [part(10, 50, 5, 5), part(-4, 60, 2, 2)];
        let b = around_all(&parts);
        assert_eq!((b.x, b.y, b.w, b.h), (-4, 50, 19, 12));
    }

    #[test]
    fn the_frame_stays_on_the_screen() {
        let screen = RECT { left: 0, top: 0, right: 1920, bottom: 1080 };
        let around = |x, y, w, h| {
            let region = Region { x, y, w, h };
            let outer = RECT { left: x - 2, top: y - 2, right: x + w + 2, bottom: y + h + 2 };
            let visible = RECT { left: outer.left.max(0), top: outer.top.max(0), right: outer.right.min(1920), bottom: outer.bottom.min(1080) };
            frame_rect(region, outer, visible)
        };
        let r = around(100, 100, 300, 200);
        assert_eq!((r.left, r.top, r.right, r.bottom), (98, 98, 402, 302));
        let r = around(0, 0, 1920, 1080);
        assert_eq!((r.left, r.top, r.right, r.bottom), (screen.left, screen.top, screen.right, screen.bottom));
        let r = around(1800, 100, 300, 200); // into the next monitor
        assert_eq!((r.left, r.right), (1798, 2102));
    }

    #[test]
    fn readings_that_differ_by_a_letter_are_one_reading() {
        assert!(same_reading("the guardian before dawn", "the quardian before dawn"));
        assert!(same_reading("answer shape", "answer shape."));
        assert!(!same_reading("You must defeat the guardian", "The bank was muddy"));
    }

    fn page(columns: &[usize]) -> Vec<u8> {
        let (mut pixels, width) = (vec![255u8; 100 * 20 * 4], 100);
        for y in 5..15 {
            for x in columns {
                pixels[(y * width + x) * 4..][..3].copy_from_slice(&[0, 0, 0]);
            }
        }
        pixels
    }

    fn shown_patch(source: &str, text: &str) -> Patch {
        Patch { x: 0, y: 0, w: 100, h: 20, text: text.to_string(), source: source.to_string(), ..Patch::status("", 100) }
    }

    #[test]
    fn a_letter_read_differently_keeps_the_translation_until_the_pixels_change() {
        let columns: Vec<usize> = (10..90).step_by(6).collect();
        let shown = (page(&columns), vec![shown_patch("guardian", "守護者").seen_in(&page(&columns), 100)]);
        let still = Frame { pixels: shown.0.clone(), w: 100, h: 20 };
        let mut patches = vec![shown_patch("quardian", "クァルディアン")];
        hold_still(&mut patches, &shown, &still);
        assert_eq!(patches[0].text, "守護者");

        let changed = Frame { pixels: page(&columns[..8]), w: 100, h: 20 };
        let mut patches = vec![shown_patch("guardians", "守護者たち")];
        hold_still(&mut patches, &shown, &changed);
        assert_eq!(patches[0].text, "守護者たち");
    }

    #[test]
    fn a_line_unread_for_a_frame_stays_until_its_pixels_go() {
        let columns: Vec<usize> = (10..90).step_by(6).collect();
        let shown = (page(&columns), vec![shown_patch("guardian", "守護者").seen_in(&page(&columns), 100)]);
        let mut patches = Vec::new();
        hold_still(&mut patches, &shown, &Frame { pixels: shown.0.clone(), w: 100, h: 20 });
        assert_eq!(patches.len(), 1);

        let mut patches = Vec::new();
        hold_still(&mut patches, &shown, &Frame { pixels: page(&[]), w: 100, h: 20 });
        assert!(patches.is_empty());
    }

    /// A game's menu fades out as a settings screen fades in. Each look saw the label only a
    /// little fainter than the look before, so the last picture shown is the new screen already.
    #[test]
    fn a_label_that_faded_out_with_its_screen_goes() {
        let columns: Vec<usize> = (10..90).step_by(6).collect();
        let shown = (page(&[]), vec![shown_patch("Q.Load", "Q.ロード").seen_in(&page(&columns), 100)]);
        let mut patches = Vec::new();
        hold_still(&mut patches, &shown, &Frame { pixels: page(&[]), w: 100, h: 20 });
        assert!(patches.is_empty());
    }

    #[test]
    fn japanese_breaks_between_words() {
        let cut = |s: &str| pieces_to_break(s);
        assert_eq!(cut("ウェブ向けプロジェクト"), ["ウェブ", "向け", "プロジェクト"]);
        assert_eq!(cut("設定を保存してください。"), ["設定を", "保存してください。"]);
        assert_eq!(cut("Save your work"), ["Save ", "your ", "work"]);
        assert_eq!(cut("SharePoint コンテンツ"), ["SharePoint ", "コンテンツ"]);
        assert_eq!(cut("ページ（project.microsoft.com）で確認、それから"), ["ページ", "（project.microsoft.com）で", "確認、", "それから"]);
    }

    #[test]
    fn the_history_takes_a_line_again_when_it_comes_back() {
        let line = |source: &str| Patch { text: format!("訳 {source}"), source: source.into(), ..Patch::status("", 100) };
        let (mut seen, start) = (HashMap::new(), Instant::now());
        let at = |s: u64| start + Duration::from_secs(s);
        let (menu, a, b) = (line("Save"), line("Line A"), line("Line B"));
        assert_eq!(newly_shown(&[menu.clone(), a.clone()], &[], &mut seen, at(0)).len(), 2);
        // Still there: nothing new. The next line: new.
        assert!(newly_shown(&[menu.clone(), a.clone()], &[menu.clone(), a.clone()], &mut seen, at(5)).is_empty());
        assert_eq!(newly_shown(&[menu.clone(), b.clone()], &[menu.clone(), a.clone()], &mut seen, at(10)).len(), 1);
        // The mouse over the menu item: off for a moment, back.
        assert!(newly_shown(&[menu.clone(), b.clone()], &[b.clone()], &mut seen, at(11)).is_empty());
        // The game goes back to line A: it is kept again.
        let got = newly_shown(&[menu.clone(), a.clone()], &[menu.clone(), b.clone()], &mut seen, at(20));
        assert_eq!(got, [("Line A".to_string(), "訳 Line A".to_string())]);
    }

    #[test]
    fn a_status_message_is_not_held() {
        let shown = (page(&[]), vec![Patch::status("エラー", 100)]);
        let mut patches = Vec::new();
        hold_still(&mut patches, &shown, &Frame { pixels: shown.0.clone(), w: 100, h: 20 });
        assert!(patches.is_empty());
    }

    /// A game's menu item has a glow, so its box leaves room for two lines of its own size.
    #[test]
    fn a_one_line_label_narrows_before_it_wraps() {
        let label = |one_line| Patch { w: 69, h: 36, em: 15, one_line, text: "Q.読み込み".into(), ..Patch::status("", 69) };
        let (mut one, mut many) = (label(true), label(false));
        unsafe {
            let mem = CreateCompatibleDC(null_mut());
            fit(mem, &mut one, STATUS_PX);
            fit(mem, &mut many, STATUS_PX);
            DeleteDC(mem);
        }
        assert!(many.narrow == 100 && many.text_h > one.text_h, "the test needs a label that wraps: {many:?}");
        assert!(one.narrow < 100 && one.px == 15, "{one:?}");
    }

    /// The overlay is its see-through colour wherever no patch is; text drawn there would show.
    #[test]
    fn text_stays_on_its_patch() {
        let (w, h) = (120usize, 50usize);
        let picture = vec![90u8; w * h * 4];
        let mut patch = Patch { x: 10, y: 10, w: 60, h: 24, em: 20, fg: 0x00FFFFFF, text: "戻る".into(), ..Patch::status("", 60) }.keep_picture(&picture, w, h);
        let drawn = unsafe {
            let mem = CreateCompatibleDC(null_mut());
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            (info.bmiHeader.biWidth, info.bmiHeader.biHeight) = (w as i32, -(h as i32));
            (info.bmiHeader.biPlanes, info.bmiHeader.biBitCount) = (1, 32);
            let mut bits = null_mut();
            let dib = CreateDIBSection(mem, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            SelectObject(mem, dib);
            fill(mem, &RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 }, KEY_COLOR);
            fit(mem, &mut patch, STATUS_PX);
            draw_patches(mem, &[patch.clone()]);
            GdiFlush();
            let drawn = std::slice::from_raw_parts(bits as *const u32, w * h).to_vec();
            DeleteDC(mem);
            DeleteObject(dib);
            drawn
        };
        let b = patch.backdrop.as_ref().unwrap();
        let inside = |x: i32, y: i32| (patch.x..patch.x + b.w).contains(&x) && (patch.y..patch.y + b.h).contains(&y);
        let outside: Vec<(i32, i32)> = (0..h as i32).flat_map(|y| (0..w as i32).map(move |x| (x, y))).filter(|&(x, y)| !inside(x, y) && drawn[y as usize * w + x as usize] & 0xFFFFFF != 0xFF00FF).collect();
        assert!(outside.is_empty(), "drawn outside the patch: {:?}", &outside[..outside.len().min(5)]);
        assert!((0..w * h).any(|i| drawn[i] & 0xFFFFFF == 0xFFFFFF), "the text was drawn");
    }

    /// A paragraph's translation, wrapped at words, takes a line more than the original.
    #[test]
    fn a_paragraph_grows_into_the_gap_below_before_it_narrows() {
        let text = "ツール呼び出しにより、モデルは予測可能な形式で関数をリクエストできるようになります。構造化出力により、スキーマに従ったJSONを返すことが可能になります。";
        let paragraph = |max_h| Patch { w: 300, h: 76, max_h, em: 17, text: text.into(), ..Patch::status("", 300) };
        let (mut tight, mut roomy) = (paragraph(76), paragraph(200));
        unsafe {
            let mem = CreateCompatibleDC(null_mut());
            fit(mem, &mut tight, STATUS_PX);
            fit(mem, &mut roomy, STATUS_PX);
            DeleteDC(mem);
        }
        assert!(tight.narrow < 100 || tight.px < 17, "the test needs a paragraph that does not fit: {tight:?}");
        assert!(roomy.narrow == 100 && roomy.px == 17 && roomy.h > 76 && roomy.h <= 200, "{roomy:?}");
    }

    /// Draws the overlay onto a picture, to judge a layout without touching the real screen:
    /// RENDER_IN=in.png RENDER_OUT=out.png cargo test render_picture -- --ignored --nocapture
    /// With folders for RENDER_IN and RENDER_OUT, every picture in the first is drawn into the second.
    #[test]
    #[ignore = "needs Ollama and a picture"]
    fn render_picture() {
        let (input, output) = (std::env::var("RENDER_IN").unwrap(), std::env::var("RENDER_OUT").unwrap());
        let live = std::env::var("RENDER_LIVE").is_ok(); // live jobs keep the flat box
        let (mut models, mut translator) = (None, translate::Translator::new());
        let pairs: Vec<(std::path::PathBuf, std::path::PathBuf)> = if std::path::Path::new(&input).is_dir() {
            let mut files: Vec<_> = std::fs::read_dir(&input).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|e| e == "png")).collect();
            files.sort();
            files.into_iter().map(|f| (f.clone(), std::path::Path::new(&output).join(f.file_name().unwrap()))).collect()
        } else {
            vec![(input.into(), output.into())]
        };
        for (input, output) in pairs {
            println!("== {}", input.display());
            let (mut bgra, w, h) = ocr::png_bgra(input.to_str().unwrap());
            let region = Region { x: 0, y: 0, w: w as i32, h: h as i32 };
            set_job(Some(Job { region, live, id: 1, window: 0 }));
            let frame = Frame { pixels: bgra.clone(), w, h };
            let keep_going = || true;
            let shared = new_shared();
            *shared.latest.lock().unwrap() = bgra.clone(); // a still screen: nothing has moved since
            let start = Instant::now();
            let mut patches = translate_frame(&frame, live, &mut models, &mut translator, &keep_going, &|_| {}, &shared).unwrap();
            println!("first pass {:?}", start.elapsed());
            unsafe {
                let mem = CreateCompatibleDC(null_mut());
                let mut info: BITMAPINFO = std::mem::zeroed();
                info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
                info.bmiHeader.biWidth = w as i32;
                info.bmiHeader.biHeight = -(h as i32);
                info.bmiHeader.biPlanes = 1;
                info.bmiHeader.biBitCount = 32;
                let mut bits = null_mut();
                let dib = CreateDIBSection(mem, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
                SelectObject(mem, dib);
                std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits as *mut u8, bgra.len());
                for patch in &mut patches {
                    fit(mem, patch, STATUS_PX);
                }
                for p in &patches {
                    println!("{},{} {}x{} em={} px={} narrow={} bold={} picture={} {} <- {}", p.x, p.y, p.w, p.h, p.em, p.px, p.narrow, p.bold, p.backdrop.is_some(), p.text, p.source);
                }
                draw_patches(mem, &patches);
                GdiFlush();
                std::ptr::copy_nonoverlapping(bits as *const u8, bgra.as_mut_ptr(), bgra.len());
                DeleteDC(mem);
                DeleteObject(dib);
            }
            let rgb: Vec<u8> = bgra.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect();
            let file = std::io::BufWriter::new(std::fs::File::create(output).unwrap());
            let mut encoder = png::Encoder::new(file, w as u32, h as u32);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header().unwrap().write_image_data(&rgb).unwrap();
        }
    }
}
