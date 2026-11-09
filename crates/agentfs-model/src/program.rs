//! A program's line edits as the byte calls a program makes (rule 8): the
//! model's API takes only [`Write`]s, as a filesystem does, and the tests
//! name what a program means by a [`LineEdit`] it turns into bytes.
//!
//! An edit inside the file is the fewest byte calls that turn what the
//! program reads into what it wants: one `pwrite` from the first byte that
//! differs (to the last that differs, when the length stays), then an
//! `ftruncate` if the file got shorter, as a program that rewrites a file in
//! place does. An append is one `O_APPEND` write. A rewrite truncates the file
//! to nothing and writes the bytes it was given (often a stale read).

use crate::content::{Content, split_lines};
use crate::world::Write;

/// What a program means to do to a file's lines. Positions count the lines
/// the program reads ([`crate::world::World::view`]); a position past the
/// last line means a new line at the end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LineEdit {
    /// Replace that line's text (its terminator stays as it was).
    At(usize),
    /// A new line before that line.
    Insert(usize),
    /// Remove that line.
    Delete(usize),
    /// Move line `from` to become line `to`.
    Move { from: usize, to: usize },
    /// A new line, written with `O_APPEND`: `text` and a newline.
    Append,
    /// Truncate, then write this content's bytes.
    Rewrite(Content),
}

/// The byte calls a program makes to apply `edit` to `view`, a new line
/// holding `text`.
pub fn calls(view: &str, edit: &LineEdit, text: &str) -> Vec<Write> {
    match edit {
        LineEdit::Append => vec![Write::Append(format!("{text}\n"))],
        LineEdit::Rewrite(c) => {
            let mut v = vec![Write::Truncate(0)];
            let bytes = c.bytes();
            if !bytes.is_empty() {
                v.push(Write::Pwrite { offset: 0, bytes });
            }
            v
        }
        _ => minimal(view, &edited(view, edit, text)),
    }
}

/// The bytes `edit` leaves on `view`.
pub fn edited(view: &str, edit: &LineEdit, text: &str) -> String {
    let mut lines = split_lines(view);
    let new = format!("{text}\n");
    let at_end = |lines: &mut Vec<String>| {
        if let Some(l) = lines.last_mut()
            && !l.ends_with('\n')
        {
            l.push('\n');
        }
        lines.push(new.clone());
    };
    match edit {
        LineEdit::At(i) if *i < lines.len() => {
            lines[*i] = if lines[*i].ends_with('\n') {
                new.clone()
            } else {
                text.to_string()
            };
        }
        LineEdit::At(_) | LineEdit::Append => at_end(&mut lines),
        LineEdit::Insert(i) if *i < lines.len() => lines.insert(*i, new.clone()),
        LineEdit::Insert(_) => at_end(&mut lines),
        LineEdit::Delete(i) => {
            if *i < lines.len() {
                lines.remove(*i);
            }
        }
        LineEdit::Move { from, to } => {
            if from != to && *from < lines.len() {
                let mut l = lines.remove(*from);
                if !l.ends_with('\n') {
                    l.push('\n');
                }
                lines.insert((*to).min(lines.len()), l);
            }
        }
        LineEdit::Rewrite(c) => return c.bytes(),
    }
    lines.concat()
}

/// The fewest byte calls that turn `old` into `new`: one `pwrite` of the
/// bytes from the first that differs (to the last that differs, when the
/// lengths are equal), and an `ftruncate` when `new` is shorter.
pub fn minimal(old: &str, new: &str) -> Vec<Write> {
    let (a, b) = (old.as_bytes(), new.as_bytes());
    let d = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    if a.len() == b.len() {
        if d == a.len() {
            return Vec::new();
        }
        let e = a[d..]
            .iter()
            .rev()
            .zip(b[d..].iter().rev())
            .take_while(|(x, y)| x == y)
            .count();
        return vec![Write::Pwrite {
            offset: d,
            bytes: new[d..b.len() - e].to_string(),
        }];
    }
    let mut v = Vec::new();
    if d < b.len() {
        v.push(Write::Pwrite {
            offset: d,
            bytes: new[d..].to_string(),
        });
    }
    if b.len() < a.len() {
        v.push(Write::Truncate(b.len()));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_calls_rebuild_the_bytes() {
        for (old, new) in [
            ("a\nb\nc\n", "a\nX\nc\n"),
            ("a\nb\nc\n", "a\nc\n"),
            ("a\nb\n", "a\nb\nc\n"),
            ("a\nb", "a\nbc\n"),
            ("abc", ""),
            ("", "x\n"),
            ("same\n", "same\n"),
        ] {
            assert_eq!(Write::apply_all(old, &minimal(old, new)), new, "{old:?}");
        }
        assert_eq!(
            minimal("a\nb\nc\n", "a\nX\nc\n"),
            [Write::Pwrite {
                offset: 2,
                bytes: "X".into()
            }]
        );
    }

    #[test]
    fn line_edits_keep_a_partial_last_line_partial() {
        assert_eq!(edited("a\nfoo", &LineEdit::At(1), "x"), "a\nx");
        assert_eq!(edited("a\nfoo", &LineEdit::At(5), "x"), "a\nfoo\nx\n");
        assert_eq!(edited("a\nb\n", &LineEdit::Insert(1), "x"), "a\nx\nb\n");
        assert_eq!(
            edited("a\nb\nc", &LineEdit::Move { from: 2, to: 0 }, ""),
            "c\na\nb\n"
        );
        assert_eq!(edited("a\nb\n", &LineEdit::Delete(0), ""), "b\n");
    }

    #[test]
    fn a_pwrite_past_the_end_leaves_a_hole() {
        let calls = [Write::Pwrite {
            offset: 3,
            bytes: "x".into(),
        }];
        assert_eq!(Write::apply_all("a", &calls), "a\0\0x");
        assert_eq!(Write::apply_all("abc", &[Write::Truncate(1)]), "a");
        assert_eq!(Write::apply_all("a", &[Write::Truncate(2)]), "a\0");
    }
}
