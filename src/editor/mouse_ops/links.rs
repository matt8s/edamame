use ratatui::style::{Modifier, Style};
use ratatui::text::Line;

use crate::config::Theme;
use crate::editor::link::LinkTarget;
use crate::editor::EditorState;
use crate::ui::LinkRun;

use super::coord::{click_to_char_offset, rendered_click_to_line_col_on_text};

/// Does `style` belong to a rendered link?
///
/// Keyed on the *foreground*, not `Modifier::UNDERLINED`: the default theme underlines H2-H6
/// too, which classified every heading as a link and let run coalescing swallow a real link
/// inside one.  Every theme paints link text in a link color (`link_heading` is the
/// underline-free anchor slot).  A candidate test, not a verdict — `syntax_function` is derived
/// from the link color — so callers go through [`link_at_rendered_pos`], which only believes a
/// run it can pair with a real `Inline::Link` in the block's AST.
pub(super) fn is_link_style(style: Style, theme: &Theme) -> bool {
    let link_fgs = [
        theme.link_text.fg,
        theme.link_file.fg,
        theme.link_heading.fg,
    ];
    if link_fgs.iter().any(Option::is_some) {
        return link_fgs.iter().any(|fg| fg.is_some() && *fg == style.fg);
    }
    // A colorless theme (`Monochrome Dark`) leaves only the underline; it over-matches on
    // headings, which the AST pairing in `link_at_rendered_pos` filters out.
    style.add_modifier.contains(Modifier::UNDERLINED)
}

/// The raw URL string of the link under `(col, row)`, as written, for the hint line's hover
/// display (`./notes.md`, not the resolved path).  No raw-scan fallback, unlike
/// `follow_link_at_click`: during raw reveal the URL is already on screen.
pub fn hovered_link_url(
    state: &EditorState,
    col: u16,
    row: u16,
    viewport_width: usize,
) -> Option<String> {
    link_at_rendered_pos(state, col as usize, row as usize, viewport_width).map(|(url, _)| url)
}

/// If `(col, row)` lands on a link or footnote, set `state.pending_link_follow` and return
/// `true`.  The AST-backed rendered-line path comes first; the raw-source scan fallback covers
/// the raw-reveal window of the cursor block and Raw mode.
pub(super) fn follow_link_at_click(
    state: &mut EditorState,
    col: u16,
    row: u16,
    viewport_width: usize,
) -> bool {
    if let Some((url, _)) = link_at_rendered_pos(state, col as usize, row as usize, viewport_width)
    {
        let base_dir = state
            .buffer
            .path()
            .and_then(|p| p.parent())
            .map(|p| p.to_owned());
        state.pending_link_follow = Some(LinkTarget::parse(&url, base_dir.as_deref()));
        return true;
    }

    let Some(offset) = click_to_char_offset(state, col as usize, row as usize, viewport_width)
    else {
        return false;
    };
    let source = state.buffer.contents();
    let click_byte = state.buffer.rope().char_to_byte(offset);
    if let Some(url) = link_at_offset(&source, click_byte) {
        let base_dir = state
            .buffer
            .path()
            .and_then(|p| p.parent())
            .map(|p| p.to_owned());
        state.pending_link_follow = Some(LinkTarget::parse(&url, base_dir.as_deref()));
        return true;
    }
    if let Some(target) = super::footnotes::footnote_at_offset(&source, click_byte) {
        state.pending_link_follow = Some(target);
        return true;
    }
    if let Some(target) = super::footnotes::back_link_glyph_at_click(state, col, row) {
        state.pending_link_follow = Some(target);
        return true;
    }
    false
}

/// [`follow_link_at_click`] for footnotes only: the Rendered-mode plain-click path, where a
/// plain click on a link places the cursor (only Ctrl-click opens it).
pub(super) fn follow_footnote_at_click(
    state: &mut EditorState,
    col: u16,
    row: u16,
    viewport_width: usize,
) -> bool {
    // The `↩` glyph has no raw byte, so the offset scan below would map past it.
    if let Some(target) = super::footnotes::back_link_glyph_at_click(state, col, row) {
        state.pending_link_follow = Some(target);
        return true;
    }
    let Some(offset) = click_to_char_offset(state, col as usize, row as usize, viewport_width)
    else {
        return false;
    };
    let source = state.buffer.contents();
    let click_byte = state.buffer.rope().char_to_byte(offset);
    if let Some(target) = super::footnotes::footnote_at_offset(&source, click_byte) {
        state.pending_link_follow = Some(target);
        return true;
    }
    false
}

/// The link under rendered `(col, row)`: raw URL and optional title.  The single derivation
/// behind the hand pointer, the hint-line hover, and click-to-follow.
///
/// Position resolution is scroll- and wrap-aware. Recomposed inline-math rows carry exact
/// source-link identities, independent of paint runs. Otherwise, link runs are counted per
/// *block*, not per line: the block's N-th link-styled run pairs with its N-th `ui::link_view::LinkRun`
/// (the same list `link_view::build_snapshots` pairs against, though by a different run test —
/// see [`link_run_ranges`]).  A per-line count made every item of a link list resolve to the
/// first URL.  A run pairing with `LinkRun::ImagePlaceholder` resolves to `None` (painted in
/// the link color, but not a link).
pub(super) fn link_at_rendered_pos(
    state: &EditorState,
    col: usize,
    row: usize,
    viewport_width: usize,
) -> Option<(String, Option<String>)> {
    let (line_idx, char_col) = rendered_click_to_line_col_on_text(state, col, row, viewport_width)?;
    if let Some(links) = state.parsed.inline_math.links.get(&line_idx) {
        return links
            .iter()
            .find(|link| link.cols.contains(&char_col))
            .map(|link| (link.url.clone(), link.title.clone()));
    }
    let theme = state.theme();
    let line = state.parsed.lines.get(line_idx)?;
    let runs = link_run_ranges(line, theme);
    let run_in_line = runs
        .iter()
        .position(|(start, end)| char_col >= *start && char_col < *end)?;

    let block_byte = state
        .parsed
        .source_map
        .original_byte_for_rendered_line(line_idx)?;
    let block_range = state
        .parsed
        .source_map
        .original_range_for_byte(block_byte)?;
    let rendered_range = state
        .parsed
        .source_map
        .rendered_lines_for_byte(block_range.start);
    let preceding: usize = (rendered_range.start..line_idx.min(rendered_range.end))
        .filter_map(|idx| state.parsed.lines.get(idx))
        .map(|earlier| link_run_ranges(earlier, theme).len())
        .sum();

    // Slice the block out of the rope rather than `Buffer::contents()`: this runs on every
    // mouse-move over a link run, and materializing the document is O(document) per report.
    // The App flushes the parse before mouse dispatch, so `unwrap_or_default` is defensive.
    let block_src = state
        .buffer
        .byte_slice_to_string(
            block_range.start,
            block_range.end.min(state.buffer.len_bytes()),
        )
        .unwrap_or_default();
    let mut runs_in_block: Vec<LinkRun> = Vec::new();
    for block in &crate::markdown::parse(&block_src) {
        crate::ui::link_view::collect_link_runs_from_block_public(
            block,
            state.parsed.reflow_paragraphs,
            &mut runs_in_block,
        );
    }
    match runs_in_block.into_iter().nth(preceding + run_in_line)? {
        LinkRun::Link { url, title } => Some((url, title)),
        LinkRun::ImagePlaceholder => None,
    }
}

/// Char-column ranges of every run of consecutive link-styled spans in `line`; adjacent spans
/// coalesce so a link with bold/italic substyling counts once.
///
/// The foreground-keyed counterpart of `link_view::underlined_char_ranges`.  Both consume one
/// `link_view::LinkRun` per run but deliberately disagree: a heading anchor (`link_heading`)
/// has the link color and no underline, so it is a run here and not there — which is why this
/// path resolves anchors and the snapshot path does not.  A change to either predicate must be
/// argued against both.
fn link_run_ranges(line: &Line<'_>, theme: &Theme) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut col = 0usize;
    let mut run_start: Option<usize> = None;
    for span in &line.spans {
        let span_len = span.content.chars().count();
        if is_link_style(span.style, theme) {
            if run_start.is_none() {
                run_start = Some(col);
            }
        } else if let Some(start) = run_start.take() {
            out.push((start, col));
        }
        col += span_len;
    }
    if let Some(start) = run_start {
        out.push((start, col));
    }
    out
}

/// Scan the raw source line containing `click_byte` for `[text](url)` and return the URL when
/// the click falls inside it.  No AST: autolinks and reference links are not detected.
pub fn link_at_offset(source: &str, click_byte: usize) -> Option<String> {
    let click_byte = click_byte.min(source.len());
    let line_start = source[..click_byte].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let rel_after = source[click_byte..]
        .find('\n')
        .map(|i| click_byte + i)
        .unwrap_or(source.len());
    let line = &source[line_start..rel_after];
    let col = click_byte.saturating_sub(line_start);

    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' && !crate::editor::footnote_edit::is_escaped(bytes, i) {
            // Brackets are balanced to support `[text containing [inner]]`.
            let mut depth = 1usize;
            let mut j = i + 1;
            while j < bytes.len() {
                match bytes[j] {
                    b'[' => depth += 1,
                    b']' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    b'\\' => {
                        j += 1;
                    }
                    _ => {}
                }
                j += 1;
            }
            if depth != 0 || j >= bytes.len() {
                return None;
            }
            let close_bracket = j;
            if close_bracket + 1 >= bytes.len() || bytes[close_bracket + 1] != b'(' {
                i = close_bracket + 1;
                continue;
            }
            let url_start = close_bracket + 2;
            let mut pdepth = 1usize;
            let mut k = url_start;
            while k < bytes.len() {
                match bytes[k] {
                    b'(' => pdepth += 1,
                    b')' => {
                        pdepth -= 1;
                        if pdepth == 0 {
                            break;
                        }
                    }
                    b'\\' => {
                        k += 1;
                    }
                    _ => {}
                }
                k += 1;
            }
            if pdepth != 0 || k >= bytes.len() {
                return None;
            }
            let url_end = k;
            if col >= i && col <= url_end {
                let url_bytes = &bytes[url_start..url_end];
                let url = String::from_utf8_lossy(url_bytes).trim().to_owned();
                return if url.is_empty() { None } else { Some(url) };
            }
            i = url_end + 1;
        } else {
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Buffer;
    use crate::input::MouseDispatcher;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;

    fn measured(source: &str, width: usize) -> EditorState {
        let theme = Box::leak(Box::new(Theme::default()));
        let mut state = EditorState::new(Buffer::from_str(source), theme);
        state.inline_math_supported = true;
        state.set_viewport_width(width);
        state.refresh_parsed();
        for info in state.parsed.inline_math.requests.clone() {
            state
                .images
                .set_decoded(&info.url, image::DynamicImage::new_rgba8(20, 20));
        }
        state.refresh_parsed();
        assert!(!state.parsed.inline_math.formulas.is_empty());
        state
    }

    fn click_text(state: &mut EditorState, needle: &str, expected: &str, width: usize) {
        let source = state.buffer.contents();
        let (idx, chars) = state
            .parsed
            .lines
            .iter()
            .enumerate()
            .find_map(|(i, line)| {
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                text.find(needle)
                    .map(|b| (i, text[..b].chars().collect::<Vec<_>>()))
            })
            .unwrap_or_else(|| panic!("missing {needle:?}: {:?}", state.parsed.lines));
        let (sub_row, cell) = crate::document::wrap::char_cells_at(
            &state.parsed.lines[idx],
            state.parsed.row_indent(idx),
            width,
            chars.len(),
        )
        .unwrap();
        let col = cell.start as u16;
        let row = (state.parsed.visual_rows_before(idx, width) + sub_row - state.scroll) as u16;
        let mut dispatcher = MouseDispatcher::new();
        let action = dispatcher
            .dispatch(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: col,
                    row,
                    modifiers: KeyModifiers::CONTROL,
                },
                Rect::new(0, 0, width as u16, 80),
            )
            .unwrap();
        crate::editor::mouse_ops::apply(state, action, &mut None, &[], 80, width);
        assert_eq!(
            state.pending_link_follow.take(),
            Some(LinkTarget::Url(expected.into())),
            "click {needle}"
        );
        assert_eq!(
            hovered_link_url(state, col, row, width).as_deref(),
            Some(expected)
        );
        let snapshots = crate::ui::link_view::build_snapshots(
            state,
            Rect::new(0, 0, width as u16, 80),
            state.scroll,
        );
        assert_eq!(
            snapshots
                .iter()
                .find_map(|s| s.hit_test(col, row))
                .map(|s| s.url.as_str()),
            Some(expected)
        );
        assert_eq!(state.buffer.contents(), source);
    }

    #[test]
    fn inline_math_unicode_multiline_scrolled_bands_keep_mouse_targets() {
        let source = "Intro\n\n$x$ [日本 alpha\nbeta gamma delta](https://a.invalid \"A title\")[終](https://b.invalid)";
        let mut state = measured(source, 12);
        state.scroll = 2;
        click_text(&mut state, "beta", "https://a.invalid", 12);
        click_text(&mut state, "終", "https://b.invalid", 12);
        assert_eq!(state.buffer.contents(), source);
    }

    #[test]
    fn inline_math_keeps_links_on_uncomposed_hard_break_rows() {
        let mut state = measured(
            "$x$  \n[one two three four five](https://a.invalid) [B](https://b.invalid)",
            12,
        );
        state.scroll = 1;
        click_text(&mut state, "three", "https://a.invalid", 12);
        click_text(&mut state, "B", "https://b.invalid", 12);
    }

    #[test]
    fn inline_math_adjacent_links_keep_exact_mouse_targets() {
        let mut state = measured("$x$ [A](https://a.invalid)[B](https://b.invalid)", 80);
        click_text(&mut state, "B", "https://b.invalid", 80);
    }

    #[test]
    fn inline_math_inside_link_keeps_exact_mouse_target_after_formula() {
        let mut state = measured(
            "[Before $x$ after](https://a.invalid) [B](https://b.invalid)",
            80,
        );
        click_text(&mut state, "after", "https://a.invalid", 80);
    }

    #[test]
    fn inline_math_wrapped_link_bands_keep_exact_mouse_targets() {
        let mut state = measured(
            "$x$ [one two three four five six](https://a.invalid) [B](https://b.invalid)",
            12,
        );
        click_text(&mut state, "three", "https://a.invalid", 12);
    }
}
