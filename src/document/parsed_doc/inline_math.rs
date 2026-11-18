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
        // Use the same document event parser as the AST, including reference resolution.
        // Source ranges remain stable when composition drops separators or splits rows.
        let source_links: Vec<_> = crate::markdown::parse_offsets::DocParser::new(self.source())
            .into_offset_iter()
            .filter_map(|(event, source)| match event {
                pulldown_cmark::Event::Start(pulldown_cmark::Tag::Link {
                    dest_url, title, ..
                }) => Some(InlineLink {
                    cols: 0..0,
                    source,
                    url: dest_url.into_string(),
                    title: (!title.is_empty()).then(|| title.into_string()),
                }),
                _ => None,
            })
            .collect();
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
                let positions: Vec<_> = (0..=styled.len())
                    .map(|c| row_map::rendered_to_raw_col(self, block, abs - rows.start, c))
                    .collect();
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
                    source_links
                        .iter()
                        .find(|link| link.source.contains(&byte))
                        .cloned()
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
