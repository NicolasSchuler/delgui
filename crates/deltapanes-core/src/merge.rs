//! Reading a rendered diff back as hunks a panel can be assembled from.
//!
//! The structure is deliberately *not* parsed out of delta's rendered output.
//! delta's two-file mode shells out to `git diff --no-index`, so merge mode runs
//! that step itself and pipes the result into delta: one diff, read from a
//! grammar file content cannot imitate, drawn by the same binary as always.
//!
//! Reading `@@` back out of the render was the first design and it is a trap.
//! `--hunk-header-style` cannot be passed twice and the app already passes it,
//! so the flag alone kills the whole diff; a content line that looks like a
//! header is indistinguishable from one once a `[delta]` section has emptied the
//! line-number columns; and `[diff] context` in a gitconfig silently changes what
//! a hunk even is. In a unified diff none of that is possible: every body line
//! carries a ` `, `+`, `-` or `\` prefix, so only a real header starts with `@@`.
//!
//! What still comes from the render is *where* each hunk was drawn, and only as
//! a count -- [`HUNK_LABEL`] marks one row per hunk, and if the marks do not
//! number exactly the hunks git reported, no controls are offered at all.

use std::ops::Range;

use crate::ansi::Line;

/// Marks the row delta draws at the start of each hunk, via `--hunk-label`.
///
/// U+241F is the printable stand-in for the ASCII unit separator: a character
/// whose entire job is to delimit fields, which is why it does not occur in text
/// anyone diffs. It only has to survive being compared against the first span of
/// a row, and [`locate`] refuses to guess if the count is ever wrong anyway.
pub const HUNK_LABEL: &str = "␟";

/// One difference, as the two line ranges it spans.
///
/// Ranges are 0-based and half-open, over lines counted the way git counts them
/// (see [`lines`]). `old` indexes the left buffer -- the result being assembled
/// -- and `new` the right one, the candidate being taken from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub old: Range<usize>,
    pub new: Range<usize>,
    /// What git named as the enclosing context on the header line, if anything:
    /// the `fn main() {` in `@@ -2,1 +2,1 @@ fn main() {`. Taken from the diff
    /// rather than from the render, so no delta style option can withhold it.
    pub context: String,
}

/// Every hunk in a unified diff.
pub fn parse(unified_diff: &[u8]) -> Vec<Hunk> {
    String::from_utf8_lossy(unified_diff)
        .lines()
        .filter_map(parse_header)
        .collect()
}

/// `@@ -l,s +m,t @@ context` as two ranges. Anything else is `None`, including
/// every body line: they are prefixed, so a line of content that reads like a
/// header arrives here as `+@@ …` and does not match.
pub fn parse_header(line: &str) -> Option<Hunk> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, context) = rest.split_once(" @@")?;
    Some(Hunk {
        old: side(old)?,
        new: side(new)?,
        context: context.trim().to_string(),
    })
}

/// One side of a header: `l,s`, or `l` when the length is one.
///
/// A length of zero is not an empty line 0 -- it is an insertion point *after*
/// line `l`, which is why the start is decremented only when there is a line
/// there to point at. `@@ -0,0 +1,3 @@` (into an empty buffer) and
/// `@@ -5,0 +6,2 @@` (append after line 5) both depend on it.
fn side(spec: &str) -> Option<Range<usize>> {
    let (start, len) = match spec.split_once(',') {
        Some((a, b)) => (a.parse::<usize>().ok()?, b.parse::<usize>().ok()?),
        None => (spec.parse::<usize>().ok()?, 1),
    };
    let start = if len == 0 {
        start
    } else {
        start.checked_sub(1)?
    };
    Some(start..start + len)
}

/// The rows of the rendering each hunk covers, its marker row excluded.
///
/// `None` when the marks delta drew do not number exactly the hunks git
/// reported: a mismatch means we cannot say which rows belong to which hunk, and
/// a control row placed by guesswork would act on lines it does not sit beside.
///
/// `rows` must be the same slice that gets drawn -- `render::body` trims the
/// blank row delta prints between files, and indices taken before that trim are
/// off by one.
pub fn locate(rows: &[Line], hunks: usize) -> Option<Vec<Range<usize>>> {
    let marks: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| {
            row.spans
                .first()
                .is_some_and(|s| s.text.starts_with(HUNK_LABEL))
        })
        .map(|(i, _)| i)
        .collect();
    if marks.len() != hunks {
        return None;
    }
    let blank = |row: &Line| row.spans.is_empty() && row.fill_to_eol.is_none();
    Some(
        marks
            .iter()
            .enumerate()
            .map(|(k, &start)| {
                let start = start + 1;
                let mut end = marks.get(k + 1).copied().unwrap_or(rows.len());
                // delta separates hunks with a blank row. Merge mode draws its
                // own separator -- a rule and the control row -- so that one is
                // left out. A *changed* empty line is not blank in this sense:
                // it carries the `ESC[K` fill that paints its background.
                while end > start && blank(&rows[end - 1]) {
                    end -= 1;
                }
                start..end
            })
            .collect(),
    )
}

/// Lines the way git counts them, keeping their terminators.
///
/// `split_inclusive` is exactly git's model: a trailing incomplete line is still
/// a line, and an empty buffer has no lines at all. `str::lines` is not -- it
/// discards the terminators, so rejoining could not tell `a\nb` from `a\nb\n`
/// and would quietly add a newline to a file that did not have one.
pub fn lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// `base` with one hunk's lines replaced by the candidate's.
///
/// `None` rather than a clamp when a range falls outside the buffers: that means
/// the hunk describes text these two panels no longer hold, and splicing
/// something plausible into a buffer the user is about to save is the one
/// outcome worth refusing.
pub fn take(base: &str, cand: &str, hunk: &Hunk) -> Option<String> {
    let (b, c) = (lines(base), lines(cand));
    if hunk.old.end > b.len() || hunk.new.end > c.len() {
        return None;
    }
    let mut out = String::with_capacity(base.len() + cand.len());
    append(&mut out, &b[..hunk.old.start]);
    append(&mut out, &c[hunk.new.clone()]);
    append(&mut out, &b[hunk.old.end..]);
    Some(out)
}

/// Append a run of lines, terminating what came before it first.
///
/// A buffer whose last line has no newline would otherwise be glued to the run
/// that follows it. The guard on the *incoming* run is what keeps that from
/// firing at the end: nothing is appended for an empty run, so a file that ended
/// without a newline still ends without one.
fn append(out: &mut String, lines: &[&str]) {
    if lines.is_empty() {
        return;
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    for line in lines {
        out.push_str(line);
    }
}

/// Whether this hunk list really describes the difference between these two
/// buffers: the regions *between* hunks must be identical on both sides.
///
/// If that holds, applying any single hunk is correct by construction, because
/// the hunk is then exactly the region where the two disagree. It is the guard
/// against inputs the tests did not imagine -- a delta or git upgrade, an exotic
/// gitconfig -- and its answer is "offer no controls", never "splice anyway".
pub fn verify(base: &str, cand: &str, hunks: &[Hunk]) -> bool {
    let (b, c) = (lines(base), lines(cand));
    let (mut bi, mut ci) = (0usize, 0usize);
    for h in hunks {
        if h.old.start < bi || h.new.start < ci {
            return false; // overlapping or out of order
        }
        if h.old.end > b.len() || h.new.end > c.len() {
            return false;
        }
        if b[bi..h.old.start] != c[ci..h.new.start] {
            return false;
        }
        (bi, ci) = (h.old.end, h.new.end);
    }
    b[bi..] == c[ci..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_length_of_one_may_be_written_without_the_comma() {
        assert_eq!(parse_header("@@ -1 +1 @@").unwrap().old, 0..1);
        assert_eq!(parse_header("@@ -1 +1 @@").unwrap().new, 0..1);
    }

    /// A zero length is an insertion point after that line, not a line index.
    #[test]
    fn a_length_of_zero_points_between_lines() {
        assert_eq!(parse_header("@@ -0,0 +1,3 @@").unwrap().old, 0..0);
        assert_eq!(parse_header("@@ -5,0 +6,2 @@").unwrap().old, 5..5);
        assert_eq!(parse_header("@@ -1,3 +0,0 @@").unwrap().new, 0..0);
    }

    #[test]
    fn the_enclosing_context_is_kept() {
        let h = parse_header("@@ -2,7 +2,7 @@ fn main() {").unwrap();
        assert_eq!(h.context, "fn main() {");
        assert_eq!(parse_header("@@ -2,7 +2,7 @@").unwrap().context, "");
    }

    /// The reason the structure is read from the diff and not from the render:
    /// in a unified diff a line of content that looks like a header is prefixed,
    /// so it cannot be mistaken for one.
    #[test]
    fn a_diff_of_diffs_has_only_its_own_headers() {
        let patch = b"--- a/x\n+++ b/y\n@@ -1,2 +1,2 @@\n \x40\x40 -9,9 +9,9 @@\n-@@ -1 +1 @@\n+@@ -2 +2 @@\n";
        assert_eq!(parse(patch).len(), 1);
    }

    #[test]
    fn taking_splices_the_candidates_lines_over_the_base() {
        let h = parse_header("@@ -2 +2,2 @@").unwrap();
        assert_eq!(
            take("a\nb\nc\n", "a\nB\nX\nc\n", &h).as_deref(),
            Some("a\nB\nX\nc\n")
        );
    }

    /// The join exists so an unterminated buffer cannot be glued to what follows
    /// it -- and must not fire at the end, where there is nothing following.
    #[test]
    fn a_file_without_a_final_newline_keeps_not_having_one() {
        let h = parse_header("@@ -2 +2 @@").unwrap();
        assert_eq!(take("a\nb", "a\nX", &h).as_deref(), Some("a\nX"));
        let h = parse_header("@@ -1 +1 @@").unwrap();
        assert_eq!(take("a\nb", "X\nb", &h).as_deref(), Some("X\nb"));
        // …and does fire in the middle: the candidate's last line has no
        // terminator, but the base continues past it.
        let h = parse_header("@@ -1 +1 @@").unwrap();
        assert_eq!(take("a\nb\n", "X", &h).as_deref(), Some("X\nb\n"));
    }

    #[test]
    fn a_hunk_outside_the_buffers_is_refused_rather_than_clamped() {
        let h = parse_header("@@ -9,2 +9,2 @@").unwrap();
        assert_eq!(take("a\n", "b\n", &h), None);
    }

    /// What is being verified is the *result*: that applying these hunks turns
    /// the base into the candidate. A hunk that covers text which happens to
    /// match on both sides passes, and should -- taking it is a no-op, and the
    /// same tolerance is what makes a hunk list split more finely than git's own
    /// harmless rather than a hazard.
    #[test]
    fn verification_rejects_hunks_that_do_not_describe_these_buffers() {
        let h = vec![parse_header("@@ -2 +2 @@").unwrap()];
        assert!(verify("a\nb\nc\n", "a\nB\nc\n", &h));
        assert!(
            !verify("a\nb\nc\n", "a\nB\nCHANGED\n", &h),
            "a change it does not cover"
        );
        assert!(
            !verify("a\nb\nc\n", "a\nB\n", &h),
            "a line it does not account for"
        );
        assert!(verify("same\n", "same\n", &[]));
        assert!(
            !verify("one\n", "other\n", &[]),
            "a difference with no hunk at all"
        );
    }

    #[test]
    fn overlapping_or_unordered_hunks_are_rejected() {
        let h = vec![
            parse_header("@@ -3 +3 @@").unwrap(),
            parse_header("@@ -1 +1 @@").unwrap(),
        ];
        assert!(!verify("a\nb\nc\n", "A\nb\nC\n", &h));
    }
}
