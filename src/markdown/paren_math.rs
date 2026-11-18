//! Offset-preserving parenthesis-math extension around pulldown's inline grammar.
//!
//! Formula bodies must be opaque to Markdown, not recovered after emphasis or
//! links have already consumed their punctuation. A private same-byte-length
//! parser shadow uses inert inline-HTML atoms; restored events borrow the original source.
//! The source passed by the caller is never changed.

use std::ops::Range;

use pulldown_cmark::{Event, Tag, TagEnd};

pub(crate) enum Events<'a, I> {
    Direct(I),
    Buffered(std::vec::IntoIter<(Event<'a>, Range<usize>)>),
}

impl<'a, I: Iterator<Item = (Event<'a>, Range<usize>)>> Iterator for Events<'a, I> {
    type Item = (Event<'a>, Range<usize>);
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Direct(i) => i.next(),
            Self::Buffered(i) => i.next(),
        }
    }
}

/// Only an opener in Markdown prose qualifies. Code, HTML, destinations,
/// metadata and existing dollar math are not scanned as prose. Physical
/// newline boundaries terminate an unmatched opener.
pub(crate) fn adapt<'a, I>(
    source: &'a str,
    events: I,
    parse_shadow: impl FnOnce(&str) -> Vec<(Event<'static>, Range<usize>)>,
) -> Events<'a, I>
where
    I: Iterator<Item = (Event<'a>, Range<usize>)>,
{
    if !source.contains(r"\(") {
        return Events::Direct(events);
    }
    let original: Vec<_> = events.collect();
    let mut prose = Vec::new();
    let mut excluded = 0usize;
    for (event, range) in &original {
        match event {
            Event::Start(
                Tag::CodeBlock(_) | Tag::MetadataBlock(_) | Tag::Link { .. } | Tag::Image { .. },
            ) => excluded += 1,
            Event::End(
                TagEnd::CodeBlock | TagEnd::MetadataBlock(_) | TagEnd::Link | TagEnd::Image,
            ) => excluded = excluded.saturating_sub(1),
            Event::Text(_) if excluded == 0 => prose.push(range.clone()),
            _ => {}
        }
    }
    let bytes = source.as_bytes();
    let mut spans = Vec::new();
    let mut open = None;
    let mut i = 0usize;
    let mut slashes = 0usize;
    let mut text = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\n' || bytes[i] == b'\r' {
            open = None;
        }
        if bytes[i] == b'\\' {
            let unescaped = slashes.is_multiple_of(2);
            if unescaped && bytes.get(i + 1) == Some(&b'(') && open.is_none() {
                while text < prose.len() && prose[text].end <= i + 1 {
                    text += 1;
                }
                if prose.get(text).is_some_and(|r| r.contains(&(i + 1))) {
                    open = Some(i);
                }
            } else if unescaped && bytes.get(i + 1) == Some(&b')') {
                if let Some(start) = open.take() {
                    if i > start + 2 && !source[start + 2..i].trim().is_empty() {
                        spans.push(start..i + 2);
                    }
                }
            }
            slashes += 1;
        } else {
            slashes = 0;
        }
        i += 1;
    }
    if spans.is_empty() {
        return Events::Buffered(original.into_iter());
    }
    let mut shadow = bytes.to_vec();
    for range in &spans {
        // Punctuation boundaries preserve underscore flanking. Inline HTML
        // cannot close an unmatched prose backtick the way a code sentinel can.
        // The leading '(' also prevents a line-start tag becoming an HTML block.
        shadow[range.clone()].fill(b'm');
        shadow[range.start] = b'(';
        shadow[range.start + 1] = b'<';
        shadow[range.end - 2] = b'>';
        shadow[range.end - 1] = b')';
    }
    let shadow = String::from_utf8(shadow).expect("whole UTF-8 spans replaced by ASCII");
    let parsed = parse_shadow(&shadow);
    // A shadow label must not accidentally resolve to a different reference
    // definition. Such an ambiguous document keeps its original parse instead.
    let is_link = |e: &Event<'_>| matches!(e, Event::Start(Tag::Link { .. } | Tag::Image { .. }));
    let in_math = |range: &Range<usize>| {
        let i = spans.partition_point(|s| s.end <= range.start);
        spans.get(i).is_some_and(|s| s.contains(&range.start))
    };
    let old_links: Vec<_> = original
        .iter()
        .filter(|(e, r)| is_link(e) && !in_math(r))
        .collect();
    let new_links: Vec<_> = parsed.iter().filter(|(e, _)| is_link(e)).collect();
    if old_links.len() != new_links.len() || !old_links.iter().zip(&new_links).all(|(a, b)| a == b)
    {
        return Events::Buffered(original.into_iter());
    }
    let mut output = Vec::new();
    let mut atom = 0usize;
    let mut restored = 0usize;
    for (event, range) in parsed {
        while atom < spans.len() && spans[atom].end < range.start {
            atom += 1;
        }
        if matches!(event, Event::InlineHtml(_)) {
            if let Some(span) = spans
                .get(atom)
                .filter(|s| range.start == s.start + 1 && range.end == s.end - 1)
            {
                output.push((
                    Event::InlineMath(source[span.start + 2..span.end - 2].into()),
                    span.clone(),
                ));
                restored += 1;
                continue;
            }
        }
        if let Event::Text(value) = event {
            let leading = spans.get(atom).is_some_and(|s| range.start == s.end - 1);
            let tail = if leading { atom + 1 } else { atom };
            let trailing = spans.get(tail).is_some_and(|s| range.end == s.start + 1);
            let from = usize::from(leading);
            let to = value.len().saturating_sub(usize::from(trailing));
            if from < to {
                output.push((
                    Event::Text(value[from..to].to_string().into()),
                    range.start + from..range.end - usize::from(trailing),
                ));
            }
        } else {
            output.push((event, range));
        }
    }
    if restored == spans.len() {
        Events::Buffered(output.into_iter())
    } else {
        // Never expose a synthetic atom or partially repaired event stream.
        Events::Buffered(original.into_iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulldown_cmark::Parser;

    #[test]
    fn paren_math_respects_disabled_math_options() {
        let source = r"Before \(x\) after.";
        let events: Vec<_> = crate::markdown::parse_offsets::DocParser::with_options(
            source,
            pulldown_cmark::Options::empty(),
        )
        .into_offset_iter()
        .collect();
        assert!(!events
            .iter()
            .any(|(e, _)| matches!(e, Event::InlineMath(_))));
    }

    #[test]
    fn ordinary_dollar_documents_keep_the_direct_iterator() {
        let source = "Before $x$ after.";
        let events = adapt(source, Parser::new(source).into_offset_iter(), |_| {
            panic!("no shadow needed")
        });
        assert!(matches!(events, Events::Direct(_)));
    }

    #[test]
    fn a_shadow_reference_collision_keeps_original_link_semantics() {
        let source = "[\\(x\\)]\n\n[(<m>)]: /wrong\n";
        let options = crate::markdown::parse_offsets::BASE_OPTIONS;
        let expected: Vec<_> = Parser::new_ext(source, options)
            .into_offset_iter()
            .collect();
        let actual: Vec<_> = adapt(
            source,
            Parser::new_ext(source, options).into_offset_iter(),
            |shadow| {
                Parser::new_ext(shadow, options)
                    .into_offset_iter()
                    .map(|(e, r)| (e.into_static(), r))
                    .collect()
            },
        )
        .collect();
        assert_eq!(
            actual, expected,
            "a protected atom cannot manufacture a reference link"
        );
    }
}
