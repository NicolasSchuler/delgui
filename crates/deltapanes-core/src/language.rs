//! Guessing a syntax from pasted text.
//!
//! delta infers highlighting from a filename, and a pasted panel has none. A
//! file panel can borrow its extension, but a paste has to be sniffed.
//!
//! The bias here is deliberately towards **returning `None`**. Prose is a normal
//! thing to paste, and delta renders unhighlighted text perfectly well, whereas
//! prose mistaken for code gets sprayed with meaningless colour. A wrong guess
//! is worse than no guess, so every rule below demands positive evidence and
//! ties are resolved as "don't know".

use std::cmp::Reverse;

/// Minimum score before we claim to recognise anything.
const THRESHOLD: u32 = 3;

/// Sniffing happens while the user edits, so one pasted minified line must not
/// make each keystroke clone megabytes before delta's own size guard is reached.
/// This is bytes rather than characters because allocation is the cost being
/// bounded; [`sample`] backs up to a UTF-8 boundary when the limit cuts a line.
const MAX_SAMPLE_BYTES: usize = 64 * 1024;
const MAX_SAMPLE_LINES: usize = 400;

/// Returns an extension token suitable for `--default-language`, or `None` when
/// the text does not look like code.
pub fn detect(text: &str) -> Option<&'static str> {
    let sample = sample(text);
    let trimmed = sample.trim_start();
    if trimmed.is_empty() {
        return None;
    }

    // Unambiguous openings first: these are structural, not statistical.
    if let Some(rest) = trimmed.strip_prefix("#!") {
        let line = rest.lines().next().unwrap_or_default();
        for (needle, lang) in [
            ("python", "py"),
            ("node", "js"),
            ("ruby", "rb"),
            ("perl", "pl"),
            ("bash", "sh"),
            ("zsh", "sh"),
            ("sh", "sh"),
        ] {
            if line.contains(needle) {
                return Some(lang);
            }
        }
        return Some("sh");
    }
    if trimmed.starts_with("<?xml") || trimmed.starts_with("<!DOCTYPE html") {
        return Some(if trimmed.starts_with("<?xml") {
            "xml"
        } else {
            "html"
        });
    }
    if looks_like_json(trimmed) {
        return Some("json");
    }
    if sample
        .lines()
        .any(|l| l.starts_with("@@ ") || l.starts_with("diff --git "))
    {
        return Some("diff");
    }

    let mut scores: Vec<(&'static str, u32)> = RULES
        .iter()
        .map(|(lang, needles)| {
            let hits = needles
                .iter()
                .filter(|n| contains_token(&sample, n))
                .count() as u32;
            (*lang, hits)
        })
        .collect();
    scores.sort_by_key(|(_, score)| Reverse(*score));

    match scores.as_slice() {
        // A clear winner, or a tie we refuse to break.
        [(lang, best), rest @ ..] if *best >= THRESHOLD => {
            if rest.first().is_some_and(|(_, second)| *second == *best) {
                None
            } else {
                Some(lang)
            }
        }
        _ => None,
    }
}

/// Copy a representative prefix without splitting a UTF-8 code point.
///
/// `str::lines` also preserves the old CRLF behaviour: the carriage return is
/// stripped before lines are joined with a single newline.
fn sample(text: &str) -> String {
    let mut sample = String::with_capacity(text.len().min(MAX_SAMPLE_BYTES));
    for (index, line) in text.lines().take(MAX_SAMPLE_LINES).enumerate() {
        if index > 0 {
            if sample.len() == MAX_SAMPLE_BYTES {
                break;
            }
            sample.push('\n');
        }

        let remaining = MAX_SAMPLE_BYTES - sample.len();
        let mut end = line.len().min(remaining);
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        sample.push_str(&line[..end]);
        if end < line.len() {
            break;
        }
    }
    sample
}

/// Match a needle only when it stands alone, so that "in" inside "point" or
/// "fn" inside "info" does not count. This is what keeps prose scoring at zero.
fn contains_token(haystack: &str, needle: &str) -> bool {
    let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric() && c != '_');
    let mut from = 0;
    while let Some(idx) = haystack[from..].find(needle) {
        let start = from + idx;
        let end = start + needle.len();
        let before = haystack[..start].chars().next_back();
        let after = haystack[end..].chars().next();
        // Punctuation-only needles ("=>", "::") carry their own boundaries.
        let needs_boundary = needle.chars().next().is_some_and(char::is_alphanumeric);
        if !needs_boundary || (boundary(before) && boundary(after)) {
            return true;
        }
        from = end;
    }
    false
}

fn looks_like_json(t: &str) -> bool {
    let starts = t.starts_with('{') || t.starts_with('[');
    let ends = t.trim_end().ends_with('}') || t.trim_end().ends_with(']');
    starts && ends && t.contains("\":")
}

/// Tokens that are rare in prose and common in the language. Kept small on
/// purpose: more needles means more ways for an English paragraph to score.
#[rustfmt::skip]
const RULES: &[(&str, &[&str])] = &[
    ("rs",   &["fn", "let", "impl", "pub", "->", "::", "match", "mut", "use", "&str", "Vec<"]),
    ("py",   &["def", "import", "self", "elif", "None", "True", "lambda", "__init__", "print("]),
    ("js",   &["function", "const", "=>", "var", "return", "require(", "console.log", "null", "let"]),
    ("ts",   &["interface", "=>", "const", "export", "import", "readonly", ": string", ": number"]),
    ("go",   &["func", "package", ":=", "import", "nil", "defer", "chan", "struct"]),
    ("c",    &["#include", "int", "void", "char", "return", "struct", "sizeof", "NULL"]),
    ("java", &["public", "class", "static", "void", "import", "extends", "@Override", "new"]),
    ("rb",   &["def", "end", "require", "module", "nil", "puts", "attr_accessor", "do"]),
    ("sh",   &["echo", "fi", "esac", "$(", "then", "done", "export", "elif"]),
    ("sql",  &["SELECT", "FROM", "WHERE", "JOIN", "INSERT", "UPDATE", "GROUP BY", "CREATE TABLE"]),
    ("html", &["<div", "<span", "</", "<p>", "<a ", "<body", "<script", "<head"]),
    ("css",  &["px", "color:", "margin", "padding", "font-size", "display:", "background"]),
    ("toml", &["[package]", "[dependencies]", "version =", "name =", "true", "false"]),
    ("yaml", &["- name:", "steps:", "image:", "env:", "runs-on:", "version:"]),
];

#[cfg(test)]
mod tests {
    use super::{MAX_SAMPLE_BYTES, detect, sample};

    /// The case that matters most: prose must not be mistaken for code.
    #[test]
    fn prose_is_not_a_language() {
        let samples = [
            "The quick brown fox jumps over the lazy dog. It does this repeatedly,\n\
             and no one is quite sure why.",
            "Dear Nicolas,\n\nThank you for your submission. The reviewers found the\n\
             argument in section 3 compelling, but would like to see the derivation\n\
             expanded before publication.\n\nBest regards",
            "Abstract. We present a method for comparing two texts without first\n\
             writing them to disk. Our approach lets the user paste directly, and\n\
             defers rendering to an existing tool.",
            "shopping list\nmilk\nbread\nsomething for dinner\n",
            "",
            "   \n\n  ",
        ];
        for s in samples {
            assert_eq!(detect(s), None, "misdetected prose as code: {s:?}");
        }
    }

    #[test]
    fn recognises_common_languages() {
        assert_eq!(
            detect("fn main() {\n    let mut x: Vec<u8> = vec![];\n    x.push(1);\n}"),
            Some("rs")
        );
        assert_eq!(
            detect("def f(self):\n    import os\n    if x is None:\n        return True"),
            Some("py")
        );
        assert_eq!(
            detect("package main\n\nimport \"fmt\"\n\nfunc main() { x := 1; _ = x }"),
            Some("go")
        );
        assert_eq!(
            detect("SELECT a FROM t JOIN u ON x WHERE y GROUP BY z"),
            Some("sql")
        );
    }

    #[test]
    fn structural_formats_win_outright() {
        assert_eq!(detect("#!/usr/bin/env python3\nx = 1\n"), Some("py"));
        assert_eq!(detect("#!/bin/bash\nls\n"), Some("sh"));
        assert_eq!(detect("{\n  \"name\": \"x\",\n  \"v\": 2\n}"), Some("json"));
        assert_eq!(detect("<?xml version=\"1.0\"?><a/>"), Some("xml"));
        assert_eq!(
            detect("diff --git a/x b/x\n@@ -1 +1 @@\n-a\n+b\n"),
            Some("diff")
        );
    }

    /// Markdown is mostly prose, and delta highlights it very lightly. Guessing
    /// wrong here costs more than leaving it alone.
    #[test]
    fn markdown_prose_stays_unhighlighted() {
        assert_eq!(
            detect("# Title\n\nSome ordinary paragraph text goes here.\n"),
            None
        );
    }

    #[test]
    fn sniffing_is_byte_bounded_and_utf8_safe() {
        let text = "é".repeat(MAX_SAMPLE_BYTES);
        let sampled = sample(&text);

        assert!(sampled.len() <= MAX_SAMPLE_BYTES);
        assert!(sampled.is_char_boundary(sampled.len()));
        assert_eq!(sampled.len() % 'é'.len_utf8(), 0);
    }

    #[test]
    fn content_after_the_byte_budget_cannot_change_the_guess() {
        let mut text = "ordinary prose ".repeat(MAX_SAMPLE_BYTES / 4);
        text.push_str("\nfn main() { let mut value: Vec<u8> = Vec::new(); }");

        assert_eq!(detect(&text), None);
    }

    #[test]
    fn early_evidence_survives_a_large_suffix() {
        let mut text = String::from("fn main() { let mut value: Vec<u8> = Vec::new(); }\n");
        text.push_str(&"x".repeat(MAX_SAMPLE_BYTES * 2));

        assert_eq!(detect(&text), Some("rs"));
    }
}
