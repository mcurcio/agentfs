//! A small reader of the markdown in `docs/`, for the tests that check the
//! documentation against the code (docs/configuration.md, docs/architecture.md).
//!
//! Test-only, and shared by path (`#[path]`) between the `agentfs` and
//! `agentfs-csi` binaries' unit tests and `agentfs-core`'s
//! `tests/docs_reference.rs`, so no crate depends on another for it. It
//! knows just enough markdown: ATX headings, fenced code blocks (skipped),
//! GitHub tables (cells split on unescaped `|`) and code spans.

#![allow(dead_code)]

/// A heading and what follows it up to the next heading of any level.
#[derive(Debug, Clone)]
pub struct Section {
    /// The heading's text, without the `#`s.
    pub heading: String,
    pub level: usize,
    /// The section's lines outside fenced code blocks.
    pub text: String,
    pub tables: Vec<Table>,
}

/// A table: its header cells and its body rows.
#[derive(Debug, Clone, Default)]
pub struct Table {
    pub header: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl Table {
    /// The index of the header cell named `name`.
    pub fn column(&self, name: &str) -> Option<usize> {
        self.header.iter().position(|h| h == name)
    }
}

impl Section {
    /// Every body row of every table in the section.
    pub fn rows(&self) -> impl Iterator<Item = &Vec<String>> {
        self.tables.iter().flat_map(|t| t.rows.iter())
    }
}

/// The heading line's level and text, if `line` is an ATX heading.
pub fn heading(line: &str) -> Option<(usize, String)> {
    let level = line.bytes().take_while(|b| *b == b'#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = &line[level..];
    if !(rest.is_empty() || rest.starts_with(' ')) {
        return None;
    }
    Some((level, rest.trim().trim_end_matches('#').trim().to_owned()))
}

/// Splits `markdown` into sections. Text before the first heading is a
/// section with an empty heading at level 0.
pub fn sections(markdown: &str) -> Vec<Section> {
    let mut out = vec![Section {
        heading: String::new(),
        level: 0,
        text: String::new(),
        tables: Vec::new(),
    }];
    let mut fence: Option<&str> = None;
    let mut table: Option<Table> = None;
    let flush = |out: &mut Vec<Section>, table: &mut Option<Table>| {
        if let Some(t) = table.take()
            && let Some(last) = out.last_mut()
        {
            last.tables.push(t);
        }
    };
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if let Some(marker) = fence {
            if trimmed.starts_with(marker) {
                fence = None;
            }
            continue;
        }
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            flush(&mut out, &mut table);
            fence = Some(&trimmed[..3]);
            continue;
        }
        if let Some((level, text)) = heading(line) {
            flush(&mut out, &mut table);
            out.push(Section {
                heading: text,
                level,
                text: String::new(),
                tables: Vec::new(),
            });
            continue;
        }
        if let Some(last) = out.last_mut() {
            last.text.push_str(line);
            last.text.push('\n');
        }
        if trimmed.starts_with('|') {
            let cells = split_row(trimmed);
            match &mut table {
                None => {
                    table = Some(Table {
                        header: cells,
                        rows: Vec::new(),
                    })
                }
                Some(t) => {
                    let separator = cells
                        .iter()
                        .all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':')));
                    if !(separator && t.rows.is_empty()) {
                        t.rows.push(cells);
                    }
                }
            }
        } else {
            flush(&mut out, &mut table);
        }
    }
    flush(&mut out, &mut table);
    out
}

/// The cells of a table row, trimmed, split on `|` not escaped with `\`
/// (GitHub tables need `\|` even inside code spans). `\|` becomes `|`.
pub fn split_row(line: &str) -> Vec<String> {
    let line = line.trim();
    let line = line.strip_prefix('|').unwrap_or(line);
    let line = line.strip_suffix('|').unwrap_or(line);
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cell.push('|');
                chars.next();
            }
            '|' => cells.push(std::mem::take(&mut cell).trim().to_owned()),
            c => cell.push(c),
        }
    }
    cells.push(cell.trim().to_owned());
    cells
}

/// The contents of every code span in `text` (single backticks).
pub fn code_spans(text: &str) -> Vec<String> {
    let mut spans = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('`') else { break };
        spans.push(after[..end].to_owned());
        rest = &after[end + 1..];
    }
    spans
}

/// The cell's content if it is exactly one code span, as in a key column.
pub fn sole_code_span(cell: &str) -> Option<&str> {
    let inner = cell.strip_prefix('`')?.strip_suffix('`')?;
    (!inner.contains('`')).then_some(inner)
}

/// The section whose heading is exactly `heading`.
pub fn section<'a>(sections: &'a [Section], heading: &str) -> Option<&'a Section> {
    sections.iter().find(|s| s.heading == heading)
}

/// Reads `docs/<name>` from the workspace, given a crate's
/// `CARGO_MANIFEST_DIR`.
pub fn read_doc(manifest_dir: &str, name: &str) -> String {
    let path = std::path::Path::new(manifest_dir)
        .join("../../docs")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_and_spans() {
        let md = "# T\n\nintro\n\n## `[a]`\n\n| Key | Commands |\n|---|---|\n\
                  | `a.b` | `x`, `y` |\n| `--p auto\\|on` | all |\n\n```sh\n# not a heading\n| no |\n```\n\
                  ### Sub\ntext `c`\n";
        let s = sections(md);
        let a = section(&s, "`[a]`").unwrap();
        assert_eq!(a.tables.len(), 1);
        assert_eq!(a.tables[0].header, ["Key", "Commands"]);
        assert_eq!(a.tables[0].column("Commands"), Some(1));
        assert_eq!(a.tables[0].rows[0], ["`a.b`", "`x`, `y`"]);
        assert_eq!(sole_code_span(&a.tables[0].rows[1][0]), Some("--p auto|on"));
        assert_eq!(code_spans("`x`, `y` and `z"), ["x", "y"]);
        assert!(section(&s, "not a heading").is_none());
        assert_eq!(section(&s, "Sub").unwrap().level, 3);
        assert_eq!(heading("#nope"), None);
    }
}
