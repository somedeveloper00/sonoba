//! PP-OCRv6 small: DB text detection, then CTC recognition, both on the CPU.
//! The values below match RapidOCR's PP-OCRv6 config so results stay comparable.

use crate::patch::{self, Glyphs};
use rayon::prelude::*;
use rten::{Model, RunOptions, ThreadPool};
use rten_tensor::NdTensor;
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

static DET: &[u8] = include_bytes!("../models/PP-OCRv6_det_small.onnx");
static REC: &[u8] = include_bytes!("../models/PP-OCRv6_rec_small.onnx");
static DICT: &str = include_str!("../models/dict.txt");

/// A script PP-OCRv6 has no letters for, and PaddleOCR's PP-OCRv5 model that reads it. Each
/// comes as a download, and also reads English.
pub struct Script {
    pub id: &'static str, // PaddleOCR's name for the model, which names the file and the repository too
    dict: &'static str,
    letters: &'static [RangeInclusive<char>],
    right_to_left: bool,
}

pub static SCRIPTS: [Script; 7] = [
    Script { id: "korean", dict: include_str!("../models/dict_korean.txt"), letters: &['\u{AC00}'..='\u{D7A3}'], right_to_left: false },
    Script { id: "cyrillic", dict: include_str!("../models/dict_cyrillic.txt"), letters: &['\u{0400}'..='\u{04FF}'], right_to_left: false },
    Script {
        id: "arabic",
        dict: include_str!("../models/dict_arabic.txt"),
        letters: &['\u{0600}'..='\u{06FF}', '\u{0750}'..='\u{077F}', '\u{FB50}'..='\u{FDFF}', '\u{FE70}'..='\u{FEFF}'],
        right_to_left: true,
    },
    Script { id: "th", dict: include_str!("../models/dict_th.txt"), letters: &['\u{0E00}'..='\u{0E7F}'], right_to_left: false },
    Script { id: "devanagari", dict: include_str!("../models/dict_devanagari.txt"), letters: &['\u{0900}'..='\u{097F}'], right_to_left: false },
    Script { id: "ta", dict: include_str!("../models/dict_ta.txt"), letters: &['\u{0B80}'..='\u{0BFF}'], right_to_left: false },
    Script { id: "te", dict: include_str!("../models/dict_te.txt"), letters: &['\u{0C00}'..='\u{0C7F}'], right_to_left: false },
];

impl Script {
    pub fn file(&self) -> String {
        format!("{}_PP-OCRv5_mobile_rec.onnx", self.id)
    }

    pub fn repo(&self) -> String {
        format!("PaddlePaddle/{}_PP-OCRv5_mobile_rec_onnx", self.id)
    }

    fn has(&self, c: char) -> bool {
        self.letters.iter().any(|r| r.contains(&c))
    }

    /// Whether this model's reading is text in its script, not a guess at some other script.
    /// A sure reading may mix in English; a less sure one must be mostly the script.
    fn wrote(&self, text: &str, score: f32) -> bool {
        let ours = text.chars().filter(|&c| self.has(c)).count();
        let letters = text.chars().filter(|c| c.is_alphabetic()).count();
        ours > 0 && (score >= OTHER_SCORE || (score >= TEXT_SCORE && ours * 2 >= letters))
    }
}

// shortcut: long sides above this are scaled down, so a whole 4K screen of small text
// reads poorly; tile the image if full-screen regions turn out to matter.
const MAX_DET_SIDE: usize = 1600;
const PAD: usize = 16; // the detector misses text touching the image edge
const DET_THRESH: f32 = 0.3;
const BOX_THRESH: f32 = 0.5;
const UNCLIP_RATIO: f32 = 1.6;
const REC_HEIGHT: usize = 48;
const REC_MIN_WIDTH: usize = 160;
const REC_MAX_WIDTH: usize = 3200;
const TEXT_SCORE: f32 = 0.5;
const SURE_SCORE: f32 = 0.93; // a line read this well is in a script PP-OCRv6 knows, and needs no second look
const OTHER_SCORE: f32 = 0.8; // a reading by another script's model this sure is no guess, even if it is mostly English
const PICTURE_TEXT_SCORE: f32 = 0.8; // over a picture, a reading less sure than this is a pattern, not text
const ROOMY_SCORE: f32 = 0.97; // below this, a line with an accented letter is read again with more room
const ROW_GAP: i32 = 8; // labels in a row stand at most this many line heights apart

#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub glyphs: Glyphs,
    pub text: String,
}

/// A BGR image with values already in the -1..1 range the models expect.
struct Rgbf {
    w: usize,
    h: usize,
    px: Vec<f32>,
}

impl Rgbf {
    /// `bgra` is top-down, tightly packed.
    fn from_bgra_padded(bgra: &[u8], w: usize, h: usize, pad: usize) -> Self {
        let (pw, ph) = (w + pad * 2, h + pad * 2);
        // Padding with the corner colour keeps the edge from reading as a text stroke.
        let fill = [bgra[0], bgra[1], bgra[2]].map(norm);
        let mut px = Vec::with_capacity(pw * ph * 3);
        for _ in 0..ph * pw {
            px.extend_from_slice(&fill);
        }
        for y in 0..h {
            for x in 0..w {
                let (s, d) = ((y * w + x) * 4, ((y + pad) * pw + x + pad) * 3);
                for c in 0..3 {
                    px[d + c] = norm(bgra[s + c]);
                }
            }
        }
        Self { w: pw, h: ph, px }
    }

    /// Bilinear resample of the sub-rectangle into `dw`x`dh` CHW planes, leaving `stride`
    /// columns per row so a line can be right-padded with zeros.
    fn sample_chw(&self, (x0, y0, sw, sh): (usize, usize, usize, usize), dw: usize, dh: usize, stride: usize) -> Vec<f32> {
        let mut out = vec![0f32; 3 * dh * stride];
        let (fx, fy) = (sw as f32 / dw as f32, sh as f32 / dh as f32);
        for dy in 0..dh {
            let sy = ((dy as f32 + 0.5) * fy - 0.5).clamp(0.0, (sh - 1) as f32);
            let (iy, ty) = (sy as usize, sy.fract());
            let iy1 = (iy + 1).min(sh - 1);
            for dx in 0..dw {
                let sx = ((dx as f32 + 0.5) * fx - 0.5).clamp(0.0, (sw - 1) as f32);
                let (ix, tx) = (sx as usize, sx.fract());
                let ix1 = (ix + 1).min(sw - 1);
                for c in 0..3 {
                    let at = |x: usize, y: usize| self.px[((y0 + y) * self.w + x0 + x) * 3 + c];
                    let top = at(ix, iy) * (1.0 - tx) + at(ix1, iy) * tx;
                    let bot = at(ix, iy1) * (1.0 - tx) + at(ix1, iy1) * tx;
                    out[(c * dh + dy) * stride + dx] = top * (1.0 - ty) + bot * ty;
                }
            }
        }
        out
    }
}

fn norm(v: u8) -> f32 {
    v as f32 / 127.5 - 1.0
}

pub struct Ocr {
    det: Model,
    rec: Model,
    chars: Vec<&'static str>,
    others: Vec<(&'static Script, Model, Vec<&'static str>)>,
    last: AtomicUsize, // the one of `others` that read the last line it was asked to
    pool: Arc<ThreadPool>,
    threads: usize,
}

fn dictionary(lines: &'static str) -> Vec<&'static str> {
    let mut chars = vec![""]; // class 0 is the CTC blank
    chars.extend(lines.lines().filter(|s| !s.is_empty())); // git may check dict.txt out as CRLF
    chars.push(" ");
    chars
}

impl Ocr {
    /// PP-OCRv6 reads Chinese, English, Japanese and Latin scripts. `others` are the models for
    /// the other scripts that have been downloaded.
    pub fn new(threads: usize, others: &[(&'static Script, PathBuf)]) -> Result<Self, String> {
        let load = |b| Model::load_static_slice(b).map_err(|e| e.to_string());
        let others = others
            .iter()
            .map(|(script, path)| {
                let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
                Ok((*script, Model::load(bytes).map_err(|e| e.to_string())?, dictionary(script.dict)))
            })
            .collect::<Result<_, String>>()?;
        let pool = Arc::new(ThreadPool::with_num_threads(threads));
        Ok(Self { det: load(DET)?, rec: load(REC)?, chars: dictionary(DICT), others, last: AtomicUsize::new(0), pool, threads })
    }

    pub fn matches(&self, threads: usize, others: &[(&'static Script, PathBuf)]) -> bool {
        self.threads == threads && self.others.iter().map(|o| o.0.id).eq(others.iter().map(|o| o.0.id))
    }

    fn run_options(&self) -> Option<RunOptions> {
        Some(RunOptions::default().with_thread_pool(Some(self.pool.clone())))
    }

    pub fn read(&self, bgra: &[u8], w: usize, h: usize) -> Result<Vec<Line>, String> {
        let img = Rgbf::from_bgra_padded(bgra, w, h, PAD);
        let (pad, boxes) = (PAD as i32, self.detect(&img)?);
        // A line with wide gaps in it, such as a menu bar, is read again piece by piece: the
        // letters it was read as tell how wide a gap must be to part two labels. The recogniser
        // can leave the space out of a wide gap too, as in "BackSave".
        let read_box = |rect: (usize, usize, usize, usize)| -> Result<Vec<Line>, String> {
            let Some((line, words)) = self.read_line(bgra, &img, (w, h), rect)? else { return Ok(Vec::new()) };
            let (x, y, bw, bh) = rect;
            let (left, top) = (x as i32 - pad, y as i32 - pad);
            let mut cuts: Vec<i32> = patch::split_at_gaps(bgra, w, h, (left, top, bw as i32, bh as i32), &line.text).iter().skip(1).map(|piece| piece.0).collect();
            // Over a bright picture some gaps are not blank; where the recogniser put each letter
            // still shows them. Both ways can find the same gap.
            if !line.glyphs.icon {
                cuts.extend(wide_gaps(&words).into_iter().map(|share| left + (share * bw as f32) as i32));
            }
            cuts.sort_unstable();
            cuts.dedup_by(|later, kept| (*later - *kept) * 2 < bh as i32);
            if cuts.is_empty() {
                return Ok(vec![line]);
            }
            let edges: Vec<i32> = std::iter::once(left).chain(cuts).chain(std::iter::once(left + bw as i32)).collect();
            let pieces = edges.windows(2).map(|e| (e[0], top, e[1] - e[0], bh as i32));
            let mut out = Vec::new();
            for (px, py, pw, ph) in pieces {
                out.extend(self.read_line(bgra, &img, (w, h), ((px + pad) as usize, (py + pad) as usize, pw as usize, ph as usize))?.map(|(line, _)| line));
            }
            Ok(out)
        };
        let lines: Vec<Vec<Line>> = self.pool.run(|| boxes.par_iter().map(|&rect| read_box(rect)).collect::<Result<_, _>>())?;
        let mut lines: Vec<Line> = lines.into_iter().flatten().collect();
        reading_order(&mut lines);
        Ok(lines)
    }

    fn read_line(&self, bgra: &[u8], img: &Rgbf, (w, h): (usize, usize), rect: (usize, usize, usize, usize)) -> Result<Option<(Line, Words)>, String> {
        let (x, y, bw, bh) = rect;
        let in_frame = (x as i32 - PAD as i32, y as i32 - PAD as i32, bw as i32, bh as i32);
        let Some(ink) = patch::glyphs(bgra, w, h, in_frame, None) else { return Ok(None) };
        // Ink from the lines above and below sinks the recogniser's confidence, so crop it off.
        let margin = (ink.h / 2) as usize;
        let top = ((ink.y + PAD as i32) as usize).saturating_sub(margin).max(y);
        let bottom = ((ink.y + ink.h + PAD as i32) as usize + margin).min(y + bh);
        let (text, score, words) = self.recognize(img, (x, top, bw, bottom.saturating_sub(top)))?;
        // A lattice, a leaf pattern or a scene's detail can pass for a few letters, but reads unsure.
        if ink.picture && score < PICTURE_TEXT_SCORE {
            return Ok(None);
        }
        Ok(patch::glyphs(bgra, w, h, in_frame, Some(&text))
            .filter(|_| !text.is_empty())
            .map(|glyphs| (Line { glyphs, text: without_icon(text.clone(), glyphs.icon) }, words)))
    }

    fn detect(&self, img: &Rgbf) -> Result<Vec<(usize, usize, usize, usize)>, String> {
        let ratio = (MAX_DET_SIDE as f32 / img.w.max(img.h) as f32).min(1.0);
        let round32 = |v: usize| (((v as f32 * ratio) / 32.0).round() as usize).max(1) * 32;
        let (dw, dh) = (round32(img.w), round32(img.h));
        let input = NdTensor::from_data([1, 3, dh, dw], img.sample_chw((0, 0, img.w, img.h), dw, dh, dw));
        let out = self.det.run_one(input.into(), self.run_options()).map_err(|e| e.to_string())?;
        let (shape, prob) = out.into_shape_vec::<f32, 4>().map_err(|e| e.to_string())?;
        let (mh, mw) = (shape[2], shape[3]);
        let (sx, sy) = (img.w as f32 / mw as f32, img.h as f32 / mh as f32);
        let mut boxes: Vec<_> = text_boxes(&prob, mw, mh)
            .into_iter()
            .map(|(x, y, bw, bh)| {
                let x0 = (x * sx).max(0.0) as usize;
                let y0 = (y * sy).max(0.0) as usize;
                let x1 = ((x + bw) * sx).ceil().min(img.w as f32) as usize;
                let y1 = ((y + bh) * sy).ceil().min(img.h as f32) as usize;
                (x0, y0, x1 - x0, y1 - y0)
            })
            .filter(|&(_, _, bw, bh)| bw > 3 && bh > 3)
            .collect();
        boxes.sort_by_key(|&(x, y, _, _)| (y, x));
        Ok(boxes)
    }

    /// The text in `rect` and how sure the reading is.
    fn recognize(&self, img: &Rgbf, rect: (usize, usize, usize, usize)) -> Result<(String, f32, Words), String> {
        let (x, y, bw, bh) = rect;
        if bw == 0 || bh == 0 {
            return Ok((String::new(), 0.0, Vec::new()));
        }
        let (mut text, mut score, mut words) = self.read_with(&self.rec, &self.chars, img, rect)?;
        // An ascender close under the box's edge can read as an accent, "Online" as "Oníine". A
        // little more room above and below settles it, and the surer reading wins.
        let accented = |c: char| ('\u{C0}'..='\u{24F}').contains(&c) && c.is_alphabetic();
        if score < ROOMY_SCORE && text.chars().any(accented) {
            let room = bh / 6;
            let roomy = (x, y.saturating_sub(room), bw, (bh + room * 2).min(img.h - y.saturating_sub(room)));
            let (again, again_score, again_words) = self.read_with(&self.rec, &self.chars, img, roomy)?;
            if again_score > score {
                (text, score, words) = (again, again_score, again_words);
            }
        }
        // Other scripts read badly in the main model, so doubt is what sends a line to their
        // models. It drops the letters it has no class for and can be sure of the full stop it
        // keeps, so a reading far too short for the line is doubt too.
        let too_short = text.chars().count() * bh * 2 < bw;
        if score < SURE_SCORE || too_short {
            // The script that won last goes first, as a screen tends to stay in one language.
            let first = self.last.load(Ordering::Relaxed);
            let (mut best, mut best_score) = (None, if too_short { 0.0 } else { score });
            for i in (0..self.others.len()).map(|i| (i + first) % self.others.len()) {
                let (script, model, chars) = &self.others[i];
                let (other, other_score, _) = self.read_with(model, chars, img, rect)?;
                if other_score > best_score && script.wrote(&other, other_score) {
                    (best, best_score) = (Some((i, if script.right_to_left { logical_order(&other) } else { other })), other_score);
                    if other_score >= SURE_SCORE {
                        break;
                    }
                }
            }
            if let Some((i, other)) = best {
                self.last.store(i, Ordering::Relaxed);
                return Ok((other, best_score, Vec::new()));
            }
        }
        Ok(if score < TEXT_SCORE { (String::new(), 0.0, Vec::new()) } else { (text, score, words) })
    }

    fn read_with(&self, model: &Model, chars: &[&str], img: &Rgbf, rect: (usize, usize, usize, usize)) -> Result<(String, f32, Words), String> {
        let (_, _, bw, bh) = rect;
        let width = (bw * REC_HEIGHT).div_ceil(bh).clamp(1, REC_MAX_WIDTH);
        let stride = width.max(REC_MIN_WIDTH);
        let input = NdTensor::from_data([1, 3, REC_HEIGHT, stride], img.sample_chw(rect, width, REC_HEIGHT, stride));
        let out = model.run_one(input.into(), self.run_options()).map_err(|e| e.to_string())?;
        let (shape, probs) = out.into_shape_vec::<f32, 3>().map_err(|e| e.to_string())?;
        if shape[2] != chars.len() {
            return Err(format!("model has {} classes, dictionary {}", shape[2], chars.len()));
        }
        let (text, score, words) = ctc_decode(&probs, shape[2], chars);
        // The steps span the padded input; the line fills `width` of it.
        let to_box = stride as f32 / width as f32;
        Ok((text, score, words.into_iter().map(|(a, b, n)| (a * to_box, b * to_box, n)).collect()))
    }
}

/// Connected regions of the detector's probability map, as (x, y, w, h) in map pixels,
/// grown by the DB unclip distance. Axis-aligned only: screen text is not rotated.
fn text_boxes(prob: &[f32], w: usize, h: usize) -> Vec<(f32, f32, f32, f32)> {
    let mut on: Vec<bool> = prob.iter().map(|&p| p > DET_THRESH).collect();
    // Same 2x2 dilation as RapidOCR's use_dilation, joining strokes split by the threshold.
    for y in (0..h).rev() {
        for x in (0..w).rev() {
            let i = y * w + x;
            on[i] = on[i]
                || (x > 0 && on[i - 1])
                || (y > 0 && on[i - w])
                || (x > 0 && y > 0 && on[i - w - 1]);
        }
    }
    let mut seen = vec![false; w * h];
    let mut stack = Vec::new();
    let mut boxes = Vec::new();
    for start in 0..w * h {
        if !on[start] || seen[start] {
            continue;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
        let (mut sum, mut n) = (0f32, 0f32);
        seen[start] = true;
        stack.push(start);
        while let Some(i) = stack.pop() {
            let (x, y) = (i % w, i / w);
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
            sum += prob[i];
            n += 1.0;
            for (nx, ny) in [(x.wrapping_sub(1), y), (x + 1, y), (x, y.wrapping_sub(1)), (x, y + 1)] {
                if nx < w && ny < h && on[ny * w + nx] && !seen[ny * w + nx] {
                    seen[ny * w + nx] = true;
                    stack.push(ny * w + nx);
                }
            }
        }
        if sum / n < BOX_THRESH {
            continue;
        }
        let (bw, bh) = ((x1 - x0 + 1) as f32, (y1 - y0 + 1) as f32);
        let grow = bw * bh * UNCLIP_RATIO / (2.0 * (bw + bh));
        boxes.push((x0 as f32 - grow, y0 as f32 - grow, bw + grow * 2.0, bh + grow * 2.0));
    }
    boxes
}

/// The model reads Arabic as it sees it, left to right. Runs of Latin letters and digits keep
/// their order and inner spaces, as in PaddleOCR's own decoder.
fn logical_order(text: &str) -> String {
    let ltr = |c: char| c.is_ascii_alphanumeric() || ":*./%+-".contains(c);
    let mut runs: Vec<&str> = Vec::new();
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let len = if ltr(c) { rest.find(|c: char| !ltr(c) && c != ' ').unwrap_or(rest.len()) } else { c.len_utf8() };
        let run = rest[..len].trim_end_matches(' '); // a space after the run sits between it and the Arabic
        runs.push(run);
        runs.extend((run.len()..len).map(|_| " "));
        rest = &rest[len..];
    }
    runs.reverse();
    runs.concat()
}

/// Each word's first and last letter, as shares of the line's width, and its number of letters.
type Words = Vec<(f32, f32, usize)>;

/// The text, how sure the model was of it, and where its words are.
fn ctc_decode(probs: &[f32], classes: usize, chars: &[&str]) -> (String, f32, Words) {
    let (mut text, mut sum, mut n, mut prev) = (String::new(), 0f32, 0f32, 0);
    let steps = (probs.len() / classes.max(1)).max(1) as f32;
    let (mut words, mut word): (Words, Option<(f32, f32, usize)>) = (Vec::new(), None);
    for (i, step) in probs.chunks_exact(classes).enumerate() {
        let (best, &p) = step.iter().enumerate().fold((0, &step[0]), |a, b| if b.1 > a.1 { b } else { a });
        if best != 0 && best != prev {
            text.push_str(chars[best]);
            sum += p;
            n += 1.0;
            let at = (i as f32 + 0.5) / steps;
            match (chars[best] == " ", &mut word) {
                (true, _) => words.extend(word.take()),
                (false, Some(w)) => (w.1, w.2) = (at, w.2 + 1),
                (false, None) => word = Some((at, at, 1)),
            }
        }
        prev = best;
    }
    words.extend(word);
    if n == 0.0 { (String::new(), 0.0, Vec::new()) } else { (text.trim().to_string(), sum / n, words) }
}

/// Where to cut a line at gaps far wider than a space, such as between a menu bar's items, as
/// shares of its width. A space is about two letters' pitch from letter to letter, a wide one in
/// justified text three; four, and half as wide again as the line's usual space, is a gap between
/// labels. Justified text spaces its words evenly, so none stands out by half.
fn wide_gaps(words: &[(f32, f32, usize)]) -> Vec<f32> {
    // Of two middle values, `lower` takes the lower: a menu's tight gaps count as its usual space.
    let middle = |mut v: Vec<f32>, lower: bool| {
        v.sort_by(f32::total_cmp);
        v.get(v.len().saturating_sub(lower as usize) / 2).copied()
    };
    let Some(pitch) = middle(words.iter().filter(|w| w.2 > 1).map(|w| (w.1 - w.0) / (w.2 - 1) as f32).collect(), false) else { return Vec::new() };
    let gaps: Vec<f32> = words.windows(2).map(|w| w[1].0 - w[0].1).collect();
    let usual = middle(gaps.iter().copied().filter(|&g| g < pitch * 4.0).collect(), true).unwrap_or(pitch * 2.0);
    words.windows(2).zip(&gaps).filter(|&(_, &g)| g >= pitch * 4.0 && g >= usual * 1.5).map(|(w, _)| (w[0].1 + w[1].0) / 2.0).collect()
}

/// The recogniser reads an icon as a stray character or two, such as a circled digit.
fn without_icon(text: String, icon: bool) -> String {
    match text.split_once(' ') {
        Some((first, rest)) if icon && first.chars().count() <= 2 => rest.to_string(),
        _ => text,
    }
}

/// Puts lines of a same-row group into left-to-right order, keeping rows top to bottom.
fn reading_order(lines: &mut [Line]) {
    lines.sort_by_key(|l| (l.glyphs.y, l.glyphs.x));
    for i in 1..lines.len() {
        let mut j = i;
        while j > 0 && same_row(&lines[j - 1].glyphs, &lines[j].glyphs) && lines[j].glyphs.x < lines[j - 1].glyphs.x {
            lines.swap(j - 1, j);
            j -= 1;
        }
    }
}

fn same_row(a: &Glyphs, b: &Glyphs) -> bool {
    let overlap = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
    overlap * 2 > a.h.min(b.h)
}

/// The font size that gives a line this much ink, going by which parts of the letters the
/// text uses: "same" fills only the x-height, "Sign" also reaches up and down.
fn font_size(ink_h: i32, text: &str) -> f32 {
    let has = |set: &str| text.chars().any(|c| set.contains(c) || !c.is_ascii());
    let tall = has("bdfhiklt!?'\"()") || text.chars().any(|c| c.is_uppercase() || c.is_ascii_digit());
    // A comma dips below the line about half as far as a g does.
    let deep = if has("gjpqyQ()") { 0.21 } else if has(",;") { 0.12 } else { 0.0 };
    ink_h as f32 / (if tall { 0.72 } else { 0.52 } + deep)
}

/// One paragraph, with its lines' combined ink.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub glyphs: Glyphs,
    pub lines: i32,
    pub em: i32,
    pub bold: bool,
    pub text: String,
}

/// Joins wrapped lines so each paragraph reaches the translator as a whole sentence. Columns side
/// by side take turns in reading order, row by row, so the paragraph a line carries on is one of
/// the last few, not always the last.
pub fn paragraphs(lines: &[Line]) -> Vec<Block> {
    const COLUMNS: usize = 8; // the most paragraphs a row can run across
    let mut out: Vec<Block> = Vec::new();
    let mut ends: Vec<(&Line, f32, usize)> = Vec::new(); // each paragraph's last line, its lines' sizes added up, its bold lines
    for (i, line) in lines.iter().enumerate() {
        let (g, em) = (&line.glyphs, font_size(line.glyphs.letters, &line.text));
        // How far the lines around reach: a wrapped line nearly does, a chat message rarely. Only
        // lines that start or centre where it does are in its column: a game's menu bar above the
        // dialogue may end where the dialogue does, by chance.
        let around = |a: &Line| {
            let (a, near) = (&a.glyphs, &lines[i.saturating_sub(3)..(i + 3).min(lines.len())]);
            let column = |n: &Glyphs| (a.x - n.x).abs() <= a.h * 3 || (a.x * 2 + a.w - n.x * 2 - n.w).abs() <= a.h * 2;
            near.iter().map(|n| &n.glyphs).filter(|n| (a.h - n.h).abs() * 3 <= a.h.max(n.h) && column(n)).map(|n| n.x + n.w).max().unwrap_or(0)
        };
        let found = (out.len().saturating_sub(COLUMNS)..out.len())
            .rev()
            .find(|&k| continues(ends[k].0, line, out[k].glyphs.x + out[k].glyphs.w, around(ends[k].0)));
        match found {
            Some(k) => {
                let block = &mut out[k];
                let hyphen = block.text.ends_with('-') && line.text.starts_with(|c: char| c.is_lowercase());
                if hyphen {
                    block.text.pop();
                } else if !(block.text.ends_with(is_cjk) && line.text.starts_with(is_cjk)) {
                    block.text.push(' ');
                }
                block.text.push_str(&line.text);
                let b = &mut block.glyphs;
                b.picture |= g.picture;
                let (right, bottom) = ((b.x + b.w).max(g.x + g.w), (b.y + b.h).max(g.y + g.h));
                (b.x, b.y) = (b.x.min(g.x), b.y.min(g.y));
                (b.w, b.h) = (right - b.x, bottom - b.y);
                block.lines += 1;
                ends[k] = (line, ends[k].1 + em, ends[k].2 + g.bold() as usize);
                block.em = (ends[k].1 / block.lines as f32).round() as i32;
                block.bold = ends[k].2 * 2 > block.lines as usize; // a bold heading run into its body leaves it regular
            }
            None => {
                out.push(Block { glyphs: *g, lines: 1, em: em.round() as i32, bold: g.bold(), text: line.text.clone() });
                ends.push((line, em, g.bold() as usize));
            }
        }
    }
    match_rows(&mut out);
    match_columns(&mut out);
    out
}

/// Labels side by side in the same colours, such as a menu bar or a row of boxes, share one size
/// and weight. Each is measured on its own, and a short word gives a poor measure: which letters
/// it has decides how much of the size its ink shows. The middle size and the commoner weight win.
/// Their tops line up and their bottoms nearly do, letters below the line aside; a heading beside
/// body text does neither. Over a picture the background varies from label to label, and a
/// highlighted item differs in colour only. Neighbours chain into one row. A size more than twice
/// or half the middle one is other text that happens to line up, and a row stops at a gap far
/// wider than the letters are tall, where another column begins. Half bold leaves each as it was.
fn match_rows(blocks: &mut [Block]) {
    let alike = |a: &Block, b: &Block| {
        let (ga, gb) = (&a.glyphs, &b.glyphs);
        let h = ga.h.max(gb.h);
        let aligned = (ga.y - gb.y).abs() * 4 <= h && (ga.y + ga.h - gb.y - gb.h).abs() * 3 <= h;
        let gap = (ga.x.max(gb.x) - (ga.x + ga.w).min(gb.x + gb.w)).max(0);
        let colours = ga.same_style(gb) || (ga.picture && gb.picture) || ((ga.picture || gb.picture) && ga.same_fg(gb));
        a.lines == 1 && b.lines == 1 && aligned && colours && gap <= h * ROW_GAP
    };
    for members in groups(blocks, alike).into_iter().filter(|m| m.len() > 1) {
        let mut ems: Vec<i32> = members.iter().map(|&i| blocks[i].em).collect();
        ems.sort_unstable();
        let middle = ems[ems.len() / 2];
        let (bolds, all) = (members.iter().filter(|&&i| blocks[i].bold).count() * 2, members.len());
        for i in members {
            if blocks[i].em.max(middle) <= blocks[i].em.min(middle) * 2 {
                blocks[i].em = middle;
                if bolds != all {
                    blocks[i].bold = bolds > all;
                }
            }
        }
    }
}

/// Labels stacked in a column in the same colours and weight, such as a settings screen's
/// headings, share one size as well. They line up at the left or the middle, and three at least
/// make a column. Only a size near the middle one moves: a speaker's name over a line of dialogue,
/// or a heading over body text, differs by more, or in weight.
fn match_columns(blocks: &mut [Block]) {
    const NEAR_PERCENT: i32 = 15;
    let alike = |a: &Block, b: &Block| {
        let (ga, gb) = (&a.glyphs, &b.glyphs);
        let h = ga.h.max(gb.h);
        let aligned = (ga.x - gb.x).abs() * 2 <= h || (ga.x * 2 + ga.w - gb.x * 2 - gb.w).abs() <= h;
        let gap = (ga.y.max(gb.y) - (ga.y + ga.h).min(gb.y + gb.h)).max(0);
        let colours = ga.same_style(gb) || ((ga.picture || gb.picture) && ga.same_fg(gb));
        a.lines == 1 && b.lines == 1 && a.bold == b.bold && aligned && colours && gap <= h * ROW_GAP
    };
    for members in groups(blocks, alike).into_iter().filter(|m| m.len() > 2) {
        let mut ems: Vec<i32> = members.iter().map(|&i| blocks[i].em).collect();
        ems.sort_unstable();
        let middle = ems[ems.len() / 2];
        for i in members {
            if (blocks[i].em - middle).abs() * 100 <= middle * NEAR_PERCENT {
                blocks[i].em = middle;
            }
        }
    }
}

/// Blocks chained by `alike`, each group in reading order.
fn groups(blocks: &[Block], alike: impl Fn(&Block, &Block) -> bool) -> Vec<Vec<usize>> {
    let mut group: Vec<usize> = (0..blocks.len()).collect(); // each block's group, named by one of its blocks
    for i in 0..blocks.len() {
        for j in i + 1..blocks.len() {
            if group[i] != group[j] && alike(&blocks[i], &blocks[j]) {
                let (keep, gone) = (group[i], group[j]);
                group.iter_mut().filter(|g| **g == gone).for_each(|g| *g = keep);
            }
        }
    }
    (0..blocks.len()).map(|name| (0..blocks.len()).filter(|&i| group[i] == name).collect()).filter(|m: &Vec<usize>| !m.is_empty()).collect()
}

/// Japanese and Chinese wrap anywhere, with no space to put back.
fn is_cjk(c: char) -> bool {
    matches!(c, '\u{3000}'..='\u{30FF}' | '\u{4E00}'..='\u{9FFF}' | '\u{FF00}'..='\u{FFEF}')
}

/// Code stays as it is: the model would otherwise paraphrase it. Prose rarely spends one
/// character in twelve on these.
pub fn looks_like_code(text: &str) -> bool {
    let symbols = text.matches(['=', '(', ')', '{', '}', '[', ']', ';', '_', '<', '>']).count();
    let prose_brackets = text.matches(" (").count() * 2; // "Save (Ctrl+S)": code has them right after a name
    symbols.saturating_sub(prose_brackets) * 12 >= text.chars().count()
}

/// True when `b` carries on the paragraph that `a` ended, wrapped by the layout. `around_right`
/// is how far the lines near `a` in its column reach.
fn continues(line_a: &Line, line_b: &Line, para_right: i32, around_right: i32) -> bool {
    let (a, b) = (&line_a.glyphs, &line_b.glyphs);
    let h = a.h.max(b.h);
    let gap = b.y - (a.y + a.h);
    // A short last line without a tall or deep letter, or without its glow, has a lower box.
    let (em_a, em_b) = (font_size(a.letters, &line_a.text), font_size(b.letters, &line_b.text));
    let similar_height = (a.h - b.h).abs() * 3 <= h || (em_a - em_b).abs() * 3.0 <= em_a.max(em_b);
    let same_column = (a.x + a.w).min(b.x + b.w) > a.x.max(b.x);
    // Wrapped lines share an edge or a middle: left, with room for an indent, centred, or right.
    // A menu bar over a game's dialogue overlaps it but lines up with it in none of these.
    let lined_up = (a.x - b.x).abs() <= h * 3 || (a.x * 2 + a.w - b.x * 2 - b.w).abs() <= h * 2 || (a.x + a.w - b.x - b.w).abs() <= h;
    // Over a picture the background differs from line to line, and a short word may happen to sit
    // on a flat stretch of it; the letters' colour does not differ.
    let alike = a.same_style(b) || ((a.picture || b.picture) && a.same_fg(b));
    // A wrapped line was full; a menu item that happens to end where the dialogue below ends is
    // far shorter than the dialogue.
    let filled = a.w * 2 >= b.w;
    if same_row(a, b) || gap * 10 > h * 11 || !similar_height || !same_column || !lined_up || !alike || !filled {
        return false;
    }
    // A hard line break: b's first word would have fitted on a, so nothing forced a wrap.
    let first_word = line_b.text.split(' ').next().unwrap_or("").chars().count() + 1;
    let word_w = b.w * first_word as i32 / line_b.text.chars().count().max(1) as i32;
    let wrapped = a.x + a.w + word_w > para_right.max(around_right).max(b.x + b.w);
    // Lowercase carries a sentence on, unless the line before ended one or stopped short: chat
    // messages often start with a lowercase name.
    if line_b.text.starts_with(|c: char| c.is_lowercase()) {
        return wrapped && !line_a.text.trim_end().ends_with(['.', '!', '?', '。', '！', '？']);
    }
    // Japanese has no words to go by: a line that ran to the right edge was wrapped.
    if line_a.text.ends_with(is_cjk) && line_b.text.starts_with(is_cjk) {
        return a.x + a.w + h > para_right.max(b.x + b.w);
    }
    if line_a.text.split(' ').count() < 4 {
        return false; // menu items and labels, not wrapped prose
    }
    wrapped
}

#[cfg(test)]
pub fn png_bgra(path: &str) -> (Vec<u8>, usize, usize) {
    let mut reader = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()))
        .read_info()
        .unwrap();
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    let ch = info.color_type.samples();
    let bgra = buf[..info.buffer_size()]
        .chunks_exact(ch)
        .flat_map(|p| [p[2], p[1], p[0], 255])
        .collect();
    (bgra, info.width as usize, info.height as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(x: i32, y: i32, w: i32, h: i32, text: &str) -> Line {
        Line { glyphs: Glyphs { x, y, w, h, letters: h, bg: [0; 3], fg: [255; 3], stem: 2, icon: false, picture: false }, text: text.into() }
    }

    fn l(x: i32, y: i32, w: i32, text: &str) -> Line {
        at(x, y, w, 18, text)
    }

    /// A game's menu bar sits just above its dialogue and overlaps it, but lines up with it at
    /// neither edge nor middle.
    #[test]
    fn a_menu_bar_does_not_run_into_the_dialogue_below() {
        let lines = [l(530, 0, 240, "Save Q.Save Q.Load Skip"), l(0, 24, 700, "Then, why didn't you use your hiding skills to hide from the guards?")];
        assert_eq!(paragraphs(&lines).len(), 2);
        // One item ending where the dialogue ends, over a line the reader caught without its "I".
        let lines = [l(660, 0, 40, "Auto"), l(0, 24, 700, "wouldn't call it strange, Machiko-san. It's this world that's weird.")];
        assert_eq!(paragraphs(&lines).len(), 2);
    }

    /// A game's dialogue: the menu bar above happens to end where the dialogue does, and the last
    /// line, one short word with no glow found round it, has a lower box and sits on a flat
    /// stretch of the picture.
    #[test]
    fn a_dialogue_keeps_its_short_last_line() {
        let over_picture = |mut l: Line| {
            (l.glyphs.letters, l.glyphs.bg, l.glyphs.picture) = (21, [60, 45, 52], true);
            l
        };
        let mut last = at(2, 123, 90, 18, "country.");
        last.glyphs.bg = [33, 20, 33];
        let lines = [
            over_picture(at(666, 4, 188, 32, "Q.Load Skip Auto")),
            over_picture(at(0, 52, 826, 32, "To explain it in a way you'd understand: an athlete is someone who practices")),
            over_picture(at(0, 84, 822, 32, "a certain sport, and those who are among the best end up representing their")),
            last,
        ];
        let got: Vec<String> = paragraphs(&lines).into_iter().map(|b| b.text).collect();
        assert_eq!(got[1..], ["To explain it in a way you'd understand: an athlete is someone who practices a certain sport, and those who are among the best end up representing their country."]);
    }

    /// Reading order takes the two columns' lines in turn, row by row.
    #[test]
    fn columns_side_by_side_each_make_a_paragraph() {
        let lines = [
            l(0, 0, 200, "Create standalone project"),
            l(300, 0, 200, "Project Online PWA data is"),
            l(0, 24, 210, "files, or connect to share"),
            l(300, 24, 190, "saved to the SharePoint"),
            l(0, 48, 120, "your projects."),
            l(300, 48, 140, "content database."),
        ];
        let got: Vec<String> = paragraphs(&lines).into_iter().map(|b| b.text).collect();
        assert_eq!(got, ["Create standalone project files, or connect to share your projects.", "Project Online PWA data is saved to the SharePoint content database."]);
    }

    #[test]
    fn wrapped_lines_join_and_labels_stay_apart() {
        let wrapped = [l(0, 0, 500, "The quick update changes how"), l(0, 24, 300, "tasks are scheduled.")];
        let joined = paragraphs(&wrapped);
        assert_eq!(joined.len(), 1);
        assert_eq!(joined[0].text, "The quick update changes how tasks are scheduled.");
        let g = joined[0].glyphs;
        assert_eq!((g.x, g.y, g.w, g.h, joined[0].lines), (0, 0, 500, 42, 2));

        let menu = [l(0, 0, 60, "Sign in"), l(0, 24, 80, "Settings"), l(0, 48, 300, "About this application")];
        assert_eq!(paragraphs(&menu).len(), 3);

        let far = [l(0, 0, 500, "First paragraph ends here."), l(0, 48, 400, "Second one starts.")];
        assert_eq!(paragraphs(&far).len(), 2);

        let columns = [l(0, 0, 100, "Name"), l(400, 0, 100, "Value")];
        assert_eq!(paragraphs(&columns).len(), 2);

        // A lowercase start is no reason to join text onto a differently coloured panel.
        let mut code = l(0, 24, 300, "python");
        code.glyphs.bg = [35, 31, 27];
        assert_eq!(paragraphs(&[l(0, 0, 500, "pay more for the output."), code]).len(), 2);
    }

    #[test]
    fn chat_messages_starting_with_a_lowercase_name_stay_apart() {
        let chat = [
            l(0, 0, 240, "kai_88 hello from Canada!"),
            l(0, 30, 260, "eunbi 오늘도 방송 감사합니다"),
            l(0, 60, 200, "ana this game looks amazing"),
            l(0, 90, 350, "nova_k first time watching this game"),
            l(0, 120, 180, "luis the music is so good"),
        ];
        // The widest one still looks wrapped onto the next; the rest are each their own.
        let got: Vec<String> = paragraphs(&chat).into_iter().map(|b| b.text).collect();
        assert_eq!(got.len(), 4, "{got:?}");
        // Wrapped prose still joins, also where the next line is the widest.
        let prose = [l(0, 0, 480, "the update changes how tasks are scheduled"), l(0, 24, 500, "and how long each of them may run before it stops,"), l(0, 48, 120, "every time.")];
        assert_eq!(paragraphs(&prose).len(), 1);
    }

    #[test]
    fn an_icon_read_as_text_is_dropped() {
        assert_eq!(without_icon("② personal".into(), true), "personal");
        assert_eq!(without_icon("Q Search snippets".into(), true), "Search snippets");
        assert_eq!(without_icon("Projects".into(), true), "Projects");
        assert_eq!(without_icon("A shortcut".into(), false), "A shortcut");
    }

    #[test]
    fn font_size_allows_for_the_letters_in_the_line() {
        assert_eq!(font_size(14, "Sign in").round(), 15.0);
        assert_eq!(font_size(11, "HELLO").round(), 15.0);
        assert_eq!(font_size(8, "same").round(), 15.0);
        // A game's 23 px dialogue: lines without a g or y, and a short last line.
        assert_eq!(font_size(19, "No need to look for me, Taka-kun.").round(), 23.0);
        assert_eq!(font_size(19, "What am I, then?").round(), 23.0);
        assert_eq!(font_size(21, "person!").round(), 23.0);
        assert_eq!(font_size(21, "Did you sleep well?").round(), 23.0);
    }

    #[test]
    fn code_is_told_from_prose() {
        assert!(looks_like_code("while not done: action = llm(context) result = run_tool(action) context += result"));
        assert!(looks_like_code("result = run_tool(action)"));
        assert!(!looks_like_code("Settings (Beta)"));
        assert!(!looks_like_code("Save (Ctrl+S)"));
        assert!(!looks_like_code("Tool calling lets a model request a function in a predictable shape (mostly)."));
        assert!(!looks_like_code("error: cannot borrow `x` as mutable because it is also borrowed as immutable"));
    }

    #[test]
    fn hyphenated_wrap_rejoins_the_word() {
        let lines = [l(0, 0, 500, "this is an inter-"), l(0, 24, 300, "national update")];
        assert_eq!(paragraphs(&lines)[0].text, "this is an international update");
    }

    #[test]
    fn same_row_reads_left_to_right() {
        let mut lines = [l(300, 3, 100, "right"), l(0, 0, 100, "left"), l(0, 30, 100, "below")];
        reading_order(&mut lines);
        let order: Vec<_> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(order, ["left", "right", "below"]);
    }

    #[test]
    fn a_menu_bar_is_cut_where_its_letters_are_far_apart() {
        // Letters 0.02 apart: "Q.Load Skip" a little wider than a space, then two wide gaps.
        let word = |start: f32, letters: usize| (start, start + 0.02 * (letters - 1) as f32, letters);
        let menu = [word(0.0, 6), word(0.15, 4), word(0.36, 4), word(0.58, 5)];
        let cuts = wide_gaps(&menu);
        assert_eq!(cuts.len(), 2, "{cuts:?}");
        assert!(cuts[0] > 0.21 && cuts[0] < 0.36 && cuts[1] > 0.42 && cuts[1] < 0.58, "{cuts:?}");
        // Prose, and justified prose whose spaces are wide: whole.
        let prose = [word(0.0, 4), word(0.1, 3), word(0.18, 5), word(0.3, 2)];
        assert!(wide_gaps(&prose).is_empty());
        let justified = [word(0.0, 4), word(0.12, 3), word(0.22, 5), word(0.36, 2)];
        assert!(wide_gaps(&justified).is_empty());
        // A game's menu, as read: items two and a half to five and a half letters apart.
        let game = [(0.054, 0.177, 6), (0.254, 0.377, 6), (0.454, 0.531, 4), (0.669, 0.746, 4), (0.869, 0.962, 5)];
        assert_eq!(wide_gaps(&game).len(), 2, "Skip | Auto | Prefs");
        let game = [(0.042, 0.178, 6), (0.263, 0.381, 6), (0.449, 0.534, 4), (0.669, 0.754, 4), (0.873, 0.975, 5)];
        assert_eq!(wide_gaps(&game).len(), 2, "Skip | Auto | Prefs");
    }

    #[test]
    fn ctc_collapses_repeats_and_blanks() {
        let chars = ["", "a", "b"];
        #[rustfmt::skip]
        let probs = [
            0.1, 0.8, 0.1, // a
            0.1, 0.8, 0.1, // repeat
            0.9, 0.0, 0.1, // blank
            0.1, 0.8, 0.1, // a again
            0.0, 0.1, 0.9, // b
        ];
        assert_eq!(ctc_decode(&probs, 3, &chars).0, "aab");
    }

    #[test]
    fn doubt_shows_in_the_score() {
        assert!(ctc_decode(&[0.3, 0.4, 0.3], 3, &["", "a", "b"]).1 < TEXT_SCORE);
    }

    /// Boxes are taller than the glyphs, so a blank line looks smaller than a line height.
    #[test]
    fn blank_line_separates_paragraphs() {
        let ocr = Ocr::new(4, &[]).unwrap();
        let (bgra, w, h) = png_bgra("testdata/paragraphs.png");
        let lines = ocr.read(&bgra, w, h).unwrap();
        let got = paragraphs(&lines);
        assert_eq!(got.len(), 4, "{got:#?}");
        assert_eq!(got[3].text, "Press any key to continue");
    }

    #[test]
    fn bold_is_recognised_at_heading_and_body_sizes() {
        let ocr = Ocr::new(4, &[]).unwrap();
        let (bgra, w, h) = png_bgra("testdata/weights.png");
        let blocks = paragraphs(&ocr.read(&bgra, w, h).unwrap());
        let got: Vec<_> = blocks
            .iter()
            .map(|b| (b.text.as_str(), b.glyphs.bold()))
            .collect();
        assert_eq!(
            got,
            [
                ("First the problem Jev is solving", true),
                ("First the problem Jev is solving", false),
                ("Tool calling lets a model request a function", true),
                ("Tool calling lets a model request a function", false),
                ("Press any key to continue", false),
            ]
        );
    }

    /// Video frames are noisy, and once brought the seam search down to a line of no height.
    #[test]
    fn noisy_frames_never_panic() {
        let ocr = Ocr::new(4, &[]).unwrap();
        let (clean, w, h) = png_bgra("testdata/paragraphs.png");
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..24 {
            let mut noisy = clean.clone();
            for _ in 0..round * 400 {
                let (x, y) = ((next() % w as u64) as usize, (next() % h as u64) as usize);
                let (dw, dh) = ((next() % 40 + 1) as usize, (next() % 6 + 1) as usize);
                let shade = (next() % 256) as u8;
                for row in y..(y + dh).min(h) {
                    for col in x..(x + dw).min(w) {
                        noisy[(row * w + col) * 4..][..3].fill(shade);
                    }
                }
            }
            let lines = ocr.read(&noisy, w, h).unwrap();
            for block in paragraphs(&lines) {
                let patch = patch::Patch::new(&noisy, w, h, &block, "訳".into());
                assert!(patch.w > 0 && patch.h > 0);
            }
        }
    }

    /// The same renders RapidOCR read without a single mistake.
    #[test]
    fn reads_screen_samples() {
        const WANT: &str = "The quick update changes how background tasks are scheduled. If your session expires, \
            you'll need to sign in again before saving. Don't close this window until the upload has finished, \
            otherwise the file may be incomplete.";
        let ocr = Ocr::new(4, &[]).unwrap();
        for name in ["dark-14px", "game-22px", "mono-13px", "ui-12px", "ui-14px", "web-16px"] {
            let (bgra, w, h) = png_bgra(&format!("testdata/{name}.png"));
            let start = std::time::Instant::now();
            let lines = ocr.read(&bgra, w, h).unwrap();
            let elapsed = start.elapsed();
            let got = paragraphs(&lines).iter().map(|b| b.text.as_str()).collect::<Vec<_>>().join(" / ");
            println!("{name}: {elapsed:?} {got}");
            assert_eq!(got, WANT, "{name}");
        }
    }

    #[test]
    fn reads_hangul_with_its_own_model() {
        let (bgra, w, h) = png_bgra("testdata/korean.png");
        let lines = with_korean().read(&bgra, w, h).unwrap();
        let got = paragraphs(&lines).iter().map(|b| b.text.as_str()).collect::<Vec<_>>().join(" / ");
        println!("{got}");
        assert!(got.contains("이 프로그램은 화면의 글자를 번역합니다") && got.contains("언어를 바꿀 수 있습니다"), "{got}");
    }

    #[test]
    fn a_menu_bar_reads_as_one_label_per_item() {
        let ocr = Ocr::new(4, &[]).unwrap();
        let (bgra, w, h) = png_bgra("testdata/menu.png");
        let got: Vec<String> = paragraphs(&ocr.read(&bgra, w, h).unwrap()).into_iter().map(|b| b.text).collect();
        assert_eq!(got, ["Back", "Save", "Q.Save", "Q.Load", "Skip", "Auto", "Prefs", "Save your work before you close the window."]);
    }

    /// A game's dialogue in a monospaced pixel font, where every space is as wide as a menu's gaps.
    #[test]
    fn a_sentence_in_a_pixel_font_stays_whole() {
        let ocr = Ocr::new(4, &[]).unwrap();
        let (bgra, w, h) = png_bgra("testdata/pixel-mono.png");
        let got: Vec<String> = paragraphs(&ocr.read(&bgra, w, h).unwrap()).into_iter().map(|b| b.text).collect();
        assert_eq!(got, ["Mira", "A little rest never hurt anyone, quite the contrary! You should stay by the fire today."]);
    }

    /// A monospaced font sets "It" as far from the next word as an icon stands from its label, and
    /// a line of two words has few letters to tell its grid by.
    #[test]
    fn short_words_in_a_monospaced_font_are_neither_icons_nor_labels() {
        let ocr = Ocr::new(4, &[]).unwrap();
        let (bgra, w, h) = png_bgra("testdata/mono-short.png");
        let got: Vec<String> = paragraphs(&ocr.read(&bgra, w, h).unwrap()).into_iter().map(|b| b.text).collect();
        assert_eq!(got, ["It is still a little chilly in the hall.", "I see.", "Why me?"]);
    }

    #[test]
    fn labels_in_a_row_share_size_and_weight() {
        let block = |x, (y, h), em, bold, fg| {
            let glyphs = Glyphs { x, y, w: 40, h, letters: h, bg: [40, 120, 40], fg, stem: 1, icon: false, picture: false };
            Block { glyphs, lines: 1, em, bold, text: "x".into() }
        };
        let (white, black, label, heading) = ([255; 3], [0; 3], (0, 14), (-6, 22));
        let mut row = [
            block(0, label, 17, true, white),
            block(50, label, 13, false, white),
            block(100, label, 17, true, white),
            block(150, label, 12, false, black),
            block(200, heading, 19, true, white),
        ];
        match_rows(&mut row);
        let got: Vec<_> = row.iter().map(|b| (b.em, b.bold)).collect();
        assert_eq!(got, [(17, true), (17, true), (17, true), (12, false), (19, true)]);
    }

    /// A settings screen's headings in a script font, one under another; a speaker's name over a
    /// line of dialogue in the same colour is no column of labels.
    #[test]
    fn labels_in_a_column_share_size() {
        let block = |x, y, em| {
            let glyphs = Glyphs { x, y, w: 120, h: 44, letters: 30, bg: [60, 40, 90], fg: [255; 3], stem: 2, icon: false, picture: true };
            Block { glyphs, lines: 1, em, bold: false, text: "x".into() }
        };
        let mut column = [block(165, 242, 41), block(166, 387, 38), block(160, 500, 47), block(150, 613, 42)];
        match_columns(&mut column);
        assert_eq!(column.map(|b| b.em), [42, 42, 42, 42]);
        let mut talk = [block(20, 0, 28), block(22, 50, 23), block(20, 100, 23)];
        match_columns(&mut talk);
        assert_eq!(talk.map(|b| b.em), [28, 23, 23]);
    }

    fn with_korean() -> Ocr {
        Ocr::new(4, &[(&SCRIPTS[0], PathBuf::from("models").join(SCRIPTS[0].file()))]).unwrap()
    }

    #[test]
    fn the_korean_download_leaves_other_languages_alone() {
        let ocr = with_korean();
        for (name, want) in [("mixed-en", "Please read the manual"), ("mixed-ja", "取扱説明書をよく読んでください"), ("mixed-zh", "请仔细阅读说明书")] {
            let (bgra, w, h) = png_bgra(&format!("testdata/{name}.png"));
            let got = paragraphs(&ocr.read(&bgra, w, h).unwrap()).iter().map(|b| b.text.clone()).collect::<Vec<_>>().join(" / ");
            println!("{name}: {got}");
            assert!(got.contains(want), "{name}: {got}");
            assert!(!got.chars().any(|c| SCRIPTS[0].has(c)), "{name}: {got}");
        }
        let (bgra, w, h) = png_bgra("testdata/ui-14px.png");
        let got = paragraphs(&ocr.read(&bgra, w, h).unwrap()).iter().map(|b| b.text.clone()).collect::<Vec<_>>().join(" ");
        assert!(!got.chars().any(|c| SCRIPTS[0].has(c)), "{got}");
    }

    #[test]
    fn wrapped_japanese_takes_no_space() {
        let line = |y: i32, text: &str| Line { glyphs: Glyphs { x: 0, y, w: 400, h: 20, letters: 20, bg: [255; 3], fg: [0; 3], stem: 2, icon: false, picture: false }, text: text.to_string() };
        let blocks = paragraphs(&[line(0, "取扱説明書をよく読んで、"), line(26, "正しく使ってください。")]);
        assert_eq!(blocks[0].text, "取扱説明書をよく読んで、正しく使ってください。");
    }

    #[test]
    fn arabic_is_put_back_in_reading_order() {
        assert_eq!(logical_order("2026 ابحرم"), "مرحبا 2026");
        assert_eq!(logical_order("ابت hello world مالس"), "سلام hello world تبا");
    }

    fn script(id: &str) -> &'static Script {
        SCRIPTS.iter().find(|s| s.id == id).unwrap()
    }

    /// Guesses as these models made them at a test image in a script they cannot read.
    #[test]
    fn another_script_wins_only_with_its_own_letters() {
        assert!(script("cyrillic").wrote("Язык можно изменить", 0.6));
        assert!(!script("cyrillic").wrote("Mnopeite va alξEtE tn", 0.95));
        assert!(!script("th").wrote("0| ETEe ๒ 2XM ษๆอ.", 0.6));
        assert!(script("korean").wrote("Windows 설정", 0.95));
        assert!(!script("korean").wrote("Windows 설정", 0.6));
    }

    fn text_of(ocr: &Ocr, name: &str) -> String {
        let (bgra, w, h) = png_bgra(&format!("testdata/{name}.png"));
        paragraphs(&ocr.read(&bgra, w, h).unwrap()).iter().map(|b| b.text.clone()).collect::<Vec<_>>().join(" / ")
    }

    /// Needs every script's model in the folder OCR_MODELS names, as the settings window saves them.
    #[test]
    #[ignore]
    fn reads_the_scripts_that_come_as_downloads() {
        let dir = PathBuf::from(std::env::var("OCR_MODELS").expect("OCR_MODELS"));
        let all: Vec<_> = SCRIPTS.iter().map(|s| (s, if s.id == "korean" { PathBuf::from("models") } else { dir.clone() }.join(s.file()))).collect();
        let ocr = Ocr::new(4, &all).unwrap();
        for (name, want) in [
            ("russian", "Эта программа переводит текст на экране. Язык можно изменить в настройках."),
            ("arabic", "هذا البرنامج يترجم النص على الشاشة"),
            ("thai", "โปรแกรมนี้แปลข้อความบนหน้าจอ / คุณสามารถเปลี่ยนภาษาได้ในการตั้งค่า"),
            ("hindi", "का अनुवाद करता है।"),
            ("tamil", "அமைப்புகளில் மொழியை மாற்றலாம்."),
            ("telugu", "తెరపై ఉన్న వచనాన్ని అనువదిస్తుంది."),
            ("korean", "화면의 글자를 번역합니다"),
            ("mixed-en", "Please read the manual before you start the machine."),
            ("mixed-ja", "使う前に取扱説明書をよく読んでください。"),
            ("mixed-zh", "使用前请仔细阅读说明书。"),
        ] {
            let got = text_of(&ocr, name);
            println!("{name}: {got}");
            assert!(got.contains(want), "{name}: {got}");
        }
        assert_eq!(text_of(&ocr, "ui-14px"), text_of(&Ocr::new(4, &[]).unwrap(), "ui-14px"));
    }
}




