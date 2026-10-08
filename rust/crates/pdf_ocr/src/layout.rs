//! Page layout: which ink components form which text lines, and in what order
//! a reader takes those lines.
//!
//! Lines are built by sweeping glyph-sized components left to right and
//! joining each to an open line whose vertical band it shares, within a few
//! glyph heights; gaps wider than that start a new line, which is how column
//! gutters and table cells come apart. Marks too small to be glyphs on their
//! own (dots, accents, commas, hyphens) join the nearest line afterwards.
//! Reading order is a recursive XY-cut: split the region at its widest
//! whitespace, columns before rows when the gutter is the clearer gap.

use crate::components::Component;

#[derive(Clone, Debug, PartialEq)]
pub struct TextLine {
    /// Indices into the component list.
    pub members: Vec<usize>,
    /// Union of the members' boxes: [x0, y0, x1, y1], end-exclusive pixels.
    pub bounds: [f64; 4],
    /// Typical glyph height on this line, in pixels.
    pub scale: f64,
}

impl TextLine {
    pub fn height(&self) -> f64 {
        self.bounds[3] - self.bounds[1]
    }
}

/// Median height of components that look like glyphs.
pub fn typical_height(components: &[Component], page_height: usize) -> Option<f64> {
    let mut heights: Vec<f64> = components
        .iter()
        .filter(|c| {
            let (w, h) = (c.width(), c.height());
            let fill = c.pixels as f64 / (w as f64 * h as f64);
            h >= 6
                && (h as usize) <= page_height / 8
                && w <= 5 * h
                && c.pixels >= 6
                && (0.05..=0.95).contains(&fill)
        })
        .map(|c| c.height() as f64)
        .collect();
    (heights.len() >= 3).then(|| median(&mut heights))
}

/// Angle (radians) by which text baselines fall to the right, from how sharply
/// glyph bottoms line up when sheared by each candidate angle. Zero when the
/// page is level, has too little text to tell, or no angle is clearly better.
pub fn estimate_skew(components: &[Component], typical: f64) -> f64 {
    let points: Vec<(f64, f64)> = components
        .iter()
        .filter(|c| {
            let h = c.height() as f64;
            h >= 0.5 * typical && h <= 2.0 * typical && (c.width() as f64) <= 2.5 * typical
        })
        .map(|c| (c.center_x(), c.y1 as f64))
        .collect();
    if points.len() < 20 {
        return 0.0;
    }
    // Each point's weight is split between its two nearest bins, so the score
    // changes smoothly with the angle instead of in bin-sized steps. Coarse
    // bins find the neighbourhood; fine ones then tell 1.9 degrees from 2.0.
    let score = |angle: f64, bin: f64| -> f64 {
        let slope = angle.tan();
        let projected: Vec<f64> = points.iter().map(|&(x, y)| (y - x * slope) / bin).collect();
        let lo = projected
            .iter()
            .cloned()
            .fold(f64::INFINITY, f64::min)
            .floor();
        let hi = projected
            .iter()
            .cloned()
            .fold(f64::NEG_INFINITY, f64::max)
            .floor();
        let mut counts = vec![0f64; (hi - lo) as usize + 2];
        for p in projected {
            let at = p - lo;
            let (i, fraction) = (at.floor() as usize, at.fract());
            counts[i] += 1.0 - fraction;
            counts[i + 1] += fraction;
        }
        counts.iter().map(|c| c * c).sum()
    };
    let degrees = std::f64::consts::PI / 180.0;
    let (coarse_bin, fine_bin) = ((typical / 4.0).max(1.0), (typical / 12.0).max(1.0));
    let mut coarse = (0.0, score(0.0, coarse_bin));
    for step in -24..=24 {
        let angle = step as f64 * 0.25 * degrees;
        let s = score(angle, coarse_bin);
        if s > coarse.1 {
            coarse = (angle, s);
        }
    }
    let mut best = (coarse.0, score(coarse.0, fine_bin));
    for step in -15..=15 {
        let angle = coarse.0 + step as f64 * 0.02 * degrees;
        let s = score(angle, fine_bin);
        if s > best.1 {
            best = (angle, s);
        }
    }
    if best.0.abs() < 0.1 * degrees || best.1 < 1.05 * score(0.0, fine_bin) {
        0.0
    } else {
        best.0
    }
}

/// Whether text lines run up and down the image, as on a page scanned
/// sideways: glyph centres then line up in x instead of in y.
pub fn text_runs_vertically(components: &[Component], typical: f64) -> bool {
    let glyphs: Vec<(f64, f64)> = components
        .iter()
        .filter(|c| {
            let size = c.width().max(c.height()) as f64;
            (0.5 * typical..=2.5 * typical).contains(&size)
        })
        .map(|c| (c.center_x(), c.center_y()))
        .collect();
    if glyphs.len() < 30 {
        return false;
    }
    let bin = (typical / 3.0).max(1.0);
    let sharpness = |values: Vec<f64>| -> f64 {
        let mut counts = std::collections::HashMap::<i64, f64>::new();
        for v in values {
            *counts.entry((v / bin).floor() as i64).or_default() += 1.0;
        }
        counts.values().map(|c| c * c).sum()
    };
    let across = sharpness(glyphs.iter().map(|g| g.1).collect());
    let down = sharpness(glyphs.iter().map(|g| g.0).collect());
    down > 1.5 * across
}

struct OpenLine {
    members: Vec<usize>,
    x_end: f64,
    center: f64,
    scale: f64,
}

/// Group components into text lines, in reading order.
pub fn find_lines(components: &[Component], typical: f64) -> Vec<TextLine> {
    // Glyph-sized components found lines. Smaller marks (dots, commas, quotes,
    // dashes) join the line they are beside as the sweep passes them, so a
    // dash still bridges the gap it sits in, but never start a line.
    let mut items = Vec::new();
    for (i, c) in components.iter().enumerate() {
        let (w, h) = (c.width() as f64, c.height() as f64);
        let rule = w > 8.0 * h && h < 0.5 * typical && w > 4.0 * typical;
        if h >= 0.3 * typical && h <= 4.0 * typical && w <= 30.0 * typical && !rule {
            items.push((i, true));
        } else if h < 0.3 * typical && w <= 4.0 * typical && c.pixels >= 2 {
            items.push((i, false));
        }
    }
    items.sort_by_key(|&(i, _)| (components[i].x0, components[i].y0));

    let mut done: Vec<OpenLine> = Vec::new();
    let mut open: Vec<OpenLine> = Vec::new();
    let mut small = Vec::new();
    for (i, seed) in items {
        let c = &components[i];
        let (x0, h, cy) = (c.x0 as f64, c.height() as f64, c.center_y());
        // Lines this far behind can never be joined again: seeds come in x order.
        let mut k = 0;
        while k < open.len() {
            if open[k].x_end + 2.5 * open[k].scale < x0 {
                done.push(open.swap_remove(k));
            } else {
                k += 1;
            }
        }
        let mut best: Option<(usize, f64, f64)> = None;
        let mut near: Option<(usize, f64)> = None;
        for (k, line) in open.iter().enumerate() {
            if x0 - line.x_end > 2.5 * line.scale || h > 3.0 * line.scale {
                continue;
            }
            let distance = (cy - line.center).abs();
            // Commas, periods and quotes: small beside the line's glyphs and
            // off its core band, but plainly part of it.
            if h < 0.65 * line.scale
                && distance < line.scale
                && near.is_none_or(|(_, d)| distance < d)
            {
                near = Some((k, distance));
            }
            if !seed || h < 0.35 * line.scale {
                continue;
            }
            let (top, bottom) = (
                line.center - line.scale / 2.0,
                line.center + line.scale / 2.0,
            );
            let overlap = (c.y1 as f64).min(bottom) - (c.y0 as f64).max(top);
            let ratio = overlap / h.min(line.scale);
            if ratio >= 0.5
                && best
                    .is_none_or(|(_, r, d)| ratio > r + 1e-9 || (ratio > r - 1e-9 && distance < d))
            {
                best = Some((k, ratio, distance));
            }
        }
        if best.is_none() {
            if let Some((k, _)) = near {
                // Joins without moving the line's band: it is not a glyph body.
                let line = &mut open[k];
                line.members.push(i);
                line.x_end = line.x_end.max(c.x1 as f64);
                continue;
            }
            if !seed {
                small.push(i); // attached to the nearest line once all exist
                continue;
            }
        }
        match best {
            Some((k, _, _)) => {
                let line = &mut open[k];
                line.members.push(i);
                line.x_end = line.x_end.max(c.x1 as f64);
                // Adapt quickly at first, then follow a gently drifting baseline.
                let rate = (1.0 / line.members.len() as f64).max(0.25);
                line.center += rate * (cy - line.center);
                line.scale += rate * (h - line.scale);
            }
            None => open.push(OpenLine {
                members: vec![i],
                x_end: c.x1 as f64,
                center: cy,
                scale: h,
            }),
        }
    }
    done.append(&mut open);

    let mut lines: Vec<TextLine> = done
        .into_iter()
        .map(|line| TextLine {
            bounds: bounds_of(components, &line.members),
            members: line.members,
            scale: line.scale,
        })
        .collect();
    merge_fragments(&mut lines);

    // Dots, accents, commas and dashes join the nearest line around them.
    for &i in &small {
        let c = &components[i];
        let (cx, cy) = (c.center_x(), c.center_y());
        let mut best: Option<(usize, f64)> = None;
        for (k, line) in lines.iter().enumerate() {
            let middle = (line.bounds[1] + line.bounds[3]) / 2.0;
            let reach = line.height() / 2.0 + 0.6 * line.scale;
            if cx < line.bounds[0] - line.scale
                || cx > line.bounds[2] + line.scale
                || (cy - middle).abs() > reach
            {
                continue;
            }
            let distance = (cy - middle).abs();
            if best.is_none_or(|(_, d)| distance < d) {
                best = Some((k, distance));
            }
        }
        if let Some((k, _)) = best {
            lines[k].members.push(i);
        }
    }
    for line in &mut lines {
        line.bounds = bounds_of(components, &line.members);
    }
    lines.retain(|line| line.height() >= 4.0);

    let lines: Vec<TextLine> = lines
        .into_iter()
        .flat_map(|line| split_long(components, line))
        .collect();
    let mut order = Vec::with_capacity(lines.len());
    xy_cut(&lines, (0..lines.len()).collect(), &mut order, 0);
    let mut slots: Vec<Option<TextLine>> = lines.into_iter().map(Some).collect();
    order.into_iter().filter_map(|i| slots[i].take()).collect()
}

/// Fold "lines" of a few marks that sit inside a real line's box (an opening
/// quote met before the letters it belongs to, a stray comma) into it.
fn merge_fragments(lines: &mut Vec<TextLine>) {
    let mut order: Vec<usize> = (0..lines.len()).collect();
    order.sort_by_key(|&i| lines[i].members.len());
    let mut merged = vec![false; lines.len()];
    for &f in &order {
        let fragment = &lines[f];
        if fragment.members.len() > 3 {
            break;
        }
        let height = fragment.height();
        let target = (0..lines.len())
            .filter(|&k| k != f && !merged[k] && lines[k].members.len() > fragment.members.len())
            .filter(|&k| {
                let line = &lines[k];
                let overlap =
                    fragment.bounds[3].min(line.bounds[3]) - fragment.bounds[1].max(line.bounds[1]);
                height < 0.75 * line.height()
                    && overlap >= 0.5 * height
                    && fragment.bounds[0] >= line.bounds[0] - line.scale
                    && fragment.bounds[2] <= line.bounds[2] + line.scale
            })
            .min_by(|&a, &b| {
                let middle = |k: usize| (lines[k].bounds[1] + lines[k].bounds[3]) / 2.0;
                let centre = (fragment.bounds[1] + fragment.bounds[3]) / 2.0;
                (middle(a) - centre)
                    .abs()
                    .total_cmp(&(middle(b) - centre).abs())
            });
        if let Some(k) = target {
            let members = std::mem::take(&mut lines[f].members);
            lines[k].members.extend(members);
            merged[f] = true;
        }
    }
    let mut index = 0;
    lines.retain(|_| {
        index += 1;
        !merged[index - 1]
    });
}

fn bounds_of(components: &[Component], members: &[usize]) -> [f64; 4] {
    let mut b = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    for &i in members {
        let c = &components[i];
        b[0] = b[0].min(c.x0 as f64);
        b[1] = b[1].min(c.y0 as f64);
        b[2] = b[2].max(c.x1 as f64);
        b[3] = b[3].max(c.y1 as f64);
    }
    b
}

/// Lines wider than the recognizer comfortably reads in one pass (about 60
/// line heights) are split at their widest inner gap.
fn split_long(components: &[Component], line: TextLine) -> Vec<TextLine> {
    let too_long = (line.bounds[2] - line.bounds[0]) > 60.0 * line.height().max(1.0);
    if !too_long || line.members.len() < 2 {
        return vec![line];
    }
    let mut members = line.members.clone();
    members.sort_by_key(|&i| components[i].x0);
    let mut reach = components[members[0]].x1;
    let (mut split, mut widest) = (0, 0i64);
    for (k, &i) in members.iter().enumerate().skip(1) {
        let gap = i64::from(components[i].x0) - i64::from(reach);
        if gap > widest {
            (split, widest) = (k, gap);
        }
        reach = reach.max(components[i].x1);
    }
    if split == 0 {
        return vec![line];
    }
    let (left, right) = members.split_at(split);
    [left.to_vec(), right.to_vec()]
        .into_iter()
        .flat_map(|members| {
            split_long(
                components,
                TextLine {
                    bounds: bounds_of(components, &members),
                    members,
                    scale: line.scale,
                },
            )
        })
        .collect()
}

/// The widest gap between projections of `items` onto one axis: (gap, the
/// coordinate where the second group starts).
fn widest_gap(lines: &[TextLine], items: &[usize], axis: usize) -> (f64, f64) {
    let mut spans: Vec<(f64, f64)> = items
        .iter()
        .map(|&i| (lines[i].bounds[axis], lines[i].bounds[axis + 2]))
        .collect();
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (mut best, mut at) = (0.0, 0.0);
    let mut reach = spans[0].1;
    for &(start, end) in &spans[1..] {
        if start - reach > best {
            (best, at) = (start - reach, start);
        }
        reach = reach.max(end);
    }
    (best, at)
}

fn xy_cut(lines: &[TextLine], mut items: Vec<usize>, out: &mut Vec<usize>, depth: usize) {
    if items.len() <= 1 || depth > 64 {
        items.sort_by(|&a, &b| lines[a].bounds[1].total_cmp(&lines[b].bounds[1]));
        out.extend(items);
        return;
    }
    let mut heights: Vec<f64> = items.iter().map(|&i| lines[i].height()).collect();
    let typical = median(&mut heights);
    let (vertical, x_at) = widest_gap(lines, &items, 0);
    let (horizontal, y_at) = widest_gap(lines, &items, 1);
    // A gutter that runs the whole height of a region is columns. A wide
    // horizontal gap may only be paragraph breaks that line up across them.
    let (axis, at) = if vertical >= 0.8 * typical {
        (0, x_at)
    } else if horizontal > 0.0 {
        (1, y_at)
    } else {
        items.sort_by(|&a, &b| {
            let (la, lb) = (&lines[a].bounds, &lines[b].bounds);
            la[1].total_cmp(&lb[1]).then(la[0].total_cmp(&lb[0]))
        });
        out.extend(items);
        return;
    };
    let (first, second): (Vec<usize>, Vec<usize>) =
        items.into_iter().partition(|&i| lines[i].bounds[axis] < at);
    xy_cut(lines, first, out, depth + 1);
    xy_cut(lines, second, out, depth + 1);
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A glyph-like box.
    fn glyph(x: u32, y: u32, w: u32, h: u32) -> Component {
        Component {
            x0: x,
            y0: y,
            x1: x + w,
            y1: y + h,
            pixels: w * h / 2,
        }
    }

    /// Words of 4 glyphs (10 wide, 3 apart), 12 apart, on a baseline at `y`.
    fn line_of_words(x: u32, y: u32, words: u32) -> Vec<Component> {
        let mut out = Vec::new();
        for word in 0..words {
            for k in 0..4 {
                out.push(glyph(x + word * 64 + k * 13, y, 10, 20));
            }
        }
        out
    }

    #[test]
    fn two_columns_read_column_by_column_with_dots_attached() {
        let mut components = Vec::new();
        for row in 0..3 {
            components.extend(line_of_words(10, 20 + row * 34, 4)); // left column
            components.extend(line_of_words(400, 20 + row * 34, 4)); // right column
        }
        components.push(glyph(14, 12, 3, 3)); // an i-dot over the first glyph
        let typical = typical_height(&components, 1000).unwrap();
        let lines = find_lines(&components, typical);
        assert_eq!(lines.len(), 6);
        let starts: Vec<(f64, f64)> = lines.iter().map(|l| (l.bounds[0], l.bounds[1])).collect();
        assert_eq!(
            starts,
            vec![
                (10.0, 12.0),
                (10.0, 54.0),
                (10.0, 88.0),
                (400.0, 20.0),
                (400.0, 54.0),
                (400.0, 88.0)
            ]
        );
        assert_eq!(lines[0].members.len(), 17);
    }

    #[test]
    fn a_skewed_page_is_measured() {
        // Lines as wide as a 300 dpi page's: over a short line one pixel of
        // rounding is itself a tenth of a degree.
        let mut components = Vec::new();
        for row in 0..12 {
            for k in 0..100u32 {
                let x = 20 + k * 14;
                let y = (30 + row * 40) as f64 + x as f64 * 0.035; // 2 degrees
                components.push(glyph(x, y as u32, 10, 20));
            }
        }
        let skew = estimate_skew(&components, 20.0).to_degrees();
        assert!((skew - 0.035f64.atan().to_degrees()).abs() < 0.1, "{skew}");
        let level: Vec<Component> = (0..400)
            .map(|k| glyph(20 + (k % 40) * 14, 30 + (k / 40) * 40, 10, 20))
            .collect();
        assert_eq!(estimate_skew(&level, 20.0), 0.0);
    }
}
