//! 8-connected components of ink, labelled run by run.
//!
//! Each row's ink runs are unioned with the runs they touch (diagonals
//! included) in the row above. Memory follows the number of runs rather than
//! the number of pixels, which keeps a 300 dpi page cheap.

use crate::binarize::Binary;

/// One component's bounding box (end-exclusive) and ink pixel count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Component {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
    pub pixels: u32,
}

impl Component {
    pub fn width(&self) -> u32 {
        self.x1 - self.x0
    }

    pub fn height(&self) -> u32 {
        self.y1 - self.y0
    }

    pub fn center_x(&self) -> f64 {
        (self.x0 + self.x1) as f64 / 2.0
    }

    pub fn center_y(&self) -> f64 {
        (self.y0 + self.y1) as f64 / 2.0
    }
}

/// Above this many runs the "page" is noise or a photograph, not text.
const MAX_RUNS: usize = 12_000_000;

pub fn connected_components(binary: &Binary) -> Vec<Component> {
    let (w, h) = (binary.width, binary.height);
    // Runs as (x0, x1, y); row_start[y]..row_start[y + 1] are row y's runs.
    let mut runs: Vec<(u32, u32, u32)> = Vec::new();
    let mut row_start = Vec::with_capacity(h + 1);
    for y in 0..h {
        row_start.push(runs.len());
        let row = binary.row(y);
        let mut x = 0;
        while x < w {
            if row[x] == 0 {
                x += 1;
                continue;
            }
            let start = x;
            while x < w && row[x] != 0 {
                x += 1;
            }
            runs.push((start as u32, x as u32, y as u32));
        }
        if runs.len() > MAX_RUNS {
            return Vec::new();
        }
    }
    row_start.push(runs.len());

    let mut parent: Vec<u32> = (0..runs.len() as u32).collect();
    for y in 1..h {
        let (above, current) = (
            row_start[y - 1]..row_start[y],
            row_start[y]..row_start[y + 1],
        );
        let mut j = above.start;
        for i in current {
            let (x0, x1, _) = runs[i];
            // Skip runs above that end before this one could touch them.
            while j < above.end && runs[j].1 < x0 {
                j += 1;
            }
            let mut k = j;
            while k < above.end && runs[k].0 <= x1 {
                union(&mut parent, i as u32, k as u32);
                k += 1;
            }
        }
    }

    let mut index = vec![u32::MAX; runs.len()];
    let mut components: Vec<Component> = Vec::new();
    for i in 0..runs.len() {
        let root = find(&mut parent, i as u32) as usize;
        let (x0, x1, y) = runs[i];
        if index[root] == u32::MAX {
            index[root] = components.len() as u32;
            components.push(Component {
                x0,
                y0: y,
                x1,
                y1: y + 1,
                pixels: 0,
            });
        }
        let c = &mut components[index[root] as usize];
        c.x0 = c.x0.min(x0);
        c.x1 = c.x1.max(x1);
        c.y1 = c.y1.max(y + 1);
        c.pixels += x1 - x0;
    }
    components
}

fn find(parent: &mut [u32], mut i: u32) -> u32 {
    while parent[i as usize] != i {
        parent[i as usize] = parent[parent[i as usize] as usize];
        i = parent[i as usize];
    }
    i
}

fn union(parent: &mut [u32], a: u32, b: u32) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    if ra != rb {
        // Keep the earlier run as root so components are emitted top-down.
        let (low, high) = if ra < rb { (ra, rb) } else { (rb, ra) };
        parent[high as usize] = low;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binary(rows: &[&str]) -> Binary {
        let width = rows[0].len();
        let bits = rows
            .iter()
            .flat_map(|row| row.bytes().map(|b| u8::from(b == b'#')))
            .collect();
        Binary {
            width,
            height: rows.len(),
            bits,
        }
    }

    #[test]
    fn diagonal_touches_join_and_gaps_separate() {
        let image = binary(&[
            "#....##..",
            ".#...##..",
            "..#......",
            ".......#.",
            "###....##",
        ]);
        let mut found = connected_components(&image);
        found.sort_by_key(|c| (c.y0, c.x0));
        assert_eq!(
            found,
            vec![
                Component {
                    x0: 0,
                    y0: 0,
                    x1: 3,
                    y1: 3,
                    pixels: 3
                },
                Component {
                    x0: 5,
                    y0: 0,
                    x1: 7,
                    y1: 2,
                    pixels: 4
                },
                Component {
                    x0: 7,
                    y0: 3,
                    x1: 9,
                    y1: 5,
                    pixels: 3
                },
                Component {
                    x0: 0,
                    y0: 4,
                    x1: 3,
                    y1: 5,
                    pixels: 3
                },
            ]
        );
    }

    #[test]
    fn u_shapes_merge_when_their_arms_meet_later() {
        let image = binary(&["#.#", "#.#", "###"]);
        assert_eq!(connected_components(&image).len(), 1);
    }
}
