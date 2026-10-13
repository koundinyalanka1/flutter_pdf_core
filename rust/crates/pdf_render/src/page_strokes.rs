//! Dash lengths and phase are measured along user-space paths, before the CTM.
use super::*;
use crate::canvas::stroke_outline_styled;
use crate::geom::Point;

impl Renderer<'_> {
    pub(super) fn set_dash(&mut self, values: &[PdfObject], state: &mut GraphicsState) {
        let Some(PdfObject::Array(entries)) = values.first().map(|v| self.doc.resolve_value(v))
        else {
            self.warn("invalid stroke dash pattern; previous line style retained");
            return;
        };
        let dash: Option<Vec<_>> = entries.iter().map(as_number).collect();
        let phase = values.get(1).and_then(as_number).unwrap_or(0.0);
        let Some(dash) = dash.filter(|dash| {
            dash.iter().all(|v| v.is_finite() && *v >= 0.0)
                && (dash.is_empty() || dash.iter().sum::<f64>() > 0.0)
        }) else {
            self.warn("invalid stroke dash pattern; previous line style retained");
            return;
        };
        if !phase.is_finite() || phase < 0.0 || !dash.iter().sum::<f64>().is_finite() {
            self.warn("invalid stroke dash phase; previous line style retained");
            return;
        }
        state.dash = dash;
        state.dash_phase = phase;
    }

    /// Stroke `path` (user space). `adjust` lets thin horizontal and vertical
    /// lines snap to whole pixels; text outlines pass `false` and keep their
    /// exact shape.
    pub(super) fn paint_stroke(
        &mut self,
        path: &Path,
        resources: &Dictionary,
        state: &GraphicsState,
        adjust: bool,
    ) {
        // Most strokes are solid: they are stroked as they are, not copied.
        let dashed = if state.dash.is_empty() {
            std::borrow::Cow::Borrowed(path)
        } else {
            match dash_path(path, &state.dash, state.dash_phase) {
                Some(path) => std::borrow::Cow::Owned(path),
                None => {
                    self.warn("stroke dash detail exceeds renderer limit; stroke skipped");
                    return;
                }
            }
        };
        let ctm = &state.ctm;
        // Device pixels per unit of user space, averaged over the two axes —
        // the measure PDFium uses for the same rule.
        let unit = (ctm.a.hypot(ctm.b) + ctm.c.hypot(ctm.d)) / 2.0;
        let width = state.line_width.abs();
        let device_width = width * unit;
        let adjust = adjust && self.type3_depth == 0;
        let stroke = |path: &Path, width: f64| {
            stroke_outline_styled(
                path,
                width,
                state.line_cap,
                state.line_join,
                state.miter_limit,
            )
        };
        // Nothing is thinner than one device pixel. Width 0 already meant
        // that; a 0.25pt table rule shown on a phone came out a faint grey
        // that read as missing, where PDFium draws it as a solid pixel.
        let outline = if width == 0.0 || !device_width.is_finite() || device_width < 1.0 {
            let device = dashed.transform(ctm);
            let device = if adjust {
                snap_axis_strokes(&device, 1.0).unwrap_or(device)
            } else {
                device
            };
            stroke(&device, 1.0)
        } else if adjust && device_width < 2.5 {
            let pixels = device_width.round().max(1.0);
            match snap_axis_strokes(&dashed.transform(ctm), pixels) {
                Some(snapped) => stroke(&snapped, pixels),
                None => stroke(&dashed, width).transform(ctm),
            }
        } else {
            stroke(&dashed, width).transform(ctm)
        };
        self.paint_color(&outline, FillRule::NonZero, false, resources, state);
    }
}

/// Automatic stroke adjustment (ISO 32000-1, 10.7.5) for a thin stroke built
/// only from horizontal and vertical segments (device space): both edges of a
/// `width`-pixel line fall on pixel boundaries, so a rule is one or two solid
/// rows of pixels instead of a grey smear straddling a boundary. `None` when
/// any segment is slanted or curved; those stay anti-aliased.
fn snap_axis_strokes(path: &Path, width: f64) -> Option<Path> {
    // Lines a PDF draws straight are straight to within rounding noise.
    const STRAIGHT: f64 = 1e-3;
    // Halves round up, the same at any whole-pixel shift (see `snap_rect_fill`).
    let snap = |value: f64| (value - width / 2.0 + 0.5).floor() + width / 2.0;
    let mut snapped = path.clone();
    for (subpath, out) in path.subpaths.iter().zip(snapped.subpaths.iter_mut()) {
        let points = &subpath.points;
        let count = points.len();
        if count < 2 {
            continue;
        }
        let segments = if subpath.closed { count } else { count - 1 };
        // Judged on the original points; snapping a point twice is the same
        // as snapping it once, so a corner shared by two segments is fine.
        for i in 0..segments {
            let (a, b) = (points[i], points[(i + 1) % count]);
            let (dx, dy) = ((b.x - a.x).abs(), (b.y - a.y).abs());
            if !(dx.is_finite() && dy.is_finite()) {
                return None;
            }
            if dx < 1e-12 && dy < 1e-12 {
                continue;
            }
            let next = (i + 1) % count;
            if dy <= STRAIGHT {
                out.points[i].y = snap(points[i].y);
                out.points[next].y = snap(points[next].y);
            } else if dx <= STRAIGHT {
                out.points[i].x = snap(points[i].x);
                out.points[next].x = snap(points[next].x);
            } else {
                return None;
            }
        }
    }
    Some(snapped)
}

fn dash_path(path: &Path, pattern: &[f64], phase: f64) -> Option<Path> {
    if pattern.is_empty() {
        return Some(path.clone());
    }
    let mut pattern = pattern.to_vec();
    if pattern.len() % 2 != 0 {
        pattern.extend_from_within(..);
    }
    let total: f64 = pattern.iter().sum();
    let mut output = Path::new();
    let mut steps = 0;
    for subpath in &path.subpaths {
        let first_output = output.subpaths.len();
        let mut index = 0;
        let mut offset = phase % total;
        while (pattern[index] > 0.0 && offset >= pattern[index])
            || (pattern[index] == 0.0 && offset > 0.0)
        {
            offset -= pattern[index];
            index = (index + 1) % pattern.len();
        }
        let mut remaining = pattern[index] - offset;
        let mut active = false;
        let mut points = subpath.points.clone();
        if subpath.closed && points.len() > 1 {
            points.push(points[0]);
        }
        for pair in points.windows(2) {
            let (start, end) = (pair[0], pair[1]);
            let length = (end.x - start.x).hypot(end.y - start.y);
            if length < 1e-12 {
                continue;
            }
            let at = |distance: f64| {
                Point::new(
                    start.x + (end.x - start.x) * distance / length,
                    start.y + (end.y - start.y) * distance / length,
                )
            };
            let mut distance = 0.0;
            while distance < length {
                steps += 1;
                if steps > 100_000 {
                    return None;
                }
                let take = remaining.min(length - distance);
                if index % 2 == 0 {
                    let first = at(distance);
                    if !active {
                        output.move_to(first.x, first.y);
                    }
                    let last = at(distance + take);
                    output.line_to(last.x, last.y);
                    active = true;
                } else {
                    active = false;
                }
                distance += take;
                remaining -= take;
                if remaining <= 1e-12 {
                    index = (index + 1) % pattern.len();
                    remaining = pattern[index];
                    if index % 2 != 0 {
                        active = false;
                    }
                }
            }
        }
        if subpath.closed && output.subpaths.len() > first_output {
            let first = &output.subpaths[first_output];
            let last = output.subpaths.last().unwrap();
            if first.points.first() == subpath.points.first()
                && last.points.last() == subpath.points.first()
            {
                if output.subpaths.len() == first_output + 1 {
                    output.subpaths[first_output].closed = true;
                } else {
                    let first = output.subpaths.remove(first_output);
                    output
                        .subpaths
                        .last_mut()
                        .unwrap()
                        .points
                        .extend_from_slice(&first.points[1..]);
                }
            }
        }
    }
    Some(output)
}
