//! ISO 32000-1 8.7.4.5: Gouraud triangles and Coons/tensor patches.
//! Decode to a bounded scratch image, then composite once so adjoining triangles
//! do not apply constant alpha twice at their shared edges.
use super::*;
use functions::Bits;

#[derive(Clone, Copy, Default)]
struct Vertex {
    p: (f64, f64),
    c: [f64; 4],
}
struct MeshReader<'a> {
    bits: Bits<'a>,
    coordinate_bits: usize,
    component_bits: usize,
    flag_bits: usize,
    decode: Vec<f64>,
    components: usize,
    transform: Matrix,
}
impl MeshReader<'_> {
    fn value(&mut self, bits: usize, pair: usize) -> Option<f64> {
        Some(
            self.decode[pair]
                + self.bits.read(bits)? as f64 / ((1u64 << bits) - 1) as f64
                    * (self.decode[pair + 1] - self.decode[pair]),
        )
    }
    fn point(&mut self) -> Option<(f64, f64)> {
        let x = self.value(self.coordinate_bits, 0)?;
        let y = self.value(self.coordinate_bits, 2)?;
        let p = self.transform.apply(x, y);
        (p.0.is_finite() && p.1.is_finite()).then_some(p)
    }
    fn color(&mut self) -> Option<[f64; 4]> {
        let mut c = [0.0; 4];
        for (i, value) in c.iter_mut().enumerate().take(self.components) {
            *value = self.value(self.component_bits, 4 + 2 * i)?;
        }
        Some(c)
    }
    fn vertex(&mut self, has_flag: bool) -> Option<(usize, Vertex)> {
        let flag = if has_flag {
            self.bits.read(self.flag_bits)? as usize
        } else {
            0
        };
        let v = Vertex {
            p: self.point()?,
            c: self.color()?,
        };
        self.bits.align();
        Some((flag, v))
    }
}

struct Raster<'a> {
    pixels: Vec<u8>,
    width: usize,
    height: usize,
    work: usize,
    triangles: usize,
    components: usize,
    function_budget: usize,
    function: Option<&'a ColorFunction>,
}
impl Raster<'_> {
    fn triangle(&mut self, a: Vertex, b: Vertex, c: Vertex) -> Option<()> {
        self.triangles += 1;
        if self.triangles > 200_000 {
            return None;
        }
        let area = edge(a.p, b.p, c.p);
        if !area.is_finite()
            || [a.p, b.p, c.p]
                .iter()
                .any(|p| !p.0.is_finite() || !p.1.is_finite())
        {
            return None;
        }
        if area.abs() < 1e-12 {
            return Some(());
        }
        let x0 =
            a.p.0
                .min(b.p.0)
                .min(c.p.0)
                .floor()
                .clamp(0.0, self.width as f64) as usize;
        let x1 =
            a.p.0
                .max(b.p.0)
                .max(c.p.0)
                .ceil()
                .clamp(0.0, self.width as f64) as usize;
        let y0 =
            a.p.1
                .min(b.p.1)
                .min(c.p.1)
                .floor()
                .clamp(0.0, self.height as f64) as usize;
        let y1 =
            a.p.1
                .max(b.p.1)
                .max(c.p.1)
                .ceil()
                .clamp(0.0, self.height as f64) as usize;
        self.work = self.work.checked_add((x1 - x0) * (y1 - y0))?;
        if self.work > 128_000_000 {
            return None;
        }
        for y in y0..y1 {
            for x in x0..x1 {
                let p = (x as f64 + 0.5, y as f64 + 0.5);
                let u = edge(b.p, c.p, p) / area;
                let v = edge(c.p, a.p, p) / area;
                let w = 1.0 - u - v;
                if u < -1e-9 || v < -1e-9 || w < -1e-9 {
                    continue;
                }
                let mut components = [0.0; 4];
                for (i, value) in components.iter_mut().enumerate().take(self.components) {
                    *value = u * a.c[i] + v * b.c[i] + w * c.c[i];
                }
                let color = if let Some(f) = self.function {
                    color_from(
                        &f.evaluate_inputs_bounded(&[components[0]], &mut self.function_budget)?,
                    )?
                } else {
                    color_from(&components[..self.components])?
                };
                let at = (y * self.width + x) * 4;
                self.pixels[at..at + 4].copy_from_slice(&[
                    (color.r.clamp(0.0, 1.0) * 255.0).round() as u8,
                    (color.g.clamp(0.0, 1.0) * 255.0).round() as u8,
                    (color.b.clamp(0.0, 1.0) * 255.0).round() as u8,
                    255,
                ]);
            }
        }
        Some(())
    }
}
fn edge(a: (f64, f64), b: (f64, f64), p: (f64, f64)) -> f64 {
    (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)
}

impl Renderer<'_> {
    pub(super) fn draw_mesh(
        &mut self,
        object: &PdfObject,
        kind: i64,
        components: usize,
        state: &GraphicsState,
    ) {
        let PdfObject::Stream(stream) = object else {
            self.warn("mesh shading skipped: missing data stream");
            return;
        };
        let dict = &stream.dictionary;
        let cb = dict
            .get("BitsPerCoordinate")
            .and_then(PdfObject::as_i64)
            .unwrap_or(0) as usize;
        let vb = dict
            .get("BitsPerComponent")
            .and_then(PdfObject::as_i64)
            .unwrap_or(0) as usize;
        let fb = dict
            .get("BitsPerFlag")
            .and_then(PdfObject::as_i64)
            .unwrap_or(0) as usize;
        if !matches!(cb, 1 | 2 | 4 | 8 | 12 | 16 | 24 | 32)
            || !matches!(vb, 1 | 2 | 4 | 8 | 12 | 16)
            || (kind != 5 && !matches!(fb, 2 | 4 | 8))
        {
            self.warn("mesh shading skipped: invalid bit widths");
            return;
        }
        let function = if let Some(object) = dict.get("Function") {
            let Some(f) = ColorFunction::load(self.doc, object, 0, &mut 4096) else {
                self.warn("mesh shading skipped: invalid colour function");
                return;
            };
            if f.inputs() != 1 || f.outputs() != components {
                self.warn("mesh shading skipped: colour function component mismatch");
                return;
            }
            Some(f)
        } else {
            None
        };
        let inputs = if function.is_some() { 1 } else { components };
        let decode = numbers(self.doc, dict.get("Decode"));
        if decode.len() != 4 + 2 * inputs || decode.iter().any(|n| !n.is_finite()) {
            self.warn("mesh shading skipped: invalid decode array");
            return;
        }
        let Ok(data) = self.doc.stream_data(stream) else {
            self.warn("mesh shading skipped: undecodable stream");
            return;
        };
        let memory = self.canvas.width * self.canvas.height * 4;
        if data.len() > 32_000_000 || self.temporary_bytes.saturating_add(memory) > 256_000_000 {
            self.warn("mesh shading skipped: allocation limit exceeded");
            return;
        }
        let mut pixels = Vec::new();
        if pixels.try_reserve_exact(memory).is_err() {
            self.warn("mesh shading skipped: scratch allocation failed");
            return;
        }
        pixels.resize(memory, 0);
        let mut reader = MeshReader {
            bits: Bits::new(&data),
            coordinate_bits: cb,
            component_bits: vb,
            flag_bits: fb,
            decode,
            components: inputs,
            transform: state.ctm,
        };
        let mut raster = Raster {
            pixels,
            width: self.canvas.width,
            height: self.canvas.height,
            work: 0,
            triangles: 0,
            components: inputs,
            function_budget: 64_000_000,
            function: function.as_ref(),
        };
        let success = match kind {
            4 => triangles(&mut reader, &mut raster),
            5 => lattice(
                &mut reader,
                &mut raster,
                dict.get("VerticesPerRow")
                    .and_then(PdfObject::as_i64)
                    .unwrap_or(0),
            ),
            6 | 7 => patches(&mut reader, &mut raster, kind),
            _ => None,
        };
        if success.is_none() {
            self.warn("mesh shading skipped: malformed data or tessellation budget exceeded");
            return;
        }
        for y in 0..raster.height {
            for x in 0..raster.width {
                let i = (y * raster.width + x) * 4;
                if raster.pixels[i + 3] != 0 {
                    let p = &raster.pixels[i..i + 3];
                    self.canvas.blend(
                        x,
                        y,
                        Rgb::new(
                            p[0] as f32 / 255.0,
                            p[1] as f32 / 255.0,
                            p[2] as f32 / 255.0,
                        ),
                        state.fill_alpha,
                        state.clip.as_deref(),
                    );
                }
            }
        }
    }
}
fn triangles(reader: &mut MeshReader<'_>, raster: &mut Raster<'_>) -> Option<()> {
    let mut previous: Option<[Vertex; 3]> = None;
    while reader.bits.remaining() > 0 {
        let (flag, v) = reader.vertex(true)?;
        let triangle = match flag {
            0 => [v, reader.vertex(true)?.1, reader.vertex(true)?.1],
            1 => {
                let p = previous?;
                [p[1], p[2], v]
            }
            2 => {
                let p = previous?;
                [p[0], p[2], v]
            }
            _ => return None,
        };
        raster.triangle(triangle[0], triangle[1], triangle[2])?;
        previous = Some(triangle);
    }
    previous.map(|_| ())
}
fn lattice(reader: &mut MeshReader<'_>, raster: &mut Raster<'_>, columns: i64) -> Option<()> {
    if !(2..=8192).contains(&columns) {
        return None;
    }
    let columns = columns as usize;
    let mut previous = Vec::new();
    let mut rows = 0;
    while reader.bits.remaining() > 0 {
        let mut row = Vec::with_capacity(columns);
        for _ in 0..columns {
            row.push(reader.vertex(false)?.1);
        }
        if !previous.is_empty() {
            for i in 1..columns {
                raster.triangle(previous[i - 1], previous[i], row[i - 1])?;
                raster.triangle(previous[i], row[i], row[i - 1])?;
            }
        }
        previous = row;
        rows += 1;
    }
    (rows >= 2).then_some(())
}
#[derive(Clone)]
struct Patch {
    points: [(f64, f64); 16],
    colors: [[f64; 4]; 4],
}
fn patches(reader: &mut MeshReader<'_>, raster: &mut Raster<'_>, kind: i64) -> Option<()> {
    let mut previous: Option<Patch> = None;
    let mut count = 0;
    while reader.bits.remaining()
        >= reader.flag_bits
            + 8 * reader.coordinate_bits
            + 2 * reader.components * reader.component_bits
    {
        count += 1;
        if count > 4096 {
            return None;
        }
        let flag = reader.bits.read(reader.flag_bits)? as usize;
        let mut patch = Patch {
            points: [(0.0, 0.0); 16],
            colors: [[0.0; 4]; 4],
        };
        let first = match flag {
            0 => 0,
            1..=3 => {
                let p = previous.as_ref()?;
                for i in 0..4 {
                    patch.points[i] = p.points[(flag * 3 + i) % 12];
                }
                patch.colors[0] = p.colors[flag];
                patch.colors[1] = p.colors[(flag + 1) % 4];
                4
            }
            _ => return None,
        };
        for i in first..if kind == 6 { 12 } else { 16 } {
            patch.points[i] = reader.point()?;
        }
        for i in if first == 0 { 0 } else { 2 }..4 {
            patch.colors[i] = reader.color()?;
        }
        let subdivisions = patch_steps(&patch, kind);
        let mut previous_row = Vec::new();
        for y in 0..=subdivisions {
            let row: Vec<_> = (0..=subdivisions)
                .map(|x| {
                    patch_vertex(
                        &patch,
                        kind,
                        x as f64 / subdivisions as f64,
                        y as f64 / subdivisions as f64,
                    )
                })
                .collect();
            if y != 0 {
                for i in 1..=subdivisions {
                    raster.triangle(previous_row[i - 1], previous_row[i], row[i - 1])?;
                    raster.triangle(previous_row[i], row[i], row[i - 1])?;
                }
            }
            previous_row = row;
        }
        previous = Some(patch);
    }
    (count > 0 && reader.bits.remaining() < 8).then_some(())
}
fn patch_steps(p: &Patch, kind: i64) -> usize {
    let mut bend: f64 = 0.0;
    let grid = [[0, 1, 2, 3], [11, 12, 13, 4], [10, 15, 14, 5], [9, 8, 7, 6]];
    if kind == 7 {
        for i in 0..4 {
            for j in 0..2 {
                for ids in [
                    [grid[i][j], grid[i][j + 1], grid[i][j + 2]],
                    [grid[j][i], grid[j + 1][i], grid[j + 2][i]],
                ] {
                    let a = p.points[ids[0]];
                    let b = p.points[ids[1]];
                    let c = p.points[ids[2]];
                    bend = bend.max((a.0 - 2.0 * b.0 + c.0).hypot(a.1 - 2.0 * b.1 + c.1));
                }
            }
        }
    } else {
        for edge in 0..4 {
            for i in 0..2 {
                let a = p.points[(edge * 3 + i) % 12];
                let b = p.points[(edge * 3 + i + 1) % 12];
                let c = p.points[(edge * 3 + i + 2) % 12];
                bend = bend.max((a.0 - 2.0 * b.0 + c.0).hypot(a.1 - 2.0 * b.1 + c.1));
            }
        }
    }
    (bend.mul_add(24.0, 0.0).sqrt().ceil() as usize).clamp(16, 128)
}
fn bernstein(t: f64) -> [f64; 4] {
    let s = 1.0 - t;
    [s * s * s, 3.0 * s * s * t, 3.0 * s * t * t, t * t * t]
}
fn patch_vertex(p: &Patch, kind: i64, u: f64, v: f64) -> Vertex {
    let bu = bernstein(u);
    let bv = bernstein(v);
    let mut point = (0.0, 0.0);
    if kind == 7 {
        let grid = [[0, 1, 2, 3], [11, 12, 13, 4], [10, 15, 14, 5], [9, 8, 7, 6]];
        for y in 0..4 {
            for x in 0..4 {
                let c = p.points[grid[y][x]];
                point.0 += c.0 * bu[x] * bv[y];
                point.1 += c.1 * bu[x] * bv[y];
            }
        }
    } else {
        for i in 0..4 {
            for (id, w) in [
                (i, bu[i] * (1.0 - v)),
                (9 - i, bu[i] * v),
                ((12 - i) % 12, bv[i] * (1.0 - u)),
                (3 + i, bv[i] * u),
            ] {
                point.0 += p.points[id].0 * w;
                point.1 += p.points[id].1 * w;
            }
        }
        for (id, w) in [
            (0, (1.0 - u) * (1.0 - v)),
            (3, u * (1.0 - v)),
            (6, u * v),
            (9, (1.0 - u) * v),
        ] {
            point.0 -= p.points[id].0 * w;
            point.1 -= p.points[id].1 * w;
        }
    }
    let weights = [(1.0 - u) * (1.0 - v), u * (1.0 - v), u * v, (1.0 - u) * v];
    let mut color = [0.0; 4];
    for (i, w) in weights.iter().enumerate() {
        for (j, c) in color.iter_mut().enumerate() {
            *c += p.colors[i][j] * w;
        }
    }
    Vertex { p: point, c: color }
}
