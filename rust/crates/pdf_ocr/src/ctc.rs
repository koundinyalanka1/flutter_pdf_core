//! Best-path CTC decoding: the most likely class per step, repeats merged,
//! blanks dropped, with where along the line each character was read.

/// One decoded character and the output steps that produced it.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedChar {
    pub ch: char,
    pub first_step: usize,
    pub last_step: usize,
    /// Highest probability the network gave this character on its steps.
    pub confidence: f32,
}

/// Decode the first `steps` rows of `logits` (`[steps][charset.len() + 1]`).
pub fn greedy(logits: &[f32], steps: usize, charset: &[char]) -> Vec<DecodedChar> {
    let classes = charset.len() + 1;
    let mut out: Vec<DecodedChar> = Vec::new();
    let mut previous = 0;
    for step in 0..steps {
        let row = &logits[step * classes..(step + 1) * classes];
        let (best, &top) = row
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .expect("at least the blank class");
        let probability = 1.0 / row.iter().map(|&v| (v - top).exp()).sum::<f32>();
        if best != 0 {
            match out.last_mut() {
                Some(last) if best == previous => {
                    last.last_step = step;
                    last.confidence = last.confidence.max(probability);
                }
                _ => out.push(DecodedChar {
                    ch: charset[best - 1],
                    first_step: step,
                    last_step: step,
                    confidence: probability,
                }),
            }
        }
        previous = best;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeats_merge_unless_a_blank_separates_them() {
        let charset = ['a', 'b'];
        // Steps: a a blank a b b blank
        let picks = [1, 1, 0, 1, 2, 2, 0];
        let mut logits = Vec::new();
        for &p in &picks {
            let mut row = [0.0f32; 3];
            row[p] = 5.0;
            logits.extend_from_slice(&row);
        }
        let chars = greedy(&logits, picks.len(), &charset);
        let text: String = chars.iter().map(|c| c.ch).collect();
        assert_eq!(text, "aab");
        assert_eq!((chars[0].first_step, chars[0].last_step), (0, 1));
        assert_eq!((chars[2].first_step, chars[2].last_step), (4, 5));
        assert!(chars.iter().all(|c| c.confidence > 0.98));
        // Decoding stops at `steps`, ignoring trailing padding rows.
        assert_eq!(greedy(&logits, 3, &charset).len(), 1);
    }
}
