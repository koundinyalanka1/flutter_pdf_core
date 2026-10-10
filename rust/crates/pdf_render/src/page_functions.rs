//! Bounded PDF functions (ISO 32000-1, 7.10). Functions are parsed once and
//! evaluated without executing PostScript outside the type-4 operator subset.
use super::*;

#[derive(Clone)]
pub(in crate::page) struct ColorFunction {
    domain: Vec<f64>,
    range: Vec<f64>,
    outputs: usize,
    kind: Kind,
}
#[derive(Clone)]
enum Kind {
    Exponential {
        c0: Vec<f64>,
        c1: Vec<f64>,
        n: f64,
    },
    Stitch {
        functions: Vec<ColorFunction>,
        bounds: Vec<f64>,
        encode: Vec<f64>,
    },
    Components(Vec<ColorFunction>),
    Sampled {
        sizes: Vec<usize>,
        samples: Vec<f64>,
        encode: Vec<f64>,
        decode: Vec<f64>,
        cubic: bool,
    },
    Calculator(Vec<Token>),
}

impl ColorFunction {
    pub(in crate::page) fn load(
        doc: &PdfDocument,
        object: &PdfObject,
        depth: usize,
        remaining: &mut usize,
    ) -> Option<Self> {
        if depth > 12 || *remaining == 0 {
            return None;
        }
        *remaining -= 1;
        let object = doc.resolve_value(object);
        if let PdfObject::Array(items) = &object {
            if items.is_empty() || items.len() > 32 {
                return None;
            }
            let functions = items
                .iter()
                .map(|o| Self::load(doc, o, depth + 1, remaining))
                .collect::<Option<Vec<_>>>()?;
            if functions
                .iter()
                .any(|f| f.inputs() != functions[0].inputs() || f.outputs != 1)
            {
                return None;
            }
            return Some(Self {
                domain: functions[0].domain.clone(),
                range: Vec::new(),
                outputs: functions.len(),
                kind: Kind::Components(functions),
            });
        }
        let dict = object.as_dict()?;
        let domain = numbers(doc, dict.get("Domain"));
        if domain.is_empty() || domain.len() > 16 || !valid_pairs(&domain) {
            return None;
        }
        let range = numbers(doc, dict.get("Range"));
        if dict.contains_key("Range")
            && (range.is_empty() || range.len() > 64 || !valid_pairs(&range))
        {
            return None;
        }
        let inputs = domain.len() / 2;
        let (kind, outputs) = match dict.get("FunctionType").and_then(PdfObject::as_i64)? {
            0 => {
                let PdfObject::Stream(stream) = &object else {
                    return None;
                };
                if range.is_empty() {
                    return None;
                }
                let outputs = range.len() / 2;
                let raw_sizes = numbers(doc, dict.get("Size"));
                if raw_sizes.len() != inputs
                    || raw_sizes
                        .iter()
                        .any(|n| *n < 1.0 || *n > 4_000_000.0 || n.fract() != 0.0 || !n.is_finite())
                {
                    return None;
                }
                let sizes: Vec<usize> = raw_sizes.iter().map(|n| *n as usize).collect();
                let count = sizes.iter().try_fold(outputs, |a, b| a.checked_mul(*b))?;
                if count > 4_000_000 {
                    return None;
                }
                let allocation_units = count.div_ceil(1024);
                if allocation_units > *remaining {
                    return None;
                }
                *remaining -= allocation_units;
                let bits = dict.get("BitsPerSample").and_then(PdfObject::as_i64)? as usize;
                if !matches!(bits, 1 | 2 | 4 | 8 | 12 | 16 | 24 | 32) {
                    return None;
                }
                let cubic = match dict.get("Order").and_then(PdfObject::as_i64).unwrap_or(1) {
                    1 => false,
                    3 => true,
                    _ => return None,
                };
                let combinations = sizes.iter().try_fold(1usize, |n, size| {
                    n.checked_mul(if cubic && *size >= 4 { 4 } else { 2 })
                })?;
                if combinations > 4096 {
                    return None;
                }
                let encode = if dict.contains_key("Encode") {
                    numbers(doc, dict.get("Encode"))
                } else {
                    sizes.iter().flat_map(|n| [0.0, (n - 1) as f64]).collect()
                };
                let decode = if dict.contains_key("Decode") {
                    numbers(doc, dict.get("Decode"))
                } else {
                    range.clone()
                };
                if encode.len() != inputs * 2
                    || decode.len() != outputs * 2
                    || encode.iter().chain(&decode).any(|n| !n.is_finite())
                {
                    return None;
                }
                let bytes = doc.stream_data(stream).ok()?;
                let mut reader = Bits::new(&bytes);
                let max = ((1u64 << bits) - 1) as f64;
                let samples = (0..count)
                    .map(|_| reader.read(bits).map(|n| n as f64 / max))
                    .collect::<Option<Vec<_>>>()?;
                (
                    Kind::Sampled {
                        sizes,
                        samples,
                        encode,
                        decode,
                        cubic,
                    },
                    outputs,
                )
            }
            2 => {
                if inputs != 1 {
                    return None;
                }
                let n = dict.get("N").and_then(as_number)?;
                let c0 = if dict.contains_key("C0") {
                    numbers(doc, dict.get("C0"))
                } else {
                    vec![0.0]
                };
                let c1 = if dict.contains_key("C1") {
                    numbers(doc, dict.get("C1"))
                } else {
                    vec![1.0]
                };
                if c0.is_empty()
                    || c0.len() != c1.len()
                    || c0.len() > 32
                    || !n.is_finite()
                    || c0.iter().chain(&c1).any(|n| !n.is_finite())
                    || (domain[0] < 0.0 && n.fract() != 0.0)
                    || (domain[0] <= 0.0 && domain[1] >= 0.0 && n < 0.0)
                {
                    return None;
                }
                let outputs = c0.len();
                (Kind::Exponential { c0, c1, n }, outputs)
            }
            3 => {
                if inputs != 1 || domain[0] >= domain[1] {
                    return None;
                }
                let PdfObject::Array(items) = doc.resolve_value(dict.get("Functions")?) else {
                    return None;
                };
                if items.is_empty() || items.len() > 256 {
                    return None;
                }
                let functions = items
                    .iter()
                    .map(|o| Self::load(doc, o, depth + 1, remaining))
                    .collect::<Option<Vec<_>>>()?;
                let bounds = numbers(doc, dict.get("Bounds"));
                let encode = numbers(doc, dict.get("Encode"));
                if bounds.len() + 1 != functions.len()
                    || encode.len() != functions.len() * 2
                    || bounds.iter().chain(&encode).any(|n| !n.is_finite())
                    || bounds.windows(2).any(|b| b[0] >= b[1])
                    || bounds.iter().any(|b| *b <= domain[0] || *b >= domain[1])
                    || functions
                        .iter()
                        .any(|f| f.inputs() != 1 || f.outputs != functions[0].outputs)
                {
                    return None;
                }
                let outputs = functions[0].outputs;
                (
                    Kind::Stitch {
                        functions,
                        bounds,
                        encode,
                    },
                    outputs,
                )
            }
            4 => {
                if range.is_empty() {
                    return None;
                }
                let PdfObject::Stream(stream) = &object else {
                    return None;
                };
                let bytes = doc.stream_data(stream).ok()?;
                if bytes.len() > 64 * 1024 {
                    return None;
                }
                let tokens = parse_program(&bytes)?;
                (Kind::Calculator(tokens), range.len() / 2)
            }
            _ => return None,
        };
        if !range.is_empty() && range.len() != outputs * 2 {
            return None;
        }
        Some(Self {
            domain,
            range,
            outputs,
            kind,
        })
    }
    pub(super) fn inputs(&self) -> usize {
        self.domain.len() / 2
    }
    pub(super) fn outputs(&self) -> usize {
        self.outputs
    }
    pub(super) fn evaluate(&self, t: f64) -> Option<Vec<f64>> {
        self.evaluate_inputs(&[t])
    }
    pub(in crate::page) fn evaluate_inputs(&self, values: &[f64]) -> Option<Vec<f64>> {
        self.evaluate_inputs_bounded(values, &mut 16_384)
    }
    pub(super) fn evaluate_inputs_bounded(
        &self,
        values: &[f64],
        budget: &mut usize,
    ) -> Option<Vec<f64>> {
        if *budget == 0 {
            return None;
        }
        *budget -= 1;
        if values.len() != self.inputs() || values.iter().any(|v| !v.is_finite()) {
            return None;
        }
        let v: Vec<f64> = values
            .iter()
            .zip(self.domain.chunks_exact(2))
            .map(|(v, d)| v.clamp(d[0], d[1]))
            .collect();
        let mut result = match &self.kind {
            Kind::Exponential { c0, c1, n } => {
                let t = v[0].powf(*n);
                c0.iter().zip(c1).map(|(a, b)| a + t * (b - a)).collect()
            }
            Kind::Stitch {
                functions,
                bounds,
                encode,
            } => {
                let i = bounds
                    .partition_point(|b| *b <= v[0])
                    .min(functions.len() - 1);
                let lo = if i == 0 {
                    self.domain[0]
                } else {
                    bounds[i - 1]
                };
                let hi = if i == bounds.len() {
                    self.domain[1]
                } else {
                    bounds[i]
                };
                functions[i].evaluate_inputs_bounded(
                    &[encode[i * 2]
                        + (v[0] - lo) / (hi - lo) * (encode[i * 2 + 1] - encode[i * 2])],
                    budget,
                )?
            }
            Kind::Components(items) => items
                .iter()
                .map(|f| f.evaluate_inputs_bounded(&v, budget))
                .collect::<Option<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect(),
            Kind::Sampled {
                sizes,
                samples,
                encode,
                decode,
                cubic,
            } => {
                let mut terms = vec![(0usize, 1.0f64)];
                let mut stride = self.outputs;
                for (dim, size) in sizes.iter().enumerate() {
                    let lo = self.domain[dim * 2];
                    let hi = self.domain[dim * 2 + 1];
                    let t = if hi == lo {
                        encode[dim * 2]
                    } else {
                        encode[dim * 2]
                            + (v[dim] - lo) / (hi - lo) * (encode[dim * 2 + 1] - encode[dim * 2])
                    }
                    .clamp(0.0, (size - 1) as f64);
                    let base = t.floor() as isize;
                    let f = t - t.floor();
                    let weights = if *cubic && *size >= 4 {
                        // Interpolating cardinal cubic spline, with endpoint extension.
                        vec![
                            (base - 1, -0.5 * f + f * f - 0.5 * f * f * f),
                            (base, 1.0 - 2.5 * f * f + 1.5 * f * f * f),
                            (base + 1, 0.5 * f + 2.0 * f * f - 1.5 * f * f * f),
                            (base + 2, -0.5 * f * f + 0.5 * f * f * f),
                        ]
                    } else {
                        vec![(base, 1.0 - f), (base + 1, f)]
                    };
                    let cost = terms.len().checked_mul(weights.len())?;
                    if cost > *budget {
                        return None;
                    }
                    *budget -= cost;
                    terms = terms
                        .iter()
                        .flat_map(|(offset, weight)| {
                            weights.iter().map(move |(index, w)| {
                                (
                                    offset
                                        + (*index).clamp(0, *size as isize - 1) as usize * stride,
                                    weight * w,
                                )
                            })
                        })
                        .collect();
                    stride *= size;
                }
                (0..self.outputs)
                    .map(|c| {
                        let n: f64 = terms.iter().map(|(i, w)| samples[i + c] * w).sum();
                        decode[c * 2] + n * (decode[c * 2 + 1] - decode[c * 2])
                    })
                    .collect()
            }
            Kind::Calculator(program) => {
                let mut stack: Vec<Value> = v.into_iter().map(Value::Real).collect();
                execute(program, &mut stack, budget, 0)?;
                if stack.len() != self.outputs {
                    return None;
                }
                stack
                    .iter()
                    .map(Value::number)
                    .collect::<Option<Vec<_>>>()?
            }
        };
        if result.len() != self.outputs || result.iter().any(|v| !v.is_finite()) {
            return None;
        }
        for (v, range) in result.iter_mut().zip(self.range.chunks_exact(2)) {
            *v = v.clamp(range[0], range[1]);
        }
        Some(result)
    }
}
fn valid_pairs(v: &[f64]) -> bool {
    v.len() % 2 == 0 && v.iter().all(|n| n.is_finite()) && v.chunks_exact(2).all(|r| r[0] <= r[1])
}

pub(super) struct Bits<'a> {
    bytes: &'a [u8],
    pub(super) position: usize,
}
impl<'a> Bits<'a> {
    pub(super) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }
    pub(super) fn remaining(&self) -> usize {
        self.bytes.len() * 8 - self.position
    }
    pub(super) fn align(&mut self) {
        self.position = self.position.div_ceil(8) * 8;
    }
    pub(super) fn read(&mut self, count: usize) -> Option<u32> {
        if count > 32 || self.remaining() < count {
            return None;
        }
        let mut n = 0;
        for _ in 0..count {
            n = (n << 1) | ((self.bytes[self.position / 8] >> (7 - self.position % 8)) & 1) as u32;
            self.position += 1;
        }
        Some(n)
    }
}

#[derive(Clone)]
enum Token {
    Value(Value),
    Op(String),
}
#[derive(Clone)]
enum Value {
    Int(i32),
    Real(f64),
    Bool(bool),
    Proc(Rc<Vec<Token>>),
}
impl Value {
    fn number(&self) -> Option<f64> {
        match self {
            Self::Int(n) => Some(*n as f64),
            Self::Real(n) => Some(*n),
            _ => None,
        }
    }
    fn int(&self) -> Option<i32> {
        match self {
            Self::Int(n) => Some(*n),
            _ => None,
        }
    }
    fn boolean(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }
}
fn parse_program(bytes: &[u8]) -> Option<Vec<Token>> {
    fn procedure(
        bytes: &[u8],
        p: &mut usize,
        depth: usize,
        budget: &mut usize,
    ) -> Option<Vec<Token>> {
        if depth > 32 {
            return None;
        }
        let mut tokens = Vec::new();
        loop {
            skip(bytes, p);
            let c = *bytes.get(*p)?;
            *p += 1;
            if c == b'}' {
                return Some(tokens);
            }
            if *budget == 0 {
                return None;
            }
            *budget -= 1;
            if c == b'{' {
                tokens.push(Token::Value(Value::Proc(Rc::new(procedure(
                    bytes,
                    p,
                    depth + 1,
                    budget,
                )?))));
                continue;
            }
            let start = *p - 1;
            while *p < bytes.len()
                && !bytes[*p].is_ascii_whitespace()
                && !b"{}%".contains(&bytes[*p])
            {
                *p += 1;
            }
            let s = std::str::from_utf8(&bytes[start..*p]).ok()?;
            let token = if let Ok(n) = s.parse::<i32>() {
                Token::Value(Value::Int(n))
            } else if let Ok(n) = s.parse::<f64>() {
                if !n.is_finite() {
                    return None;
                }
                Token::Value(Value::Real(n))
            } else if s == "true" || s == "false" {
                Token::Value(Value::Bool(s == "true"))
            } else if matches!(
                s,
                "abs"
                    | "add"
                    | "atan"
                    | "ceiling"
                    | "cos"
                    | "cvi"
                    | "cvr"
                    | "div"
                    | "exp"
                    | "floor"
                    | "idiv"
                    | "ln"
                    | "log"
                    | "mod"
                    | "mul"
                    | "neg"
                    | "round"
                    | "sin"
                    | "sqrt"
                    | "sub"
                    | "truncate"
                    | "and"
                    | "bitshift"
                    | "eq"
                    | "ge"
                    | "gt"
                    | "le"
                    | "lt"
                    | "ne"
                    | "not"
                    | "or"
                    | "xor"
                    | "if"
                    | "ifelse"
                    | "copy"
                    | "dup"
                    | "exch"
                    | "index"
                    | "pop"
                    | "roll"
            ) {
                Token::Op(s.to_owned())
            } else {
                return None;
            };
            tokens.push(token);
        }
    }
    fn skip(bytes: &[u8], p: &mut usize) {
        loop {
            while bytes.get(*p).is_some_and(u8::is_ascii_whitespace) {
                *p += 1;
            }
            if bytes.get(*p) != Some(&b'%') {
                break;
            }
            while bytes.get(*p).is_some_and(|c| *c != b'\n' && *c != b'\r') {
                *p += 1;
            }
        }
    }
    let mut p = 0;
    skip(bytes, &mut p);
    if bytes.get(p) != Some(&b'{') {
        return None;
    }
    p += 1;
    let result = procedure(bytes, &mut p, 0, &mut 4096)?;
    skip(bytes, &mut p);
    (p == bytes.len()).then_some(result)
}
fn execute(
    program: &[Token],
    stack: &mut Vec<Value>,
    budget: &mut usize,
    depth: usize,
) -> Option<()> {
    if depth > 32 {
        return None;
    }
    for token in program {
        if *budget == 0 || stack.len() > 256 {
            return None;
        }
        *budget -= 1;
        let Token::Op(op) = token else {
            if let Token::Value(v) = token {
                stack.push(v.clone());
            }
            continue;
        };
        match op.as_str() {
            "dup" => stack.push(stack.last()?.clone()),
            "exch" => {
                let n = stack.len();
                if n < 2 {
                    return None;
                }
                stack.swap(n - 1, n - 2);
            }
            "pop" => {
                stack.pop()?;
            }
            "copy" => {
                let n = stack.pop()?.int()?;
                if n < 0 || n as usize > stack.len() || stack.len() + n as usize > 256 {
                    return None;
                }
                stack.extend_from_within(stack.len() - n as usize..);
            }
            "index" => {
                let n = stack.pop()?.int()?;
                if n < 0 || n as usize >= stack.len() {
                    return None;
                }
                stack.push(stack[stack.len() - 1 - n as usize].clone());
            }
            "roll" => {
                let j = stack.pop()?.int()?;
                let n = stack.pop()?.int()?;
                if n < 0 || n as usize > stack.len() {
                    return None;
                }
                if n != 0 {
                    let start = stack.len() - n as usize;
                    stack[start..].rotate_right(j.rem_euclid(n) as usize);
                }
            }
            "if" | "ifelse" => {
                let otherwise = if op == "ifelse" {
                    Some(stack.pop()?)
                } else {
                    None
                };
                let yes = stack.pop()?;
                let condition = stack.pop()?.boolean()?;
                let Value::Proc(yes) = yes else {
                    return None;
                };
                let no = if let Some(v) = otherwise {
                    let Value::Proc(p) = v else {
                        return None;
                    };
                    Some(p)
                } else {
                    None
                };
                if condition {
                    execute(&yes, stack, budget, depth + 1)?;
                } else if let Some(no) = no {
                    execute(&no, stack, budget, depth + 1)?;
                }
            }
            "not" => {
                let v = match stack.pop()? {
                    Value::Int(n) => Value::Int(!n),
                    Value::Bool(b) => Value::Bool(!b),
                    _ => return None,
                };
                stack.push(v);
            }
            "and" | "or" | "xor" => {
                let b = stack.pop()?;
                let a = stack.pop()?;
                stack.push(match (a, b) {
                    (Value::Int(a), Value::Int(b)) => Value::Int(match op.as_str() {
                        "and" => a & b,
                        "or" => a | b,
                        _ => a ^ b,
                    }),
                    (Value::Bool(a), Value::Bool(b)) => Value::Bool(match op.as_str() {
                        "and" => a & b,
                        "or" => a | b,
                        _ => a ^ b,
                    }),
                    _ => return None,
                });
            }
            "bitshift" => {
                let shift = stack.pop()?.int()?;
                let n = stack.pop()?.int()?;
                let result = if shift.unsigned_abs() >= 32 {
                    0
                } else if shift >= 0 {
                    n.wrapping_shl(shift as u32)
                } else {
                    ((n as u32) >> -shift) as i32
                };
                stack.push(Value::Int(result));
            }
            "eq" | "ne" | "gt" | "ge" | "lt" | "le" => {
                let b = stack.pop()?;
                let a = stack.pop()?;
                let result = if let (Some(a), Some(b)) = (a.number(), b.number()) {
                    match op.as_str() {
                        "eq" => a == b,
                        "ne" => a != b,
                        "gt" => a > b,
                        "ge" => a >= b,
                        "lt" => a < b,
                        _ => a <= b,
                    }
                } else if matches!(op.as_str(), "eq" | "ne") {
                    let equal = matches!((a,b),(Value::Bool(a),Value::Bool(b)) if a==b);
                    if op == "eq" {
                        equal
                    } else {
                        !equal
                    }
                } else {
                    return None;
                };
                stack.push(Value::Bool(result));
            }
            "add" | "sub" | "mul" | "div" | "idiv" | "mod" | "exp" | "atan" => {
                let b = stack.pop()?;
                let a = stack.pop()?;
                let x = a.number()?;
                let y = b.number()?;
                if op == "mod" {
                    stack.push(Value::Int(a.int()?.checked_rem(b.int()?)?));
                    continue;
                }
                if op == "idiv" {
                    let n = (x / y).trunc();
                    if !n.is_finite() || n < i32::MIN as f64 || n > i32::MAX as f64 {
                        return None;
                    }
                    stack.push(Value::Int(n as i32));
                    continue;
                }
                let n = match op.as_str() {
                    "add" => x + y,
                    "sub" => x - y,
                    "mul" => x * y,
                    "div" => x / y,
                    "exp" => x.powf(y),
                    _ => {
                        if x == 0.0 && y == 0.0 {
                            return None;
                        }
                        x.atan2(y).to_degrees().rem_euclid(360.0)
                    }
                };
                if !n.is_finite() {
                    return None;
                }
                let integer = matches!(op.as_str(), "add" | "sub" | "mul")
                    && a.int().is_some()
                    && b.int().is_some()
                    && n >= i32::MIN as f64
                    && n <= i32::MAX as f64;
                stack.push(if integer {
                    Value::Int(n as i32)
                } else {
                    Value::Real(n)
                });
            }
            _ => {
                let a = stack.pop()?;
                let x = a.number()?;
                let n = match op.as_str() {
                    "abs" => x.abs(),
                    "ceiling" => x.ceil(),
                    "cos" => x.to_radians().cos(),
                    "cvi" => x.trunc(),
                    "cvr" => x,
                    "floor" => x.floor(),
                    "ln" => x.ln(),
                    "log" => x.log10(),
                    "neg" => -x,
                    "round" => (x + 0.5).floor(),
                    "sin" => x.to_radians().sin(),
                    "sqrt" => x.sqrt(),
                    "truncate" => x.trunc(),
                    _ => return None,
                };
                if !n.is_finite() {
                    return None;
                }
                let integer = op == "cvi"
                    || (a.int().is_some()
                        && matches!(
                            op.as_str(),
                            "abs" | "neg" | "ceiling" | "floor" | "round" | "truncate"
                        ));
                if op == "cvi" && (n < i32::MIN as f64 || n > i32::MAX as f64) {
                    return None;
                }
                stack.push(if integer && n >= i32::MIN as f64 && n <= i32::MAX as f64 {
                    Value::Int(n as i32)
                } else {
                    Value::Real(n)
                });
            }
        }
    }
    (stack.len() <= 256).then_some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdf_core::parser::Parser;
    fn function(dictionary: &str, bytes: &[u8]) -> ColorFunction {
        let doc = super::super::super::tests::doc_with_content("", [0, 0, 1, 1]);
        let dict = Parser::new(dictionary.as_bytes())
            .parse_object()
            .unwrap()
            .as_dict()
            .unwrap()
            .clone();
        ColorFunction::load(
            &doc,
            &PdfObject::Stream(PdfStream::new(dict, bytes.to_vec())),
            0,
            &mut 4096,
        )
        .unwrap()
    }
    #[test]
    fn sampled_multidimensional_order_and_reversed_decode() {
        let f=function("<< /FunctionType 0 /Domain [0 1 0 1] /Range [0 1] /Size [2 2] /BitsPerSample 8 /Decode [1 0] >>", &[0,64,128,255]);
        assert!((f.evaluate_inputs(&[1.0, 0.0]).unwrap()[0] - 191.0 / 255.0).abs() < 1e-9);
        assert!((f.evaluate_inputs(&[0.5, 0.5]).unwrap()[0] - (1.0 - 447.0 / 1020.0)).abs() < 1e-9);
        assert_eq!(f.evaluate_inputs(&[5.0, 5.0]).unwrap(), vec![0.0]);
    }
    #[test]
    fn sampled_packed_twelve_bit_and_cubic_samples() {
        let f = function(
            "<< /FunctionType 0 /Domain [0 1] /Range [0 1] /Size [2] /BitsPerSample 12 >>",
            &[0, 0x0f, 0xff],
        );
        assert_eq!(f.evaluate(0.5).unwrap(), vec![0.5]);
        let f = function(
            "<< /FunctionType 0 /Domain [0 3] /Range [0 1] /Size [4] /BitsPerSample 8 /Order 3 >>",
            &[0, 85, 170, 255],
        );
        assert!((f.evaluate(1.5).unwrap()[0] - 0.5).abs() < 1e-9);
    }
    #[test]
    fn calculator_conditional_stack_and_arithmetic() {
        let f=function("<< /FunctionType 4 /Domain [0 1] /Range [0 1 0 1 0 1] >>", b"{ % RGB ramp\n dup 0.5 lt { 2 mul dup 0 exch 1 exch sub } { 1 exch sub 2 mul 0 1 } ifelse }");
        assert_eq!(f.evaluate(0.25).unwrap(), vec![0.5, 0.0, 0.5]);
        assert_eq!(f.evaluate(0.75).unwrap(), vec![0.5, 0.0, 1.0]);
        let f = function(
            "<< /FunctionType 4 /Domain [0 1] /Range [0 100] >>",
            b"{ pop 8 1 bitshift 3 xor 1 add 2 idiv 3 mod }",
        );
        assert_eq!(f.evaluate(0.0).unwrap(), vec![1.0]);
    }
    #[test]
    fn calculator_invalid_programs_fail_without_panics() {
        let f = function(
            "<< /FunctionType 4 /Domain [0 1] /Range [0 1] >>",
            b"{ 0 div }",
        );
        assert!(f.evaluate(1.0).is_none());
        let f = function(
            "<< /FunctionType 4 /Domain [0 1] /Range [0 1] >>",
            b"{ -1 index }",
        );
        assert!(f.evaluate(0.5).is_none());
        assert!(parse_program(b"{ file }").is_none());
        assert!(parse_program(b"{ 1 } trailing").is_none());
    }
}
