//! The OCR engine: a page image in, lines and words with boxes out.
//!
//! 1. Separate ink from paper and label connected components.
//! 2. Find which way up the page is: glyph centres line up across text lines,
//!    which tells a sideways page from an upright one, and the network reads
//!    a few long lines both ways round to tell upright from upside down.
//! 3. Measure skew from how glyph bottoms line up; if the page is tilted,
//!    resample it level and label again.
//! 4. Group components into lines, in reading order.
//! 5. Normalize each line and run the network on it, lines in parallel.
//! 6. Split the decoded text into words. Each word's box is the union of the
//!    ink components the network read as that word, so boxes hug the glyphs
//!    rather than the network's coarser view of where characters were.
//! 7. Map boxes back through the skew and the turn into the original image.

use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(feature = "embedded-model")]
use std::sync::OnceLock;

use crate::binarize::{default_window, sauvola};
use crate::components::{connected_components, Component};
use crate::ctc::{greedy, DecodedChar};
use crate::image::{GrayImage, Skew, Turn};
use crate::layout::{estimate_skew, find_lines, text_runs_vertically, typical_height, TextLine};
use crate::net::{ModelError, Network};
use crate::normalize::{crop_rect, normalize, LineImage, HEIGHT};
use crate::result::{OcrLine, OcrPage, OcrWord};

#[derive(Clone, Debug)]
pub struct OcrOptions {
    /// Threads recognizing lines; 0 uses every available core.
    pub threads: usize,
    /// Lines whose mean character confidence is lower are dropped: what the
    /// network is unsure of is mostly pictures, rules and specks.
    pub min_confidence: f32,
    /// Measure and correct page skew before finding lines.
    pub deskew: bool,
    /// Recognize pages scanned sideways or upside down.
    pub detect_orientation: bool,
}

impl Default for OcrOptions {
    fn default() -> Self {
        Self {
            threads: 0,
            min_confidence: 0.5,
            deskew: true,
            detect_orientation: true,
        }
    }
}

/// A clockwise quarter-turn reading has to beat the page as it is by this
/// much mean step confidence before the page is turned.
const TURN_MARGIN: f32 = 0.03;
/// Lines sampled to decide which way up a page is, and how much of each.
const ORIENTATION_LINES: usize = 6;
const ORIENTATION_COLUMNS: usize = 384;
/// Read upright with at least this mean step confidence, a page that is not
/// sideways is not checked upside down.
const CONFIDENT: f32 = 0.9;

#[cfg(feature = "embedded-model")]
static EMBEDDED_MODEL: &[u8] = include_bytes!("../models/latin.ocrm");

pub struct OcrEngine {
    net: Network,
}

impl OcrEngine {
    pub fn from_model(bytes: &[u8]) -> Result<Self, ModelError> {
        let net = Network::from_bytes(bytes)?;
        if net.height != HEIGHT {
            return Err(ModelError(format!(
                "line height {} is not {HEIGHT}",
                net.height
            )));
        }
        Ok(Self { net })
    }

    /// The Latin-script model compiled into this library, parsed once.
    #[cfg(feature = "embedded-model")]
    pub fn embedded() -> &'static OcrEngine {
        static ENGINE: OnceLock<OcrEngine> = OnceLock::new();
        ENGINE.get_or_init(|| {
            OcrEngine::from_model(EMBEDDED_MODEL).expect("embedded OCR model is valid")
        })
    }

    pub fn model_name(&self) -> &str {
        &self.net.name
    }

    pub fn recognize(&self, image: &GrayImage, options: &OcrOptions) -> OcrPage {
        let labeled = label(image);
        let turns = match &labeled {
            Some((components, typical)) if options.detect_orientation => {
                self.orientation(image, components, *typical)
            }
            _ => 0,
        };
        if turns == 0 {
            return self.read_upright(image, labeled, options);
        }
        let turned = image.turned(turns);
        let labeled = label(&turned);
        let mut page = self.read_upright(&turned, labeled, options);
        let turn = Turn::new(turns, image.width, image.height);
        page.lines = page
            .lines
            .into_iter()
            .map(|line| map_line(line, |x, y| turn.to_source(x, y)))
            .collect();
        page.width = image.width as f64;
        page.height = image.height as f64;
        page.orientation_degrees = u16::from(turns) * 90;
        page
    }

    /// Clockwise quarter turns that bring the page's text upright.
    fn orientation(&self, image: &GrayImage, components: &[Component], typical: f64) -> u8 {
        let sideways = text_runs_vertically(components, typical);
        // (turns, mean confidence) for each reading tried.
        let mut readings = Vec::with_capacity(4);
        if let Some((upright, flipped)) = self.both_ways(image, components, typical, !sideways) {
            readings.extend([(0u8, upright), (2, flipped)]);
        }
        if sideways {
            let turned = image.turned(1);
            if let Some((components, typical)) = label(&turned) {
                if let Some((upright, flipped)) =
                    self.both_ways(&turned, &components, typical, false)
                {
                    readings.extend([(1u8, upright), (3, flipped)]);
                }
            }
        }
        let as_is = readings.iter().find(|r| r.0 == 0).map_or(0.0, |r| r.1);
        readings
            .into_iter()
            .filter(|&(turns, score)| turns == 0 || score > as_is + TURN_MARGIN)
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map_or(0, |(turns, _)| turns)
    }

    /// Mean confidence reading the start of the page's longest lines as they
    /// are and turned half round; `None` when there are too few lines to
    /// judge. With `early`, a confident upright reading is taken as final.
    fn both_ways(
        &self,
        image: &GrayImage,
        components: &[Component],
        typical: f64,
        early: bool,
    ) -> Option<(f32, f32)> {
        let mut lines = find_lines(components, typical);
        lines.sort_by(|a, b| (b.bounds[2] - b.bounds[0]).total_cmp(&(a.bounds[2] - a.bounds[0])));
        let sample = &lines[..lines.len().min(ORIENTATION_LINES)];
        if sample.len() < 2 {
            return None;
        }
        let starts: Vec<(Vec<f32>, usize)> = sample
            .iter()
            .map(|line| {
                let normalized =
                    normalize(image, crop_rect(line.bounds, image.width, image.height));
                let width = normalized.width.min(ORIENTATION_COLUMNS);
                let data = normalized
                    .data
                    .chunks_exact(normalized.width)
                    .flat_map(|row| row[..width].iter().copied())
                    .collect();
                (data, width)
            })
            .collect();
        let n = starts.len() as f32;
        let upright = in_parallel(&starts, |(data, width)| self.readability(data, *width))
            .iter()
            .sum::<f32>()
            / n;
        if early && upright >= CONFIDENT {
            return Some((upright, 0.0));
        }
        // Reversing a normalized line turns it half round.
        let flipped = in_parallel(&starts, |(data, width)| {
            let reversed: Vec<f32> = data.iter().rev().copied().collect();
            self.readability(&reversed, *width)
        })
        .iter()
        .sum::<f32>()
            / n;
        Some((upright, flipped))
    }

    /// Mean confidence of the characters the network reads on a line. Blank
    /// steps are left out: a good model is sure of blanks either way up.
    fn readability(&self, data: &[f32], width: usize) -> f32 {
        let (input, padded, steps) = self.network_input(data, width);
        let logits = self.net.run(&input, padded);
        let classes = self.net.classes();
        let (mut total, mut count) = (0.0f32, 0usize);
        for row in logits.chunks_exact(classes).take(steps) {
            let (best, top) = row
                .iter()
                .enumerate()
                .fold(
                    (0, f32::NEG_INFINITY),
                    |b, (i, &v)| if v > b.1 { (i, v) } else { b },
                );
            if best != 0 {
                total += 1.0 / row.iter().map(|&v| (v - top).exp()).sum::<f32>();
                count += 1;
            }
        }
        if count == 0 {
            0.0
        } else {
            total / count as f32
        }
    }

    /// A normalized line as the network takes it: width rounded up to the
    /// stride, then trailing paper. Returns the input, its width and the
    /// number of steps that belong to the line.
    fn network_input(&self, data: &[f32], width: usize) -> (Vec<f32>, usize, usize) {
        let stride = self.net.stride;
        let steps = width.div_ceil(stride);
        let padded = steps * stride + self.net.trailing_paper.div_ceil(stride) * stride;
        let mut input = vec![0f32; HEIGHT * padded];
        for (row, source) in input.chunks_exact_mut(padded).zip(data.chunks_exact(width)) {
            row[..width].copy_from_slice(source);
        }
        (input, padded, steps)
    }

    /// Read a page that is already upright (apart from a little skew).
    fn read_upright(
        &self,
        image: &GrayImage,
        labeled: Option<(Vec<Component>, f64)>,
        options: &OcrOptions,
    ) -> OcrPage {
        let page = |skew: f64, lines| OcrPage {
            width: image.width as f64,
            height: image.height as f64,
            skew_degrees: skew.to_degrees(),
            orientation_degrees: 0,
            lines,
        };
        let Some((components, typical)) = labeled else {
            return page(0.0, Vec::new());
        };
        let angle = if options.deskew {
            estimate_skew(&components, typical)
        } else {
            0.0
        };
        let skew = Skew::new(angle, image.width, image.height);
        let level;
        let (work, components, typical) = if angle == 0.0 {
            (image, components, typical)
        } else {
            level = image.deskewed(&skew);
            match label(&level) {
                Some((components, typical)) => (&level, components, typical),
                None => return page(angle, Vec::new()),
            }
        };
        let lines = find_lines(&components, typical);
        let recognized = self.recognize_lines(work, &components, &lines, options.threads);
        let lines = recognized
            .into_iter()
            .flatten()
            .filter(|line| line.confidence >= options.min_confidence)
            .map(|line| unskew(line, &skew))
            .collect();
        page(angle, lines)
    }

    fn recognize_lines(
        &self,
        image: &GrayImage,
        components: &[Component],
        lines: &[TextLine],
        threads: usize,
    ) -> Vec<Option<OcrLine>> {
        let available = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let threads = if threads == 0 { available } else { threads }.clamp(1, lines.len().max(1));
        let next = AtomicUsize::new(0);
        let mut results = vec![None; lines.len()];
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..threads)
                .map(|_| {
                    scope.spawn(|| {
                        let mut done = Vec::new();
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some(line) = lines.get(i) else {
                                break done;
                            };
                            done.push((i, self.recognize_line(image, components, line)));
                        }
                    })
                })
                .collect();
            for worker in workers {
                for (i, line) in worker.join().expect("OCR worker panicked") {
                    results[i] = line;
                }
            }
        });
        results
    }

    fn recognize_line(
        &self,
        image: &GrayImage,
        components: &[Component],
        line: &TextLine,
    ) -> Option<OcrLine> {
        let crop = crop_rect(line.bounds, image.width, image.height);
        let normalized = normalize(image, crop);
        let (input, width, steps) = self.network_input(&normalized.data, normalized.width);
        let logits = self.net.run(&input, width);
        let chars = greedy(&logits, steps, &self.net.charset);
        let stride = self.net.stride;
        build_line(&chars, &normalized, stride, components, line)
    }
}

/// `f` over a handful of items, one thread each.
fn in_parallel<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    std::thread::scope(|scope| {
        let f = &f;
        let workers: Vec<_> = items
            .iter()
            .map(|item| scope.spawn(move || f(item)))
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().expect("OCR worker panicked"))
            .collect()
    })
}

fn label(image: &GrayImage) -> Option<(Vec<Component>, f64)> {
    let binary = sauvola(image, default_window(image.width, image.height), 0.25);
    let components = connected_components(&binary);
    let typical = typical_height(&components, image.height)?;
    Some((components, typical))
}

/// Words from the decoded characters, boxed by the ink they cover.
fn build_line(
    chars: &[DecodedChar],
    normalized: &LineImage,
    stride: usize,
    components: &[Component],
    line: &TextLine,
) -> Option<OcrLine> {
    let mut words: Vec<(usize, usize)> = Vec::new();
    let mut start = None;
    for (i, c) in chars.iter().enumerate() {
        if c.ch == ' ' {
            if let Some(s) = start.take() {
                words.push((s, i));
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        words.push((s, chars.len()));
    }
    if words.is_empty() {
        return None;
    }
    // Where along the page each word was read, and the cuts halfway between.
    let x_at = |step: usize| normalized.page_x((step * stride) as f64);
    let spans: Vec<(f64, f64)> = words
        .iter()
        .map(|&(s, e)| (x_at(chars[s].first_step), x_at(chars[e - 1].last_step + 1)))
        .collect();
    let mut cuts = vec![f64::NEG_INFINITY];
    cuts.extend(spans.windows(2).map(|pair| (pair[0].1 + pair[1].0) / 2.0));
    cuts.push(f64::INFINITY);

    let empty = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    let mut boxes = vec![empty; words.len()];
    for &m in &line.members {
        let c = &components[m];
        let cx = c.center_x();
        let k = cuts
            .windows(2)
            .position(|cut| cx >= cut[0] && cx < cut[1])
            .unwrap_or(0);
        let margin = 1.5 * line.scale;
        if cx < spans[k].0 - margin || cx > spans[k].1 + margin {
            continue; // a speck beyond the text, not part of the word
        }
        let b = &mut boxes[k];
        b[0] = b[0].min(c.x0 as f64);
        b[1] = b[1].min(c.y0 as f64);
        b[2] = b[2].max(c.x1 as f64);
        b[3] = b[3].max(c.y1 as f64);
    }

    let mut out_words = Vec::with_capacity(words.len());
    let mut confidence_sum = 0.0;
    let mut char_count = 0;
    for (k, &(s, e)) in words.iter().enumerate() {
        let mut b = boxes[k];
        if b[0] > b[2] {
            // No ink assigned: fall back to where the network read it.
            b = [
                spans[k].0,
                line.bounds[1],
                spans[k].1.max(spans[k].0 + 1.0),
                line.bounds[3],
            ];
        }
        let text: String = chars[s..e].iter().map(|c| c.ch).collect();
        let sum: f32 = chars[s..e].iter().map(|c| c.confidence).sum();
        confidence_sum += sum;
        char_count += e - s;
        out_words.push(OcrWord {
            text,
            confidence: sum / (e - s) as f32,
            quad: [[b[0], b[1]], [b[2], b[1]], [b[2], b[3]], [b[0], b[3]]],
            bounds: b,
        });
    }
    let text = out_words
        .iter()
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    Some(OcrLine {
        text,
        confidence: confidence_sum / char_count as f32,
        bounds: union(out_words.iter().map(|w| w.bounds)),
        words: out_words,
    })
}

fn union(boxes: impl Iterator<Item = [f64; 4]>) -> [f64; 4] {
    boxes.fold(
        [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ],
        |a, b| {
            [
                a[0].min(b[0]),
                a[1].min(b[1]),
                a[2].max(b[2]),
                a[3].max(b[3]),
            ]
        },
    )
}

/// Boxes measured on the levelled image, mapped back onto the original.
fn unskew(line: OcrLine, skew: &Skew) -> OcrLine {
    if skew.angle == 0.0 {
        return line;
    }
    map_line(line, |x, y| skew.to_source(x, y))
}

/// A line with every word's corners moved by `to`, and boxes recomputed.
fn map_line(mut line: OcrLine, to: impl Fn(f64, f64) -> (f64, f64)) -> OcrLine {
    for word in &mut line.words {
        word.quad = word.quad.map(|[x, y]| {
            let (sx, sy) = to(x, y);
            [sx, sy]
        });
        let xs = word.quad.map(|p| p[0]);
        let ys = word.quad.map(|p| p[1]);
        word.bounds = [
            xs.iter().cloned().fold(f64::INFINITY, f64::min),
            ys.iter().cloned().fold(f64::INFINITY, f64::min),
            xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
            ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        ];
    }
    line.bounds = union(line.words.iter().map(|w| w.bounds));
    line
}
