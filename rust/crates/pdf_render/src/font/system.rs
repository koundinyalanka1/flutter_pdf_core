//! The fonts installed on the device, as a last resort for characters no
//! bundled face covers — CJK above all, which every phone and desktop ships
//! but which would add twenty megabytes to the app.
//!
//! Font directories hold hundreds of megabytes, so nothing here reads a whole
//! file. Indexing seeks to each face's table directory and reads only the
//! small tables that say what it covers and how it looks; drawing from a face
//! later reads only that face's own tables, never the rest of a collection.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::truetype::TrueTypeFont;

/// What one installed face covers and looks like.
#[derive(Debug, Clone)]
pub struct FaceInfo {
    pub path: PathBuf,
    pub index: usize,
    pub family: String,
    pub postscript: String,
    pub weight: u16,
    pub italic: bool,
    /// From `OS/2` when the font says, otherwise guessed from its name.
    pub serif: bool,
    pub monospace: bool,
    /// Code points with glyphs, as sorted inclusive ranges.
    pub coverage: Vec<(u32, u32)>,
}

impl FaceInfo {
    pub fn covers(&self, ch: u32) -> bool {
        let index = self.coverage.partition_point(|&(lo, _)| lo <= ch);
        index > 0 && ch <= self.coverage[index - 1].1
    }
}

/// Directories the platforms keep fonts in. Missing ones are skipped.
pub fn default_directories() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = [
        // Android
        "/system/fonts",
        "/product/fonts",
        "/system/product/fonts",
        // iOS and macOS
        "/System/Library/Fonts",
        "/System/Library/Fonts/Supplemental",
        "/System/Library/Fonts/Core",
        "/System/Library/Fonts/CoreUI",
        "/System/Library/Fonts/CoreAddition",
        "/System/Library/Fonts/LanguageSupport",
        "/Library/Fonts",
        // Linux and other Unix
        "/usr/share/fonts",
        "/usr/local/share/fonts",
        // Windows
        "C:\\Windows\\Fonts",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join("Library/Fonts"));
        dirs.push(home.join(".fonts"));
        dirs.push(home.join(".local/share/fonts"));
    }
    if let Some(windir) = std::env::var_os("WINDIR") {
        dirs.push(PathBuf::from(windir).join("Fonts"));
    }
    dirs
}

/// Every face in the given directories (searched a few levels deep).
pub fn scan(directories: &[PathBuf]) -> Vec<FaceInfo> {
    let mut faces = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in directories {
        walk(dir, 0, &mut |path| {
            // The same file is often reachable through two directories.
            let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
            if seen.insert(key) {
                faces.extend(scan_file(path));
            }
        });
    }
    faces
}

fn walk(dir: &Path, depth: usize, visit: &mut dyn FnMut(&Path)) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            walk(&path, depth + 1, visit);
            continue;
        }
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        if matches!(extension.as_deref(), Some("ttf" | "otf" | "ttc" | "otc")) {
            visit(&path);
        }
    }
}

/// Index every face of one font file without reading the file whole.
pub fn scan_file(path: &Path) -> Vec<FaceInfo> {
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    let Some(header) = read_at(&mut file, 0, 12) else {
        return Vec::new();
    };
    let offsets: Vec<u32> = if &header[0..4] == b"ttcf" {
        let count = be32(&header, 8).min(256) as usize;
        let Some(list) = read_at(&mut file, 12, count * 4) else {
            return Vec::new();
        };
        list.chunks_exact(4).map(|c| be32(c, 0)).collect()
    } else {
        vec![0]
    };
    offsets
        .iter()
        .enumerate()
        .filter_map(|(index, &offset)| scan_face(&mut file, path, index, u64::from(offset)))
        .collect()
}

/// A face's table directory: tag → (offset, length), offsets absolute.
fn directory(file: &mut File, base: u64) -> Option<HashMap<[u8; 4], (u64, usize)>> {
    let header = read_at(file, base, 12)?;
    let tag = be32(&header, 0);
    if tag != 0x0001_0000 && tag != 0x7472_7565 && tag != 0x4F54_544F {
        return None;
    }
    let count = usize::from(be16(&header, 4)).min(512);
    let records = read_at(file, base + 12, count * 16)?;
    Some(
        records
            .chunks_exact(16)
            .map(|r| {
                (
                    [r[0], r[1], r[2], r[3]],
                    (u64::from(be32(r, 8)), be32(r, 12) as usize),
                )
            })
            .collect(),
    )
}

fn scan_face(file: &mut File, path: &Path, index: usize, base: u64) -> Option<FaceInfo> {
    let tables = directory(file, base)?;
    let mut table = |tag: &[u8; 4], limit: usize| -> Option<Vec<u8>> {
        let &(offset, length) = tables.get(tag)?;
        read_at(file, offset, length.min(limit))
    };
    let cmap = table(b"cmap", 4 << 20)?;
    let coverage = cmap_ranges(&cmap);
    if coverage.is_empty() {
        return None;
    }
    let names = table(b"name", 256 << 10).unwrap_or_default();
    let os2 = table(b"OS/2", 128).unwrap_or_default();
    let post = table(b"post", 32).unwrap_or_default();

    let family = name_record(&names, 16)
        .or_else(|| name_record(&names, 1))
        .unwrap_or_default();
    let postscript = name_record(&names, 6).unwrap_or_default();
    let weight = if os2.len() >= 6 { be16(&os2, 4) } else { 400 };
    let italic = os2.len() >= 64 && be16(&os2, 62) & 1 != 0;
    let class = (os2.len() >= 32).then(|| (be16(&os2, 30) as i16) >> 8);
    let lower = format!("{family} {postscript}").to_ascii_lowercase();
    let serif = match class {
        Some(1..=7) => true,
        Some(8) => false,
        _ => looks_serif(&lower),
    };
    let monospace = (post.len() >= 16 && be32(&post, 12) != 0)
        || ["mono", "courier", "consol", "menlo"]
            .iter()
            .any(|w| lower.contains(w));
    Some(FaceInfo {
        path: path.to_owned(),
        index,
        family,
        postscript,
        weight,
        italic,
        serif,
        monospace,
        coverage,
    })
}

/// Serif designs by name, for fonts whose `OS/2` leaves it unsaid.
pub fn looks_serif(lower_name: &str) -> bool {
    if lower_name.contains("sans") {
        return false;
    }
    [
        "serif",
        "times",
        "roman",
        "georgia",
        "garamond",
        "baskerville",
        "palatino",
        "book",
        "mincho",
        "ming",
        "song",
        "batang",
        "myeongjo",
        "myungjo",
        "kaiti",
        "fangsong",
        "cambria",
        "caslon",
        "didot",
        "bodoni",
        "charter",
        "century",
        "liberation serif",
        "tinos",
    ]
    .iter()
    .any(|w| lower_name.contains(w))
}

/// Read face `index` of `path` with only the tables drawing needs, as a
/// self-contained single-face font — a collection's other faces never load.
pub fn load_face(path: &Path, index: usize) -> Option<TrueTypeFont> {
    let mut file = File::open(path).ok()?;
    let header = read_at(&mut file, 0, 12)?;
    let base = if &header[0..4] == b"ttcf" {
        u64::from(be32(&read_at(&mut file, 12 + index as u64 * 4, 4)?, 0))
    } else {
        0
    };
    let tag = be32(&read_at(&mut file, base, 4)?, 0);
    let tables = directory(&mut file, base)?;
    let wanted: &[&[u8; 4]] = &[
        b"head", b"maxp", b"hhea", b"hmtx", b"cmap", b"post", b"name", b"OS/2", b"loca", b"glyf",
        b"CFF ",
    ];
    let mut chosen = Vec::new();
    for tag in wanted {
        if let Some(&(offset, length)) = tables.get(*tag) {
            chosen.push((**tag, read_at(&mut file, offset, length)?));
        }
    }
    // A minimal sfnt: header, directory, then the tables, 4-aligned.
    let mut out = Vec::new();
    out.extend_from_slice(&tag.to_be_bytes());
    out.extend_from_slice(&(chosen.len() as u16).to_be_bytes());
    out.extend_from_slice(&[0; 6]);
    let mut offset = 12 + chosen.len() * 16;
    for (tag, bytes) in &chosen {
        out.extend_from_slice(tag);
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&(offset as u32).to_be_bytes());
        out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        offset += bytes.len().div_ceil(4) * 4;
    }
    for (_, bytes) in chosen {
        out.extend_from_slice(&bytes);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }
    TrueTypeFont::parse_face(out, 0)
}

/// Covered code points from a `cmap` table, as sorted ranges — read from the
/// subtable's own segments rather than by expanding every character.
pub fn cmap_ranges(cmap: &[u8]) -> Vec<(u32, u32)> {
    if cmap.len() < 4 {
        return Vec::new();
    }
    let count = usize::from(be16(cmap, 2));
    let mut best: Option<(u8, usize)> = None;
    for i in 0..count {
        let record = 4 + i * 8;
        if record + 8 > cmap.len() {
            break;
        }
        let (platform, encoding) = (be16(cmap, record), be16(cmap, record + 2));
        let rank = match (platform, encoding) {
            (3, 10) | (0, 4) | (0, 6) => 4,
            (3, 1) | (0, 3) => 3,
            (0, _) => 2,
            _ => 0,
        };
        let offset = be32(cmap, record + 4) as usize;
        if rank > 0 && best.is_none_or(|(r, _)| rank > r) {
            best = Some((rank, offset));
        }
    }
    let Some((_, at)) = best else {
        return Vec::new();
    };
    let Some(format) = cmap
        .get(at..at + 2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
    else {
        return Vec::new();
    };
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    match format {
        4 => {
            let Some(seg_x2) = cmap.get(at + 6..at + 8) else {
                return ranges;
            };
            let seg_x2 = usize::from(u16::from_be_bytes([seg_x2[0], seg_x2[1]]));
            let (ends, starts) = (at + 14, at + 16 + seg_x2);
            for s in 0..seg_x2 / 2 {
                if starts + s * 2 + 2 > cmap.len() {
                    break;
                }
                let (start, end) = (be16(cmap, starts + s * 2), be16(cmap, ends + s * 2));
                if start <= end && start != 0xFFFF {
                    ranges.push((u32::from(start), u32::from(end)));
                }
            }
        }
        12 | 13 => {
            let groups = cmap.get(at + 12..at + 16).map(|b| be32(b, 0)).unwrap_or(0) as usize;
            for g in 0..groups.min(1 << 20) {
                let record = at + 16 + g * 12;
                if record + 12 > cmap.len() {
                    break;
                }
                let (start, end) = (be32(cmap, record), be32(cmap, record + 4));
                if start <= end {
                    ranges.push((start, end));
                }
            }
        }
        6 => {
            if at + 10 <= cmap.len() {
                let (first, count) = (be16(cmap, at + 6), be16(cmap, at + 8));
                if count > 0 {
                    ranges.push((u32::from(first), u32::from(first) + u32::from(count) - 1));
                }
            }
        }
        0 => {
            for code in 0..256usize {
                if cmap.get(at + 6 + code).is_some_and(|&g| g != 0) {
                    ranges.push((code as u32, code as u32));
                }
            }
        }
        _ => {}
    }
    ranges.sort_unstable();
    let mut merged: Vec<(u32, u32)> = Vec::with_capacity(ranges.len());
    for (lo, hi) in ranges {
        match merged.last_mut() {
            Some((_, end)) if lo <= end.saturating_add(1) => *end = (*end).max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    merged
}

fn name_record(table: &[u8], wanted: u16) -> Option<String> {
    if table.len() < 6 {
        return None;
    }
    let count = usize::from(be16(table, 2));
    let strings = usize::from(be16(table, 4));
    let mut mac = None;
    for i in 0..count {
        let record = 6 + i * 12;
        if record + 12 > table.len() {
            break;
        }
        if be16(table, record + 6) != wanted {
            continue;
        }
        let platform = be16(table, record);
        let (len, offset) = (
            usize::from(be16(table, record + 8)),
            usize::from(be16(table, record + 10)),
        );
        let Some(bytes) = table.get(strings + offset..strings + offset + len) else {
            continue;
        };
        match platform {
            0 | 3 => {
                let units: Vec<u16> = bytes
                    .chunks_exact(2)
                    .map(|c| u16::from_be_bytes([c[0], c[1]]))
                    .collect();
                return Some(String::from_utf16_lossy(&units));
            }
            1 if mac.is_none() => mac = Some(bytes.iter().map(|&b| b as char).collect()),
            _ => {}
        }
    }
    mac
}

fn read_at(file: &mut File, offset: u64, length: usize) -> Option<Vec<u8>> {
    if length > (64 << 20) {
        return None;
    }
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut buffer = vec![0; length];
    file.read_exact(&mut buffer).ok()?;
    Some(buffer)
}

fn be16(data: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([data[at], data[at + 1]])
}

fn be32(data: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_lookup_finds_inclusive_ranges() {
        let face = FaceInfo {
            path: PathBuf::new(),
            index: 0,
            family: String::new(),
            postscript: String::new(),
            weight: 400,
            italic: false,
            serif: false,
            monospace: false,
            coverage: vec![(0x20, 0x7E), (0x4E00, 0x9FFF)],
        };
        assert!(face.covers(0x20) && face.covers(0x7E) && face.covers(0x4E2D));
        assert!(!face.covers(0x7F) && !face.covers(0x1F));
    }

    #[test]
    fn serif_guesses_come_from_design_words() {
        assert!(looks_serif("times new roman"));
        assert!(looks_serif("hiragino mincho pron"));
        assert!(!looks_serif("noto sans cjk jp"));
        assert!(!looks_serif("dejavu sans"));
    }

    /// The bundled Roboto is a real sfnt: its cmap must read back as ranges
    /// that cover basic Latin, and loading it through the file path must
    /// produce a drawable face.
    #[test]
    fn a_real_font_scans_and_loads_from_disk() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fonts/Roboto-Regular.ttf");
        let faces = scan_file(&path);
        assert_eq!(faces.len(), 1);
        let face = &faces[0];
        assert_eq!(face.family, "Roboto");
        assert!(face.covers('A' as u32) && face.covers('Ж' as u32));
        let font = load_face(&path, 0).expect("loads");
        let gid = font.glyph_for_char('A' as u32).unwrap();
        assert!(font.glyph_outline(gid).is_some());
    }
}
