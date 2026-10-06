//! Line selection: which lines of a file a spec actually asks for.
//!
//! Everything is 1-indexed and inclusive, the way `sed -n '12,40p'` is, because that is
//! what the caller is replacing. Selections are kept as sorted, merged, non-overlapping
//! spans so a line is never rendered twice — two overlapping ranges in one spec cost what
//! one costs.

use crate::{Error, Result};

/// An inclusive, 1-indexed run of lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start) + 1
    }

    pub fn is_empty(&self) -> bool {
        self.end < self.start
    }

    pub fn contains(&self, line: usize) -> bool {
        line >= self.start && line <= self.end
    }
}

/// Parse a `sed`-style selection: `12`, `12-40`, `12-` (to the end), or any
/// comma-separated mix. Out-of-range ends are clamped later, against the real line count.
pub fn parse(spec: &str) -> Result<Vec<Span>> {
    let bad = || Error::BadRange {
        spec: spec.to_owned(),
    };
    let mut spans = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(bad());
        }
        let span = match part.split_once('-') {
            None => {
                let line: usize = part.parse().map_err(|_| bad())?;
                Span::new(line, line)
            }
            Some((start, "")) => {
                let start: usize = start.trim().parse().map_err(|_| bad())?;
                Span::new(start, usize::MAX)
            }
            Some((start, end)) => {
                let start: usize = start.trim().parse().map_err(|_| bad())?;
                let end: usize = end.trim().parse().map_err(|_| bad())?;
                if end < start {
                    return Err(bad());
                }
                Span::new(start, end)
            }
        };
        if span.start == 0 {
            // Line 0 does not exist; silently treating it as line 1 would hide a bug in
            // whatever computed the range.
            return Err(bad());
        }
        spans.push(span);
    }
    Ok(normalize(spans, usize::MAX))
}

/// Clamp to `total` lines, drop what falls outside, then sort and merge the rest.
pub fn normalize(mut spans: Vec<Span>, total: usize) -> Vec<Span> {
    spans.retain(|s| !s.is_empty() && s.start <= total);
    for span in &mut spans {
        span.end = span.end.min(total);
    }
    spans.sort_by_key(|s| (s.start, s.end));

    let mut merged: Vec<Span> = Vec::with_capacity(spans.len());
    for span in spans {
        match merged.last_mut() {
            // Adjacent spans merge too: 1-10 and 11-20 render as one run, so no marker
            // claims a gap where there is none.
            Some(last) if span.start <= last.end.saturating_add(1) => {
                last.end = last.end.max(span.end);
            }
            _ => merged.push(span),
        }
    }
    merged
}

/// The window a spec asks for before any search filter narrows it.
///
/// `lines` wins if given; otherwise `head` and `tail` combine — asking for both is how you
/// see the shape of a file you have never opened, and when they overlap the file is simply
/// shown whole.
pub fn window(
    lines: Option<&str>,
    head: Option<usize>,
    tail: Option<usize>,
    total: usize,
) -> Result<Vec<Span>> {
    if let Some(spec) = lines {
        return Ok(normalize(parse(spec)?, total));
    }
    let mut spans = Vec::new();
    if let Some(head) = head.filter(|n| *n > 0) {
        spans.push(Span::new(1, head.min(total)));
    }
    if let Some(tail) = tail.filter(|n| *n > 0) {
        spans.push(Span::new(total.saturating_sub(tail) + 1, total));
    }
    if spans.is_empty() {
        spans.push(Span::new(1, total));
    }
    Ok(normalize(spans, total))
}

/// `sed`-style address ranges: every `from` match opens a span, the next `to` match
/// closes it, and an unclosed span runs to the end of the file.
///
/// `from` alone is "from here to the end", `to` alone is "the top of the file down to
/// here", and both repeat the way `sed -n '/a/,/b/p'` repeats — a heading pattern matching
/// three times yields three sections, not one.
pub fn ranges(lines: &[&str], from: Option<&regex::Regex>, to: Option<&regex::Regex>) -> Vec<Span> {
    let total = lines.len();
    let mut spans = Vec::new();
    let mut open: Option<usize> = from.is_none().then_some(1);
    for (index, line) in lines.iter().enumerate() {
        let number = index + 1;
        match open {
            None => {
                if from.is_some_and(|re| re.is_match(line)) {
                    open = Some(number);
                    // A `to` that also matches the opening line would close it instantly,
                    // which is never what the caller meant, so the close starts next line.
                }
            }
            Some(start) => {
                if to.is_some_and(|re| re.is_match(line)) && number > start {
                    spans.push(Span::new(start, number));
                    // The line that closed a section can be the one that opens the next —
                    // with `from` and `to` both `^## `, every section is selected rather
                    // than every other one.
                    open = from.is_some_and(|re| re.is_match(line)).then_some(number);
                }
            }
        }
    }
    if let Some(start) = open
        && total > 0
    {
        spans.push(Span::new(start, total));
    }
    normalize(spans, total)
}

/// Grow each matching line into a span of `context` lines either side, then merge.
pub fn with_context(matches: &[usize], context: usize, total: usize) -> Vec<Span> {
    let spans = matches
        .iter()
        .map(|line| {
            Span::new(
                line.saturating_sub(context).max(1),
                line.saturating_add(context),
            )
        })
        .collect();
    normalize(spans, total)
}

/// Keep at most `max` lines, from the front. Reports whether anything was dropped.
pub fn cap(spans: &[Span], max: Option<usize>) -> (Vec<Span>, usize) {
    let Some(max) = max else {
        return (spans.to_vec(), 0);
    };
    let total = count(spans);
    if total <= max {
        return (spans.to_vec(), 0);
    }
    let mut kept = Vec::new();
    let mut room = max;
    for span in spans {
        if room == 0 {
            break;
        }
        let take = span.len().min(room);
        kept.push(Span::new(span.start, span.start + take - 1));
        room -= take;
    }
    (kept, total - max)
}

/// Keep only the parts of `spans` that fall inside `within`.
///
/// Search context is clipped this way: asking for `head: 40` and a pattern must not
/// return line 80 because a match on line 39 had three lines of context.
pub fn intersect(spans: &[Span], within: &[Span]) -> Vec<Span> {
    let mut kept = Vec::new();
    for span in spans {
        for limit in within {
            let start = span.start.max(limit.start);
            let end = span.end.min(limit.end);
            if start <= end {
                kept.push(Span::new(start, end));
            }
        }
    }
    normalize(kept, usize::MAX)
}

pub fn count(spans: &[Span]) -> usize {
    spans.iter().map(Span::len).sum()
}

/// How the selection reads in a header: `12-40,98-120`.
pub fn describe(spans: &[Span]) -> String {
    spans
        .iter()
        .map(|s| {
            if s.start == s.end {
                s.start.to_string()
            } else {
                format!("{}-{}", s.start, s.end)
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(pairs: &[(usize, usize)]) -> Vec<Span> {
        pairs.iter().map(|(a, b)| Span::new(*a, *b)).collect()
    }

    #[test]
    fn sed_style_specs_parse() {
        assert_eq!(parse("12").unwrap(), spans(&[(12, 12)]));
        assert_eq!(parse("12-40").unwrap(), spans(&[(12, 40)]));
        assert_eq!(
            parse(" 12-40 , 98-120 ").unwrap(),
            spans(&[(12, 40), (98, 120)])
        );
        assert_eq!(parse("40-").unwrap(), spans(&[(40, usize::MAX)]));
    }

    fn re(pattern: &str) -> regex::Regex {
        regex::Regex::new(pattern).unwrap()
    }

    #[test]
    fn address_ranges_repeat_like_sed() {
        let lines = ["## a", "one", "## b", "two", "three", "## c", "four"];
        assert_eq!(
            ranges(&lines, Some(&re("^## ")), Some(&re("^## "))),
            spans(&[(1, 7)]),
            "adjacent sections merge into one run rather than repeating a line"
        );
        let lines = ["## a", "one", "", "filler", "## b", "two"];
        assert_eq!(
            ranges(&lines, Some(&re("^## a")), Some(&re("^$"))),
            spans(&[(1, 3)])
        );
    }

    #[test]
    fn an_unclosed_range_runs_to_the_end_of_the_file() {
        let lines = ["one", "## here", "two", "three"];
        assert_eq!(ranges(&lines, Some(&re("^## ")), None), spans(&[(2, 4)]));
        assert_eq!(
            ranges(&lines, Some(&re("^## ")), Some(&re("^never$"))),
            spans(&[(2, 4)])
        );
    }

    #[test]
    fn a_close_pattern_alone_reads_from_the_top() {
        let lines = ["one", "stop", "two"];
        assert_eq!(ranges(&lines, None, Some(&re("^stop$"))), spans(&[(1, 2)]));
    }

    #[test]
    fn a_range_that_never_opens_selects_nothing() {
        let lines = ["one", "two"];
        assert!(ranges(&lines, Some(&re("^nope$")), None).is_empty());
        assert!(ranges(&[], Some(&re("^a$")), None).is_empty());
    }

    #[test]
    fn a_close_pattern_does_not_close_the_line_that_opened_the_range() {
        let lines = ["## a", "one", "## b", "two"];
        assert_eq!(
            ranges(&lines, Some(&re("^## a")), Some(&re("^## "))),
            spans(&[(1, 3)]),
            "the section runs to the next heading, not zero lines"
        );
    }

    #[test]
    fn a_range_that_cannot_mean_anything_is_refused() {
        for bad in [
            "", "0", "0-5", "40-12", "a-b", "12-x", "12,", "-5", "12..40",
        ] {
            assert!(
                matches!(parse(bad), Err(Error::BadRange { .. })),
                "`{bad}` should be refused"
            );
        }
    }

    #[test]
    fn overlapping_and_adjacent_spans_are_never_rendered_twice() {
        assert_eq!(
            normalize(spans(&[(1, 10), (5, 20)]), 100),
            spans(&[(1, 20)])
        );
        assert_eq!(
            normalize(spans(&[(1, 10), (11, 20)]), 100),
            spans(&[(1, 20)])
        );
        assert_eq!(
            normalize(spans(&[(30, 40), (1, 5)]), 100),
            spans(&[(1, 5), (30, 40)])
        );
        assert_eq!(count(&normalize(spans(&[(1, 10), (5, 20)]), 100)), 20);
    }

    #[test]
    fn selections_are_clamped_to_the_file() {
        assert_eq!(normalize(spans(&[(1, 500)]), 12), spans(&[(1, 12)]));
        assert!(
            normalize(spans(&[(90, 100)]), 12).is_empty(),
            "wholly past the end"
        );
        assert_eq!(
            window(Some("1-500"), None, None, 12).unwrap(),
            spans(&[(1, 12)])
        );
    }

    #[test]
    fn head_and_tail_together_show_both_ends() {
        assert_eq!(
            window(None, Some(3), Some(2), 100).unwrap(),
            spans(&[(1, 3), (99, 100)])
        );
        assert_eq!(window(None, Some(3), None, 100).unwrap(), spans(&[(1, 3)]));
        assert_eq!(
            window(None, None, Some(3), 100).unwrap(),
            spans(&[(98, 100)])
        );
        // A short file asked for from both ends is just the file.
        assert_eq!(
            window(None, Some(60), Some(60), 10).unwrap(),
            spans(&[(1, 10)])
        );
        assert_eq!(window(None, None, None, 10).unwrap(), spans(&[(1, 10)]));
        assert!(
            window(None, None, None, 0).unwrap().is_empty(),
            "empty file"
        );
    }

    #[test]
    fn lines_wins_over_head_and_tail() {
        assert_eq!(
            window(Some("5-6"), Some(99), Some(99), 100).unwrap(),
            spans(&[(5, 6)])
        );
    }

    #[test]
    fn context_merges_neighbouring_matches() {
        assert_eq!(with_context(&[10, 12], 2, 100), spans(&[(8, 14)]));
        assert_eq!(
            with_context(&[1], 3, 100),
            spans(&[(1, 4)]),
            "clamped at the top"
        );
        assert_eq!(
            with_context(&[100], 3, 100),
            spans(&[(97, 100)]),
            "and the bottom"
        );
        assert_eq!(with_context(&[10, 90], 1, 100), spans(&[(9, 11), (89, 91)]));
    }

    #[test]
    fn a_cap_drops_from_the_end_and_says_how_much() {
        let (kept, dropped) = cap(&spans(&[(1, 10), (20, 30)]), Some(12));
        assert_eq!(kept, spans(&[(1, 10), (20, 21)]));
        assert_eq!(dropped, 9);
        assert_eq!(cap(&spans(&[(1, 10)]), Some(50)), (spans(&[(1, 10)]), 0));
        assert_eq!(cap(&spans(&[(1, 10)]), None), (spans(&[(1, 10)]), 0));
    }

    #[test]
    fn context_never_escapes_the_window_it_was_found_in() {
        let within = spans(&[(1, 40)]);
        assert_eq!(intersect(&spans(&[(37, 43)]), &within), spans(&[(37, 40)]));
        assert!(intersect(&spans(&[(50, 60)]), &within).is_empty());
        assert_eq!(
            intersect(&spans(&[(5, 60)]), &spans(&[(1, 10), (50, 80)])),
            spans(&[(5, 10), (50, 60)])
        );
    }

    #[test]
    fn headers_describe_the_selection_compactly() {
        assert_eq!(describe(&spans(&[(12, 40), (98, 120)])), "12-40,98-120");
        assert_eq!(describe(&spans(&[(7, 7)])), "7");
    }
}
