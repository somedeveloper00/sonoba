//! Saved settings: one JSON file, rewritten whenever something changes.

use crate::engine::{self, Device, Options};
use crate::models;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;

const MOD_ALT: u32 = 1;
const MOD_CONTROL: u32 = 2;
const MOUSE_MIDDLE: u32 = 4; // virtual key codes, which Hotkey::key holds for mouse buttons too
const MOUSE_FORWARD: u32 = 6;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hotkey {
    pub mods: u32, // MOD_ALT and so on, as RegisterHotKey takes them
    pub key: u32,  // a virtual key code
}

impl Hotkey {
    pub const OFF: Hotkey = Hotkey { mods: 0, key: 0 };

    pub fn is_mouse(&self) -> bool {
        (MOUSE_MIDDLE..=MOUSE_FORWARD).contains(&self.key)
    }
}

pub const ACTIONS: usize = 5; // one shot, live, clear, show the original, copy

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub model: String, // a file in the models folder
    pub device: Device,
    pub cpu_threads: Option<usize>, // None: every performance core
    pub gpu_limit_mib: Option<u64>, // None: whatever fits
    pub show_frame: bool, // a thin frame around the region while a job runs
    pub flash_frame: bool, // the frame for a moment whenever a translation finishes
    pub shared: bool, // live translations show in screen sharing and recordings too
    pub source: String, // "auto" leaves the target language alone, "ja" rewrites Japanese too
    pub target: String, // a language code of the model, "ja(easy)" included
    pub ui_lang: String, // "ja" or "en"
    pub intro_done: bool, // the first-run guide was seen through
    pub license_accepted: bool, // nothing translates or downloads until the user agrees
    pub form: String, // "" follows the original's tone; "polite" or "plain" for Japanese
    pub glossary: Vec<(String, String)>, // term, and the translation it must get
    pub font: String, // "" for the app's default
    pub bold_text: bool, // always, not only where the original is bold
    pub blur_background: bool, // use the captured picture, blurred on the GPU, behind text
    pub opacity: u8, // percent, of everything drawn over the screen
    pub keep_history: bool, // in memory only: nothing read off the screen goes to disk
    pub hotkeys: [Hotkey; ACTIONS],
}

/// Japanese on a Japanese Windows, English on any other.
fn system_language() -> &'static str {
    const PRIMARY: u16 = 0x3FF;
    const JAPANESE: u16 = 0x11;
    if unsafe { GetUserDefaultUILanguage() } & PRIMARY == JAPANESE { "ja" } else { "en" }
}

impl Default for Config {
    fn default() -> Self {
        let ctrl_alt = |key: char| Hotkey { mods: MOD_CONTROL | MOD_ALT, key: key as u32 };
        Self {
            model: models::DEFAULT.to_string(),
            device: Device::Auto,
            cpu_threads: None,
            gpu_limit_mib: None,
            show_frame: false,
            flash_frame: false,
            shared: false,
            source: "auto".to_string(),
            target: "ja".to_string(),
            ui_lang: system_language().to_string(),
            intro_done: false,
            license_accepted: false,
            form: String::new(),
            glossary: Vec::new(),
            font: String::new(),
            bold_text: false,
            blur_background: false,
            opacity: 100,
            keep_history: true,
            hotkeys: [ctrl_alt('T'), ctrl_alt('R'), ctrl_alt('X'), ctrl_alt('Q'), ctrl_alt('C')],
        }
    }
}

impl Config {
    /// Whatever is missing or malformed falls back to its default.
    fn parse(text: &str) -> Self {
        let mut config = Self::default();
        let Ok(saved) = serde_json::from_str::<Value>(text.trim_start_matches('\u{FEFF}')) else { return config };
        if let Some(model) = saved["model"].as_str() {
            config.model = model.to_string();
        }
        config.device = match saved["device"].as_str() {
            Some("cpu") => Device::Cpu,
            Some("gpu") => Device::Gpu,
            _ => Device::Auto,
        };
        config.cpu_threads = saved["cpu_threads"].as_u64().map(|n| (n as usize).max(1));
        config.gpu_limit_mib = saved["gpu_limit_mib"].as_u64();
        config.show_frame = saved["show_frame"].as_bool().unwrap_or(false);
        config.flash_frame = saved["flash_frame"].as_bool().unwrap_or(false);
        config.shared = saved["shared"].as_bool().unwrap_or(false);
        if let Some(source) = saved["source"].as_str().filter(|s| *s == "ja") {
            config.source = source.to_string();
        }
        if let Some(target) = saved["target"].as_str() {
            config.target = target.to_string();
        }
        if let Some(language) = saved["ui_lang"].as_str() {
            config.ui_lang = language.to_string();
        }
        config.intro_done = saved["intro_done"].as_bool().unwrap_or(false);
        config.license_accepted = saved["license_accepted"].as_bool().unwrap_or(false);
        if let Some(form) = saved["form"].as_str().filter(|f| ["polite", "plain"].contains(f)) {
            config.form = form.to_string();
        }
        if let Some(terms) = saved["glossary"].as_array() {
            let pair = |t: &Value| Some((t[0].as_str()?.to_string(), t[1].as_str()?.to_string()));
            config.glossary = terms.iter().filter_map(pair).collect();
        }
        config.font = saved["font"].as_str().unwrap_or("").to_string();
        config.blur_background = saved["blur_background"].as_bool().unwrap_or(false);
        config.bold_text = saved["bold_text"].as_bool().unwrap_or(false);
        config.opacity = saved["opacity"].as_u64().map_or(100, |o| o.clamp(30, 100) as u8);
        config.keep_history = saved["keep_history"].as_bool().unwrap_or(true);
        for (slot, name) in config.hotkeys.iter_mut().zip(["once", "live", "clear", "peek", "copy"]) {
            if let (Some(mods), Some(key)) = (saved[name]["mods"].as_u64(), saved[name]["key"].as_u64()) {
                *slot = Hotkey { mods: mods as u32, key: key as u32 };
            }
        }
        config
    }

    fn to_json(&self) -> String {
        let hotkey = |h: &Hotkey| json!({ "mods": h.mods, "key": h.key });
        let device = match self.device {
            Device::Auto => "auto",
            Device::Gpu => "gpu",
            Device::Cpu => "cpu",
        };
        let saved = json!({
            "model": self.model,
            "device": device,
            "cpu_threads": self.cpu_threads,
            "gpu_limit_mib": self.gpu_limit_mib,
            "show_frame": self.show_frame,
            "flash_frame": self.flash_frame,
            "shared": self.shared,
            "source": self.source,
            "target": self.target,
            "ui_lang": self.ui_lang,
            "intro_done": self.intro_done,
            "license_accepted": self.license_accepted,
            "once": hotkey(&self.hotkeys[0]),
            "live": hotkey(&self.hotkeys[1]),
            "clear": hotkey(&self.hotkeys[2]),
            "peek": hotkey(&self.hotkeys[3]),
            "copy": hotkey(&self.hotkeys[4]),
            "form": self.form,
            "glossary": self.glossary,
            "font": self.font,
            "blur_background": self.blur_background,
            "bold_text": self.bold_text,
            "opacity": self.opacity,
            "keep_history": self.keep_history,
        });
        serde_json::to_string_pretty(&saved).unwrap_or_default()
    }

    pub fn threads(&self) -> usize {
        self.cpu_threads.unwrap_or_else(engine::performance_cores).max(1)
    }

    pub fn engine_options(&self) -> Options {
        Options { device: self.device, threads: self.threads(), gpu_limit_mib: self.gpu_limit_mib }
    }
}

/// A copy unpacked from the zip keeps everything in its `data` folder, so that deleting the folder
/// removes the app. Where it cannot write there (Program Files), the user's profile is used.
pub fn portable_dir() -> Option<&'static PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::current_exe().ok()?.parent()?.join("data");
        let probe = dir.join(".write-test");
        std::fs::write(&probe, "").ok()?;
        let _ = std::fs::remove_file(probe);
        Some(dir)
    })
    .as_ref()
}

/// The app's folder in the user's profile, under the environment variable `root`.
pub fn profile_dir(root: &str) -> PathBuf {
    settled_dir(Path::new(&std::env::var_os(root).unwrap_or_default()))
}

/// The folder was called screen-translate before the rename to Sonoba. It moves over at the first
/// use, and is used where it is for as long as the old app holds its files open.
fn settled_dir(root: &Path) -> PathBuf {
    let (new, old) = (root.join("Sonoba"), root.join("screen-translate"));
    if new.exists() || !old.exists() || std::fs::rename(&old, &new).is_ok() { new } else { old }
}

fn path() -> PathBuf {
    portable_dir().cloned().unwrap_or_else(|| profile_dir("APPDATA")).join("config.json")
}

static CONFIG: Mutex<Option<Config>> = Mutex::new(None);

pub fn get() -> Config {
    let mut config = CONFIG.lock().unwrap();
    config.get_or_insert_with(|| Config::parse(&std::fs::read_to_string(path()).unwrap_or_default())).clone()
}

pub fn update(change: impl FnOnce(&mut Config)) {
    let mut config = get();
    change(&mut config);
    if let Some(dir) = path().parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path(), config.to_json()); // a failed save only loses it at the next start
    *CONFIG.lock().unwrap() = Some(config);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_old_profile_folder_moves_over_once() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = std::env::temp_dir().join(format!("sonoba-test-{}", std::process::id()));
        let (new, old) = (root.join("Sonoba"), root.join("screen-translate"));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("config.json"), "{}").unwrap();

        let held = std::fs::OpenOptions::new().read(true).share_mode(0).open(old.join("config.json")).unwrap();
        assert_eq!(settled_dir(&root), old);
        drop(held);

        assert_eq!(settled_dir(&root), new);
        assert!(new.join("config.json").exists() && !old.exists());

        std::fs::create_dir_all(&old).unwrap(); // the old app, still running, saved its settings
        assert_eq!(settled_dir(&root), new);
        assert!(old.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn settings_survive_a_round_trip() {
        let mut config = Config::default();
        config.model = "custom.gguf".into();
        config.device = Device::Cpu;
        config.cpu_threads = Some(3);
        config.gpu_limit_mib = Some(1536);
        config.show_frame = true;
        config.flash_frame = true;
        config.shared = true;
        config.source = "ja".into();
        config.target = "ja(easy)".into();
        config.ui_lang = "en".into();
        config.intro_done = true;
        config.license_accepted = true;
        config.hotkeys[1] = Hotkey { mods: 6, key: 0x70 };
        config.hotkeys[4] = Hotkey { mods: 0, key: 5 };
        config.hotkeys[3] = Hotkey::OFF;
        config.form = "polite".into();
        config.glossary = vec![("bank".into(), "川岸".into())];
        config.font = "Meiryo".into();
        config.bold_text = true;
        config.blur_background = true;
        config.opacity = 80;
        config.keep_history = false;
        assert_eq!(Config::parse(&config.to_json()), config);
    }

    #[test]
    fn a_byte_order_mark_is_no_damage() {
        assert!(Config::parse("\u{FEFF}{\"show_frame\": true}").show_frame);
    }

    #[test]
    fn old_settings_keep_the_original_background() {
        assert!(!Config::parse("{\"font\":\"Meiryo\"}").blur_background);
        assert!(!Config::parse("{\"blur_background\":\"true\"}").blur_background);
    }

    #[test]
    fn a_damaged_file_means_defaults() {
        assert_eq!(Config::parse("{ not json"), Config::default());
        assert_eq!(Config::parse("{\"device\": 7}"), Config::default());
    }
}
