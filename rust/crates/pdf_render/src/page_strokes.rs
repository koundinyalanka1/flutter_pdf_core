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

    pub(super) fn paint_stroke(
        &mut self,
        path: &Path,
        resources: &Dictionary,
        state: &GraphicsState,
    ) {
        let dashed = match dash_path(path, &state.dash, state.dash_phase) {
            Some(path) => path,
            None => {
                self.warn("stroke dash detail exceeds renderer limit; stroke skipped");
                return;
            }
        };
        let outline = if state.line_width == 0.0 {
            stroke_outline_styled(
                &dashed.transform(&state.ctm),
                1.0,
                state.line_cap,
                state.line_join,
                state.miter_limit,
            )
        } else {
            stroke_outline_styled(
                &dashed,
                state.line_width.abs(),
                state.line_cap,
                state.line_join,
                state.miter_limit,
            )
            .transform(&state.ctm)
        };
        self.paint_color(&outline, FillRule::NonZero, false, resources, state);
    }
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
