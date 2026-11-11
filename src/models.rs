//! The models: PLaMo 3 Translate in its four sizes and Hy-MT2, plus any GGUF file dropped into the folder.

use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufReader, Read, Seek};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::Duration;
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

pub const DEFAULT: &str = "plamo-3-translate-2b-Q4_K_M.gguf";
/// A model the app can download. The settings page shows it as a family, then a size.
struct Known {
    file: &'static str,
    repo: &'static str, // on Hugging Face
    family: &'static str,
    size: &'static str,
    tier: &'static str,
    gb: f64,                 // the file
    memory_gb: Option<f64>, // GPU memory once loaded, where measured (RTX 4070 Ti, Vulkan, 4 slots)
    license: &'static str,
}

const fn known(file: &'static str, repo: &'static str, family: &'static str, size: &'static str, tier: &'static str, gb: f64, memory_gb: Option<f64>, license: &'static str) -> Known {
    Known { file, repo, family, size, tier, gb, memory_gb, license }
}

const CATALOG: [Known; 6] = [
    known("plamo-3-translate-0.8b-Q4_K_M.gguf", "pfnet/plamo-3-translate-0.8b-GGUF", "plamo", "0.8B", "light", 0.6, Some(0.65), "plamo"),
    known("plamo-3-translate-2b-Q4_K_M.gguf", "pfnet/plamo-3-translate-2b-GGUF", "plamo", "2B", "standard", 1.8, Some(2.1), "plamo"),
    known("plamo-3-translate-8b-Q4_K_M.gguf", "pfnet/plamo-3-translate-8b-GGUF", "plamo", "8B", "high", 5.3, None, "plamo"),
    known("plamo-3-translate-31b-Q4_K_M.gguf", "pfnet/plamo-3-translate-31b-GGUF", "plamo", "31B", "best", 19.5, None, "plamo"),
    // Tencent's own smaller files need kernels llama.cpp does not have yet; unsloth's 3-bit one is the smallest that stays sound.
    known("Hy-MT2-1.8B-UD-IQ3_XXS.gguf", "unsloth/Hy-MT2-1.8B-GGUF", "hymt", "1.8B 3bit", "light", 0.8, Some(1.4), "apache"),
    known("Hy-MT2-1.8B-Q4_K_M.gguf", "tencent/Hy-MT2-1.8B-GGUF", "hymt", "1.8B 4bit", "standard", 1.1, Some(1.7), "apache"),
];
const SOURCE: &str = "https://huggingface.co";

pub const VERIFYING: i32 = 100;
/// Percent while a download runs, then VERIFYING; -1 when there is none.
pub static PROGRESS: AtomicI32 = AtomicI32::new(-1);
pub static CANCEL: AtomicBool = AtomicBool::new(false);
pub static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);
pub static DOWNLOADING: Mutex<String> = Mutex::new(String::new()); // the file PROGRESS is about

/// How a model expects its prompt, which is all the app needs to know about it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Format {
    Plamo,
    HyMt, // the template inside the GGUF file, with the request in the user turn as its model card shows
    Chat, // anything else: the template inside the GGUF file, plus a request to translate
}

/// The file says what it is in its own header, whatever another app has named it. A header that
/// does not say (Tencent's Hy-MT2 calls itself "1.8B") leaves it to the file name.
pub fn format_of(file: &str) -> Format {
    static KNOWN: Mutex<Option<HashMap<PathBuf, Format>>> = Mutex::new(None);
    let path = path(file);
    let mut known = KNOWN.lock().unwrap();
    let known = known.get_or_insert_with(HashMap::new);
    if let Some(format) = known.get(&path) {
        return *format;
    }
    let names = std::fs::File::open(&path).ok().and_then(|f| names_inside(BufReader::new(f)));
    let format = format_from(names.as_deref().unwrap_or(""), file);
    if names.is_some() {
        known.insert(path, format); // a file that is not there yet is read once it is
    }
    format
}

fn format_from(names: &str, file: &str) -> Format {
    let kind = |text: &str| {
        let text = text.to_lowercase().replace([' ', '_'], "-");
        if text.contains("plamo-3-translate") {
            Some(Format::Plamo)
        } else if text.contains("hy-mt") {
            Some(Format::HyMt)
        } else {
            None
        }
    };
    kind(names).or_else(|| kind(file)).unwrap_or(Format::Chat)
}

/// The model's name, base name and base model from a GGUF header, or None for a file that is not GGUF.
fn names_inside(mut file: impl Read + Seek) -> Option<String> {
    const WANTED: [&str; 3] = ["general.name", "general.basename", "general.base_model.0.name"];
    const SIZES: [i64; 13] = [1, 1, 2, 2, 4, 4, 4, 1, 0, 0, 8, 8, 8]; // by value type; 8 is a string, 9 an array
    const STRING: u32 = 8;
    const ARRAY: u32 = 9;
    fn number<const N: usize>(file: &mut impl Read) -> Option<[u8; N]> {
        let mut bytes = [0; N];
        file.read_exact(&mut bytes).ok().map(|_| bytes)
    }
    fn length(file: &mut impl Read) -> Option<u64> {
        number(file).map(u64::from_le_bytes).filter(|n| *n < 1 << 24) // more is not a header
    }
    fn text(file: &mut impl Read) -> Option<String> {
        let mut bytes = vec![0; length(file)? as usize];
        file.read_exact(&mut bytes).ok()?;
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }
    if &number::<4>(&mut file)? != b"GGUF" {
        return None;
    }
    file.seek_relative(4 + 8).ok()?; // the version, and the number of tensors
    let mut names = String::new();
    for _ in 0..length(&mut file)? {
        let Some(key) = text(&mut file) else { break };
        let Some(kind) = number(&mut file).map(u32::from_le_bytes) else { break };
        let skipped = match kind {
            STRING if WANTED.contains(&key.as_str()) => text(&mut file).map(|name| names = format!("{names} {name}")),
            STRING => length(&mut file).and_then(|n| file.seek_relative(n as i64).ok()),
            ARRAY => (|| {
                let (of, count) = (u32::from_le_bytes(number(&mut file)?), u64::from_le_bytes(number(&mut file)?));
                match of {
                    STRING => (0..count).try_for_each(|_| length(&mut file).and_then(|n| file.seek_relative(n as i64).ok())),
                    ARRAY => None,
                    _ => file.seek_relative(*SIZES.get(of as usize)? * i64::try_from(count).ok()?).ok(),
                }
            })(),
            _ => SIZES.get(kind as usize).and_then(|size| file.seek_relative(*size).ok()),
        };
        if skipped.is_none() {
            break; // what was read so far still counts
        }
    }
    Some(names)
}

/// The PLaMo Community License binds whoever uses a PLaMo model, and nobody else.
pub fn needs_license(file: &str) -> bool {
    format_of(file) == Format::Plamo
}

/// The license the settings page shows: PLaMo's for any PLaMo file, the catalog's for the rest.
pub fn license_of(file: &str) -> Option<&'static str> {
    if needs_license(file) { Some("plamo") } else { CATALOG.iter().find(|k| k.file == file).map(|k| k.license) }
}

pub struct Entry {
    pub file: String,
    pub family: Option<&'static str>, // None: a file dropped into the folder
    pub size: &'static str,
    pub tier: &'static str,
    pub gb: Option<f64>,
    pub memory_gb: Option<f64>,
}

/// The catalog, then whatever else the folder holds.
pub fn list() -> Vec<Entry> {
    let mut entries: Vec<Entry> = CATALOG
        .iter()
        .map(|k| Entry { file: k.file.to_string(), family: Some(k.family), size: k.size, tier: k.tier, gb: Some(k.gb), memory_gb: k.memory_gb })
        .collect();
    if let Ok(folder) = std::fs::read_dir(folder()) {
        for file in folder.flatten().filter_map(|f| f.file_name().into_string().ok()) {
            if file.ends_with(".gguf") && !entries.iter().any(|e| e.file == file) {
                entries.push(Entry { file, family: None, size: "", tier: "", gb: None, memory_gb: None });
            }
        }
    }
    entries
}

/// The app's own folder for models. The OCR models are always here.
pub fn dir() -> PathBuf {
    crate::config::portable_dir().cloned().unwrap_or_else(|| crate::config::profile_dir("LOCALAPPDATA")).join("models")
}

/// Where the translation models are kept: the folder chosen in the settings, or the app's own.
pub fn folder() -> PathBuf {
    let chosen = crate::config::get().model_folder;
    if chosen.is_empty() { dir() } else { PathBuf::from(chosen) }
}

/// Only the translation models follow the chosen folder: the zip brings an OCR model in the app's own.
fn path_in(chosen: &str, file: &str) -> PathBuf {
    if chosen.is_empty() || !file.ends_with(".gguf") { dir().join(file) } else { Path::new(chosen).join(file) }
}

pub fn path(file: &str) -> PathBuf {
    path_in(&crate::config::get().model_folder, file)
}

pub fn is_present(file: &str) -> bool {
    path(file).exists()
}

fn part(file: &str) -> PathBuf {
    let mut part = path(file).into_os_string();
    part.push(".part");
    part.into()
}

/// A download that stopped half way, which the next one picks up.
pub fn has_part(file: &str) -> bool {
    part(file).exists()
}

pub fn can_download(file: &str) -> bool {
    CATALOG.iter().any(|k| k.file == file)
}

pub fn delete(file: &str) -> std::io::Result<()> {
    if has_part(file) {
        std::fs::remove_file(part(file))?;
    }
    if is_present(file) {
        std::fs::remove_file(path(file))?;
    }
    Ok(())
}

fn tool(name: &str) -> Command {
    let mut command = Command::new(name); // curl and certutil come with Windows
    command.stdin(Stdio::null()).stderr(Stdio::null()).creation_flags(CREATE_NO_WINDOW);
    command
}

/// (size, sha-256) of a file, from the repository's listing.
fn expected(repo: &str, file: &str) -> Result<(u64, String), String> {
    let url = format!("{SOURCE}/api/models/{repo}/tree/main");
    let listing = tool("curl").args(["-sSfL", &url]).output().map_err(|e| e.to_string())?;
    let listing: Value = serde_json::from_slice(&listing.stdout).map_err(|_| "モデルの一覧を取得できません".to_string())?;
    let entry = listing.as_array().and_then(|files| files.iter().find(|f| f["path"] == file));
    let size = entry.and_then(|f| f["size"].as_u64());
    let hash = entry.and_then(|f| f["lfs"]["oid"].as_str());
    size.zip(hash.map(str::to_string)).ok_or_else(|| "モデルの情報が見つかりません".to_string())
}

fn sha256(file: &PathBuf) -> Result<String, String> {
    let output = tool("certutil").arg("-hashfile").arg(file).arg("SHA256").output().map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&output.stdout);
    let is_hash = |line: &&str| line.len() == 64 && line.chars().all(|c| c.is_ascii_hexdigit());
    text.lines().map(str::trim).find(is_hash).map(str::to_lowercase).ok_or_else(|| "ハッシュを計算できません".to_string())
}

fn download(file: &str) -> Result<(), String> {
    // The OCR models for other scripts are saved beside the translation models.
    let (repo, remote) = match crate::ocr::SCRIPTS.iter().find(|s| s.file() == file) {
        Some(script) => (script.repo(), "inference.onnx"),
        None => {
            let repo = CATALOG.iter().find(|k| k.file == file).map(|k| k.repo).ok_or("このモデルはダウンロードできません")?;
            (repo.to_string(), file)
        }
    };
    let (total, hash) = expected(&repo, remote)?;
    let (target, part) = (path(file), part(file)); // the part is kept when cancelled, and picked up again by -C -
    std::fs::create_dir_all(target.parent().unwrap_or(Path::new(""))).map_err(|e| e.to_string())?;
    let url = format!("{SOURCE}/{repo}/resolve/main/{remote}");
    if std::fs::metadata(&part).map_or(0, |m| m.len()) < total {
        let mut curl = tool("curl").args(["-sSfL", "-C", "-", "-o"]).arg(&part).arg(&url).stdout(Stdio::null()).spawn().map_err(|e| e.to_string())?;
        crate::engine::tie_to_app(&curl);
        loop {
            if let Some(status) = curl.try_wait().map_err(|e| e.to_string())? {
                if !status.success() {
                    return Err("ダウンロードに失敗しました".to_string());
                }
                break;
            }
            if CANCEL.swap(false, Ordering::Relaxed) {
                let _ = curl.kill();
                let _ = curl.wait();
                return Ok(());
            }
            let got = std::fs::metadata(&part).map_or(0, |m| m.len());
            PROGRESS.store((got * 99 / total.max(1)) as i32, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    PROGRESS.store(VERIFYING, Ordering::Relaxed);
    if sha256(&part)? != hash {
        let _ = std::fs::remove_file(&part);
        return Err("ダウンロードしたファイルが壊れています".to_string());
    }
    std::fs::rename(&part, target).map_err(|e| e.to_string())
}

pub fn start_download(file: String) {
    PROGRESS.store(0, Ordering::Relaxed);
    CANCEL.store(false, Ordering::Relaxed);
    *DOWNLOADING.lock().unwrap() = file.clone();
    std::thread::spawn(move || {
        *LAST_ERROR.lock().unwrap() = download(&file).err();
        PROGRESS.store(-1, Ordering::Relaxed);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_name_decides_the_prompt_format() {
        assert_eq!(format_of(DEFAULT), Format::Plamo);
        assert_eq!(format_of("plamo-3-translate-8b-Q3_K_M.gguf"), Format::Plamo);
        assert_eq!(format_of("Hy-MT2-1.8B-Q4_K_M.gguf"), Format::HyMt);
        assert_eq!(format_of("gemma-4-q4.gguf"), Format::Chat);
    }

    /// A GGUF header with these values, laid out as llama.cpp writes it.
    fn header(values: &[(&str, Option<&str>)]) -> std::io::Cursor<Vec<u8>> {
        let text = |s: &str| [&(s.len() as u64).to_le_bytes()[..], s.as_bytes()].concat();
        let mut bytes = [&b"GGUF"[..], &3u32.to_le_bytes(), &0u64.to_le_bytes(), &(values.len() as u64).to_le_bytes()].concat();
        for (key, value) in values {
            bytes.extend(text(key));
            match value {
                Some(value) => bytes.extend([&8u32.to_le_bytes()[..], &text(value)].concat()),
                // a vocabulary of two words, then a number: both are stepped over
                None => bytes.extend([&9u32.to_le_bytes()[..], &8u32.to_le_bytes(), &2u64.to_le_bytes(), &text("a"), &text("bc")].concat()),
            }
        }
        std::io::Cursor::new(bytes)
    }

    #[test]
    fn the_name_inside_the_file_decides_before_the_file_name() {
        let plamo = header(&[("general.architecture", Some("plamo3")), ("general.name", Some("plamo-3-translate-2b")), ("tokenizer.ggml.tokens", None)]);
        assert_eq!(format_from(&names_inside(plamo).unwrap(), "model.gguf"), Format::Plamo);
        let after_the_words = header(&[("tokenizer.ggml.tokens", None), ("general.base_model.0.name", Some("PLaMo 3 Translate 2B"))]);
        assert_eq!(format_from(&names_inside(after_the_words).unwrap(), "shared.gguf"), Format::Plamo);
        let tencent = header(&[("general.architecture", Some("hunyuan-dense")), ("general.name", Some("1.8B"))]);
        assert_eq!(format_from(&names_inside(tencent).unwrap(), "Hy-MT2-1.8B-Q4_K_M.gguf"), Format::HyMt);
        let other = header(&[("general.name", Some("Qwen3 4B"))]);
        assert_eq!(format_from(&names_inside(other).unwrap(), "qwen3-4b.gguf"), Format::Chat);
        assert_eq!(format_from(&names_inside(header(&[("general.name", Some("Hy-MT2 1.8B"))])).unwrap(), "plamo-3-translate-x.gguf"), Format::HyMt);
    }

    /// What the real files in a folder say: MODELS_IN=<folder> cargo test real_models -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_models() {
        for file in std::fs::read_dir(std::env::var("MODELS_IN").unwrap()).unwrap().flatten().filter(|f| f.path().extension().is_some_and(|e| e == "gguf")) {
            let started = std::time::Instant::now();
            let names = names_inside(BufReader::new(std::fs::File::open(file.path()).unwrap()));
            let format = format_from(names.as_deref().unwrap_or(""), "x.gguf");
            println!("{} -> {names:?}, without its file name {format:?}, {:.1} ms", file.file_name().to_string_lossy(), started.elapsed().as_secs_f64() * 1000.0);
        }
    }

    #[test]
    fn a_file_that_is_not_gguf_or_is_cut_short_falls_back_to_its_name() {
        assert_eq!(names_inside(std::io::Cursor::new(b"not a model".to_vec())), None);
        let mut cut = header(&[("general.name", Some("plamo-3-translate-2b")), ("general.basename", Some("plamo"))]).into_inner();
        cut.truncate(cut.len() - 3);
        assert_eq!(format_from(&names_inside(std::io::Cursor::new(cut)).unwrap(), "x.gguf"), Format::Plamo);
        assert_eq!(format_from("", "pfnet_PLaMo-3-Translate-2b.Q4_K_M.gguf"), Format::Plamo);
    }

    #[test]
    fn translation_models_follow_the_chosen_folder_and_ocr_models_stay() {
        assert_eq!(path_in("D:/shared", DEFAULT), Path::new("D:/shared").join(DEFAULT));
        assert_eq!(path_in("", DEFAULT), dir().join(DEFAULT));
        assert_eq!(path_in("D:/shared", "korean_PP-OCRv5_mobile_rec.onnx"), dir().join("korean_PP-OCRv5_mobile_rec.onnx"));
    }

    #[test]
    fn only_plamo_needs_its_license() {
        assert!(needs_license(DEFAULT) && needs_license("plamo-3-translate-8b-Q3_K_M.gguf"));
        assert!(!needs_license("Hy-MT2-1.8B-Q4_K_M.gguf") && !needs_license("gemma-4-q4.gguf"));
        assert_eq!(license_of("Hy-MT2-1.8B-Q4_K_M.gguf"), Some("apache"));
        assert_eq!(license_of("gemma-4-q4.gguf"), None);
    }

    #[test]
    fn the_catalog_covers_every_size_and_the_default() {
        assert_eq!(list().iter().take(6).filter(|e| can_download(&e.file) && e.family.is_some()).count(), 6);
        assert!(CATALOG.iter().filter(|k| k.family == "plamo").any(|k| k.file == DEFAULT && k.tier == "standard"));
        assert!(can_download(DEFAULT) && can_download("Hy-MT2-1.8B-Q4_K_M.gguf"));
        assert!(!can_download("gemma-4-q4.gguf"));
    }
}
