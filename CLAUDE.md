# flutter_pdf_core: notes for Claude Code

The guide for this engine lives in `AGENTS.md`, so every coding agent reads
the same rules. Keep it there; this file only imports it.

@AGENTS.md

## Claude Code only

- **The file-writing tool decodes `\uXXXX` escapes** in file content. To
  put a literal backslash-u sequence in source, build it with `concat!` or
  an escaped string.
