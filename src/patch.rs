//! What gets drawn over one paragraph: the translation in the original's colours, on a
//! rectangle that covers the original glyphs and nothing else.

use crate::ocr::Block;
use std::collections::HashMap;
use std::sync::Arc;

/// Overlay pixels of exactly this colour are see-through. COLORREF, as are `bg` and `fg`.
pub const KEY_COLOR: u32 = 0x00FF00FF;
const GLYPH_DISTANCE: i32 = 64; // how far from the background a pixel must be to count as text
const MARGIN: i32 = 2; // covers the antialiased fringe outside the glyphs
const GLYPH_STALE_PERCENT: usize = 30; // of the text's edges; compression noise moves a few
pub const WOBBLE_PERCENT: usize = 2; // edges that change without the text changing, around a thin outline
const EDGE_COLUMNS: usize = 2;
const EDGE_PX: usize = 2; // a text edge rises within this many pixels, most of a video's do not
const LETTER_FRINGE: usize = 2; // antialiasing around the letters goes with them
const GLOW_PX: usize = 4; // how far a glow or outline reaches from small letters
const GLOW_PER_HEIGHT: usize = 8; // and from big ones: an eighth of the patch's height
const SMOOTHING: usize = 4; // passes over a painted-out letter, so that no outline of it shows
const SAME_BG: i32 = 30;
const SAME_FG: i32 = 80; // thin text rarely reaches its true colour, so allow for that
const BOLD_STEM: usize = 8; // a stem wider than 1/8 of the line height, less one antialiased pixel
const MIN_BOLD_STEM: usize = 4; // a regular stem in small text is one pixel and its two antialiased edges
const STATUS_BG: u32 = 0x00201C1C;
const STATUS_FG: u32 = 0x00F5F5F5;
const MOVED_TENTHS: usize = 9; // of a patch's rows found again where it moved to
const MIN_SCROLL_ROWS: usize = 8;
const MONOSPACED_RUNS: usize = 5; // stretches of ink it takes to tell a grid of cells from chance
const MONOSPACED_SLACK: f32 = 0.15; // of a cell: how far off the grid a letter's middle may sit
const MONOSPACED_FEW: usize = 7; // fewer stretches than this must each be one letter: runs of them fit a grid too easily
const MONOSPACED_STRAYS: usize = 16; // one stretch in so many may sit off the grid, as a quote mark's halves do

/// The ink of one text line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Glyphs {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub letters: i32, // the height of the letters themselves: less than `h` when a glow or outline surrounds them
    pub bg: [i32; 3],
    pub fg: [i32; 3],
    pub stem: usize, // median width of a stretch of ink: most rows cross vertical stems
    pub icon: bool,  // an icon sat before the text and was left out of this rectangle
    pub picture: bool, // no common colour behind the text: a photo, a gradient, a video
}

impl Glyphs {
    pub fn bold(&self) -> bool {
        self.stem >= MIN_BOLD_STEM && self.stem.saturating_sub(1) as i32 * BOLD_STEM as i32 > self.letters
    }

    /// True when two lines sit on the same background in the same ink.
    pub fn same_style(&self, other: &Glyphs) -> bool {
        distance(self.bg, other.bg) <= SAME_BG && self.same_fg(other)
    }

    pub fn same_fg(&self, other: &Glyphs) -> bool {
        distance(self.fg, other.fg) <= SAME_FG
    }
}

/// The picture behind a patch with the letters painted out, as BGRA rows.
#[derive(Debug, PartialEq)]
pub struct Backdrop {
    pub w: i32,
    pub h: i32,
    pub bgra: Vec<u8>,
    under: Vec<[i32; 3]>, // the picture it was painted from, letters and all
}

#[derive(Debug, Clone, PartialEq)]
pub struct Patch {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub max_w: i32, // how far a single line may spread over empty background
    pub max_h: i32, // how far a paragraph may grow down over empty background
    pub one_line: bool, // the original is one line, so the translation narrows before it wraps
    pub h: i32,     // 0 until laid out for a status message
    pub em: i32,    // the original's font size, which the translation tries to keep
    pub bg: u32,
    pub fg: u32,
    pub bold: bool,
    pub backdrop: Option<Arc<Backdrop>>, // shown instead of the flat `bg`
    pub seen: Arc<Seen>, // the text as it was read, to tell when it has gone
    pub text: String,
    pub source: String, // what OCR read
    pub px: i32,     // font size, set when laid out on the UI thread
    pub narrow: i32, // letter width in percent of the font's own, ditto
    pub text_h: i32, // measured height, ditto
}

/// The pixels under a patch when its text was read. Compared with the screen before it, a text
/// that fades out with the screen would never seem to go: each look sees it only a little fainter.
#[derive(Debug, Default, PartialEq)]
pub struct Seen {
    x: usize,
    y: usize,
    w: usize,
    bgra: Vec<u8>,
}

impl Seen {
    fn of(bgra: &[u8], width: usize, (x, y, w, h): (i32, i32, i32, i32)) -> Arc<Self> {
        let height = bgra.len() / 4 / width.max(1);
        let (x0, y0) = (x.clamp(0, width as i32) as usize, y.clamp(0, height as i32) as usize);
        let (x1, y1) = ((x + w + EDGE_PX as i32).clamp(0, width as i32) as usize, (y + h).clamp(0, height as i32) as usize);
        let rows = (y0..y1).flat_map(|row| bgra[(row * width + x0) * 4..(row * width + x1) * 4].iter().copied());
        Arc::new(Seen { x: x0, y: y0, w: x1 - x0, bgra: rows.collect() })
    }

    fn pixel(&self, x: usize, y: usize) -> Option<[i32; 3]> {
        let (col, row) = (x.checked_sub(self.x)?, y.checked_sub(self.y)?);
        let p = self.bgra.get((row * self.w + col) * 4..)?.get(..3).filter(|_| col < self.w)?;
        Some([p[0] as i32, p[1] as i32, p[2] as i32])
    }
}

fn distance(a: [i32; 3], b: [i32; 3]) -> i32 {
    (0..3).map(|c| (a[c] - b[c]).abs()).sum()
}

fn pixel(bgra: &[u8], width: usize, x: usize, y: usize) -> [i32; 3] {
    let p = &bgra[(y * width + x) * 4..][..3];
    [p[0] as i32, p[1] as i32, p[2] as i32]
}

/// Finds the text line inside a detector box. The box is taller than the text and may reach
/// into the lines above and below or past a button's edge, so only the band of ink rows at
/// its middle counts, and the background comes from that middle too. Field borders and a
/// leading icon are left out, so that neither is painted over or mistaken for tall text.
/// `text` is what the recogniser read, for telling a short first word from an icon; without it
/// icons are not looked for.
pub fn glyphs(bgra: &[u8], width: usize, height: usize, (x, y, w, h): (i32, i32, i32, i32), text: Option<&str>) -> Option<Glyphs> {
    let x0 = x.clamp(0, width as i32) as usize;
    let y0 = y.clamp(0, height as i32) as usize;
    let x1 = (x + w).clamp(0, width as i32) as usize;
    let y1 = (y + h).clamp(0, height as i32) as usize;
    if x0 >= x1 || y0 >= y1 {
        return None;
    }

    // The commonest colour is the background: glyphs are thin strokes on it.
    let quarter = (y1 - y0) / 4;
    let middle = y0 + quarter..y1 - quarter;
    let mut bins: HashMap<u16, [u64; 4]> = HashMap::new();
    for row in middle.clone() {
        for col in x0..x1 {
            let [b, g, r] = pixel(bgra, width, col, row);
            let bin = bins.entry(((b >> 3) << 10 | (g >> 3) << 5 | r >> 3) as u16).or_default();
            *bin = [bin[0] + 1, bin[1] + b as u64, bin[2] + g as u64, bin[3] + r as u64];
        }
    }
    let [n, b, g, r] = *bins.values().max_by_key(|bin| bin[0])?;
    let mut bg = [(b / n) as i32, (g / n) as i32, (r / n) as i32];
    // A picture has no common colour, so the letters of a thick subtitle can come first. The
    // box reaches past the text, so its two ends are the background then. Bold letters packed
    // tight can outnumber a flat background too, but then the box's whole edge, ends, top and
    // bottom, is that colour; a patch of one colour in a picture rarely runs all round.
    let ends: Vec<usize> = (x0..x1).take(EDGE_COLUMNS).chain((x0..x1).rev().take(EDGE_COLUMNS)).collect();
    let around: Vec<[i32; 3]> = middle.clone().flat_map(|row| ends.iter().map(move |&col| (col, row))).map(|(col, row)| pixel(bgra, width, col, row)).collect();
    let edge: Vec<[i32; 3]> = (x0..x1).flat_map(|col| [(col, y0), (col, y1 - 1)]).map(|(col, row)| pixel(bgra, width, col, row)).chain(around.iter().copied()).collect();
    let flat_edge = edge.iter().filter(|&&p| distance(p, bg) <= SAME_BG).count() * 10 >= edge.len() * 9; // a see-through box over a scene varies more
    let picture = n * 2 < (middle.len() * (x1 - x0)) as u64 && !flat_edge;
    if picture {
        bg = [0, 1, 2].map(|c| around.iter().map(|p| p[c]).sum::<i32>() / around.len().max(1) as i32);
    }
    let is_ink = |col: usize, row: usize| distance(pixel(bgra, width, col, row), bg) > GLYPH_DISTANCE;

    // The box is much taller than any letter, so a column of ink in most rows is a rule.
    let rows = y1 - y0;
    let is_rule = |col: usize| !picture && (y0..y1).filter(|&row| is_ink(col, row)).count() * 10 >= rows * 7;
    let mut cols: Vec<usize> = (x0..x1).filter(|&col| !is_rule(col)).collect();

    let band = |cols: &[usize]| {
        let (mut found, mut best, mut run_top, mut blank) = (None, i32::MIN, None, 0);
        for row in y0..=y1 {
            // The curve of a rounded field leaves a pixel or two per row; letters leave more.
            if row < y1 && cols.iter().filter(|&&col| is_ink(col, row)).take(3).count() == 3 {
                run_top.get_or_insert(row);
                blank = 0;
                continue;
            }
            blank += 1; // one blank row can be the gap under the dot of an i
            if let Some(start) = run_top.filter(|_| blank > 1 || row == y1) {
                let end = row + 1 - blank;
                let in_middle = end.min(middle.end) as i32 - start.max(middle.start) as i32;
                if in_middle > best {
                    (found, best) = (Some((start, end)), in_middle);
                }
                run_top = None;
            }
        }
        found
    };
    let (mut top, mut bottom) = band(&cols)?;

    // An icon before the text: a blob about as wide as the line is tall, then a wide gap, and
    // not the width that the first word read from the box would have. A field's edge can
    // come before the icon, so look more than once. A full stop can leave a gap inside a word,
    // as in "Q.Save", so the blob is held against the word up to it.
    let first = text.and_then(|text| text.split(' ').next()).unwrap_or("");
    let first = first.split_inclusive('.').next().unwrap_or(first);
    let mut icon = false;
    // A monospaced font sets a short first word as far from the next as an icon stands from its
    // label, and its letters can be as wide as the line is tall: their grid tells them apart.
    let stretches = runs(cols.iter().filter(|&&col| (top..bottom).any(|row| is_ink(col, row))).map(|&col| col as i32));
    let monospaced = text.is_some_and(|text| fixed_pitch(&stretches, (bottom - top) as i32, text.chars().count()).is_some());
    while text.is_some() && !monospaced {
        let inked: Vec<usize> = cols.iter().copied().filter(|&col| (top..bottom).any(|row| is_ink(col, row))).collect();
        let tall = bottom - top;
        // A Hangul syllable is about as wide as the line is tall, twice a Latin letter, and Korean
        // puts spaces between words, so a first word of one syllable is common.
        let letters: usize = first.chars().map(|c| if matches!(c, '\u{AC00}'..='\u{D7A3}') { 2 } else { 1 }).sum();
        let word_w = letters * tall * 9 / 20;
        let is_word = |w: usize| first.trim_end_matches('.').chars().all(char::is_alphanumeric) && w * 2 >= word_w && w * 10 <= word_w * 17;
        let gap = inked.windows(2).position(|pair| (pair[1] - pair[0]) * 5 > tall * 3);
        let Some(i) = gap.filter(|&i| (inked[i] - inked[0]) * 2 <= tall * 3 && !is_word(inked[i] - inked[0] + 1)) else { break };
        cols.retain(|&col| col > inked[i]);
        (top, bottom) = band(&cols)?;
        icon = true;
    }

    // A lone ascender or descender falls short of the three pixels asked of a row above.
    let reach = (bottom - top) / 2;
    for _ in 0..reach {
        if top > y0 && cols.iter().any(|&col| is_ink(col, top - 1)) {
            top -= 1;
        }
        if bottom < y1 && cols.iter().any(|&col| is_ink(col, bottom)) {
            bottom += 1;
        }
    }

    // Marks a few blank rows off the letters, like the Thai vowels written under a consonant:
    // a short band of sparse ink with a blank row past it. The next line's letters are denser,
    // or run on to the box's edge. Marks between two lines go with the nearer one.
    let row_ink = |row: usize| cols.iter().filter(|&&col| is_ink(col, row)).count();
    let tall = bottom - top;
    let peak = (top..bottom).map(row_ink).max().unwrap_or(0);
    let gap = tall / 6 + 2;
    let marks = |rows: std::ops::Range<usize>| rows.len() * 3 <= tall && rows.clone().all(|row| row_ink(row) * 4 <= peak);
    if let Some(start) = (bottom..(bottom + gap).min(y1)).find(|&row| row_ink(row) > 0) {
        if let Some(end) = (start..y1).find(|&row| row_ink(row) == 0) {
            let beyond = (end..y1).find(|&row| row_ink(row) > 0).map_or(usize::MAX, |row| row - end);
            if marks(start..end) && beyond >= start - bottom {
                bottom = end;
            }
        }
    }
    if let Some(end) = (top.saturating_sub(gap).max(y0)..top).rev().find(|&row| row_ink(row) > 0).map(|row| row + 1) {
        if let Some(start) = (y0..end).rev().find(|&row| row_ink(row) == 0).map(|row| row + 1) {
            let beyond = (y0..start).rev().find(|&row| row_ink(row) > 0).map_or(usize::MAX, |row| start - 1 - row);
            if marks(start..end) && beyond >= top - end {
                top = start;
            }
        }
    }

    // Two lines with no blank row between them: a row far thinner than the solid rows on both
    // sides of it is the seam, and the line is the part holding the middle of the box.
    let ink: Vec<usize> = (top..bottom).map(|row| cols.iter().filter(|&&col| is_ink(col, row)).count()).collect();
    let peak = *ink.iter().max()?;
    let solid = |n: &usize| n * 2 >= peak;
    let (first, last) = (top + ink.iter().position(solid)?, top + ink.iter().rposition(solid)?);
    let centre = (middle.start + middle.end) / 2;
    let base = top;
    for row in (first..last).filter(|&row| ink[row - base] * 8 < peak) {
        if row < centre {
            top = row + 1;
        } else {
            bottom = row;
            break;
        }
    }

    if top >= bottom {
        return None; // the seam search can leave nothing between two seams
    }

    let (mut left, mut right) = (x1, x0);
    let (mut fg, mut farthest) = (bg, 0);
    let mut runs = [0u32; 32];
    for row in top..bottom {
        let mut run = 0;
        for &col in &cols {
            let p = pixel(bgra, width, col, row);
            let far = distance(p, bg);
            if far > GLYPH_DISTANCE {
                (left, right) = (left.min(col), right.max(col));
                run += 1;
            } else if run > 0 {
                runs[run.min(31)] += 1;
                run = 0;
            }
            // The letters fill the box's middle; its top and bottom can catch a bright speck of a
            // picture behind a short label.
            if far > farthest && middle.contains(&row) {
                (fg, farthest) = (p, far);
            }
        }
    }
    if right < left {
        return None;
    }
    // Letters with a glow or an outline, as games draw them: the halo is far from the background
    // too, but nearer to it than to the letters. The letters alone give the size and the stems.
    let h = (bottom - top) as i32;
    let letter = |col: usize, row: usize| is_letter(pixel(bgra, width, col, row), fg, bg, picture);
    // Bands of rows with letters in them. The densest is the letters; one past a gap counts only
    // if it is about as tall as the gap, as the dot of an i is and a speck of the picture is not,
    // and stops short of the box's edge, which cuts through the picture but not through letters.
    let counts: Vec<usize> = (top..bottom).map(|row| cols.iter().filter(|&&col| letter(col, row)).count()).collect();
    let mut bands: Vec<(usize, usize)> = Vec::new();
    for (i, _) in counts.iter().enumerate().filter(|(_, n)| **n > 0) {
        match bands.last_mut() {
            Some(run) if run.1 == i => run.1 += 1,
            _ => bands.push((i, i + 1)),
        }
    }
    let densest = counts.iter().enumerate().max_by_key(|(_, n)| **n).map_or(0, |(i, _)| i);
    let letters = match bands.iter().position(|r| r.0 <= densest && densest < r.1) {
        Some(main) => {
            let apart = |(start, end): (usize, usize), gap: usize| (end - start) * 2 < gap || top + start == y0 || top + end == y1;
            let (mut a, mut b) = (main, main);
            while a > 0 && !apart(bands[a - 1], bands[a].0 - bands[a - 1].1) {
                a -= 1;
            }
            while b + 1 < bands.len() && !apart(bands[b + 1], bands[b + 1].0 - bands[b].1) {
                b += 1;
            }
            (bands[b].1 - bands[a].0) as i32
        }
        None => 0,
    };
    // Over a picture the background's own pixels pass for ink, and a tight box can cut a glow
    // off so that little of it is left: there the letters' own colour gives the measure.
    let glow = letters > 0 && (letters * 8 < h * 7 || picture && letters < h);
    if glow {
        runs = [0; 32];
        for row in top..bottom {
            let mut run = 0;
            for &col in &cols {
                if letter(col, row) {
                    run += 1;
                } else if run > 0 {
                    runs[run.min(31)] += 1;
                    run = 0;
                }
            }
        }
    }
    let half = runs.iter().sum::<u32>() / 2;
    let mut seen = 0;
    let stem = runs.iter().position(|&n| { seen += n; seen > half }).unwrap_or(0);
    let (x, y) = (left as i32, top as i32);
    Some(Glyphs { x, y, w: right as i32 + 1 - x, h, letters: if glow { letters } else { h }, bg, fg, stem, icon, picture })
}

/// A pixel of the letters themselves, not of a glow around them. Over a picture, bright patches
/// of it (hair, a sky) can sit nearer white letters than the picture's average does, so there a
/// letter's pixel must be close to the letters' colour as well.
fn is_letter(p: [i32; 3], fg: [i32; 3], bg: [i32; 3], picture: bool) -> bool {
    distance(p, fg) < distance(p, bg) && (!picture || distance(p, fg) <= SAME_FG)
}

/// The detector box cut at blank gaps at least one and a half letters wide and well over the
/// line's usual space between words: a menu bar or the cells of a table, read as one line, are
/// each their own label. `text` is what the line was read as: the number of its letters gives
/// their width; their height can take in a picture's bright patches. A monospaced comma and space
/// come close to the first test, so the second keeps prose whole. A monospaced font's thin
/// letters, as in a game's pixel font, leave every space that wide: there a gap must be two
/// cells wide. Each piece reaches halfway into the gaps on either side. A glow around the
/// letters counts as blank.
pub fn split_at_gaps(bgra: &[u8], width: usize, height: usize, rect: (i32, i32, i32, i32), text: &str) -> Vec<(i32, i32, i32, i32)> {
    let chars = text.chars().filter(|c| !c.is_whitespace()).count();
    let Some(g) = glyphs(bgra, width, height, rect, None).filter(|_| chars > 1) else { return vec![rect] };
    // Without a glow, a thin stem's few pixels can sit nearer the background than the ink.
    let glow = g.letters < g.h;
    let letter = |col: i32, row: i32| {
        let p = pixel(bgra, width, col as usize, row as usize);
        // A highlighted item can be another colour than the letters around it, so here any pixel
        // nearer the letters than the background counts, as long as it is not glow.
        if glow { distance(p, g.fg) < distance(p, g.bg) } else { distance(p, g.bg) > GLYPH_DISTANCE }
    };
    let inked = |col: i32| (g.y..g.y + g.h).any(|row| letter(col, row));
    let runs = runs((g.x..g.x + g.w).filter(|&col| inked(col)));
    let gaps: Vec<(i32, i32)> = runs.windows(2).map(|r| (r[0].1, r[1].0 - r[0].1)).collect(); // (start, width)
    let between_words: i32 = gaps.iter().map(|&(_, w)| w).filter(|&w| w * 5 >= g.letters).sum();
    let letter_w = (g.w - between_words) / chars as i32;
    let cell = fixed_pitch(&runs, g.letters, text.chars().count());
    let wide = |w: i32| w * 2 >= letter_w * 3 && cell.map_or(true, |cell| w as f32 >= cell * 2.0);
    let mut spaces: Vec<i32> = gaps.iter().map(|&(_, w)| w).filter(|&w| w * 5 >= g.letters && !wide(w)).collect();
    spaces.sort_unstable();
    let usual = spaces.get(spaces.len() / 2).copied().unwrap_or(0);
    let cuts = gaps.iter().filter(|&&(_, w)| wide(w) && w * 10 >= usual * 18).map(|&(start, w)| start + w / 2);
    let (x, y, w, h) = rect;
    let edges: Vec<i32> = std::iter::once(x).chain(cuts).chain(std::iter::once(x + w)).collect();
    edges.windows(2).map(|e| (e[0], y, e[1] - e[0], h)).collect()
}

/// Each stretch of neighbouring columns, from and to.
fn runs(cols: impl Iterator<Item = i32>) -> Vec<(i32, i32)> {
    let mut runs: Vec<(i32, i32)> = Vec::new();
    for col in cols {
        match runs.last_mut() {
            Some(run) if run.1 == col => run.1 = col + 1,
            _ => runs.push((col, col + 1)),
        }
    }
    runs
}

/// The width of a cell, if the stretches of ink sit on a grid of equal cells as a monospaced
/// font's letters do. `cells` is how many letters and spaces the line was read as: the ink spans
/// that many cells, less the blank sides of the first and the last. A stretch of two letters run
/// together sits astride a boundary between cells, one of three in the middle of a cell again.
/// `tall` is the height of the letters: a gap under a fifth of it lies between two of a word.
fn fixed_pitch(runs: &[(i32, i32)], tall: i32, cells: usize) -> Option<f32> {
    if runs.len() < MONOSPACED_RUNS || cells < 2 {
        return None;
    }
    let mut between_letters: Vec<i32> = runs.windows(2).map(|r| r[1].0 - r[0].1).filter(|&w| w * 5 < tall).collect();
    between_letters.sort_unstable();
    let between_letters = between_letters.get(between_letters.len() / 2).copied().unwrap_or(0);
    let span = (runs[runs.len() - 1].1 - runs[0].0) as f32;
    (0..=20).map(|side| span / (cells as f32 - side as f32 / 20.0)).find(|&cell| {
        let letters = |&(from, to): &(i32, i32)| ((to - from + between_letters) as f32 / cell).round().max(1.0);
        if runs.len() < MONOSPACED_FEW && runs.iter().any(|run| letters(run) > 1.0) {
            return false;
        }
        // Where in a cell each stretch's first letter has its middle, as a share of the cell.
        let middles: Vec<f32> = runs.iter().map(|run| (((run.0 + run.1) as f32 - (letters(run) - 1.0) * cell) / 2.0 / cell).rem_euclid(1.0)).collect();
        let apart = |a: f32, b: f32| (a - b).rem_euclid(1.0).min((b - a).rem_euclid(1.0));
        let together = middles.iter().map(|&m| middles.iter().filter(|&&other| apart(m, other) <= MONOSPACED_SLACK).count()).max().unwrap_or(0);
        runs.len() - together <= runs.len() / MONOSPACED_STRAYS
    })
}

/// Takes the letters off a picture and leaves the rest of it. The rows just outside the box say
/// what lies behind the text, and a pixel far from that is a letter, or its glow, if it is near
/// a pixel of the letters' colour: a dark sleeve beside a label is the picture.
fn paint_out_letters(bgra: &[u8], width: usize, height: usize, (x, y, w, h): (usize, usize, usize, usize), fg: [i32; 3]) -> Backdrop {
    let radius = h.max(8);
    // The middle value of each colour along the row: a big title's tall letters can cross the rows
    // just outside its box, and an average would carry their colour into the background.
    let smooth_row = |row: usize| -> Vec<[i32; 3]> {
        (x..x + w)
            .map(|col| {
                let span = col.saturating_sub(radius)..(col + radius + 1).min(width);
                let mut channels: [Vec<i32>; 3] = Default::default();
                for p in span.map(|c| pixel(bgra, width, c, row)) {
                    (0..3).for_each(|c| channels[c].push(p[c]));
                }
                channels.map(|mut v| {
                    let middle = v.len() / 2;
                    *v.select_nth_unstable(middle).1
                })
            })
            .collect()
    };
    let (above, below) = (smooth_row(y.saturating_sub(1)), smooth_row((y + h).min(height - 1)));
    // A border line or another panel can run right along the box, as under a settings screen's
    // title; a row unlike the box around that column gives way to the box's own middle colour
    // there: of each column's middle colour, the middle one nearby, as letters are thin.
    let middle_of = |mut v: Vec<i32>| {
        let middle = v.len() / 2;
        *v.select_nth_unstable(middle).1
    };
    let columns: Vec<[i32; 3]> = (x..x + w).map(|col| [0, 1, 2].map(|c| middle_of((y..y + h).map(|row| pixel(bgra, width, col, row)[c]).collect()))).collect();
    let typical: Vec<[i32; 3]> = (0..w).map(|col| [0, 1, 2].map(|c| middle_of(columns[col.saturating_sub(radius)..(col + radius + 1).min(w)].iter().map(|p| p[c]).collect()))).collect();
    let like_the_box = |p: [i32; 3], col: usize| if distance(p, typical[col]) > GLYPH_DISTANCE * 2 { typical[col] } else { p };
    let behind = |col: usize, row: usize| {
        let (a, b) = (like_the_box(above[col], col), like_the_box(below[col], col));
        let (down, up) = (row as i32 + 1, (h - row) as i32);
        [0, 1, 2].map(|c| (a[c] * up + b[c] * down) / (up + down))
    };

    // The colour taken for the letters can be their shadow's, so a thick stroke's far side is as
    // far from it as the stroke is wide.
    let reach = (h / GLOW_PER_HEIGHT).max(GLOW_PX);
    let ink: Vec<bool> = (0..w * h).map(|i| distance(pixel(bgra, width, x + i % w, y + i / w), fg) <= SAME_FG).collect();
    let mut letter = vec![false; w * h];
    for row in 0..h {
        for col in 0..w {
            let near = || (row.saturating_sub(reach)..(row + reach + 1).min(h)).any(|r| (col.saturating_sub(reach)..(col + reach + 1).min(w)).any(|c| ink[r * w + c]));
            letter[row * w + col] = distance(pixel(bgra, width, x + col, y + row), behind(col, row)) > GLYPH_DISTANCE && near();
        }
    }
    let grown: Vec<bool> = (0..w * h)
        .map(|i| {
            let (row, col) = (i / w, i % w);
            let rows = row.saturating_sub(LETTER_FRINGE)..(row + LETTER_FRINGE + 1).min(h);
            rows.flat_map(|r| (col.saturating_sub(LETTER_FRINGE)..(col + LETTER_FRINGE + 1).min(w)).map(move |c| r * w + c)).any(|j| letter[j])
        })
        .collect();

    let mut out: Vec<[i32; 3]> = (0..w * h)
        .map(|i| if grown[i] { behind(i % w, i / w) } else { pixel(bgra, width, x + i % w, y + i / w) })
        .collect();
    for _ in 0..SMOOTHING {
        for i in (0..w * h).filter(|&i| grown[i]) {
            let (row, col) = (i / w, i % w);
            let around = [i.wrapping_sub(1).min(i - (col > 0) as usize), (i + (col + 1 < w) as usize), i - (row > 0) as usize * w, i + (row + 1 < h) as usize * w];
            out[i] = [0, 1, 2].map(|c| around.iter().map(|&j| out[j][c]).sum::<i32>() / 4);
        }
    }
    let mut pixels = Vec::with_capacity(w * h * 4);
    for p in out {
        // The overlay's see-through colour would punch a hole.
        let g = if (p[2], p[1], p[0]) == (255, 0, 255) { 1 } else { p[1] as u8 };
        pixels.extend([p[0] as u8, g, p[2] as u8, 255]);
    }
    let under = (y..y + h).flat_map(|row| (x..x + w).map(move |col| (col, row))).map(|(col, row)| pixel(bgra, width, col, row)).collect();
    Backdrop { w: w as i32, h: h as i32, bgra: pixels, under }
}

impl Patch {
    pub fn new(bgra: &[u8], width: usize, height: usize, block: &Block, text: String) -> Self {
        let g = &block.glyphs;
        let x = (g.x - MARGIN).max(0);
        let y = (g.y - MARGIN).max(0);
        let w = (g.x + g.w + MARGIN).min(width as i32) - x;
        let h = (g.y + g.h + MARGIN).min(height as i32) - y;
        // Japanese often runs wider than the English; on one line, spread before shrinking.
        let rows = g.y as usize..(g.y + g.h) as usize;
        let empty = |col: usize| rows.clone().all(|row| distance(pixel(bgra, width, col, row), g.bg) <= GLYPH_DISTANCE);
        let free = ((g.x + g.w) as usize..width).find(|&col| !empty(col)).unwrap_or(width) as i32;
        let max_w = if block.lines == 1 { (free - MARGIN - x).max(w) } else { w };
        // A paragraph wraps at words, so its translation can take a line more than the original;
        // the gap under it, if it is plain background, takes that line, up to its middle so that
        // the next paragraph still stands apart.
        let blank_row = |row: usize| (g.x as usize..(g.x + g.w) as usize).all(|col| distance(pixel(bgra, width, col, row), g.bg) <= GLYPH_DISTANCE);
        let below = ((g.y + g.h) as usize..height).find(|&row| !blank_row(row)).unwrap_or(height) as i32;
        let max_h = if block.lines > 1 && !g.picture { (g.y + g.h + (below - g.y - g.h) / 2 - y).max(h) } else { h };
        let (bg, fg) = (colorref(g.bg), colorref(g.fg));
        let seen = Seen::of(bgra, width, (x, y, w, h));
        Self { x, y, w, max_w, max_h, one_line: block.lines == 1, h, em: block.em, bg, fg, bold: block.bold, backdrop: None, seen, text, source: block.text.clone(), px: 0, narrow: 100, text_h: 0 }
    }

    /// On a still picture the text goes without the picture going with it. A video would move on
    /// from whatever is painted here, so it keeps the flat box.
    /// It reaches as far as a translation may spread, so no letter is ever drawn over nothing:
    /// its antialiased edge would take on the overlay's see-through colour.
    pub fn keep_picture(mut self, bgra: &[u8], width: usize, height: usize) -> Self {
        let area = (self.x as usize, self.y as usize, self.w.max(self.max_w) as usize, self.h as usize);
        let fg = [(self.fg >> 16 & 0xFF) as i32, (self.fg >> 8 & 0xFF) as i32, (self.fg & 0xFF) as i32];
        self.backdrop = Some(Arc::new(paint_out_letters(bgra, width, height, area, fg)));
        self
    }

    /// How much of the original text has gone, in percent. Only its sharpest edges count: a video
    /// playing behind a subtitle can change every other pixel, even the subtitle's outline.
    pub fn changed_percent(&self, frame: &[u8], basis: &[u8], width: usize) -> usize {
        self.changed_from(frame, width, |x, y| Some(pixel(basis, width, x, y)))
    }

    /// How much of the text has gone since it was read.
    pub fn text_changed(&self, frame: &[u8], width: usize) -> usize {
        self.changed_from(frame, width, |x, y| self.seen.pixel(x, y))
    }

    /// True once the text read is no longer there.
    pub fn is_gone(&self, frame: &[u8], width: usize) -> bool {
        self.text_changed(frame, width) > GLYPH_STALE_PERCENT
    }

    fn changed_from(&self, frame: &[u8], width: usize, was: impl Fn(usize, usize) -> Option<[i32; 3]>) -> usize {
        let height = frame.len() / 4 / width;
        let rows = self.y.max(0) as usize..((self.y + self.h).max(0) as usize).min(height);
        let columns = self.x.max(0) as usize..((self.x + self.w).max(0) as usize).min(width.saturating_sub(EDGE_PX));
        let edge = |a: [i32; 3], b: [i32; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
        let now = |x: usize, y: usize| edge(pixel(frame, width, x, y), pixel(frame, width, x + EDGE_PX, y));
        let then = |x: usize, y: usize| Some(edge(was(x, y)?, was(x + EDGE_PX, y)?));
        let dot = |a: [i32; 3], b: [i32; 3]| (0..3).map(|c| a[c] * b[c]).sum::<i32>();
        let mut sharpest = 0;
        for y in rows.clone() {
            for x in columns.clone() {
                if let Some(was) = then(x, y) {
                    sharpest = sharpest.max(dot(was, was));
                }
            }
        }
        let (mut edges, mut gone) = (0, 0);
        for y in rows {
            for x in columns.clone() {
                let Some(was) = then(x, y) else { continue };
                if dot(was, was) * 4 > sharpest {
                    edges += 1;
                    gone += (dot(now(x, y), was) * 4 < dot(was, was)) as usize;
                }
            }
        }
        gone * 100 / edges.max(1)
    }

    pub fn overlaps(&self, other: &Patch) -> bool {
        self.x < other.x + other.w && other.x < self.x + self.w && self.y < other.y + other.h && other.y < self.y + self.h
    }

    /// True when the picture under this patch has moved on from the one its backdrop was painted
    /// from, a little at a time or all at once.
    pub fn backdrop_behind(&self, frame: &[u8], width: usize) -> bool {
        let Some(b) = &self.backdrop else { return false };
        let height = frame.len() / 4 / width;
        if self.x < 0 || self.y < 0 || (self.x + b.w) as usize > width || (self.y + b.h) as usize > height {
            return true;
        }
        let (x, y, w) = (self.x as usize, self.y as usize, b.w as usize);
        let moved = b.under.iter().enumerate().filter(|&(i, &was)| distance(pixel(frame, width, x + i % w, y + i / w), was) > SAME_BG).count();
        moved * 100 > b.under.len()
    }

    /// True once the original text is no longer there.
    pub fn is_stale(&self, frame: &[u8], basis: &[u8], width: usize) -> bool {
        self.changed_percent(frame, basis, width) > GLYPH_STALE_PERCENT
    }

    /// True if the text under this patch is found `dy` pixels further down in `frame` (up when
    /// negative), all of it still in the picture.
    pub fn moved(&self, frame: &[u8], basis: &[u8], width: usize, dy: i32) -> bool {
        let height = (frame.len() / 4 / width) as i32;
        if basis.len() != frame.len() || self.h <= 0 || self.x < 0 || self.x + self.w > width as i32 || self.y < 0 || self.y + self.h > height {
            return false;
        }
        if self.y + dy < 0 || self.y + self.h + dy > height {
            return false;
        }
        let (start, n) = (|y: i32| (y as usize * width + self.x as usize) * 4, self.w as usize * 4);
        let same = (self.y..self.y + self.h).filter(|&y| basis[start(y)..start(y) + n] == frame[start(y + dy)..start(y + dy) + n]).count();
        same * 10 >= self.h as usize * MOVED_TENTHS
    }
}

/// How far the picture inside `rect` (left, top, right, bottom) has moved down since `basis`, or
/// up when negative: a chat pushing its lines up for a new one, a log, a page being scrolled.
/// Most of its rows that are not one flat colour must be found again that far off.
// shortcut: whole rows only, so a page that scrolls beside a still sidebar in the same rows is not
// followed; matching each block's own columns would be the upgrade.
pub fn scroll(basis: &[u8], frame: &[u8], width: usize, (x0, y0, x1, y1): (usize, usize, usize, usize)) -> Option<i32> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    if basis.len() != frame.len() || x0 >= x1 || y0 >= y1 || y1 * width * 4 > frame.len() {
        return None;
    }
    let rows = |image: &[u8]| -> Vec<Option<u64>> {
        (y0..y1)
            .map(|y| {
                let row = &image[(y * width + x0) * 4..(y * width + x1) * 4];
                let flat = row.chunks_exact(4).all(|p| p[..3] == row[..3]);
                (!flat).then(|| {
                    let mut hasher = DefaultHasher::new();
                    row.hash(&mut hasher);
                    hasher.finish()
                })
            })
            .collect()
    };
    let (was, now) = (rows(basis), rows(frame));
    let n = now.len() as i32;
    let found = |dy: i32| (0..n).filter(|&y| (0..n).contains(&(y - dy)) && now[y as usize].is_some() && now[y as usize] == was[(y - dy) as usize]).count();
    let inked = now.iter().filter(|row| row.is_some()).count();
    let (dy, best) = (1..=n * 3 / 4).flat_map(|d| [-d, d]).map(|dy| (dy, found(dy))).max_by_key(|&(dy, count)| (count, -dy.abs()))?;
    (best * 2 > inked && best > found(0) && best >= MIN_SCROLL_ROWS).then_some(dy)
}

impl Patch {
    /// The patch moved with its text, as a chat's lines move for a new one, in `frame`.
    pub fn shifted(mut self, dy: i32, frame: &[u8], width: usize) -> Self {
        self.y += dy;
        self.seen_in(frame, width)
    }

    /// The text taken as read from `frame`.
    pub fn seen_in(mut self, frame: &[u8], width: usize) -> Self {
        self.seen = Seen::of(frame, width, (self.x, self.y, self.w, self.h));
        self
    }

    /// A message in the region's top left corner, sized to its text.
    pub fn status(text: &str, width: i32) -> Self {
        Self {
            x: 0,
            y: 0,
            w: width,
            max_w: width,
            max_h: 0,
            one_line: false,
            h: 0,
            em: 0,
            bg: STATUS_BG,
            fg: STATUS_FG,
            bold: false,
            backdrop: None,
            seen: Arc::default(),
            text: text.to_string(),
            source: String::new(),
            px: 0,
            narrow: 100,
            text_h: 0,
        }
    }
}

fn colorref([b, g, r]: [i32; 3]) -> u32 {
    let color = (b as u32) << 16 | (g as u32) << 8 | r as u32;
    if color == KEY_COLOR { color ^ 0x010000 } else { color } // would otherwise punch a hole
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHITE: [u8; 3] = [255, 255, 255];
    const BLACK: [u8; 3] = [0, 0, 0];

    fn canvas(w: usize, h: usize, color: [u8; 3]) -> Vec<u8> {
        (0..w * h).flat_map(|_| [color[0], color[1], color[2], 0]).collect()
    }

    /// Paints a rectangle, every `step`th column: 3 looks like strokes of text, 1 is solid.
    fn paint(img: &mut [u8], w: usize, (x0, y0, x1, y1): (usize, usize, usize, usize), color: [u8; 3], step: usize) {
        for y in y0..y1 {
            for x in (x0..x1).step_by(step) {
                img[(y * w + x) * 4..][..3].copy_from_slice(&color);
            }
        }
    }

    #[test]
    fn dark_page_keeps_its_colours_and_finds_the_ink() {
        let mut img = canvas(200, 100, [0x10, 0x20, 0x30]);
        paint(&mut img, 200, (50, 40, 150, 60), [0xF0, 0xE0, 0xD0], 3);
        let g = glyphs(&img, 200, 100, (30, 20, 150, 60), Some("word")).unwrap();
        assert_eq!((g.bg, g.fg), ([0x10, 0x20, 0x30], [0xF0, 0xE0, 0xD0]));
        assert_eq!((g.x, g.y, g.w, g.h), (50, 40, 100, 20));
    }

    #[test]
    fn neighbours_caught_in_the_box_do_not_count() {
        let mut img = canvas(200, 40, WHITE);
        paint(&mut img, 200, (20, 12, 180, 28), BLACK, 3);
        paint(&mut img, 200, (0, 0, 200, 3), BLACK, 3); // the line above
        paint(&mut img, 200, (0, 37, 200, 40), BLACK, 3); // the line below
        let g = glyphs(&img, 200, 40, (0, 0, 200, 40), Some("word")).unwrap();
        assert_eq!((g.x, g.y, g.w, g.h), (20, 12, 160, 16));
    }

    #[test]
    fn a_button_keeps_its_own_colour_and_corners() {
        // White label on a blue button, on a white page that the box reaches into.
        let blue = [220, 110, 30];
        let mut img = canvas(120, 40, WHITE);
        paint(&mut img, 120, (0, 8, 120, 32), blue, 1);
        paint(&mut img, 120, (30, 16, 90, 24), WHITE, 3);
        let g = glyphs(&img, 120, 40, (0, 0, 120, 40), Some("word")).unwrap();
        assert_eq!(g.bg, [220, 110, 30]);
        assert_eq!((g.y, g.h), (16, 8));
    }

    #[test]
    fn a_field_border_is_not_tall_text() {
        let mut img = canvas(200, 40, WHITE);
        paint(&mut img, 200, (2, 0, 3, 40), BLACK, 1); // the input's left edge
        paint(&mut img, 200, (20, 14, 180, 26), BLACK, 3);
        let g = glyphs(&img, 200, 40, (0, 0, 200, 40), Some("word")).unwrap();
        assert_eq!((g.x, g.y, g.h), (20, 14, 12));
    }

    #[test]
    fn a_leading_icon_stays_out_of_the_rectangle() {
        let mut img = canvas(200, 40, WHITE);
        paint(&mut img, 200, (5, 8, 25, 32), BLACK, 1); // icon, taller than the text
        paint(&mut img, 200, (40, 14, 180, 26), BLACK, 3);
        for first in ["Projects", "Q", "\u{2461}"] {
            let g = glyphs(&img, 200, 40, (0, 0, 200, 40), Some(first)).unwrap();
            assert_eq!((g.x, g.y, g.h, g.icon), (40, 14, 12, true), "{first}");
        }
    }

    #[test]
    fn a_rounded_field_with_an_icon_leaves_only_the_text() {
        let mut img = canvas(200, 40, WHITE);
        paint(&mut img, 200, (0, 2, 200, 3), BLACK, 1); // top edge
        paint(&mut img, 200, (0, 37, 200, 38), BLACK, 1); // bottom edge
        for (x, y0, y1) in [(0, 8, 32), (1, 5, 8), (1, 32, 35), (2, 3, 5), (2, 35, 37)] {
            paint(&mut img, 200, (x, y0, x + 1, y1), BLACK, 1); // left edge, curving at the corners
        }
        paint(&mut img, 200, (10, 13, 22, 27), BLACK, 1); // search icon
        paint(&mut img, 200, (36, 14, 180, 26), BLACK, 3);
        let g = glyphs(&img, 200, 40, (0, 0, 200, 40), Some("Q")).unwrap();
        assert_eq!((g.x, g.y, g.h, g.icon), (36, 14, 12, true));
    }

    #[test]
    fn words_are_not_icons() {
        let mut img = canvas(200, 40, WHITE);
        paint(&mut img, 200, (10, 14, 22, 26), BLACK, 3); // "to" set apart as in monospace, or a Hangul syllable
        paint(&mut img, 200, (31, 14, 180, 26), BLACK, 3);
        for first in ["to", "이"] {
            let g = glyphs(&img, 200, 40, (0, 0, 200, 40), Some(first)).unwrap();
            assert_eq!((g.x, g.icon), (10, false), "{first}");
        }
    }

    #[test]
    fn a_lone_ascender_counts_as_part_of_the_line() {
        let mut img = canvas(200, 40, WHITE);
        paint(&mut img, 200, (20, 16, 180, 28), BLACK, 3);
        paint(&mut img, 200, (60, 11, 62, 16), BLACK, 1); // the l of "personal"
        let g = glyphs(&img, 200, 40, (0, 0, 200, 40), Some("word")).unwrap();
        assert_eq!((g.y, g.h), (11, 17));
    }

    /// A line of "text": every row differs, as the rows of real letters do.
    fn text_line(img: &mut [u8], w: usize, (x0, y0, x1, h): (usize, usize, usize, usize), seed: usize) {
        for y in y0..y0 + h {
            let row = y - y0; // the same line looks the same wherever it is
            for x in (x0..x1).filter(|&x| (x.wrapping_mul(2654435761) ^ row.wrapping_mul(40503) ^ seed.wrapping_mul(97)) >> 5 & 3 == 0) {
                img[(y * w + x) * 4..][..3].copy_from_slice(&BLACK);
            }
        }
    }

    /// A chat: a still heading, then five lines 30 pixels apart, the first one `first`.
    fn chat(first: usize) -> Vec<u8> {
        let mut img = canvas(200, 200, WHITE);
        text_line(&mut img, 200, (10, 4, 190, 12), 99);
        for (i, y) in [40, 70, 100, 130, 160].into_iter().enumerate() {
            text_line(&mut img, 200, (10, y, 190, 14), first + i);
        }
        img
    }

    fn patch_at(y: i32) -> Patch {
        let mut p = Patch::status("訳", 0);
        (p.x, p.y, p.w, p.h) = (8, y - 2, 184, 18);
        p
    }

    #[test]
    fn a_chat_taking_a_new_line_has_moved_up_by_one() {
        let (basis, frame) = (chat(1), chat(2));
        assert_eq!(scroll(&basis, &frame, 200, (0, 0, 200, 200)), Some(-30));
        assert!(patch_at(100).moved(&frame, &basis, 200, -30)); // the third line is now the second
        assert!(!patch_at(100).moved(&frame, &basis, 200, 0));
        assert!(!patch_at(40).moved(&frame, &basis, 200, -60)); // the first has left the picture
        assert!(!patch_at(4 + 2).moved(&frame, &basis, 200, -30)); // the heading stays
    }

    #[test]
    fn a_page_changing_in_place_has_not_scrolled() {
        let basis = chat(1);
        assert_eq!(scroll(&basis, &basis, 200, (0, 0, 200, 200)), None);
        let mut frame = basis.clone();
        paint(&mut frame, 200, (10, 100, 190, 114), WHITE, 1);
        text_line(&mut frame, 200, (10, 100, 190, 14), 42); // one line rewritten where it was
        assert_eq!(scroll(&basis, &frame, 200, (0, 0, 200, 200)), None);
    }

    #[test]
    fn marks_a_few_rows_under_the_letters_belong_to_the_line() {
        let mut img = canvas(300, 60, WHITE);
        paint(&mut img, 300, (20, 12, 280, 20), BLACK, 6); // marks over the letters, touching them
        paint(&mut img, 300, (20, 20, 280, 35), BLACK, 3); // the letters
        paint(&mut img, 300, (40, 37, 44, 42), BLACK, 1); // Thai vowels two rows under them
        paint(&mut img, 300, (150, 37, 154, 42), BLACK, 1);
        let g = glyphs(&img, 300, 60, (0, 0, 300, 60), Some("word")).unwrap();
        assert_eq!((g.y, g.h), (12, 30));
    }

    #[test]
    fn marks_between_two_lines_go_with_the_nearer_one() {
        let mut img = canvas(300, 70, WHITE);
        paint(&mut img, 300, (20, 10, 280, 24), BLACK, 3); // the first line
        paint(&mut img, 300, (40, 26, 44, 30), BLACK, 1); // its vowels below, two rows off
        paint(&mut img, 300, (20, 33, 280, 37), BLACK, 12); // the second line's marks above it
        paint(&mut img, 300, (20, 38, 280, 53), BLACK, 3); // the second line
        let first = glyphs(&img, 300, 70, (0, 0, 300, 40), Some("word")).unwrap();
        let second = glyphs(&img, 300, 70, (0, 20, 300, 40), Some("word")).unwrap();
        assert_eq!([(first.y, first.h), (second.y, second.h)], [(10, 20), (33, 20)]);
    }

    #[test]
    fn the_next_line_a_few_rows_down_is_not_taken_for_marks() {
        let mut img = canvas(300, 60, WHITE);
        paint(&mut img, 300, (20, 12, 280, 26), BLACK, 3); // this line
        paint(&mut img, 300, (20, 29, 280, 43), BLACK, 3); // the next line, three rows down
        for box_h in [50, 40] { // the box holds all of the next line, or cuts it
            let g = glyphs(&img, 300, 60, (0, 0, 300, box_h), Some("word")).unwrap();
            assert_eq!((g.y, g.h), (12, 14), "{box_h}");
        }
    }

    #[test]
    fn lines_with_no_gap_are_told_apart_at_the_thin_row() {
        let mut img = canvas(200, 40, WHITE);
        paint(&mut img, 200, (10, 6, 190, 18), BLACK, 3); // this line
        paint(&mut img, 200, (60, 18, 62, 20), BLACK, 1); // a descender bridges the gap
        paint(&mut img, 200, (10, 20, 190, 32), BLACK, 3); // the next line
        let g = glyphs(&img, 200, 40, (0, 0, 200, 24), Some("word")).unwrap();
        assert_eq!((g.y, g.h), (6, 12));
    }

    #[test]
    fn all_capitals_are_not_split_at_their_thin_rows() {
        let mut img = canvas(100, 30, WHITE);
        paint(&mut img, 100, (10, 8, 90, 10), BLACK, 1); // the bars of a row of E's
        paint(&mut img, 100, (10, 14, 90, 16), BLACK, 1);
        paint(&mut img, 100, (10, 20, 90, 22), BLACK, 1);
        paint(&mut img, 100, (10, 8, 90, 22), BLACK, 5); // their stems
        assert_eq!(glyphs(&img, 100, 30, (0, 0, 100, 30), Some("word")).unwrap().h, 14);
    }

    #[test]
    fn a_gap_of_one_row_stays_inside_the_line() {
        let mut img = canvas(100, 30, WHITE);
        paint(&mut img, 100, (10, 8, 90, 11), BLACK, 15); // the dots of the i's
        paint(&mut img, 100, (10, 12, 90, 22), BLACK, 3);
        assert_eq!(glyphs(&img, 100, 30, (0, 0, 100, 30), Some("word")).unwrap().h, 14);
    }

    /// A game's label has a glow, and a speck of the picture behind it can fall inside the glow,
    /// apart from the letters; the dot of an i sits just as far apart but is about as tall.
    #[test]
    fn a_speck_apart_from_the_letters_is_not_part_of_them() {
        let label = |box_top: usize, mark: (usize, usize)| {
            let mut img = canvas(200, 60, WHITE);
            paint(&mut img, 200, (8, 12, 64, 44), [190, 190, 190], 1); // the glow
            paint(&mut img, 200, (10, 20, 61, 40), BLACK, 3);
            paint(&mut img, 200, (20, mark.0, 26, mark.1), BLACK, 1);
            glyphs(&img, 200, 60, (0, box_top as i32, 200, (60 - box_top) as i32), Some("word")).unwrap().letters
        };
        assert_eq!(label(0, (14, 15)), 20); // a speck: one row, five rows above
        assert_eq!(label(0, (16, 19)), 24); // a dot: three rows, one row above
        assert_eq!(label(12, (12, 15)), 20); // the picture, cut off by the box's top
    }

    #[test]
    fn one_line_may_spread_over_empty_background_only() {
        let mut img = canvas(300, 40, WHITE);
        paint(&mut img, 300, (10, 10, 61, 30), BLACK, 3);
        paint(&mut img, 300, (200, 0, 201, 40), BLACK, 1); // a thin rule further along
        let g = glyphs(&img, 300, 40, (5, 5, 100, 30), Some("word")).unwrap();
        let mut one = Block { glyphs: g, lines: 1, em: 20, bold: false, text: "x".into() };
        assert_eq!(Patch::new(&img, 300, 40, &one, "訳".into()).max_w, 190);
        one.lines = 2;
        let p = Patch::new(&img, 300, 40, &one, "訳".into());
        assert_eq!(p.max_w, p.w);
    }

    #[test]
    fn a_backdrop_knows_when_the_picture_behind_it_has_moved_on() {
        let mut first = canvas(100, 40, [90, 60, 120]);
        paint(&mut first, 100, (20, 15, 60, 25), WHITE, 3);
        let p = Patch { x: 10, y: 10, w: 60, h: 20, ..Patch::status("", 60) }.keep_picture(&first, 100, 40);
        assert!(!p.backdrop_behind(&first, 100));
        let mut later = canvas(100, 40, [120, 70, 150]); // the scene behind the same text, some steps on
        paint(&mut later, 100, (20, 15, 60, 25), WHITE, 3);
        assert!(p.backdrop_behind(&later, 100));
    }

    #[test]
    fn patch_never_leaves_the_image() {
        let mut img = canvas(100, 50, WHITE);
        paint(&mut img, 100, (0, 10, 40, 20), BLACK, 3);
        let g = glyphs(&img, 100, 50, (-10, 0, 60, 30), Some("word")).unwrap();
        let p = Patch::new(&img, 100, 50, &Block { glyphs: g, lines: 1, em: 10, bold: false, text: "x".into() }, "訳".into());
        assert_eq!(p.x, 0);
    }

    #[test]
    fn flat_or_empty_boxes_have_no_glyphs() {
        let img = canvas(100, 50, [9, 9, 9]);
        assert!(glyphs(&img, 100, 50, (10, 10, 50, 20), Some("word")).is_none());
        assert!(glyphs(&img, 100, 50, (-20, -20, 10, 10), Some("word")).is_none());
    }

    #[test]
    fn style_separates_a_code_block_from_the_page() {
        let at = |bg, fg| Glyphs { x: 0, y: 0, w: 1, h: 1, letters: 1, bg, fg, stem: 1, icon: false, picture: false };
        let page = at([0, 0, 0], [236, 236, 236]);
        assert!(page.same_style(&at([1, 0, 0], [220, 230, 236])));
        assert!(!page.same_style(&at([35, 31, 27], [236, 236, 236])));
        assert!(!page.same_style(&at([0, 0, 0], [90, 170, 240])));
    }

    fn on_white(img: &[u8], width: usize, height: usize) -> Patch {
        let g = glyphs(img, width, height, (0, 0, width as i32, height as i32), None).unwrap();
        Patch::new(img, width, height, &Block { glyphs: g, lines: 1, em: 12, bold: false, text: "x".into() }, "訳".into())
    }

    #[test]
    fn a_moving_background_does_not_make_text_stale() {
        let mut basis = canvas(100, 40, WHITE);
        paint(&mut basis, 100, (10, 14, 90, 26), BLACK, 3);
        let patch = on_white(&basis, 100, 40);
        let mut video = canvas(100, 40, [90, 140, 30]);
        paint(&mut video, 100, (10, 14, 90, 26), BLACK, 3);
        assert!(!patch.is_stale(&video, &basis, 100));
    }

    #[test]
    fn a_video_behind_an_outlined_subtitle_changes_nothing() {
        let subtitle = |back: [u8; 3]| {
            let mut img = canvas(100, 40, back);
            paint(&mut img, 100, (9, 13, 91, 27), BLACK, 1);
            paint(&mut img, 100, (10, 14, 90, 26), WHITE, 4);
            img
        };
        let basis = subtitle([60, 70, 80]);
        let patch = on_white(&basis, 100, 40);
        for back in [[20, 30, 40], [150, 130, 100], [230, 230, 230]] {
            assert!(patch.changed_percent(&subtitle(back), &basis, 100) <= WOBBLE_PERCENT);
        }
    }

    #[test]
    fn a_thick_subtitle_over_a_picture_is_covered_from_top_to_bottom() {
        let (w, h) = (300, 60);
        let mut img = canvas(w, h, WHITE);
        for y in 0..h {
            for x in 0..w {
                let v = (40 + x / 4 + y) as u8;
                img[(y * w + x) * 4..][..3].copy_from_slice(&[v, v + 10, v + 20]);
            }
        }
        for left in (20..270).step_by(16) {
            paint(&mut img, w, (left, 12, left + 12, 48), BLACK, 1);
            paint(&mut img, w, (left + 2, 14, left + 10, 46), WHITE, 1);
        }
        let g = glyphs(&img, w, h, (0, 0, w as i32, h as i32), None).unwrap();
        assert!(g.y <= 12 && g.y + g.h >= 48, "{} {}", g.y, g.h);
        assert!(g.bg.iter().sum::<i32>() < 3 * 170 && g.fg == [255, 255, 255], "{:?} {:?}", g.bg, g.fg);
    }

    #[test]
    fn letters_come_off_a_picture_and_the_picture_stays() {
        let (w, h) = (300usize, 60usize);
        let mut img = canvas(w, h, WHITE);
        for y in 0..h {
            for x in 0..w {
                let (v, grain) = ((40 + x / 4 + y) as u8, (x % 5 * 2) as u8);
                img[(y * w + x) * 4..][..3].copy_from_slice(&[v + grain, v + 10 + grain, v + 20 + grain]);
            }
        }
        let before = img.clone();
        for left in (20..270).step_by(16) {
            paint(&mut img, w, (left, 20, left + 6, 40), WHITE, 1);
        }
        paint(&mut img, w, (275, 18, 285, 42), BLACK, 1); // a dark sleeve beside the text
        let b = paint_out_letters(&img, w, h, (10, 16, 280, 28), [255; 3]);
        let at = |x: usize, y: usize| pixel(&b.bgra, 280, x - 10, y - 16);
        assert!(distance(at(22, 30), pixel(&before, w, 22, 30)) < 30, "{:?}", at(22, 30));
        assert_eq!(at(32, 30), pixel(&before, w, 32, 30), "between the letters nothing changes");
        assert_eq!(at(280, 30), [0; 3], "the sleeve stays");
    }

    #[test]
    fn a_border_line_under_a_title_is_not_what_lies_behind_it() {
        let (w, h) = (200usize, 60usize);
        let lavender = [200, 140, 180];
        let mut img = canvas(w, h, lavender);
        paint(&mut img, w, (20, 20, 120, 40), WHITE, 3);
        paint(&mut img, w, (0, 44, 200, 46), WHITE, 1); // the panel's border, right under the box
        let b = paint_out_letters(&img, w, h, (10, 16, 180, 28), [255; 3]);
        let at = |x: usize, y: usize| pixel(&b.bgra, 180, x - 10, y - 16);
        assert!(distance(at(23, 36), lavender.map(i32::from)) < 40, "{:?}", at(23, 36));
    }

    #[test]
    fn the_see_through_colour_never_comes_out_of_a_picture() {
        let (w, h) = (40usize, 20usize);
        let mut img = canvas(w, h, WHITE);
        for p in img.chunks_mut(4) {
            p.copy_from_slice(&[255, 0, 255, 255]);
        }
        let b = paint_out_letters(&img, w, h, (5, 5, 20, 10), [255; 3]);
        assert!(b.bgra.chunks(4).all(|p| p[..3] != [255, 0, 255]));
    }

    #[test]
    fn one_letter_changing_is_not_wobble() {
        let mut basis = canvas(100, 40, WHITE);
        paint(&mut basis, 100, (10, 14, 90, 26), BLACK, 3);
        let patch = on_white(&basis, 100, 40);
        let mut next = basis.clone();
        paint(&mut next, 100, (70, 14, 78, 26), WHITE, 1);
        let changed = patch.changed_percent(&next, &basis, 100);
        assert!(changed > WOBBLE_PERCENT && !patch.is_stale(&next, &basis, 100), "{changed}");
    }

    #[test]
    fn other_text_in_the_same_place_is_stale() {
        let mut basis = canvas(100, 40, WHITE);
        paint(&mut basis, 100, (10, 14, 90, 26), BLACK, 3);
        let patch = on_white(&basis, 100, 40);
        let mut other = canvas(100, 40, WHITE);
        paint(&mut other, 100, (11, 14, 90, 26), BLACK, 3);
        assert!(patch.is_stale(&other, &basis, 100));
    }

    #[test]
    fn seams_that_leave_nothing_are_no_line() {
        let mut img = canvas(100, 30, WHITE);
        paint(&mut img, 100, (10, 0, 90, 12), BLACK, 3);
        paint(&mut img, 100, (40, 12, 41, 16), BLACK, 1); // thin rows right at the middle
        paint(&mut img, 100, (10, 16, 90, 30), BLACK, 3);
        if let Some(g) = glyphs(&img, 100, 30, (0, 0, 100, 30), Some("word")) {
            assert!(g.h > 0 && g.w > 0);
        }
    }

    #[test]
    fn key_colour_never_reaches_the_overlay() {
        assert_ne!(colorref([255, 0, 255]), KEY_COLOR);
    }
}
