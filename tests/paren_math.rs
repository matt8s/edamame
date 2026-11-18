use edamame::config::Theme;
use edamame::document::row_map::{self, RawPos};
use edamame::document::Buffer;
use edamame::editor::EditorState;

#[test]
fn currency_does_not_consume_following_prose_or_inline_code() {
    use edamame::markdown::parse_offsets::DocParser;
    use pulldown_cmark::Event;

    let source = "Literal currency: $5 and $10 total. Escaped dollar: \\$5.\n\
                  Inline code `$x^2$` is not typeset. An unmatched dollar stays literal: $";
    let events: Vec<_> = DocParser::new(source).into_offset_iter().collect();
    let math: Vec<_> = events
        .iter()
        .filter(|(event, _)| matches!(event, Event::InlineMath(_)))
        .collect();
    assert!(math.is_empty(), "currency consumed prose or code: {math:?}");
    assert!(events
        .iter()
        .any(|(event, _)| matches!(event, Event::Code(code) if code.as_ref() == "$x^2$")));
}

#[test]
fn currency_shield_preserves_math_code_and_percent_text() {
    use edamame::markdown::parse_offsets::DocParser;
    use pulldown_cmark::Event;

    let source =
        r"Pay $10 total (50%); then $10$, $2x$, $2 x$, \(\alpha\), and `$3 dollars` stay distinct.";
    let events: Vec<_> = DocParser::new(source).into_offset_iter().collect();
    let math: Vec<_> = events
        .iter()
        .filter_map(|(event, _)| match event {
            Event::InlineMath(value) => Some(value.as_ref()),
            _ => None,
        })
        .collect();
    assert_eq!(math, ["10", "2x", "2 x", r"\alpha"]);
    assert!(events
        .iter()
        .any(|(event, _)| matches!(event, Event::Code(value) if value.as_ref() == "$3 dollars")));
    let text: String = events
        .iter()
        .filter_map(|(event, _)| match event {
            Event::Text(value) => Some(value.as_ref()),
            _ => None,
        })
        .collect();
    assert!(text.contains("Pay $10 total (50%)"), "{text}");
    for (_, range) in events {
        assert!(range.end <= source.len());
        assert!(source.is_char_boundary(range.start) && source.is_char_boundary(range.end));
    }
}

#[test]
fn currency_shield_preserves_markdown_entities_and_references() {
    use edamame::markdown::parse_offsets::DocParser;
    use pulldown_cmark::{Options, Parser};

    let options = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_SMART_PUNCTUATION;
    for source in [
        "Pay $10 total and **bold** with 50% unchanged.",
        r"Pay $10 total with \% and &#37; and &percnt; preserved.",
        "日本 $10 total and [safe][ref].\n\n[ref]: /target\n",
        "`$10 total` stays code; pay $20 tomorrow.",
        "```text\n$10 total\n```\n\nPay $20 tomorrow.",
        "Pay $10 total <kbd>K</kbd> and <span title='$20 tomorrow'>text</span>.",
        "x*$10 total* keeps punctuation flanking.",
    ] {
        let actual: Vec<_> = DocParser::new(source).into_offset_iter().collect();
        let expected: Vec<_> = Parser::new_ext(source, options)
            .into_offset_iter()
            .collect();
        assert_eq!(actual, expected, "source={source:?}");
    }
}

#[test]
fn currency_keeps_html_code_and_unambiguous_math_independent() {
    use edamame::export::{render_html, HtmlExportOptions};
    let source = "Literal currency: $5 and $10 total. Escaped dollar: \\$5.\n\
                  Inline code `$x^2$` is not typeset. Math $2x$ and \\(y\\).";
    for render_figures in [false, true] {
        let html = render_html(
            source,
            &HtmlExportOptions {
                render_figures,
                ..HtmlExportOptions::default()
            },
        )
        .unwrap();
        assert!(
            html.contains("Literal currency: $5 and $10 total."),
            "{html}"
        );
        assert!(html.contains("<code>$x^2$</code>"), "{html}");
        assert!(html.contains(r"Math $2x$ and \(y\)."), "{html}");
        assert!(!html.contains("%10 total"), "{html}");
    }
}

#[test]
fn currency_shadow_cannot_create_a_reference_link() {
    use edamame::markdown::parse_offsets::DocParser;
    use pulldown_cmark::{Event, Tag};
    let source = "[$10 total]\n\n[%10 total]: /wrong\n";
    let events: Vec<_> = DocParser::new(source).into_offset_iter().collect();
    assert!(!events
        .iter()
        .any(|(event, _)| matches!(event, Event::Start(Tag::Link { .. }))));
    let text: String = events
        .iter()
        .filter_map(|(event, _)| match event {
            Event::Text(value) => Some(value.as_ref()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "[$10 total]");
}

fn ready(source: &str) -> EditorState {
    let theme = Box::leak(Box::new(Theme::default()));
    let mut state = EditorState::new(Buffer::from_str(source), theme);
    state.inline_math_supported = true;
    state.set_viewport_width(79);
    for info in state.parsed.inline_math.requests.clone() {
        let Some(edamame::diagram::DiagramSource::InlineLatex(tex)) = info.source else {
            panic!()
        };
        let loaded = edamame::diagram::math::resolve_inline_latex(
            info.url.clone(),
            &tex,
            Some(state.image_font_size),
            [204, 204, 204, 255],
            [26, 26, 26, 255],
        )
        .unwrap();
        state.images.set_decoded(&info.url, loaded.image);
    }
    state.set_viewport_width(80);
    state
}

#[test]
fn paren_math_typesets_multiple_atoms_without_changing_source() {
    let source = r"Before \(x^2\) middle \(y_1\) after $z$.";
    let state = ready(source);
    assert_eq!(state.parsed.inline_math.formulas.len(), 3);
    assert_eq!(state.buffer.contents(), source);
}

#[test]
fn paren_math_preserves_tex_punctuation_and_unicode_offsets() {
    let source = r"日本 \(x_1+\frac{a*b}{c}\) and \(\text{[a](b) *c*}\) fin.";
    let state = ready(source);
    assert_eq!(state.parsed.inline_math.formulas.len(), 2);
    for word in ["日本", "and", "fin"] {
        let byte = source.find(word).unwrap();
        let pos = RawPos {
            line: 0,
            col: source[..byte].chars().count(),
        };
        let row = row_map::row_for_pos(&state.parsed, 0, pos);
        let col = row_map::raw_to_rendered_col(&state.parsed, 0, row, pos).unwrap();
        let chars: Vec<_> = state.parsed.lines[row]
            .spans
            .iter()
            .flat_map(|s| s.content.chars())
            .collect();
        assert_eq!(chars[col], word.chars().next().unwrap());
        assert_eq!(
            row_map::rendered_to_raw_col(&state.parsed, 0, row, col),
            pos
        );
    }
    assert_eq!(state.buffer.contents(), source);
}

#[test]
fn paren_math_fallback_keeps_delimiters_and_adjacent_prose() {
    let source = r"Before \(x^2\)\(y_1\) after $z$.";
    let mut state = ready(source);
    assert_eq!(state.parsed.inline_math.formulas.len(), 3);
    state.inline_math_supported = false;
    state.set_viewport_width(79);
    let text: String = state
        .parsed
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert_eq!(text, source);
    assert_eq!(state.buffer.contents(), source);
}

#[test]
fn paren_math_ignores_code_escapes_and_unmatched_delimiters() {
    for source in [
        r"Code `\(x\)` and \\(literal\\) and unmatched \(tail",
        "```tex\n\\(x\\)\n```\n",
        "    \\(x\\)\n",
        r"Before <code title='\(x\)'>after</code>",
        r"[label](path/\(target\))",
    ] {
        let theme = Box::leak(Box::new(Theme::default()));
        let mut state = EditorState::new(Buffer::from_str(source), theme);
        state.inline_math_supported = true;
        state.set_viewport_width(79);
        assert!(state.parsed.inline_math.requests.is_empty(), "{source:?}");
        assert_eq!(state.buffer.contents(), source);
    }
}

#[test]
fn paren_math_retains_entities_literally_inside_unsupported_tex() {
    let source = r"Before \(\text{a &amp; b}\) after.";
    let doc = edamame::markdown::parse_document(source);
    assert_eq!(doc.inline_math[0].1, r"\text{a &amp; b}");
    assert_eq!(
        &source[doc.inline_math[0].0.clone()],
        r"\(\text{a &amp; b}\)"
    );
    let theme = Box::leak(Box::new(Theme::default()));
    let mut state = EditorState::new(Buffer::from_str(source), theme);
    state.inline_math_supported = true;
    state.set_viewport_width(79);
    let shown: String = state
        .parsed
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert_eq!(shown, source);
}

#[test]
fn paren_math_inside_dollar_math_is_not_scanned_again() {
    let source = r"Inside $\text{\(x\)}$ only dollar math.";
    let doc = edamame::markdown::parse_document(source);
    assert_eq!(doc.inline_math.len(), 1);
    assert_eq!(doc.inline_math[0].1, r"\text{\(x\)}");
}

#[test]
fn paren_math_does_not_pair_with_unmatched_prose_backticks() {
    let source = r"` before \(x\) after";
    let mut state = ready(source);
    assert_eq!(state.parsed.inline_math.formulas.len(), 1);
    state.inline_math_supported = false;
    state.set_viewport_width(79);
    let shown: String = state
        .parsed
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert_eq!(shown, source);
}

#[test]
fn paren_math_keeps_adjacent_underscore_emphasis() {
    for source in [r"\(x\)_word_", r"_word_\(x\)", r"\(x\)__word__"] {
        let doc = edamame::markdown::parse_document(source);
        let edamame::markdown::Block::Paragraph { inlines, .. } = &doc.blocks[0] else {
            panic!()
        };
        assert!(
            inlines.iter().any(|i| matches!(
                i,
                edamame::markdown::Inline::Italic(_) | edamame::markdown::Inline::Bold(_)
            )),
            "{source:?}: {inlines:?}"
        );
    }
}
