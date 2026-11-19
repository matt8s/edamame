//! Measured paragraph composition. Formula atoms are sized before wrapping;
//! spaces in the final cell rows are paint reservations, never layout sentinels.

use std::cell::OnceCell;
use std::collections::HashMap;
use std::ops::Range;

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::ParsedDoc;
use crate::config::Theme;
use crate::diagram::{synthetic_url, DiagramSource};
use crate::document::row_map::{self, RawPos};
use crate::document::SourceMap;
use crate::image::{DecodeStatus, ImageCache};
use crate::markdown::table_layout::char_cells;
use crate::markdown::{Block, ColOrigin, ContentKind, RowOrigin};

/// A typeset atom's rectangle in the ordinary parsed row space.
#[derive(Debug, Clone)]
pub struct InlineFormula {
    pub block: usize,
    pub row: usize,
    pub col: u16,
    pub width: u16,
    pub height: u16,
    /// Approximate baseline geometry in whole terminal rows.
    pub ascent: u16,
    pub descent: u16,
    pub url: String,
    pub raw: Range<RawPos>,
}

/// One composition result shared by painting, source mapping and reveal.
#[derive(Debug, Clone, Default)]
pub struct InlineLayout {
    pub requests: Vec<crate::document::ImageBlockInfo>,
    /// Every row of a recomposed paragraph, including rows without links.
    pub links: HashMap<usize, Vec<InlineLink>>,
    pub formulas: Vec<InlineFormula>,
    /// Absolute row → raw position per painted character, plus its end.
    pub maps: HashMap<usize, Vec<RawPos>>,
    /// Absolute row → containing paragraph's rendered rows and source lines.
    pub paragraphs: HashMap<usize, (Range<usize>, Range<u32>)>,
}

/// A rendered character range tied to the parser's exact source link, not a paint run.
#[derive(Debug, Clone)]
pub struct InlineLink {
    pub cols: Range<usize>,
    pub source: Range<usize>,
    pub url: String,
    pub title: Option<String>,
}

struct Atom {
    link: Option<InlineLink>,
    chars: Vec<(char, Style, RawPos)>,
    width: usize,
    height: usize,
    ascent: usize,
    descent: usize,
    formula: Option<String>,
    end: RawPos,
}

impl ParsedDoc {
    /// Compose top-level prose using already decoded formula dimensions. Pending,
    /// oversized and failed atoms keep their delimited source. No source is rewritten.
    pub fn measure_inline_math(
        &mut self,
        images: &ImageCache,
        font: (u16, u16),
        width: usize,
        theme: &Theme,
    ) {
        let key = |src: &str| {
            // `v2`: the inline raster is baseline-anchored on the cell grid
            // (see `math::resolve_inline_latex`), so any entry stored under
            // the v1 centred geometry must be treated as stale.  The font
            // cell and theme colours stay in the key: a resize or retheme is
            // a different raster.
            synthetic_url(&DiagramSource::InlineLatex(format!(
                "inline-text-v2:{font:?}:{:?}:{:?}:{src}",
                theme.palette.text, theme.palette.bg,
            )))
        };
        let events = &self.inline_math_sources;
        if events.is_empty() {
            return;
        }

        let mut replacements = HashMap::new();
        let mut requests = Vec::new();
        for block in 0..self.source_map.block_count() {
            let Some(range) = self.source_map.original_range_for_block(block) else {
                continue;
            };
            // Tables and container prefixes deliberately retain literal source.
            if !matches!(
                self.real_block_for_byte(range.start),
                Some(Block::Paragraph { .. })
            ) {
                continue;
            }
            if range.len() > 64 * 1024 {
                continue;
            }
            let rows = self.source_map.rendered_lines_for_block(block);
            let from = events.partition_point(|(r, _)| r.start < range.start);
            let to = events.partition_point(|(r, _)| r.start < range.end);
            let candidates: Vec<_> = events[from..to]
                .iter()
                .filter(|(r, _)| r.end <= range.end)
                .collect();
            if candidates.is_empty() {
                continue;
            }
            for (_, src) in &candidates {
                let source = DiagramSource::InlineLatex(src.clone());
                requests.push(crate::document::ImageBlockInfo {
                    block_idx: block,
                    alt: src.clone(),
                    url: key(src),
                    source: Some(source),
                });
            }
            // Pending/failed-only paragraphs need no column maps or composition.
            if !candidates.iter().any(|(_, src)| {
                matches!(images.status(&key(src)), Some(DecodeStatus::Ready { .. }))
            }) {
                continue;
            }
            // Reuse the exact resolved link events already collected by the parser.
            let paragraph_links: Vec<_> = self
                .inline_math_links
                .iter()
                .filter(|link| link.source.start < range.end && link.source.end > range.start)
                .collect();
            // A monotone char → byte index built once, not a source-prefix scan
            // for every painted character. Bound composition of pathological
            // paragraphs independently of the document/renderer's text path.
            let raw_bytes: Vec<_> = self.source()[range.clone()]
                .char_indices()
                .map(|(b, _)| range.start + b)
                .chain(std::iter::once(range.end))
                .collect();
            let raw_starts: Vec<_> = std::iter::once(0)
                .chain(
                    self.source()[range.clone()]
                        .chars()
                        .enumerate()
                        .filter_map(|(i, c)| (c == '\n').then_some(i + 1)),
                )
                .collect();
            let mut composed = Vec::new();
            for abs in rows.clone() {
                let origin = &self.row_origins[abs];
                if !matches!(
                    origin.cols,
                    ColOrigin::Content {
                        kind: ContentKind::Inline | ContentKind::Flow,
                        ..
                    }
                ) {
                    composed.push((
                        self.lines[abs].clone(),
                        origin.clone(),
                        None,
                        Vec::new(),
                        Vec::new(),
                    ));
                    continue;
                }
                let styled: Vec<_> = self.lines[abs]
                    .spans
                    .iter()
                    .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
                    .collect();
                // One row resolution per row, not one cache lookup and cell walk per char.
                let positions: Vec<_> =
                    row_map::rendered_row_positions(self, block, abs - rows.start, styled.len());
                let byte_of = |p: RawPos| {
                    raw_bytes
                        .get(
                            raw_starts
                                .get(p.line)
                                .copied()
                                .unwrap_or(raw_bytes.len() - 1)
                                + p.col,
                        )
                        .copied()
                        .unwrap_or(range.end)
                };
                let bytes: Vec<_> = positions.iter().copied().map(byte_of).collect();
                let link_at = |byte| {
                    paragraph_links
                        .iter()
                        .find(|link| link.source.contains(&byte))
                        .map(|link| InlineLink {
                            cols: 0..0,
                            source: link.source.clone(),
                            url: link.url.clone(),
                            title: link.title.clone(),
                        })
                };
                let mut atoms = Vec::new();
                let mut i = 0;
                let mut has_formula = false;
                let mut candidate = 0;
                while i < styled.len() {
                    while candidate < candidates.len() && candidates[candidate].0.start < bytes[i] {
                        candidate += 1;
                    }
                    let formula = candidates
                        .get(candidate)
                        .filter(|(r, _)| r.start == bytes[i]);
                    if let Some((r, src)) = formula {
                        let url = key(src);
                        if let Some(DecodeStatus::Ready {
                            image,
                            formula: metric,
                        }) = images.status(&url)
                        {
                            let w = image.width().div_ceil(u32::from(font.0.max(1))) as usize;
                            let h = image.height().div_ceil(u32::from(font.1.max(1))) as usize;
                            // Atomic formulas never split or silently shrink to illegibility.
                            let end = bytes.partition_point(|b| *b < r.end);
                            if w > 0 && w <= width && h > 0 && h <= 32 && end <= styled.len() {
                                // The ascent is the whole rows above the TeX baseline.  For a
                                // real inline raster that is the stored baseline metric —
                                // atomically in the `Ready` entry under a key that carries
                                // this exact font cell, so the metric and the pixels always
                                // agree.  A `Ready` entry without metrics (legacy synthetic
                                // test images) keeps the documented centred `(h-1)/2`
                                // fallback.
                                let ascent = match metric {
                                    Some(m) => (m.baseline_px / u32::from(font.1.max(1)))
                                        .min(h.saturating_sub(1) as u32)
                                        as usize,
                                    None => (h - 1) / 2,
                                };
                                atoms.push(Atom {
                                    link: link_at(bytes[i]),
                                    chars: vec![(' ', Style::default(), positions[i])],
                                    width: w,
                                    height: h,
                                    ascent,
                                    descent: h - 1 - ascent,
                                    formula: Some(url),
                                    end: positions[end],
                                });
                                has_formula = true;
                                i = end;
                                continue;
                            }
                        }
                    }
                    let (c, style) = styled[i];
                    atoms.push(Atom {
                        link: link_at(bytes[i]),
                        chars: vec![(c, style, positions[i])],
                        width: char_cells(c),
                        height: 1,
                        ascent: 0,
                        descent: 0,
                        formula: None,
                        end: positions[i + 1],
                    });
                    i += 1;
                }
                if !has_formula {
                    let mut links = Vec::new();
                    for (col, byte) in bytes.iter().take(styled.len()).enumerate() {
                        if let Some(mut link) = link_at(*byte) {
                            link.cols = col..col + 1;
                            push_link(&mut links, link);
                        }
                    }
                    composed.push((
                        self.lines[abs].clone(),
                        origin.clone(),
                        None,
                        Vec::new(),
                        links,
                    ));
                    continue;
                }
                composed.extend(compose(
                    atoms,
                    width.max(1),
                    origin,
                    *positions.last().unwrap(),
                    self.lines[abs].style,
                ));
            }
            if composed.iter().any(|(_, _, map, _, _)| map.is_some()) {
                replacements.insert(block, composed);
            }
        }
        self.inline_math.requests = requests;
        if replacements.is_empty() {
            return;
        }
        let originals: Vec<_> = (0..self.source_map.block_count())
            .map(|b| self.source_map.original_range_for_block(b).unwrap())
            .collect();
        let extended = super::build_extended_ranges(&originals, self.source.len());
        // Rebuild the row vectors in one linear pass.  `self.row_origins` is read while
        // still intact (the original, unshifted layout), so a composed block's source-line
        // range is correct no matter how earlier composed blocks changed the layout height;
        // unchanged rows are carried over as-is.
        let mut lines = Vec::new();
        let mut origins = Vec::new();
        let mut owners = Vec::new();
        let mut old_to_new = HashMap::new();
        for block in 0..originals.len() {
            let old = self.source_map.rendered_lines_for_block(block);
            old_to_new.insert(old.start, lines.len());
            // Zero-row blocks borrow a neighbour's range in SourceMap.
            if self.per_block_own[block] == 0 {
                continue;
            }
            if let Some(composed) = replacements.remove(&block) {
                let start = lines.len();
                let end = start + composed.len();
                let source_lines = self.row_origins[old.start..old.end]
                    .iter()
                    .filter_map(|o| o.lines.as_ref())
                    .fold(None, |acc: Option<Range<u32>>, r| {
                        Some(acc.map_or(r.clone(), |a| a.start.min(r.start)..a.end.max(r.end)))
                    })
                    .unwrap_or(0..1);
                self.per_block_own[block] = composed.len();
                for (line, origin, map, formulas, links) in composed {
                    let abs = lines.len();
                    self.inline_math.links.insert(abs, links);
                    if let Some(map) = map {
                        self.inline_math.maps.insert(abs, map);
                    }
                    self.inline_math
                        .paragraphs
                        .insert(abs, (start..end, source_lines.clone()));
                    for (col, w, h, offset, ascent, url, raw) in formulas {
                        self.inline_math.formulas.push(InlineFormula {
                            block,
                            row: abs + offset,
                            col: col as u16,
                            width: w as u16,
                            height: h as u16,
                            // The atom's aligned baseline: for a real inline
                            // raster this is the stored baseline metric, for
                            // a legacy metric-less `Ready` entry the
                            // documented centred fallback.  The prose row it
                            // indexes was already placed by `compose`, so
                            // painting, mapping and reveal all agree.
                            ascent: ascent as u16,
                            descent: (h - 1 - ascent) as u16,
                            url,
                            raw,
                        });
                    }
                    lines.push(line);
                    origins.push(origin);
                    owners.push(block);
                }
            } else {
                for abs in old {
                    old_to_new.insert(abs, lines.len());
                    lines.push(self.lines[abs].clone());
                    origins.push(self.row_origins[abs].clone());
                    owners.push(block);
                }
            }
        }
        for row in self
            .heading_anchors
            .values_mut()
            .chain(self.footnote_anchors.values_mut())
        {
            *row = *old_to_new.get(row).unwrap_or(row);
        }
        self.lines = lines;
        self.row_origins = origins;
        self.source_map = SourceMap::new(owners, extended, originals, self.source.len());
        self.row_cache = (0..self.lines.len()).map(|_| OnceCell::new()).collect();
        self.visual_rows.borrow_mut().clear();
        self.source_lines = OnceCell::new();
        self.joined_maps.borrow_mut().clear();
    }
}

fn push_link(links: &mut Vec<InlineLink>, link: InlineLink) {
    if let Some(last) = links
        .last_mut()
        .filter(|last| last.source == link.source && last.cols.end == link.cols.start)
    {
        last.cols.end = link.cols.end;
    } else {
        links.push(link);
    }
}

/// A formula's rect in the composed row space: `(col, width, height, top,
/// ascent, url, raw_range)`.  `ascent` is the atom's aligned baseline — the
/// prose row's index within the band, hence the row index of the TeX
/// baseline within the formula's own rect — carried from the measured atom
/// so the stored metric (not a re-derived guess) survives into
/// [`InlineFormula`].
type FormulaRect = (usize, usize, usize, usize, usize, String, Range<RawPos>);
type ComposedRow = (
    Line<'static>,
    RowOrigin,
    Option<Vec<RawPos>>,
    Vec<FormulaRect>,
    Vec<InlineLink>,
);

/// Greedy measured atomic wrap. Text words are grouped before wrapping; long
/// words hard-split by characters. Formula width is independent of source length.
fn compose(
    atoms: Vec<Atom>,
    width: usize,
    origin: &RowOrigin,
    end: RawPos,
    style: Style,
) -> Vec<ComposedRow> {
    let chars: Vec<_> = atoms
        .iter()
        .map(|a| {
            (
                if a.formula.is_some() {
                    '\u{fffc}'
                } else {
                    a.chars[0].0
                },
                a.chars[0].1,
            )
        })
        .collect();
    let widths: Vec<_> = atoms.iter().map(|a| a.width).collect();
    let bands = crate::document::wrap::visual_rows_of_measured_chars(
        &chars,
        &widths,
        width,
        crate::document::wrap::Indent::NONE,
    );
    let mut out = Vec::new();
    for (start, stop, next) in bands {
        let band = &atoms[start..stop];
        // Band baseline: the aligned prose row is the highest TeX baseline
        // among the band's atoms (each atom's ascent is the stored baseline
        // metric, or the documented centred fallback for legacy metric-less
        // `Ready` entries).  Every atom's rect is aligned so its own
        // baseline lands on this row.
        let baseline = band.iter().map(|a| a.ascent).max().unwrap_or(0);
        let descent = band.iter().map(|a| a.descent).max().unwrap_or(0);
        let height = baseline + descent + 1;
        for y in 0..height {
            let mut spans = Vec::new();
            let mut map = Vec::new();
            let mut rects = Vec::new();
            let mut links = Vec::new();
            let mut char_col = 0;
            let mut x = 0;
            for atom in band {
                if let Some(url) = &atom.formula {
                    let top = baseline.saturating_sub(atom.ascent);
                    if y == 0 {
                        rects.push((
                            x,
                            atom.width,
                            atom.height,
                            top,
                            atom.ascent,
                            url.clone(),
                            atom.chars[0].2..atom.end,
                        ));
                    }
                    spans.push(Span::raw(" ".repeat(atom.width)));
                    map.extend(std::iter::repeat_n(atom.chars[0].2, atom.width));
                } else {
                    for &(c, s, p) in &atom.chars {
                        if y == baseline {
                            spans.push(Span::styled(c.to_string(), s));
                            map.push(p);
                        } else {
                            spans.push(Span::raw(" ".repeat(atom.width)));
                            map.extend(std::iter::repeat_n(p, atom.width));
                        }
                    }
                }
                let len = if atom.formula.is_some() || y != baseline {
                    atom.width
                } else {
                    atom.chars.len()
                };
                if atom.formula.is_some() || y == baseline {
                    if let Some(mut link) = atom.link.clone() {
                        link.cols = char_col..char_col + len;
                        push_link(&mut links, link);
                    }
                }
                char_col += len;
                x += atom.width;
            }
            map.push(atoms.get(next).map_or(end, |a| a.chars[0].2));
            let mut physical = origin.clone();
            physical.hang = 0;
            if let (Some(first), Some(last)) = (band.first(), band.last()) {
                physical.lines = Some(
                    first.chars[0].2.line as u32..last.chars.last().unwrap().2.line as u32 + 1,
                );
            }
            if y != baseline {
                physical.cols = ColOrigin::Chrome;
            }
            out.push((
                Line::from(spans).style(style),
                physical,
                Some(map),
                rects,
                links,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> &'static Theme {
        Box::leak(Box::new(Theme::default()))
    }

    /// Two top-level paragraphs: the first carries a tall formula that grows its band from one
    /// row to two.  The second paragraph's source range and reveal must be read from the original,
    /// unshifted row origins — a stale per-splice offset would report the wrong source line.
    #[test]
    fn a_tall_first_formula_does_not_stale_the_later_paragraphs_source_range() {
        let src = "Tall paragraph first line with $\\frac{a}{b}$ and words.\n\
                   Second line of the first paragraph, continuing the thought.\n\
                   \n\
                   Beta with a short $y = x + 1$ formula and trailing words after it.\n";
        let font: (u16, u16) = (8, 16);
        let width = 60usize;
        let mut d = ParsedDoc::build_with_overrides(
            src,
            theme(),
            true,
            4,
            None,
            None,
            false,
            width,
            false,
            false,
            true,
            false,
            None,
        );
        let mut images = ImageCache::new();
        // Pass one: empty cache, just to learn the synthetic URLs the measure will key on.
        d.measure_inline_math(&images, font, width, theme());
        let urls: Vec<String> = d
            .inline_math
            .requests
            .iter()
            .map(|r| r.url.clone())
            .collect();
        assert_eq!(
            urls.len(),
            2,
            "expected exactly two inline formulas, got {urls:?}"
        );
        // First formula tall (two rows), second short (one row).
        images.set_decoded(
            &urls[0],
            image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                40,
                32,
                image::Rgba([255, 255, 255, 255]),
            )),
        );
        images.set_decoded(
            &urls[1],
            image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                16,
                16,
                image::Rgba([255, 255, 255, 255]),
            )),
        );
        // Pass two: real rasters, so composition runs and the row vectors rebuild.
        d.measure_inline_math(&images, font, width, theme());

        assert_eq!(
            d.inline_math.formulas.len(),
            2,
            "both formulas must be placed"
        );
        assert_eq!(
            d.inline_math.formulas[0].block, 0,
            "first formula belongs to the first paragraph"
        );
        assert_eq!(
            d.inline_math.formulas[1].block, 2,
            "second formula belongs to the second paragraph"
        );
        let first0 = d.source_map.rendered_lines_for_block(0).start;
        let first2 = d.source_map.rendered_lines_for_block(2).start;
        let entry0 = d
            .inline_math
            .paragraphs
            .get(&first0)
            .expect("first paragraph band must be recorded");
        let entry2 = d
            .inline_math
            .paragraphs
            .get(&first2)
            .expect("second paragraph band must be recorded");
        // The first paragraph spans two source lines and its tall formula grew the band past one
        // row.
        assert_eq!(
            entry0.1,
            0..2,
            "first paragraph reveal must span its two source lines"
        );
        assert!(
            entry0.0.len() >= 2,
            "tall formula band must span >= 2 rows: {:?}",
            entry0.0
        );
        // The second paragraph is a single source line: its reveal must be its own 0..1, not a
        // stale copy of the first paragraph's 0..2. A per-splice offset read taken after the
        // first block's band grew would report the first paragraph's range here.
        assert_eq!(
            entry2.1,
            0..1,
            "second paragraph reveal must be its own single source line, not the first's 0..2"
        );
    }
}
