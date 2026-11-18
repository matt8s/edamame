//! LaTeX display math → SVG → `DynamicImage` pipeline.
//!
//! [`resolve_latex`] is the public entry point (used by the App decode
//! worker, same contract as [`super::mermaid::resolve_mermaid`]).  The
//! pipeline is pure Rust — no node, no system TeX: RaTeX parses the LaTeX,
//! lays it out in display style, flattens to a display list, and serializes
//! an SVG.  We emit **`<text>`** (not embedded glyph outlines): RaTeX names
//! its faces `font-family="KaTeX_*"`, and the shared fontdb — which carries
//! the bundled KaTeX TTFs, see [`crate::image::svg`] — resolves them during
//! the rasterize step, exactly how the mermaid path renders.  That keeps a
//! single font subsystem rather than the parallel `ab_glyph` stack the
//! `standalone`/`embed-fonts` features would pull in.
//!
//! RaTeX 0.1.x is pre-1.0 with known panic bugs, so the render is wrapped in
//! `catch_unwind` like the mermaid renderer.  The shared cache-key URL
//! scheme, [`DiagramSource`](super::common::DiagramSource) and
//! [`DiagramError`] live in [`super::common`].

use std::panic::{catch_unwind, AssertUnwindSafe};

use crate::image::{rasterize_svg, LoadedImage, SvgScaleMode, SvgSizing};

use super::common::{panic_message, DiagramError};

/// Maximum LaTeX source length we will attempt to render.  RaTeX has no
/// internal bound, so an over-cap formula gets the same clean failure →
/// placeholder path as an over-cap mermaid diagram.
const MAX_LATEX_SOURCE_BYTES: usize = 64 * 1024;

/// Cell-height → RaTeX `font_size` conversion factor for **display** math.
///
/// Two unit mismatches stand between the terminal's cell *height* (pixels)
/// and the value RaTeX expects (user units per em):
///
/// * RaTeX emits its SVG labelled in `pt`, and usvg rasterizes CSS `pt` at
///   96 dpi — one `pt` becomes 96/72 px.
/// * A terminal cell spans more than one text em: ascent + descent plus
///   the terminal's configured line height ≈ 1.2–1.4 em in practice.
///
/// The `1 / (96/72 × 1.25)` term lands the formula's x-height on one text
/// em (1.25 is the middle of the common 1.2–1.4 line-height range); the
/// leading `1.25` then sets display math ~25% larger than the surrounding
/// body text, the way typeset display equations are conventionally set —
/// prominent, and large enough to keep subscripts and superscripts legible
/// in the terminal.  Exactness is impossible without font metrics from the
/// terminal, so the factor targets a look within ±10% across terminals.
///
/// Inline math does **not** use this factor: it uses
/// [`INLINE_EM_CELL_FRACTION`], because an inline formula must read as
/// body text at a baseline that can actually be anchored.
const LATEX_EM_TO_CELL: f64 = 1.25 / (96.0 / 72.0 * 1.25);

/// Inline math: the formula's em (text size) as a fraction of the
/// terminal's cell **height in pixels** — 0.9, a 12.5 % increase over the
/// old 0.8 default.
///
/// A named *calibration estimate*, not a measurement: the terminal
/// discloses the cell size in pixels and nothing else — never em size,
/// x-height or the baseline position inside the cell.  At 0.9, KaTeX's
/// x-height (0.431 em) is ≈12.4 px on a 32 px cell. This targets body-sized
/// formulas, but cannot guarantee a match to every terminal font.
/// The SVG is pt-labelled
/// and rasterized at 96 dpi, so RaTeX user units follow as
/// `em_user = cell_h · 0.9 · 72/96`.
const INLINE_EM_CELL_FRACTION: f64 = 0.9;

/// The body font's baseline position inside its cell, as a fraction of the
/// cell height — 0.78.
///
/// A named *calibration estimate*, distinct from the exact math metrics:
/// cell dimensions alone do not disclose the body font's baseline. The
/// estimate anchors it near the lower quarter of the cell; it is not an
/// independently measured font metric. The formula's own baseline (from the RaTeX
/// `DisplayList`) is exact; this constant is what that baseline is
/// anchored *to*.
const PROSE_BASELINE_CELL_FRACTION: f64 = 0.78;

/// Internal margin baked into an inline formula's SVG, as a fraction of its
/// em.  A small antialiasing guard — it keeps glyph edges off the raster
/// border — not a frame: unlike the display formula's
/// [`LATEX_PADDING_EMS`], it adds no visual breathing room, and the cell
/// rounding the editor reserves supplies the whole-cell slack.
const INLINE_PADDING_EMS: f64 = 0.05;

/// Internal margin baked into every formula's SVG, as a fraction of its em
/// (RaTeX's `padding` is in the same user units as the glyph coordinates,
/// so an em-relative value scales with the formula).  Keeps glyphs off the
/// image edge so a formula never rubs against the frame — visible in the
/// HTML export, where the figure sits on white, and honoured in-app once
/// the margin flattens onto the document background.  Because it lives in
/// the shared [`render_latex_svg`] output, the in-app raster and the export
/// PNG get the identical margin.
const LATEX_PADDING_EMS: f64 = 0.3;

/// Render a LaTeX display-math source all the way to a `LoadedImage`,
/// suitable for dropping straight into the image cache — same contract as
/// [`super::mermaid::resolve_mermaid`].
///
/// Sizing: the SVG's user-unit scale is RaTeX's `font_size` (em units).
/// We derive it from the terminal's cell pixel height via [`LATEX_EM_TO_CELL`]
/// so the formula's x-height matches body text (not one full cell), drop
/// RaTeX's default padding (a fixed frame dwarfing the glyphs), then
/// rasterize with `SvgScaleMode::Natural` (downscale only) so a formula
/// wider than the column shrinks to fit but never balloons — unlike
/// Mermaid, a formula has a meaningful natural size.  The rasterized image
/// is then fitted to the cell grid ([`fit_latex_to_cell_grid`]): the glyphs
/// are painted in `fg` (the theme's text colour) and flattened onto `bg`
/// (the document background) so dark themes stay legible and scrolling
/// doesn't smear a transparent formula black.
pub fn resolve_latex(
    url: String,
    source: &str,
    max_cells: Option<(u16, u16)>,
    font_size: Option<(u16, u16)>,
    fg: [u8; 4],
    bg: [u8; 4],
) -> Result<LoadedImage, DiagramError> {
    let svg = render_latex_svg(source, fg, font_size)?;
    let image = rasterize_svg(
        &svg,
        SvgSizing {
            envelope: max_cells,
            font_size,
            mode: SvgScaleMode::Natural,
        },
        None, // transparent — fitted / flattened onto `bg` below
    )
    .map_err(DiagramError::from)?;
    Ok(LoadedImage {
        url,
        image: fit_latex_to_cell_grid(image, font_size, bg),
        scratch: None,
        sliced: None,
        direct: None,
        // Display math stays centred; only inline math anchors a baseline.
        formula: None,
    })
}

/// Body-sized text-style math, baseline-anchored.  The cell-grid pad is the
/// documented whole-cell rounding slack, not a frame: the formula's TeX
/// baseline (the exact `DisplayList` height) is placed at the calibrated
/// prose baseline, unlike the display formula's symmetric centring.
pub fn resolve_inline_latex(
    url: String,
    source: &str,
    font_size: Option<(u16, u16)>,
    fg: [u8; 4],
    bg: [u8; 4],
) -> Result<LoadedImage, DiagramError> {
    if source.len() > MAX_LATEX_SOURCE_BYTES {
        return Err(DiagramError::RenderFailed(
            "inline latex source too large".into(),
        ));
    }
    let (svg, baseline) = {
        let _expected = crate::terminal::ExpectedPanic::new();
        catch_unwind(AssertUnwindSafe(|| {
            render_latex_svg_inner(source, fg, font_size, true)
        }))
    }
    .map_err(|p| DiagramError::RenderFailed(panic_message(&p)))?
    .map_err(|e| DiagramError::RenderFailed(e.to_string()))?;
    let baseline = baseline.expect("an inline render always reports its baseline");
    let image = rasterize_svg(
        &svg,
        SvgSizing {
            envelope: None,
            font_size,
            mode: SvgScaleMode::Natural,
        },
        None,
    )
    .map_err(DiagramError::from)?;
    let (image, stored_baseline) = place_inline_baseline_on_cell_grid(
        flatten_to_background(image, bg),
        font_size,
        bg,
        baseline,
    );
    Ok(LoadedImage {
        url,
        image,
        scratch: None,
        sliced: None,
        direct: None,
        // The baseline rides atomically with the raster: composition
        // re-derives the prose row as `baseline_px / cell_height`.
        formula: Some(crate::image::FormulaMetrics {
            baseline_px: stored_baseline,
        }),
    })
}

/// Pad an inline formula raster (already opaque, background-coloured)
/// vertically to a whole number of cells, anchoring the **TeX baseline** —
/// not the image's vertical centre — to the calibrated prose baseline.
///
/// `paint_images` fits the reserved rect top-flush, and the editor reserves
/// whole rows (`ceil(pixels / cell_height)`), so the baseline must already
/// sit at [`PROSE_BASELINE_CELL_FRACTION`] × cell_h inside one specific row
/// of the canvas.  When the baseline sits above that offset within a cell,
/// whole rows are reserved *above* the prose row for numerator overhang
/// (TeX-faithful line growth); the remaining whole rows below hold the
/// denominator's descent.  The leftover slack is the documented whole-cell
/// rounding slack — at most one cell per side — not a symmetric frame: the
/// old centring threw the baseline away and placed a `\dfrac` bar ~8 px
/// below the prose baseline at a 14×32 cell.
///
/// Returns the padded raster plus the stored baseline: its offset in px
/// from the padded top.  Row geometry re-derives it as
/// `baseline_px / cell_h` (the prose row) without re-running RaTeX.
fn place_inline_baseline_on_cell_grid(
    image: image::DynamicImage,
    font_size: Option<(u16, u16)>,
    bg: [u8; 4],
    baseline_px: f64,
) -> (image::DynamicImage, u32) {
    use image::{GenericImageView, ImageBuffer, Rgba};
    let cell_h = u32::from(font_size.map_or(16, |(_, h)| h.max(1)));
    let (w, h) = image.dimensions();
    let cell = f64::from(cell_h);
    let target = PROSE_BASELINE_CELL_FRACTION * cell;
    // Whole rows above the prose row: the baseline must land at `target`
    // inside its own row, so overhang = ceil((baseline − target) / cell).
    let above = f64::ceil((baseline_px - target) / cell).max(0.0);
    let top = (above * cell + target - baseline_px).round().max(0.0);
    let stored = (top + baseline_px).round() as u32;
    let total_px = f64::ceil((top + f64::from(h)) / cell) * cell;
    if total_px <= f64::from(h) {
        // Already an exact multiple of cells with no overhang: the baseline
        // is in place and nothing is added.
        return (image, stored.min(h.saturating_sub(1)));
    }
    let mut canvas = ImageBuffer::from_pixel(w, total_px as u32, Rgba([bg[0], bg[1], bg[2], 255]));
    image::imageops::overlay(&mut canvas, &image, 0, i64::from(top as u32));
    (image::DynamicImage::ImageRgba8(canvas), stored)
}

/// Prepare a formula image for the terminal's cell grid: symmetric
/// breathing room, opaque flatten onto the document background, then
/// vertical centring to an exact whole number of cells.
///
/// The editor reserves image rows in whole cells (`aspect_rows_of` =
/// `ceil(pixels / cell_height)`), and `paint_images` fits the image into
/// the reserved rect **downward-only, flush to the top**.  Without the
/// centring step, the rounding slack between the image's pixel height
/// and the reserved whole-cell height would land entirely below the
/// image as a letter-box gap — the "only blank below the formula" look.
/// Padding up to `rows × cell_height` with the background colour and
/// centring the content in it turns that slack into symmetric top/bottom
/// margins instead, so a formula's vertical rhythm reads like a text
/// line's.
fn fit_latex_to_cell_grid(
    image: image::DynamicImage,
    font_size: Option<(u16, u16)>,
    bg: [u8; 4],
) -> image::DynamicImage {
    let image = add_formula_breathing_room(image, font_size);
    let image = flatten_to_background(image, bg);
    center_on_cell_grid(image, font_size, bg)
}

/// Pad `image` (already opaque, background-coloured) vertically so its
/// height is an exact multiple of the terminal cell height, content
/// centred: `extra = rows × cell_h - height` split equally above and
/// below.
fn center_on_cell_grid(
    image: image::DynamicImage,
    font_size: Option<(u16, u16)>,
    bg: [u8; 4],
) -> image::DynamicImage {
    use image::{GenericImageView, ImageBuffer, Rgba};
    let cell_h = u32::from(font_size.map_or(16, |(_, h)| h.max(1)));
    let (w, h) = image.dimensions();
    let rows = h.div_ceil(cell_h).max(1);
    let total = rows * cell_h;
    if total <= h {
        return image;
    }
    let extra = total - h;
    let top = extra / 2;
    let mut canvas = ImageBuffer::from_pixel(w, total, Rgba([bg[0], bg[1], bg[2], 255]));
    image::imageops::overlay(&mut canvas, &image, 0, i64::from(top));
    image::DynamicImage::ImageRgba8(canvas)
}

/// Composite a formula image onto the document background colour,
/// replacing transparency with opaque `bg`.
///
/// Two consumers need an opaque image:
///
/// * **Halfblocks (active scroll / partial visibility)** encode each cell
///   through `to_rgb8()`, which *drops the alpha channel* — a transparent
///   pixel's RGB is read as-is, and formula transparency is
///   `Rgba([0,0,0,0])`, i.e. black.  With the native protocol idling
///   during scroll (`paint_images` falls back to the halfblocks scratch
///   while `is_scrolling`), every transparent region around a formula
///   painted black — the "black blob while scrolling" bug.  Flattened
///   onto the document `bg`, those regions encode as `bg`, which is
///   exactly what the native composite shows.
/// * **The letter-box / trailing margin** a formula's rect reserves
///   (the rect spans the whole column): the same reasoning — encode as
///   `bg`, not black.
///
/// Result is visually identical to the transparent composite whenever the
/// terminal honours alpha (native kitty/iTerm2/sixel paths), because the
/// cells beneath the image are painted with the same document `bg`.
fn flatten_to_background(image: image::DynamicImage, bg: [u8; 4]) -> image::DynamicImage {
    use image::GenericImageView;
    let [br, bg_, bb, ba] = bg;
    let (w, h) = image.dimensions();
    let mut out = image::ImageBuffer::new(w, h);
    let src = image.to_rgba8();
    for (x, y, px) in src.enumerate_pixels() {
        let [r, g, b, a] = px.0;
        // Straight alpha-over: the formula PNG is transparent or fully
        // opaque in practice (no partial coverage), but stay exact for
        // antialiased glyph edges.
        let alpha = f32::from(a) / 255.0;
        let ba_ = f32::from(ba) / 255.0;
        let mix = |s: u8, d: u8| {
            (f32::from(s) * alpha + f32::from(d) * ba_ * (1.0 - alpha)).round() as u8
        };
        out.put_pixel(
            x,
            y,
            image::Rgba([mix(r, br), mix(g, bg_), mix(b, bb), 255]),
        );
    }
    image::DynamicImage::ImageRgba8(out)
}

/// Pad a formula image with transparent rows above and below so its
/// vertical rhythm matches the text grid.
///
/// Text lines carry their own inter-line gap: a terminal cell is taller
/// than the glyph box (WezTerm line-height 1.15, most fonts 1.2+), so two
/// text lines leave roughly half a cell's worth of background between
/// glyph boxes on each side.  A rendered formula has no such built-in
/// margin — `paint_images` overlays it flush against the reserved cell
/// rect's top edge — so formulas and images sit visually tighter against
/// their neighbours than text does.  ~1/10 of the cell height per side
/// restores the look of an ordinary line gap.  The extra rows are
/// transparent, so layout, aspect-row accounting (`aspect_rows_of`) and
/// the block's reserved height all follow automatically from the new
/// dimensions.
fn add_formula_breathing_room(
    image: image::DynamicImage,
    font_size: Option<(u16, u16)>,
) -> image::DynamicImage {
    use image::imageops::overlay;
    use image::{GenericImageView, ImageBuffer, Rgba};
    let cell_h = u32::from(font_size.map_or(16, |(_, h)| h.max(1)));
    let pad = (cell_h / 10).clamp(1, 6);
    let (w, h) = image.dimensions();
    let mut canvas = ImageBuffer::from_pixel(w, h + 2 * pad, Rgba([0, 0, 0, 0]));
    overlay(&mut canvas, &image.to_rgba8(), 0, i64::from(pad));
    image::DynamicImage::ImageRgba8(canvas)
}

/// Render a LaTeX display-math source to an SVG string, wrapping any panic
/// in a [`DiagramError`] (RaTeX 0.1.x can panic on pathological input —
/// same defence as the mermaid renderer).  Enforces the same
/// [`MAX_LATEX_SOURCE_BYTES`] cap as [`resolve_latex`].
///
/// This is the shared entry point for both the TUI raster path (via
/// [`resolve_latex`]) and the HTML exporter, which embeds the returned SVG
/// as a normalized `<img>` rather than inlining it — the exact parallel to
/// [`super::mermaid::render_mermaid_svg`].
///
/// * `fg` — glyph colour as RGBA.  The TUI passes the theme's text colour;
///   the exporter passes opaque black for a light document background.
/// * `font_size` — the terminal cell's `(width, height)` in pixels; the
///   cell *height* drives RaTeX's `font_size` through [`LATEX_EM_TO_CELL`]
///   so the formula's x-height matches the surrounding body text.  `None`
///   (the exporter's case) falls back to a 16 px cell.
pub fn render_latex_svg(
    source: &str,
    fg: [u8; 4],
    font_size: Option<(u16, u16)>,
) -> Result<String, DiagramError> {
    if source.len() > MAX_LATEX_SOURCE_BYTES {
        return Err(DiagramError::RenderFailed(format!(
            "latex source too large: {} bytes (max {MAX_LATEX_SOURCE_BYTES})",
            source.len()
        )));
    }
    let outcome = {
        let _expected = crate::terminal::ExpectedPanic::new();
        catch_unwind(AssertUnwindSafe(|| {
            render_latex_svg_inner(source, fg, font_size, false)
        }))
    }
    .map_err(|payload| {
        DiagramError::RenderFailed(format!("latex render panic: {}", panic_message(&payload)))
    })?;
    let (svg, _baseline) = outcome.map_err(|e| DiagramError::RenderFailed(format!("{e:#}")))?;
    Ok(svg)
}

/// Unwrapped RaTeX pipeline: parse → layout → display list → `<text>` SVG.
///
/// Returns the SVG plus, for inline renders, the exact TeX baseline in
/// pixels from the raster top — `(height + pad) · em · 96/72`, a pure
/// function of the `DisplayList` and the unit conversions (the SVG is
/// pt-labelled, usvg rasterizes at 96 dpi).  `None` for display math, which
/// stays centred and needs no baseline.
fn render_latex_svg_inner(
    source: &str,
    fg: [u8; 4],
    font_size: Option<(u16, u16)>,
    inline: bool,
) -> Result<(String, Option<f64>), DiagramError> {
    use ratex_layout::layout_options::LayoutOptions;
    use ratex_layout::{layout, to_display_list};
    use ratex_parser::parse;
    use ratex_svg::{render_to_svg, SvgOptions};
    use ratex_types::color::Color;

    let ast = parse(source).map_err(|e| DiagramError::RenderFailed(e.to_string()))?;
    let [r, g, b, a] = fg;
    let mut options = LayoutOptions::default().with_color(Color::new(
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
        f32::from(a) / 255.0,
    ));
    if inline {
        options = options.with_style(ratex_types::math_style::MathStyle::Text);
    }
    let lbox = layout(&ast, &options);
    let display_list = to_display_list(&lbox);
    // The terminal's cell *height* in pixels drives RaTeX's `font_size`
    // (em user units; `px = user_units × 96/72` for the pt-labelled SVG).
    // Display math keeps [`LATEX_EM_TO_CELL`]; inline math uses the
    // calibrated [`INLINE_EM_CELL_FRACTION`] (em px), ~12.5 % above the old
    // default so KaTeX's x-height reads as body text.  Fall back to a 16 px
    // cell (a common terminal default) when unknown.
    let cell_px = font_size.map_or(16.0, |(_, h)| f64::from(h.max(1)));
    let em = if inline {
        cell_px * INLINE_EM_CELL_FRACTION * (72.0 / 96.0)
    } else {
        cell_px * LATEX_EM_TO_CELL
    };
    // The exact TeX baseline, in px from the raster top: `height` is the
    // distance above the baseline (em), plus the em-relative pad.
    let baseline_px = if inline {
        Some((display_list.height + INLINE_PADDING_EMS) * em * (96.0 / 72.0))
    } else {
        None
    };
    if inline {
        let (cell_w, cell_h) = font_size.unwrap_or((8, 16));
        let natural_w = (display_list.width + 2.0 * INLINE_PADDING_EMS) * em * (96.0 / 72.0);
        let natural_h = (display_list.height + display_list.depth + 2.0 * INLINE_PADDING_EMS)
            * em
            * (96.0 / 72.0);
        if !natural_w.is_finite()
            || !natural_h.is_finite()
            || natural_w > 256.0 * f64::from(cell_w.max(1))
            || natural_h > 32.0 * f64::from(cell_h.max(1))
            || crate::image::svg::budget_shrink(natural_w.ceil() as u32, natural_h.ceil() as u32)
                < 1.0
        {
            return Err(DiagramError::RenderFailed(
                "inline formula exceeds its natural-size budget".into(),
            ));
        }
    }
    // Text output, not embedded glyph outlines: `embed_glyphs = false`
    // emits `<text font-family="KaTeX_*">`, and the shared fontdb (which
    // carries the bundled KaTeX faces, see `image::svg`) resolves those
    // families during the rasterize step — exactly how the mermaid path
    // renders.  Single font subsystem; no standalone/embed-fonts features
    // and their parallel ab_glyph stack.
    let svg = render_to_svg(
        &display_list,
        &SvgOptions {
            embed_glyphs: false,
            font_size: em,
            // RaTeX's default padding (a fixed 10 user units per side) is a
            // large, scale-invariant frame next to body-sized text.  We
            // replace it with an em-relative margin so glyphs keep a small,
            // proportional gap from the image edge.  Inline keeps only the
            // small antialiasing guard ([`INLINE_PADDING_EMS`]); the display
            // formula keeps [`LATEX_PADDING_EMS`], and the breathing-room
            // step adds the extra text-line gap on top of it.
            padding: em
                * if inline {
                    INLINE_PADDING_EMS
                } else {
                    LATEX_PADDING_EMS
                },
            ..SvgOptions::default()
        },
    );
    Ok((svg, baseline_px))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fraction bar — the widest horizontal ink run — must sit at the TeX
    /// math axis (0.25 em above the formula's baseline) with the formula's
    /// baseline on the calibrated prose baseline (0.78 × cell_h inside the
    /// formula's prose row). Vertical centring alone cannot preserve this
    /// relationship for formulas with asymmetric height and depth.
    #[test]
    fn fraction_bar_sits_at_the_tex_axis_above_the_prose_baseline() {
        use image::GenericImageView;
        let loaded = resolve_inline_latex(
            "frac".into(),
            r"\dfrac{a+b}{c+d}",
            Some((14, 32)),
            [204, 204, 204, 255],
            [26, 26, 26, 255],
        )
        .expect("dfrac should render");
        let rgba = loaded.image.to_rgba8();
        let (w, h) = loaded.image.dimensions();
        // The bar is the row holding the longest horizontal ink run: the
        // numerator/denominator glyphs are a few px wide, the bar spans the
        // whole formula.
        let mut bar = (0usize, 0usize); // (run length, y)
        for y in 0..h {
            let (mut run, mut best) = (0usize, 0usize);
            for x in 0..w {
                let [r, g, b, a] = rgba.get_pixel(x, y).0;
                let lum = f32::from(r) * 0.299 + f32::from(g) * 0.587 + f32::from(b) * 0.114;
                if a > 32 && lum > 90.0 {
                    run += 1;
                    best = best.max(run);
                } else {
                    run = 0;
                }
            }
            if best > bar.0 {
                bar = (best, y as usize);
            }
        }
        assert!(
            bar.0 >= 10,
            "the fraction bar must be a wide ink run, got {} px at y={}",
            bar.0,
            bar.1
        );
        // Calibrated inline em at a 32 px cell: 0.9 × 32 = 28.8 px, so the
        // TeX axis (0.25 em; PDFLaTeX fd22 = 2.5 pt at 10 pt) is 7.2 px
        // above the formula baseline, and the prose baseline sits at
        // 0.78 × 32 ≈ 25 px inside the prose row.
        let em_px = 0.9 * 32.0;
        let axis = 0.25 * em_px;
        let prose = 0.78 * 32.0;
        let aligned = (0..(h / 32 + 1)).any(|r| {
            let expected = f64::from(r) * 32.0 + prose - axis;
            (bar.1 as f64 - expected).abs() <= 2.0
        });
        assert!(
            aligned,
            "bar at y={} must sit {} px (the TeX axis) above a prose baseline at {} px \
             inside one of the rows — centred placement puts it ~8 px below instead",
            bar.1, axis, prose
        );
    }

    /// The pinned RaTeX layout's `DisplayList` (width/height/depth, in em)
    /// for an inline (Text-style) formula — an independent recomputation, so
    /// expected pixel metrics are derived from the exact math metrics
    /// rather than hard-coded probe numbers.
    fn display_list_em(src: &str) -> (f64, f64, f64) {
        use ratex_layout::layout_options::LayoutOptions;
        use ratex_layout::{layout, to_display_list};
        use ratex_parser::parse;
        let ast = parse(src).expect("parses");
        let options = LayoutOptions::default().with_style(ratex_types::math_style::MathStyle::Text);
        let dl = to_display_list(&layout(&ast, &options));
        (dl.width, dl.height, dl.depth)
    }

    /// Placement edge: a raster whose baseline sits above the row's target
    /// offset reserves one whole overhang row; the baseline lands at the
    /// target offset in the prose row; the canvas stays a whole number of
    /// cells with bg-coloured slack, never a frame.
    #[test]
    fn baseline_placement_reserves_a_whole_overhang_row() {
        use image::{GenericImageView, Rgba};
        // 10×40 opaque red; cell height 30, target 0.78·30 = 23.4; baseline
        // 30 → overhang = ceil((30−23.4)/30) = 1 row; top = 23.4 → 23 px;
        // stored baseline = 53 (row 1, offset 23 ≈ 23.4); total =
        // ceil((23+40)/30)·30 = 90.
        let img = image::DynamicImage::ImageRgba8(image::ImageBuffer::from_pixel(
            10,
            40,
            Rgba([200, 0, 0, 255]),
        ));
        let (out, stored) =
            place_inline_baseline_on_cell_grid(img, Some((8, 30)), [10, 20, 30, 255], 30.0);
        assert_eq!(out.dimensions(), (10, 90));
        assert_eq!(stored, 53, "baseline at row 1, offset 23");
        let rgba = out.to_rgba8();
        assert_eq!(
            rgba.get_pixel(0, 0).0,
            [10, 20, 30, 255],
            "overhang row is bg"
        );
        assert_eq!(rgba.get_pixel(0, 22).0, [10, 20, 30, 255]);
        assert_eq!(
            rgba.get_pixel(0, 23).0,
            [200, 0, 0, 255],
            "content starts at top = 23"
        );
        assert_eq!(
            rgba.get_pixel(0, 62).0,
            [200, 0, 0, 255],
            "content ends at 23+40−1"
        );
        assert_eq!(
            rgba.get_pixel(0, 63).0,
            [10, 20, 30, 255],
            "bottom slack is bg"
        );
    }

    /// Placement edge: a short formula whose baseline is already at or
    /// below the target offset takes NO overhang row — the slack is only
    /// the rounding to a whole cell (the old centre split it 50/50).
    #[test]
    fn baseline_placement_keeps_a_short_formula_to_one_row() {
        use image::{GenericImageView, Rgba};
        // 10×10 opaque; cell 30; target 23.4; baseline 20 → no overhang;
        // top = (23.4−20).round() = 3; stored = 23; total = 30 (one row).
        let img = image::DynamicImage::ImageRgba8(image::ImageBuffer::from_pixel(
            10,
            10,
            Rgba([200, 0, 0, 255]),
        ));
        let (out, stored) =
            place_inline_baseline_on_cell_grid(img, Some((8, 30)), [10, 20, 30, 255], 20.0);
        assert_eq!(out.dimensions(), (10, 30), "one row, no overhang");
        assert_eq!(stored, 23, "baseline at the row target");
    }

    /// The stored inline baseline must be the exact RaTeX baseline —
    /// `(height + pad) · em · 96/72` at the calibrated em — rounded to the
    /// padded raster, and it must land at the calibrated prose-baseline
    /// offset inside a whole row of the canvas.  Checked at both a 14×32
    /// and a 10×20 cell.
    #[test]
    fn inline_metrics_match_the_display_list_baseline() {
        use image::GenericImageView;
        for (src, cell) in [
            ("\\alpha+\\beta", (14u16, 32u16)),
            ("x^2+y_1", (14, 32)),
            ("\\dfrac{a+b}{c+d}", (10, 20)),
        ] {
            let (w_em, h_em, d_em) = display_list_em(src);
            let loaded = resolve_inline_latex(
                src.into(),
                src,
                Some(cell),
                [204, 204, 204, 255],
                [26, 26, 26, 255],
            )
            .unwrap_or_else(|e| panic!("{src} should render: {e}"));
            let Some(metrics) = &loaded.formula else {
                panic!("{src}: the inline raster must carry its baseline metric");
            };
            // The exact DisplayList geometry at the calibrated em
            // (px = user units × 96/72; em px = 0.9 × cell_h):
            let em_px = 0.9 * f64::from(cell.1);
            let raw_baseline = (h_em + 0.05) * em_px;
            let natural_h = (h_em + d_em + 0.1) * em_px;
            let cell_h = f64::from(cell.1);
            let target = 0.78 * cell_h;
            // Placement: whole overhang rows + the shift to the target.
            let b = f64::ceil((raw_baseline - target) / cell_h).max(0.0);
            let top = (b * cell_h + target - raw_baseline).round();
            let (w, h) = loaded.image.dimensions();
            assert_eq!(
                h % u32::from(cell.1),
                0,
                "{src}: the canvas must be whole rows at {cell:?}"
            );
            assert_eq!(
                h as u64,
                (f64::ceil((top + natural_h.ceil()) / cell_h) * cell_h) as u64,
                "{src}: canvas {h}px must be the baseline-placed whole rows"
            );
            // No extra frame padding on the width: the raster is exactly
            // the SVG's natural width (the 0.05 em antialiasing guard is
            // the only margin).  The whole-cell width slack is a layout
            // reservation (`ceil(w / cell_w)` cells), never raster pixels.
            let natural_w = (w_em + 0.1) * em_px;
            assert!(
                (f64::from(w) - natural_w).abs() <= 1.0,
                "{src}: raster width {w} must be the natural SVG width \
                 {natural_w} (±1 rounding) — no frame padding"
            );
            // The stored baseline is the PLACED baseline: the exact
            // DisplayList baseline shifted to the prose target, rounded.
            assert!(
                (metrics.baseline_px as u64) < h as u64,
                "{src}: the baseline must be inside the raster"
            );
            assert!(
                (metrics.baseline_px as f64 - (top + raw_baseline)).abs() <= 1.0,
                "{src}: stored baseline {} px must be the exact DisplayList \
                 baseline {raw_baseline} px shifted to the prose target (±1 rounding)",
                metrics.baseline_px
            );
            let offset = metrics.baseline_px % u32::from(cell.1);
            assert!(
                (offset as f64 - target).abs() <= 1.0,
                "{src}: the baseline must sit at the prose offset {target} px \
                 in its row, got {offset}"
            );
        }
    }

    /// Baseline uniformity: a superscript-only formula and a matrix with a
    /// deep descent must place their TeX baselines at the *same* offset in
    /// their prose row — the descent grows whole rows below, the numerator
    /// overhang grows whole rows above.
    #[test]
    fn x_squared_and_matrix_baselines_share_the_prose_row_offset() {
        let x2 = resolve_inline_latex(
            "x2".into(),
            "x^2",
            Some((14, 32)),
            [204, 204, 204, 255],
            [26, 26, 26, 255],
        )
        .expect("x^2 renders");
        let matrix = resolve_inline_latex(
            "m".into(),
            r"\begin{matrix}a&b\\c&d\end{matrix}",
            Some((14, 32)),
            [204, 204, 204, 255],
            [26, 26, 26, 255],
        )
        .expect("matrix renders");
        let m2 = x2.formula.expect("x^2 metric");
        let mm = matrix.formula.expect("matrix metric");
        let target = 0.78 * 32.0;
        for (name, m) in [("x^2", m2), ("matrix", mm)] {
            let offset = (m.baseline_px % 32) as f64;
            assert!(
                (offset - target).abs() <= 1.0,
                "{name}: baseline offset {offset} must be the prose offset {target}"
            );
        }
        assert_eq!(x2.image.height(), 32, "x^2 (no descent) stays one row");
        assert!(
            matrix.image.height() > 2 * 32,
            "the matrix's descent must reserve whole rows below the prose row"
        );
    }

    #[test]
    fn inline_math_rejects_over_tall_natural_size_instead_of_shrinking() {
        assert!(
            resolve_inline_latex(
                "tall".into(),
                r"\rule{1em}{100em}",
                Some((10, 20)),
                [204, 204, 204, 255],
                [26, 26, 26, 255],
            )
            .is_err(),
            "over-tall inline math must fall back to literal source"
        );
    }

    /// Display math must rasterize to a real image through the same
    /// `LoadedImage` contract as mermaid.  The KaTeX faces are bundled
    /// into the shared fontdb (`image::svg`), so this needs no system
    /// fonts and runs in CI — the render path's regression guard.
    #[test]
    fn latex_display_math_renders_to_loaded_image() {
        let loaded = resolve_latex(
            "test".into(),
            r"x^2 + y^2 = z^2",
            Some((80, 24)),
            Some((8, 16)),
            [0xcc, 0xcc, 0xcc, 255],
            [0x1a, 0x1a, 0x1a, 255],
        )
        .expect("trivial display math should render");
        assert!(loaded.image.width() > 0);
        assert!(loaded.image.height() > 0);
    }

    /// The rendered formula's pixel height must track the terminal cell
    /// font size (16 px cell → roughly one text line), not balloon to the
    /// whole image envelope — the bug where display math rendered huge.
    /// Bundled fonts → runs in CI.
    #[test]
    fn latex_image_height_tracks_cell_font_size() {
        // Envelope is 40 cells tall but a single-line formula must come
        // out near one cell (16 px) tall — Natural mode, no fill.
        let loaded = resolve_latex(
            "test".into(),
            r"x^2 + y^2 = z^2",
            Some((80, 40)),
            Some((8, 16)),
            [0xcc, 0xcc, 0xcc, 255],
            [0x1a, 0x1a, 0x1a, 255],
        )
        .expect("display math should render");
        // Natural-mode raster keeps the SVG's own size: with the em bump
        // (`LATEX_EM_TO_CELL`) and the em-relative internal padding
        // (`LATEX_PADDING_EMS`), a single line of math at a 16 px cell
        // rasterizes to a few tens of pixels and then rounds up to a whole
        // number of cells — a couple of cells tall, nowhere near the 640 px
        // a full-envelope fill of the 40-cell envelope would produce.
        let h = loaded.image.height();
        assert!(
            (16..=64).contains(&h),
            "single-line formula should be a few cells tall, got {h}px"
        );
    }

    /// The breathing-room pad scales with the reported cell height and
    /// keeps the formula's pixels centred vertically inside it (no
    /// content shift, only transparent margin added top and bottom).
    #[test]
    fn breathing_room_pads_transparent_rows_above_and_below() {
        use image::GenericImageView;
        use image::Rgba;
        // 4×8 solid-red image, cell height 30 → pad 3 rows each side.
        let img = image::DynamicImage::ImageRgba8(image::ImageBuffer::from_pixel(
            4,
            8,
            Rgba([200, 0, 0, 255]),
        ));
        let padded = add_formula_breathing_room(img, Some((8, 30)));
        assert_eq!(padded.dimensions(), (4, 8 + 2 * 3));
        let rgba = padded.to_rgba8();
        // Top pad rows transparent, first content row red.
        assert_eq!(rgba.get_pixel(0, 0).0[3], 0);
        assert_eq!(rgba.get_pixel(0, 2).0[3], 0);
        assert_eq!(rgba.get_pixel(0, 3).0, [200, 0, 0, 255]);
        // Bottom pad rows transparent, last content row red.
        // content 3..11, bottom pad 11..14 (height 8+2*3).
        assert_eq!(rgba.get_pixel(0, 8 + 3).0[3], 0); // first bottom pad
        assert_eq!(rgba.get_pixel(0, 8 + 2 * 3 - 1).0[3], 0); // last row
        assert_eq!(rgba.get_pixel(0, 8 + 3 - 1).0, [200, 0, 0, 255]); // last content
    }

    /// Unknown cell size falls back to a 16 px cell (pad 1).
    #[test]
    fn breathing_room_falls_back_to_a_default_cell_height() {
        let img = image::DynamicImage::new_rgba8(2, 2);
        let padded = add_formula_breathing_room(img, None);
        assert_eq!(padded.height(), 2 + 2);
    }

    /// Transparent formula pixels must flatten onto the document
    /// background, never survive as black — halfblocks encode via
    /// `to_rgb8()`, which reads a transparent pixel's RGB as-is
    /// (`Rgba([0,0,0,0])` → black).
    #[test]
    fn flatten_composites_transparency_onto_the_document_background() {
        use image::{GenericImageView, Rgba};
        let img = image::DynamicImage::ImageRgba8(image::ImageBuffer::from_fn(2, 1, |x, _| {
            if x == 0 {
                Rgba([0, 0, 0, 0])
            } else {
                Rgba([200, 0, 0, 255])
            }
        }));
        let out = flatten_to_background(img, [10, 20, 30, 255]);
        let rgba = out.to_rgba8();
        assert_eq!(
            rgba.get_pixel(0, 0).0,
            [10, 20, 30, 255],
            "transparent → bg"
        );
        assert_eq!(
            rgba.get_pixel(1, 0).0,
            [200, 0, 0, 255],
            "opaque content kept"
        );
        assert_eq!(rgba.get_pixel(0, 0).0[3], 255, "output fully opaque");
        assert_eq!(out.dimensions(), (2, 1), "dimensions unchanged");
    }

    /// Rounding slack between a formula's pixel height and the reserved
    /// whole-cell rows must split above AND below the content (vertical
    /// centring) — never all below, which would read as a gap only under
    /// the formula.
    #[test]
    fn cell_grid_centring_splits_rounding_slack_evenly() {
        use image::{GenericImageView, Rgba};
        // 2×10 opaque red; cell height 30 → ceil(10/30)=1 row = 30px →
        // extra 20px, 10 above and 10 below.
        let img = image::DynamicImage::ImageRgba8(image::ImageBuffer::from_pixel(
            2,
            10,
            Rgba([200, 0, 0, 255]),
        ));
        let out = center_on_cell_grid(img, Some((8, 30)), [10, 20, 30, 255]);
        assert_eq!(out.dimensions(), (2, 30));
        let rgba = out.to_rgba8();
        assert_eq!(rgba.get_pixel(0, 0).0, [10, 20, 30, 255], "top pad = bg");
        assert_eq!(rgba.get_pixel(0, 9).0, [10, 20, 30, 255], "top half slack");
        assert_eq!(
            rgba.get_pixel(0, 10).0,
            [200, 0, 0, 255],
            "content starts at 10"
        );
        assert_eq!(
            rgba.get_pixel(0, 19).0,
            [200, 0, 0, 255],
            "content ends at 19"
        );
        assert_eq!(
            rgba.get_pixel(0, 20).0,
            [10, 20, 30, 255],
            "bottom pad = bg"
        );
        assert_eq!(
            rgba.get_pixel(0, 29).0,
            [10, 20, 30, 255],
            "bottom half slack"
        );
    }

    /// An image that already fills its cells exactly is untouched.
    #[test]
    fn cell_grid_centring_is_a_noop_on_exact_multiples() {
        let img = image::DynamicImage::new_rgba8(4, 60); // 2 rows of 30
        let out = center_on_cell_grid(img, Some((8, 30)), [0, 0, 0, 255]);
        assert_eq!(out.height(), 60);
    }
}
