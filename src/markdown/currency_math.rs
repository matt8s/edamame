//! Keep currency-like dollar openers from consuming subsequent prose/code.
//!
//! A number followed by whitespace and a prose word is conservatively currency;
//! `$10$`, `$2x$`, `$2 x$` and command/operator-led expressions remain math.
//! Ambiguous prose-like mathematics can use explicit `\(...\)` delimiters.
//! A private same-byte-length parser shadow preserves every original offset.

use std::ops::Range;

use pulldown_cmark::{Event, Tag, TagEnd};

use super::paren_math::Events;

pub(crate) fn adapt<'a, I>(
    source: &'a str,
    events: I,
    parse_shadow: impl FnOnce(&str) -> Vec<(Event<'static>, Range<usize>)>,
) -> Events<'a, I>
where
    I: Iterator<Item = (Event<'a>, Range<usize>)>,
{
    if !source
        .as_bytes()
        .iter()
        .enumerate()
        .any(|(at, &byte)| byte == b'$' && currency_opener(source, at))
    {
        return Events::Direct(events);
    }
    let original: Vec<_> = events.collect();
    let mut masked = Vec::new();
    let mut excluded = 0usize;
    for (event, range) in &original {
        match event {
            Event::Start(
                Tag::CodeBlock(_) | Tag::MetadataBlock(_) | Tag::Link { .. } | Tag::Image { .. },
            ) => excluded += 1,
            Event::End(
                TagEnd::CodeBlock | TagEnd::MetadataBlock(_) | TagEnd::Link | TagEnd::Image,
            ) => excluded = excluded.saturating_sub(1),
            Event::Text(_) | Event::InlineMath(_) if excluded == 0 => {
                for (relative, c) in source[range.clone()].char_indices() {
                    let at = range.start + relative;
                    if c == '$' && currency_opener(source, at) {
                        masked.push(at);
                    }
                }
            }
            _ => {}
        }
    }
    masked.sort_unstable();
    masked.dedup();
    if masked.is_empty() {
        return Events::Buffered(original.into_iter());
    }
    let mut shadow = source.as_bytes().to_vec();
    for &at in &masked {
        // '%' is inert Markdown punctuation, with the same delimiter flanking
        // class as '$'. Its original occurrences are restored by source offset.
        shadow[at] = b'%';
    }
    let shadow = String::from_utf8(shadow).expect("only ASCII dollars replaced");
    let parsed = parse_shadow(&shadow);
    // A protected currency amount must not manufacture an implicit reference
    // label by colliding with a real percent-containing reference definition.
    if parsed.iter().any(|(event, range)| {
        matches!(event, Event::Start(Tag::Link { .. } | Tag::Image { .. }))
            && masked.iter().any(|at| range.contains(at))
            && !original
                .iter()
                .any(|(old, old_range)| old == event && old_range == range)
    }) {
        return Events::Buffered(original.into_iter());
    }
    let events = parsed
        .into_iter()
        .map(|(event, range)| {
            let from = masked.partition_point(|&at| at < range.start);
            let to = masked.partition_point(|&at| at < range.end);
            if from == to {
                return (event, range);
            }
            let (value, kind) = match event {
                Event::Text(v) => (v, 0),
                Event::Code(v) => (v, 1),
                Event::InlineMath(v) => (v, 2),
                Event::DisplayMath(v) => (v, 3),
                Event::Html(v) => (v, 4),
                Event::InlineHtml(v) => (v, 5),
                other => return (other, range),
            };
            let positions: Vec<_> = shadow[range.clone()]
                .match_indices('%')
                .map(|(at, _)| range.start + at)
                .collect();
            // Entities/escapes are separate pulldown events in normal use.
            // Never guess if a future parser transformation merges them:
            // retain the exact source rather than inventing substitutions.
            if positions.len() != value.matches('%').count() {
                return (Event::Text(source[range.clone()].to_string().into()), range);
            }
            let mut cursor = positions.into_iter();
            let restored: String = value
                .chars()
                .map(|c| {
                    if c == '%' {
                        let at = cursor.next().expect("marker counts checked");
                        if masked.binary_search(&at).is_ok() {
                            '$'
                        } else {
                            '%'
                        }
                    } else {
                        c
                    }
                })
                .collect();
            let value = restored.into();
            let event = match kind {
                0 => Event::Text(value),
                1 => Event::Code(value),
                2 => Event::InlineMath(value),
                3 => Event::DisplayMath(value),
                4 => Event::Html(value),
                _ => Event::InlineHtml(value),
            };
            (event, range)
        })
        .collect::<Vec<_>>();
    Events::Buffered(events.into_iter())
}

fn currency_opener(source: &str, at: usize) -> bool {
    let bytes = source.as_bytes();
    if bytes.get(at.wrapping_sub(1)) == Some(&b'$') || bytes.get(at + 1) == Some(&b'$') {
        return false;
    }
    if !bytes[..at]
        .iter()
        .rev()
        .take_while(|&&b| b == b'\\')
        .count()
        .is_multiple_of(2)
    {
        return false;
    }
    let mut end = at + 1;
    if !bytes.get(end).is_some_and(u8::is_ascii_digit) {
        return false;
    }
    while bytes
        .get(end)
        .is_some_and(|b| b.is_ascii_digit() || matches!(b, b',' | b'.'))
    {
        end += 1;
    }
    let tail = &source[end..];
    if !tail.chars().next().is_some_and(char::is_whitespace) {
        return false;
    }
    tail.trim_start()
        .chars()
        .take_while(|c| c.is_alphabetic())
        .take(2)
        .count()
        == 2
}
