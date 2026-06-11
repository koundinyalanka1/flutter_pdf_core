//! Milestone 9 (part 1): chunking page text for AI consumption.
//!
//! Chunks aim for `max_chars` characters, split on paragraph boundaries
//! when possible (falling back to sentence, then hard splits), with a
//! configurable overlap so context isn't lost at boundaries.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Chunk {
    /// Sequential chunk id, 0-based.
    pub id: usize,
    /// First and last page (0-based) contributing to this chunk.
    pub page_start: usize,
    pub page_end: usize,
    pub text: String,
    pub char_count: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct ChunkOptions {
    /// Target maximum characters per chunk.
    pub max_chars: usize,
    /// Characters of trailing context repeated at the start of the next chunk.
    pub overlap: usize,
}

impl Default for ChunkOptions {
    fn default() -> Self {
        Self {
            max_chars: 2000,
            overlap: 200,
        }
    }
}

/// Chunk page texts (one string per page, 0-based order).
pub fn chunk_pages(pages: &[String], options: ChunkOptions) -> Vec<Chunk> {
    let max_chars = options.max_chars.max(64);
    let overlap = options.overlap.min(max_chars / 2);

    // Flatten into (page_index, paragraph) units.
    let mut units: Vec<(usize, &str)> = Vec::new();
    for (page, text) in pages.iter().enumerate() {
        for para in text.split("\n\n") {
            let para = para.trim();
            if !para.is_empty() {
                units.push((page, para));
            }
        }
    }

    let mut chunks: Vec<Chunk> = Vec::new();
    let mut current = String::new();
    let mut page_start: usize = 0;
    let mut page_end: usize = 0;

    let mut flush =
        |current: &mut String, page_start: usize, page_end: usize, chunks: &mut Vec<Chunk>| {
            let text = current.trim().to_owned();
            if !text.is_empty() {
                chunks.push(Chunk {
                    id: chunks.len(),
                    page_start,
                    page_end,
                    char_count: text.chars().count(),
                    text,
                });
            }
            current.clear();
        };

    for (page, para) in units {
        // Oversized paragraph: hard-split it on its own.
        if para.chars().count() > max_chars {
            flush(&mut current, page_start, page_end, &mut chunks);
            for piece in split_hard(para, max_chars, overlap) {
                chunks.push(Chunk {
                    id: chunks.len(),
                    page_start: page,
                    page_end: page,
                    char_count: piece.chars().count(),
                    text: piece,
                });
            }
            page_start = page;
            page_end = page;
            continue;
        }

        if current.is_empty() {
            page_start = page;
        } else if current.chars().count() + para.chars().count() + 2 > max_chars {
            // Take the overlap tail before flushing.
            let tail = overlap_tail(&current, overlap);
            flush(&mut current, page_start, page_end, &mut chunks);
            if !tail.is_empty() {
                current.push_str(&tail);
                current.push_str("\n\n");
            }
            page_start = page;
        } else {
            current.push_str("\n\n");
        }
        current.push_str(para);
        page_end = page;
    }
    flush(&mut current, page_start, page_end, &mut chunks);
    chunks
}

/// Last `overlap` characters, starting at a word boundary when possible.
fn overlap_tail(text: &str, overlap: usize) -> String {
    if overlap == 0 {
        return String::new();
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= overlap {
        return text.to_owned();
    }
    let mut start = chars.len() - overlap;
    // Nudge forward to the next word boundary.
    while start < chars.len() && !chars[start].is_whitespace() {
        start += 1;
    }
    chars[start..].iter().collect::<String>().trim().to_owned()
}

/// Hard-split text into ~max_chars pieces, preferring sentence boundaries.
fn split_hard(text: &str, max_chars: usize, overlap: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < chars.len() {
        let hard_end = (pos + max_chars).min(chars.len());
        let mut end = hard_end;
        if hard_end < chars.len() {
            // Look back for a sentence boundary in the last 20% of the window.
            let floor = pos + (max_chars * 4 / 5);
            let mut cursor = hard_end;
            while cursor > floor {
                if matches!(chars[cursor - 1], '.' | '!' | '?' | '\n') {
                    end = cursor;
                    break;
                }
                cursor -= 1;
            }
        }
        let piece: String = chars[pos..end].iter().collect();
        out.push(piece.trim().to_owned());
        if end >= chars.len() {
            break;
        }
        // Step back by the overlap, guaranteeing forward progress.
        let next = end.saturating_sub(overlap.min(max_chars / 2));
        pos = if next > pos { next } else { end };
    }
    out.retain(|p| !p.is_empty());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_input_is_one_chunk() {
        let pages = vec!["Hello world.".to_string(), "Second page.".to_string()];
        let chunks = chunk_pages(&pages, ChunkOptions::default());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].page_start, 0);
        assert_eq!(chunks[0].page_end, 1);
        assert!(chunks[0].text.contains("Hello world."));
        assert!(chunks[0].text.contains("Second page."));
    }

    #[test]
    fn splits_on_paragraphs_with_overlap() {
        let para = "word ".repeat(30).trim().to_owned(); // ~150 chars
        let page = format!("{para}\n\n{para}\n\n{para}");
        let chunks = chunk_pages(
            &[page],
            ChunkOptions {
                max_chars: 200,
                overlap: 40,
            },
        );
        assert!(chunks.len() >= 2, "got {} chunks", chunks.len());
        for chunk in &chunks {
            assert!(chunk.char_count <= 260, "chunk too big: {}", chunk.char_count);
            assert_eq!(chunk.id, chunks.iter().position(|c| c == chunk).unwrap());
        }
    }

    #[test]
    fn hard_splits_giant_paragraphs() {
        let giant = "abcdefghij ".repeat(100); // 1100 chars, no paragraph breaks
        let chunks = chunk_pages(
            &[giant],
            ChunkOptions {
                max_chars: 300,
                overlap: 30,
            },
        );
        assert!(chunks.len() >= 3);
        let total: usize = chunks.iter().map(|c| c.char_count).sum();
        assert!(total >= 1000, "content must not be lost");
    }

    #[test]
    fn tracks_page_ranges() {
        let pages: Vec<String> = (0..4).map(|i| format!("Page {i} content.")).collect();
        let chunks = chunk_pages(
            &pages,
            ChunkOptions {
                max_chars: 64,
                overlap: 0,
            },
        );
        assert!(chunks.len() > 1);
        assert_eq!(chunks[0].page_start, 0);
        assert_eq!(chunks.last().unwrap().page_end, 3);
    }
}
