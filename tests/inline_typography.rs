//! End-to-end inline-math typography: real rasterization, real cache
//! entries (the exact call the async decode worker makes on `ImageReady`),
//! and real paragraph composition — the prose baseline row, the whole-row
//! overhang/descent Chrome, cursor and source mapping, reveal identity, and
//! re-measurement at a new cell size.

use std::time::{Duration, Instant};

use edamame::config::Theme;
use edamame::document::row_map::{raw_to_rendered_col, row_for_pos, RawPos};
use edamame::document::Buffer;
use edamame::editor::{EditorState, Mode};
use edamame::markdown::ColOrigin;
use edamame::ui::image_view::build_snapshots;

const FG: [u8; 4] = [204, 204, 204, 255];
const BG: [u8; 4] = [26, 26, 26, 255];

fn theme() -> &'static Theme {
    Box::leak(Box::new(Theme::default()))
}

/// A fresh editor state with inline math enabled at the given cell size and
/// document width, re-parsed through the public width-change path — the same
/// re-parse `App::prepare_viewport` drives every frame.
fn state_with(source: &str, width: usize, cell: (u16, u16)) -> EditorState {
    let mut st = EditorState::new(Buffer::from_str(source), theme());
    st.inline_math_supported = true;
    st.image_font_size = cell;
    if st.viewport_width == width {
        // `EditorState::new` already sits at the default width, so nudge it
        // once to force a reparse that picks up the flags above.
        st.set_viewport_width(width + 1);
    }
    st.set_viewport_width(width);
    st
}

/// Public reparse trigger (width ping-pong; the width is restored).
fn reparsed(st: &mut EditorState) {
    let w = st.viewport_width;
    st.set_viewport_width(w + 1);
    st.set_viewport_width(w);
}

/// Decode every requested inline formula through the same cache entry point
/// the async decode worker uses for its `ImageReady` result: the real
/// rasterizer and the baseline metric that travels atomically with the
/// raster.
fn decode_all(st: &mut EditorState) {
    for info in st.parsed.inline_math.requests.clone() {
        let Some(edamame::diagram::DiagramSource::InlineLatex(src)) = info.source else {
            panic!("an inline request must carry its source");
        };
        let loaded = edamame::diagram::math::resolve_inline_latex(
            info.url.clone(),
            &src,
            Some(st.image_font_size),
            FG,
            BG,
        )
        .expect("the formula renders");
        st.images.set_decoded_with_prebuilt(
            &info.url,
            loaded.image,
            None,
            None,
            None,
            loaded.formula,
        );
    }
    reparsed(st);
}

/// A display-style fraction spans three rows: the numerator overhangs one
/// whole row above the prose baseline row, the denominator descends one row
/// below it, and the prose around the formula paints on the baseline row.
#[test]
fn fraction_band_sits_on_the_prose_baseline_row() {
    let source = "Before $\\dfrac{a+b}{c+d}$ after.";
    let mut st = state_with(source, 100, (14, 32));
    decode_all(&mut st);

    let f = &st.parsed.inline_math.formulas[0];
    assert_eq!(
        f.height, 3,
        "numerator overhang / prose row / denominator descent"
    );
    assert_eq!(
        f.ascent, 1,
        "the TeX baseline sits one row below the band top"
    );
    assert_eq!(f.descent, 1);

    let block = st.parsed.source_map.block_for_byte(0).unwrap();
    let rows = st.parsed.source_map.rendered_lines_for_block(block);
    assert_eq!(rows.len(), 3, "the paragraph is exactly the band's rows");
    let origins = st.parsed.row_origins();
    assert_eq!(
        origins[rows.start].cols,
        ColOrigin::Chrome,
        "the numerator overhang is chrome"
    );
    assert_eq!(
        origins[rows.end - 1].cols,
        ColOrigin::Chrome,
        "the denominator descent is chrome"
    );
    assert_ne!(
        origins[rows.start + 1].cols,
        ColOrigin::Chrome,
        "the prose row paints text"
    );

    // The prose after the formula sits on the baseline row, immediately
    // after the formula's cell span.
    let raw = source.find("after").unwrap();
    let pos = RawPos { line: 0, col: raw };
    let row = row_for_pos(&st.parsed, block, pos);
    assert_eq!(row, rows.start + 1, "prose maps to the baseline row");
    let col = raw_to_rendered_col(&st.parsed, block, row, pos).unwrap();
    assert_eq!(
        col,
        f.col as usize + f.width as usize + 1,
        "prose starts after the formula's cells plus the source space"
    );
}

/// `x^{ab}` overhangs: its TeX baseline sits on the second of the formula's
/// two rows, so the ascent must come from the stored baseline metric.  The
/// legacy centred `(h-1)/2` geometry would have painted the prose on the row
/// *above* the formula's baseline.
#[test]
fn superscript_ascent_comes_from_the_stored_baseline_metric() {
    let source = "Before $x^{ab}$ after.";
    let mut st = state_with(source, 100, (14, 32));
    decode_all(&mut st);

    let f = &st.parsed.inline_math.formulas[0];
    assert_eq!(f.height, 2);
    assert_eq!(
        f.ascent, 1,
        "the stored baseline metric, not the centred (h-1)/2 fallback"
    );
    assert_eq!(f.descent, 0);

    let block = st.parsed.source_map.block_for_byte(0).unwrap();
    let rows = st.parsed.source_map.rendered_lines_for_block(block);
    assert_eq!(rows.len(), 2);
    let origins = st.parsed.row_origins();
    assert_eq!(
        origins[rows.start].cols,
        ColOrigin::Chrome,
        "the superscript overhang is chrome"
    );
    assert_ne!(
        origins[rows.end - 1].cols,
        ColOrigin::Chrome,
        "the baseline row paints prose"
    );

    let raw = source.find("after").unwrap();
    let pos = RawPos { line: 0, col: raw };
    let row = row_for_pos(&st.parsed, block, pos);
    assert_eq!(row, rows.end - 1, "prose maps to the baseline row");
}

/// Every source position inside an asymmetric formula belongs to its painted
/// baseline row, including positions hidden by the atomic formula rectangle.
#[test]
fn formula_source_positions_map_to_the_metric_baseline() {
    for cell in [(14, 32), (10, 20)] {
        for math in [r"x^{ab}", r"x_{\frac{a}{b}}", r"\dfrac{a+b}{c+d}"] {
            let source = format!("Earlier paragraph.\n\nBefore ${math}$ after.");
            let mut st = state_with(&source, 100, cell);
            decode_all(&mut st);
            let f = &st.parsed.inline_math.formulas[0];
            let rows = st.parsed.source_map.rendered_lines_for_block(f.block);
            let expected = f.row + usize::from(f.ascent) - rows.start;
            assert_eq!(f.raw.start.line, f.raw.end.line);
            for col in f.raw.start.col..f.raw.end.col {
                let pos = RawPos {
                    line: f.raw.start.line,
                    col,
                };
                assert_eq!(
                    row_for_pos(&st.parsed, f.block, pos),
                    expected,
                    "{math} at {cell:?}, source column {col}: cursor must use the painted baseline"
                );
            }
        }
    }
}

/// A descent-only formula (a subscript fraction) keeps the prose on the top
/// row and hangs the descent row below it.
#[test]
fn subscript_fraction_keeps_the_prose_on_the_baseline_row() {
    let source = "Before $x_{\\frac{a}{b}}$ after.";
    let mut st = state_with(source, 100, (14, 32));
    decode_all(&mut st);

    let f = &st.parsed.inline_math.formulas[0];
    assert_eq!(f.height, 2);
    assert_eq!(f.ascent, 0);
    assert_eq!(f.descent, 1);

    let block = st.parsed.source_map.block_for_byte(0).unwrap();
    let rows = st.parsed.source_map.rendered_lines_for_block(block);
    assert_eq!(rows.len(), 2);
    let origins = st.parsed.row_origins();
    assert_ne!(
        origins[rows.start].cols,
        ColOrigin::Chrome,
        "the baseline row paints prose"
    );
    assert_eq!(
        origins[rows.end - 1].cols,
        ColOrigin::Chrome,
        "the subscript descent is chrome"
    );

    let raw = source.find("after").unwrap();
    let pos = RawPos { line: 0, col: raw };
    assert_eq!(
        row_for_pos(&st.parsed, block, pos),
        rows.start,
        "prose maps to the baseline row"
    );
}

/// A small formula stays one row inline: no overhang, no descent, the
/// paragraph row is unchanged.
#[test]
fn x_squared_stays_one_row_inline() {
    let source = "Before $x^2$ after.";
    let mut st = state_with(source, 100, (14, 32));
    decode_all(&mut st);

    let f = &st.parsed.inline_math.formulas[0];
    assert_eq!(f.height, 1);
    assert_eq!(f.ascent, 0);
    assert_eq!(f.descent, 0);

    let block = st.parsed.source_map.block_for_byte(0).unwrap();
    let rows = st.parsed.source_map.rendered_lines_for_block(block);
    assert_eq!(rows.len(), 1);
    assert_ne!(st.parsed.row_origins()[rows.start].cols, ColOrigin::Chrome);
}

/// The font cell is part of the cache identity: a resize re-keys the
/// requests, the old geometry stops composing (and its raster is GC'd), and
/// fresh rasterization at the new cell produces its own baseline metric.
#[test]
fn cell_resize_rekeys_and_remeasures_the_formula() {
    let source = "Before $\\dfrac{a+b}{c+d}$ after.";
    let mut st = state_with(source, 100, (14, 32));
    decode_all(&mut st);
    let f = &st.parsed.inline_math.formulas[0];
    assert_eq!((f.height, f.ascent, f.descent), (3, 1, 1));
    let old_url = f.url.clone();

    st.image_font_size = (10, 20);
    reparsed(&mut st);
    assert!(
        st.parsed.inline_math.formulas.is_empty(),
        "the new cell size changes the cache key, so no old geometry composes"
    );
    assert!(
        st.images.status(&old_url).is_none(),
        "the old cell's raster is no longer live"
    );

    decode_all(&mut st);
    let f = &st.parsed.inline_math.formulas[0];
    assert_eq!(
        (f.height, f.ascent, f.descent),
        (3, 1, 1),
        "the band re-measures at the new cell"
    );
}

/// A `Ready` entry without baseline metrics — the legacy synthetic test
/// path — keeps the documented centred geometry (`(h-1)/2` ascent).  Real
/// inline rasters always carry their metrics; this pins the fallback the
/// older focused tests rely on.
#[test]
fn ready_without_metrics_keeps_the_documented_centred_fallback() {
    let source = "Before $x^2$ after.";
    let mut st = state_with(source, 100, (14, 32));
    let url = st.parsed.inline_math.requests[0].url.clone();
    st.images
        .set_decoded(&url, image::DynamicImage::new_rgba8(70, 96));
    reparsed(&mut st);

    let f = &st.parsed.inline_math.formulas[0];
    assert_eq!(f.height, 3, "ceil(96 / 32)");
    assert_eq!(f.ascent, 1, "the legacy centred (h-1)/2");
    assert_eq!(f.descent, 1);
}

/// Raw reveal replaces the measured paragraph as a whole — every band row,
/// not just the cursor's row.
#[test]
fn reveal_replaces_the_whole_measured_paragraph() {
    let source = "Before $x^2$ middle $y_1$ after with more words to wrap.";
    let mut st = state_with(source, 22, (14, 32));
    decode_all(&mut st);

    st.mode = Mode::Rendered;
    st.place_cursor(2);
    st.cursor_block_entered_at = Some(Instant::now() - Duration::from_secs(1));
    let effective = st.effective_rows(22);
    let paragraph = st.parsed.source_map.rendered_lines_for_byte(2);
    assert_eq!(
        effective.block_rendered(),
        Some(paragraph),
        "the whole measured paragraph replaces at reveal"
    );
}

/// The image-layout snapshot places the formula's rect on its measured rows:
/// the band's top row, at the band's full height.
#[test]
fn snapshots_place_the_formula_on_its_measured_rows() {
    let source = "Before $\\dfrac{a+b}{c+d}$ after.";
    let mut st = state_with(source, 100, (14, 32));
    decode_all(&mut st);

    let area = ratatui::layout::Rect::new(0, 0, 100, 30);
    let snaps = build_snapshots(&st, area, 0);
    assert_eq!(snaps.len(), 1, "one formula, one snapshot");
    let f = &st.parsed.inline_math.formulas[0];
    assert_eq!(snaps[0].url, f.url);
    assert_eq!(
        snaps[0].rect.y, f.row as u16,
        "the rect starts on the band's top row"
    );
    assert_eq!(
        snaps[0].rect.height, f.height as u16,
        "the rect spans the whole band"
    );
}
