//! The recognition network: model file parsing and inference.
//!
//! A CNN (3x3 convolutions with ReLU and max-pooling) turns a normalized line
//! into a sequence of feature columns; a bidirectional LSTM reads them in both
//! directions; a linear layer scores every character class per column. This
//! is a port of `reference_logits` in `tools/ocr_train/export.py`, and
//! `tests/golden.rs` checks it against PyTorch's output.
//!
//! The model file is untrusted input as far as this code is concerned: every
//! size, shape and offset is validated before anything is allocated from it.

use std::fmt;

use serde::Deserialize;

const MAGIC: &[u8; 8] = b"PDFOCRM1";
const MAX_CHANNELS: usize = 1024;
const MAX_HIDDEN: usize = 2048;
const MAX_CLASSES: usize = 65_536;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelError(pub String);

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid OCR model: {}", self.0)
    }
}

impl std::error::Error for ModelError {}

fn invalid<T>(message: impl Into<String>) -> Result<T, ModelError> {
    Err(ModelError(message.into()))
}

#[derive(Deserialize)]
struct Header {
    format: u32,
    arch: String,
    #[serde(default)]
    name: String,
    height: usize,
    stride: usize,
    trailing_paper: usize,
    charset: String,
    convs: Vec<[usize; 2]>,
    pools: Vec<Option<[usize; 2]>>,
    hidden: usize,
    tensors: Vec<TensorEntry>,
}

#[derive(Deserialize)]
struct TensorEntry {
    name: String,
    shape: Vec<usize>,
    offset: usize,
}

struct Conv {
    inputs: usize,
    outputs: usize,
    /// [outputs][inputs * 9], row-major 3x3 kernels.
    weight: Vec<f32>,
    bias: Vec<f32>,
    pool: Option<(usize, usize)>,
}

struct Lstm {
    /// [4 * hidden][features], gates in PyTorch order: input, forget, cell, output.
    w_ih: Vec<f32>,
    /// [4 * hidden][hidden]
    w_hh: Vec<f32>,
    /// Input and recurrent biases, summed.
    bias: Vec<f32>,
}

pub struct Network {
    pub name: String,
    /// Rows of every input line.
    pub height: usize,
    /// Input columns per output step.
    pub stride: usize,
    /// Columns of blank paper to append after a line (see the training notes).
    pub trailing_paper: usize,
    /// Class `i + 1` is `charset[i]`; class 0 is the CTC blank.
    pub charset: Vec<char>,
    convs: Vec<Conv>,
    features: usize,
    hidden: usize,
    lstm: [Lstm; 2],
    head_weight: Vec<f32>,
    head_bias: Vec<f32>,
}

impl Network {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ModelError> {
        if bytes.len() < 12 || &bytes[..8] != MAGIC {
            return invalid("not a PDFOCRM1 model file");
        }
        let header_len = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        let header_end = 12usize
            .checked_add(header_len)
            .filter(|&end| end <= bytes.len());
        let Some(header_end) = header_end else {
            return invalid("header extends past the end of the file");
        };
        let header: Header = serde_json::from_slice(&bytes[12..header_end])
            .map_err(|e| ModelError(format!("header: {e}")))?;
        let data_start = header_end.div_ceil(16) * 16;
        let data = bytes.get(data_start..).unwrap_or(&[]);

        if header.format != 1 || header.arch != "cnn-bilstm" {
            return invalid(format!(
                "unsupported format {} / architecture {:?}",
                header.format, header.arch
            ));
        }
        let charset: Vec<char> = header.charset.chars().collect();
        let classes = charset.len() + 1;
        if charset.is_empty() || classes > MAX_CLASSES {
            return invalid("character set must have 1..65535 characters");
        }
        if header.convs.is_empty() || header.convs.len() != header.pools.len() {
            return invalid("every convolution needs a pooling entry");
        }
        if !(1..=MAX_HIDDEN).contains(&header.hidden) || header.trailing_paper > 4096 {
            return invalid("hidden size or trailing paper out of range");
        }
        let (mut rows, mut stride, mut channels) = (header.height, 1usize, 1usize);
        for (&[inputs, outputs], pool) in header.convs.iter().zip(&header.pools) {
            if inputs != channels || !(1..=MAX_CHANNELS).contains(&outputs) {
                return invalid("convolution channels do not chain");
            }
            channels = outputs;
            if let Some([ph, pw]) = *pool {
                if !(1..=4).contains(&ph) || !(1..=4).contains(&pw) || rows % ph != 0 {
                    return invalid("pooling does not divide the line height");
                }
                rows /= ph;
                stride *= pw;
            }
        }
        if !(8..=256).contains(&header.height) || rows == 0 || stride != header.stride {
            return invalid("height and stride do not match the layers");
        }
        let features = channels * rows;
        let hidden = header.hidden;

        let tensors = Tensors {
            entries: &header.tensors,
            data,
        };
        let mut convs = Vec::with_capacity(header.convs.len());
        for (i, (&[inputs, outputs], pool)) in header.convs.iter().zip(&header.pools).enumerate() {
            convs.push(Conv {
                inputs,
                outputs,
                weight: tensors.get(&format!("conv{i}.weight"), &[outputs, inputs, 3, 3])?,
                bias: tensors.get(&format!("conv{i}.bias"), &[outputs])?,
                pool: pool.map(|[ph, pw]| (ph, pw)),
            });
        }
        let lstm = |direction: &str| -> Result<Lstm, ModelError> {
            Ok(Lstm {
                w_ih: tensors.get(&format!("lstm.{direction}.w_ih"), &[4 * hidden, features])?,
                w_hh: tensors.get(&format!("lstm.{direction}.w_hh"), &[4 * hidden, hidden])?,
                bias: tensors.get(&format!("lstm.{direction}.bias"), &[4 * hidden])?,
            })
        };
        Ok(Self {
            name: header.name,
            height: header.height,
            stride,
            trailing_paper: header.trailing_paper,
            charset,
            convs,
            features,
            hidden,
            lstm: [lstm("forward")?, lstm("backward")?],
            head_weight: tensors.get("head.weight", &[classes, 2 * hidden])?,
            head_bias: tensors.get("head.bias", &[classes])?,
        })
    }

    pub fn classes(&self) -> usize {
        self.charset.len() + 1
    }

    /// Logits `[steps][classes]` for `height` rows of `width` floats, where
    /// `width` is a multiple of the stride.
    pub fn run(&self, input: &[f32], width: usize) -> Vec<f32> {
        assert!(width > 0 && width % self.stride == 0 && input.len() == self.height * width);
        let (mut rows, mut cols, mut channels) = (self.height, width, 1);
        let mut x = input.to_vec();
        let mut scratch = Vec::new();
        for conv in &self.convs {
            debug_assert_eq!(channels, conv.inputs);
            im2col(&x, channels, rows, cols, &mut scratch);
            let mut out = vec![0f32; conv.outputs * rows * cols];
            matmul_bias_relu(
                &conv.weight,
                &conv.bias,
                conv.inputs * 9,
                &scratch,
                rows * cols,
                &mut out,
            );
            channels = conv.outputs;
            x = out;
            if let Some((ph, pw)) = conv.pool {
                x = max_pool(&x, channels, rows, cols, ph, pw);
                rows /= ph;
                cols /= pw;
            }
        }
        // One feature column per step; feature index = channel * rows + row.
        let steps = cols;
        let mut sequence = vec![0f32; steps * self.features];
        for plane in 0..channels * rows {
            for step in 0..steps {
                sequence[step * self.features + plane] = x[plane * steps + step];
            }
        }
        let width2 = 2 * self.hidden;
        let mut hidden = vec![0f32; steps * width2];
        for (direction, lstm) in self.lstm.iter().enumerate() {
            lstm.run(
                &sequence,
                steps,
                self.features,
                self.hidden,
                direction == 1,
                &mut hidden,
                direction * self.hidden,
                width2,
            );
        }
        let classes = self.classes();
        let mut logits = vec![0f32; steps * classes];
        for step in 0..steps {
            let features = &hidden[step * width2..(step + 1) * width2];
            for class in 0..classes {
                logits[step * classes + class] = dot(
                    &self.head_weight[class * width2..(class + 1) * width2],
                    features,
                ) + self.head_bias[class];
            }
        }
        logits
    }
}

struct Tensors<'a> {
    entries: &'a [TensorEntry],
    data: &'a [u8],
}

impl Tensors<'_> {
    fn get(&self, name: &str, shape: &[usize]) -> Result<Vec<f32>, ModelError> {
        let Some(entry) = self.entries.iter().find(|t| t.name == name) else {
            return invalid(format!("missing tensor {name}"));
        };
        if entry.shape != shape {
            return invalid(format!(
                "tensor {name} has shape {:?}, expected {shape:?}",
                entry.shape
            ));
        }
        let count: usize = shape.iter().product();
        let bytes = count
            .checked_mul(2)
            .and_then(|len| entry.offset.checked_add(len).map(|end| (entry.offset, end)))
            .and_then(|(start, end)| self.data.get(start..end));
        let Some(bytes) = bytes else {
            return invalid(format!("tensor {name} extends past the end of the file"));
        };
        let values: Vec<f32> = bytes
            .chunks_exact(2)
            .map(|b| f16_to_f32(u16::from_le_bytes([b[0], b[1]])))
            .collect();
        if values.iter().any(|v| !v.is_finite()) {
            return invalid(format!("tensor {name} holds a non-finite weight"));
        }
        Ok(values)
    }
}

pub(crate) fn f16_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits & 0x8000) << 16;
    let exponent = u32::from((bits >> 10) & 0x1f);
    let fraction = u32::from(bits & 0x3ff);
    let magnitude = match exponent {
        0 if fraction == 0 => 0,
        0 => {
            // Subnormal: shift the fraction up until its leading bit is the
            // implicit one, lowering the exponent to match.
            let (mut f, mut e) = (fraction, 127 - 15 + 1);
            while f & 0x400 == 0 {
                f <<= 1;
                e -= 1;
            }
            (e << 23) | ((f & 0x3ff) << 13)
        }
        0x1f => 0x7f80_0000 | (fraction << 13),
        _ => ((exponent + 127 - 15) << 23) | (fraction << 13),
    };
    f32::from_bits(sign | magnitude)
}

/// 3x3, zero-padded patches: `cols[(c * 9 + ky * 3 + kx) * rows * width + y * width + x]`.
fn im2col(input: &[f32], channels: usize, rows: usize, width: usize, cols: &mut Vec<f32>) {
    let plane = rows * width;
    cols.clear();
    cols.resize(channels * 9 * plane, 0.0);
    for c in 0..channels {
        let source = &input[c * plane..(c + 1) * plane];
        for ky in 0..3 {
            for kx in 0..3 {
                let target = &mut cols[(c * 9 + ky * 3 + kx) * plane..][..plane];
                for y in 0..rows {
                    let Some(sy) = (y + ky).checked_sub(1).filter(|&sy| sy < rows) else {
                        continue;
                    };
                    let src = &source[sy * width..(sy + 1) * width];
                    let dst = &mut target[y * width..(y + 1) * width];
                    match kx {
                        0 => dst[1..].copy_from_slice(&src[..width - 1]),
                        1 => dst.copy_from_slice(src),
                        _ => dst[..width - 1].copy_from_slice(&src[1..]),
                    }
                }
            }
        }
    }
}

/// `out[o][j] = relu(bias[o] + sum_k weight[o][k] * cols[k][j])`, four output
/// rows at a time over column blocks that stay in cache.
fn matmul_bias_relu(
    weight: &[f32],
    bias: &[f32],
    k: usize,
    cols: &[f32],
    n: usize,
    out: &mut [f32],
) {
    const BLOCK: usize = 512;
    let outputs = bias.len();
    for start in (0..n).step_by(BLOCK) {
        let end = (start + BLOCK).min(n);
        let len = end - start;
        let mut o = 0;
        while o < outputs {
            let group = (outputs - o).min(4);
            let mut acc = [[0f32; BLOCK]; 4];
            for (g, row) in acc.iter_mut().enumerate().take(group) {
                row[..len].fill(bias[o + g]);
            }
            for kk in 0..k {
                let col = &cols[kk * n + start..kk * n + end];
                let w = |g: usize| {
                    if g < group {
                        weight[(o + g) * k + kk]
                    } else {
                        0.0
                    }
                };
                let (w0, w1, w2, w3) = (w(0), w(1), w(2), w(3));
                let [a0, a1, a2, a3] = &mut acc;
                for (j, &c) in col.iter().enumerate() {
                    a0[j] += w0 * c;
                    a1[j] += w1 * c;
                    a2[j] += w2 * c;
                    a3[j] += w3 * c;
                }
            }
            for (g, row) in acc.iter().enumerate().take(group) {
                let dst = &mut out[(o + g) * n + start..(o + g) * n + end];
                for (d, &v) in dst.iter_mut().zip(&row[..len]) {
                    *d = v.max(0.0);
                }
            }
            o += group;
        }
    }
}

fn max_pool(
    input: &[f32],
    channels: usize,
    rows: usize,
    width: usize,
    ph: usize,
    pw: usize,
) -> Vec<f32> {
    let (out_rows, out_width) = (rows / ph, width / pw);
    let mut out = vec![f32::NEG_INFINITY; channels * out_rows * out_width];
    for c in 0..channels {
        for y in 0..out_rows * ph {
            let src = &input[(c * rows + y) * width..][..out_width * pw];
            let dst = &mut out[(c * out_rows + y / ph) * out_width..][..out_width];
            for (x, &v) in src.iter().enumerate() {
                let slot = &mut dst[x / pw];
                *slot = slot.max(v);
            }
        }
    }
    out
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut lanes = [0f32; 8];
    let chunks = a.len() / 8;
    for i in 0..chunks {
        let (x, y) = (&a[i * 8..i * 8 + 8], &b[i * 8..i * 8 + 8]);
        for l in 0..8 {
            lanes[l] += x[l] * y[l];
        }
    }
    let mut sum: f32 = lanes.iter().sum();
    for i in chunks * 8..a.len() {
        sum += a[i] * b[i];
    }
    sum
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

impl Lstm {
    #[allow(clippy::too_many_arguments)]
    fn run(
        &self,
        sequence: &[f32],
        steps: usize,
        features: usize,
        hidden: usize,
        reverse: bool,
        out: &mut [f32],
        offset: usize,
        row: usize,
    ) {
        let gates_len = 4 * hidden;
        let mut projected = vec![0f32; steps * gates_len];
        for step in 0..steps {
            let x = &sequence[step * features..(step + 1) * features];
            for g in 0..gates_len {
                projected[step * gates_len + g] =
                    dot(&self.w_ih[g * features..(g + 1) * features], x) + self.bias[g];
            }
        }
        let (mut state, mut cell, mut gates) = (
            vec![0f32; hidden],
            vec![0f32; hidden],
            vec![0f32; gates_len],
        );
        for k in 0..steps {
            let step = if reverse { steps - 1 - k } else { k };
            for g in 0..gates_len {
                gates[g] = projected[step * gates_len + g]
                    + dot(&self.w_hh[g * hidden..(g + 1) * hidden], &state);
            }
            for j in 0..hidden {
                let input = sigmoid(gates[j]);
                let forget = sigmoid(gates[hidden + j]);
                let candidate = gates[2 * hidden + j].tanh();
                let output = sigmoid(gates[3 * hidden + j]);
                cell[j] = forget * cell[j] + input * candidate;
                state[j] = output * cell[j].tanh();
            }
            out[step * row + offset..step * row + offset + hidden].copy_from_slice(&state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_floats_decode_exactly() {
        for (bits, value) in [
            (0x3c00, 1.0f32),
            (0xc000, -2.0),
            (0x3555, 0.333_251_95),
            (0x7bff, 65504.0),
            (0x0001, 5.960_464_5e-8),
            (0x0200, 3.051_757_8e-5),
            (0x8000, -0.0),
        ] {
            assert_eq!(f16_to_f32(bits), value, "{bits:#06x}");
        }
        assert!(f16_to_f32(0x7c00).is_infinite() && f16_to_f32(0x7e00).is_nan());
    }

    #[test]
    fn convolution_and_pooling_match_a_naive_reference() {
        let (channels, rows, width, outputs) = (3, 4, 6, 5);
        let input: Vec<f32> = (0..channels * rows * width)
            .map(|i| ((i * 37 % 11) as f32 - 5.0) / 7.0)
            .collect();
        let weight: Vec<f32> = (0..outputs * channels * 9)
            .map(|i| ((i * 13 % 17) as f32 - 8.0) / 9.0)
            .collect();
        let bias: Vec<f32> = (0..outputs).map(|o| o as f32 * 0.1 - 0.2).collect();
        let mut cols = Vec::new();
        im2col(&input, channels, rows, width, &mut cols);
        let mut out = vec![0f32; outputs * rows * width];
        matmul_bias_relu(&weight, &bias, channels * 9, &cols, rows * width, &mut out);
        for o in 0..outputs {
            for y in 0..rows as isize {
                for x in 0..width as isize {
                    let mut sum = bias[o];
                    for c in 0..channels {
                        for ky in -1..=1isize {
                            for kx in -1..=1isize {
                                let (sy, sx) = (y + ky, x + kx);
                                if sy >= 0 && sx >= 0 && sy < rows as isize && sx < width as isize {
                                    let w = weight[o * channels * 9
                                        + c * 9
                                        + ((ky + 1) * 3 + kx + 1) as usize];
                                    sum +=
                                        w * input[(c * rows + sy as usize) * width + sx as usize];
                                }
                            }
                        }
                    }
                    let got = out[(o * rows + y as usize) * width + x as usize];
                    assert!(
                        (got - sum.max(0.0)).abs() < 1e-5,
                        "o{o} y{y} x{x}: {got} vs {sum}"
                    );
                }
            }
        }
        let pooled = max_pool(&out, outputs, rows, width, 2, 3);
        assert_eq!(pooled.len(), outputs * 2 * 2);
        let expected = (0..2)
            .flat_map(|y| (0..3).map(move |x| (y, x)))
            .map(|(y, x)| out[y * width + x])
            .fold(f32::MIN, f32::max);
        assert_eq!(pooled[0], expected);
    }

    #[test]
    fn malformed_model_files_are_rejected() {
        assert!(Network::from_bytes(b"not a model").is_err());
        let mut truncated = MAGIC.to_vec();
        truncated.extend_from_slice(&1000u32.to_le_bytes());
        truncated.extend_from_slice(b"{}");
        let error = Network::from_bytes(&truncated)
            .err()
            .expect("a truncated header is rejected");
        assert!(error.0.contains("header extends"), "{error}");
    }
}
