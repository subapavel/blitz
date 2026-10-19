//! The collapsing border model of CSS tables (CSS 2.1 §17.6.2, CSS Tables 3 §"Collapsed borders").
//!
//! In a table with `border-collapse: collapse` the borders of cells, rows and row groups are not boxes of their own:
//! they are lines centred on the grid lines between the cells. Where several borders meet on one segment of a line,
//! exactly one wins (§17.6.2.1); a cell's used border on a side is half of the winning width on each segment along
//! that side, and the table holds half of the winning width of its outer lines. This module is pure: it knows rows,
//! tracks and spans, and nothing about the document. [`super::table`] reads the styles, calls [`resolve`] and gives
//! the halves to Taffy.
//!
//! Out of scope (F22c): `<col>` / `<colgroup>` borders, which need column boxes; `rowspan` (every cell spans one
//! row); `direction: rtl` in the last conflict rule.

use style::color::AbsoluteColor;
use style::values::specified::BorderStyle;

/// Indices of the sides in an edge array: `[top, right, bottom, left]`.
pub const TOP: usize = 0;
pub const RIGHT: usize = 1;
pub const BOTTOM: usize = 2;
pub const LEFT: usize = 3;

/// One border: the used width in CSS px (`0` for `none` and `hidden`, as in a computed style), the style and the
/// colour, already resolved against the `currentColor` of the element it came from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BorderSpec {
    pub width: f32,
    pub style: BorderStyle,
    pub color: AbsoluteColor,
}

impl BorderSpec {
    /// No border.
    pub const NONE: BorderSpec = BorderSpec {
        width: 0.0,
        style: BorderStyle::None,
        color: AbsoluteColor::TRANSPARENT_BLACK,
    };

    /// The width that takes part in a comparison: a `none` border has none (and `hidden` is handled before widths are
    /// compared).
    fn effective_width(&self) -> f32 {
        if self.style == BorderStyle::None {
            0.0
        } else {
            self.width
        }
    }

    /// The width a resolved segment takes up: nothing for `none` and `hidden`.
    fn used_width(&self) -> f32 {
        if self.style.none_or_hidden() {
            0.0
        } else {
            self.width.max(0.0)
        }
    }
}

/// The kind of element a border comes from, in the order of CSS 2.1 §17.6.2.1 rule 5 (later wins): table, row group,
/// row, cell. Column groups and columns belong between the table and the row groups (F22c).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EdgeOrigin {
    Table,
    RowGroup,
    Row,
    Cell,
}

/// A border competing for a segment, with where it comes from: `row` and `track` (both from 0) are the position of the
/// element that owns it (a row or a row group is at track 0), for the last rule, "the one further to the top and
/// left wins".
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Candidate {
    pub spec: BorderSpec,
    pub origin: EdgeOrigin,
    pub row: u32,
    pub track: u32,
}

impl Candidate {
    fn position(&self) -> (u32, u32) {
        (self.row, self.track)
    }
}

/// The border that wins between two candidates for the same segment (CSS 2.1 §17.6.2.1):
///
/// 1. `hidden` wins over everything and suppresses the border;
/// 2. `none` has the lowest priority;
/// 3. the wider border wins;
/// 4. at equal width the style wins, in the order `double`, `solid`, `dashed`, `dotted`, `ridge`, `outset`,
///    `groove`, `inset`;
/// 5. at equal style the element wins, in the order cell, row, row group, (column, column group,) table;
/// 6. between two elements of the same kind, the one further to the top and left.
///
/// The result does not depend on the order of the arguments (unless the two are the very same box).
pub(crate) fn winning_border(a: Candidate, b: Candidate) -> Candidate {
    let earlier = |a: Candidate, b: Candidate| if b.position() < a.position() { b } else { a };

    // 1. Hidden.
    match (
        a.spec.style == BorderStyle::Hidden,
        b.spec.style == BorderStyle::Hidden,
    ) {
        (true, false) => return a,
        (false, true) => return b,
        (true, true) => return earlier(a, b),
        (false, false) => {}
    }
    // 2 and 3. None is width 0; the wider wins.
    let (wa, wb) = (a.spec.effective_width(), b.spec.effective_width());
    if wa != wb {
        return if wa > wb { a } else { b };
    }
    if wa == 0.0 {
        // Both are none: nothing to paint, nothing to choose.
        return earlier(a, b);
    }
    // 4. Style. `BorderStyle` is ordered `hidden < none < inset < groove < outset < ridge < dotted < dashed < solid
    // < double`, which is exactly the order of the rule.
    if a.spec.style != b.spec.style {
        return if a.spec.style > b.spec.style { a } else { b };
    }
    // 5. Origin.
    if a.origin != b.origin {
        return if a.origin > b.origin { a } else { b };
    }
    // 6. Position.
    earlier(a, b)
}

/// The borders of one `<tr>` (or anonymous row) and the row group it is in.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RowInput {
    /// The row's own borders, `None` for an anonymous row.
    pub row: Option<[BorderSpec; 4]>,
    /// The row group of the row; the rows of one group are consecutive.
    pub group: Option<GroupInput>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct GroupInput {
    /// Identifies the group (its node id): consecutive rows with the same id are in the same group.
    pub id: usize,
    pub edges: [BorderSpec; 4],
}

/// A cell: the node, its position (tracks, not raw columns) and its own borders.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CellInput {
    pub node: usize,
    pub row: u32,
    pub track: u32,
    pub span: u32,
    pub edges: [BorderSpec; 4],
}

/// Where a cell of the grid is, for the consumer of [`CollapsedBorders`] (the paint step).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CollapsedCell {
    pub node: usize,
    pub row: u32,
    pub track: u32,
    pub span: u32,
}

/// The resolved borders of a collapsed table: one [`BorderSpec`] per segment of every grid line.
///
/// `horizontal` is `(rows + 1) x tracks`: line `i` runs under row `i - 1` (line 0 is the table's top edge, line `rows`
/// its bottom edge), segment `t` is the width of track `t`. `vertical` is `rows x (tracks + 1)`: line `j` is left of
/// track `j`, one segment per row. A segment inside a cell that spans tracks has no border.
#[derive(Clone, Debug, PartialEq)]
pub struct CollapsedBorders {
    pub rows: u32,
    pub tracks: u32,
    pub horizontal: Vec<BorderSpec>,
    pub vertical: Vec<BorderSpec>,
    /// The kind of element each segment's border came from, laid out like `horizontal` and `vertical` (a segment
    /// without a border says `Table`). A consumer that compares a segment with another border (a repeated header
    /// copy, F22b) needs it for rule 5.
    pub horizontal_origin: Vec<EdgeOrigin>,
    pub vertical_origin: Vec<EdgeOrigin>,
    pub cells: Vec<CollapsedCell>,
}

impl CollapsedBorders {
    /// The segment of horizontal line `line` (0..=rows) at track `track`.
    pub fn horizontal(&self, line: u32, track: u32) -> &BorderSpec {
        &self.horizontal[line as usize * self.tracks as usize + track as usize]
    }

    /// The kind of element the border of [`Self::horizontal`]`(line, track)` came from.
    pub fn horizontal_origin(&self, line: u32, track: u32) -> EdgeOrigin {
        self.horizontal_origin[line as usize * self.tracks as usize + track as usize]
    }

    /// The kind of element the border of [`Self::vertical`]`(row, line)` came from.
    pub fn vertical_origin(&self, row: u32, line: u32) -> EdgeOrigin {
        self.vertical_origin[row as usize * (self.tracks as usize + 1) + line as usize]
    }

    /// The segment of vertical line `line` (0..=tracks) in row `row`.
    pub fn vertical(&self, row: u32, line: u32) -> &BorderSpec {
        &self.vertical[row as usize * (self.tracks as usize + 1) + line as usize]
    }

    /// Half of the widest segment on each side of cell number `cell` (an index into `cells`): its used border widths
    /// (CSS Tables 3: the cell's border is half of the collapsed borders along the side, for a cell that spans several
    /// tracks the maximum).
    pub fn cell_half_widths(&self, cell: usize) -> taffy::Rect<f32> {
        let c = self.cells[cell];
        let tracks = c.track..(c.track + c.span).min(self.tracks);
        let widest = |line: u32| {
            tracks
                .clone()
                .map(|t| self.horizontal(line, t).used_width())
                .fold(0.0, f32::max)
        };
        taffy::Rect {
            top: widest(c.row) / 2.0,
            bottom: widest(c.row + 1) / 2.0,
            left: self.vertical(c.row, c.track).used_width() / 2.0,
            right: self
                .vertical(c.row, (c.track + c.span).min(self.tracks))
                .used_width()
                / 2.0,
        }
    }

    /// Half of the widest segment along each outer edge of the table: the part of the outer lines that lies inside
    /// the table's border box (CSS 2.1 §17.6.2). A later row with a wider border than the first "spills into the
    /// margin" in the text of the spec, but Chrome 141 makes the table's left and right border half of the maximum
    /// over all rows, like the top and bottom ones, and so does this (F22 plan §7, probe `spill`).
    pub fn table_half_widths(&self) -> taffy::Rect<f32> {
        let horizontal = |line: u32| {
            (0..self.tracks)
                .map(|t| self.horizontal(line, t).used_width())
                .fold(0.0, f32::max)
                / 2.0
        };
        let vertical = |line: u32| {
            (0..self.rows)
                .map(|r| self.vertical(r, line).used_width())
                .fold(0.0, f32::max)
                / 2.0
        };
        taffy::Rect {
            top: horizontal(0),
            bottom: horizontal(self.rows),
            left: vertical(0),
            right: vertical(self.tracks),
        }
    }
}

const NO_CELL: u32 = u32::MAX;

/// Resolves the collapsed borders of a table of `rows.len()` rows and `tracks` tracks. Linear in the number of
/// segments; every vector is indexed by grid position, so the result is the same on every run.
pub(crate) fn resolve(
    rows: &[RowInput],
    tracks: u32,
    cells: &[CellInput],
    table: [BorderSpec; 4],
) -> CollapsedBorders {
    let nr = rows.len();
    let nt = tracks as usize;
    let mut out = CollapsedBorders {
        rows: nr as u32,
        tracks,
        horizontal: Vec::new(),
        vertical: Vec::new(),
        horizontal_origin: Vec::new(),
        vertical_origin: Vec::new(),
        cells: cells
            .iter()
            .map(|c| CollapsedCell {
                node: c.node,
                row: c.row,
                track: c.track,
                span: c.span,
            })
            .collect(),
    };
    if nr == 0 || nt == 0 {
        return out;
    }

    // Which cell covers each grid slot.
    let mut cover = vec![NO_CELL; nr * nt];
    for (i, c) in cells.iter().enumerate() {
        let row = c.row as usize;
        if row >= nr {
            continue;
        }
        for t in (c.track as usize)..((c.track + c.span) as usize).min(nt) {
            cover[row * nt + t] = i as u32;
        }
    }
    let cell_at = |row: usize, track: usize| match cover[row * nt + track] {
        NO_CELL => None,
        i => Some(&cells[i as usize]),
    };
    // Whether a row is the first / last of its row group (its top / bottom border is the group's).
    let group_id = |row: usize| rows[row].group.map(|g| g.id);
    let first_in_group =
        |row: usize| group_id(row).is_some() && (row == 0 || group_id(row - 1) != group_id(row));
    let last_in_group = |row: usize| {
        group_id(row).is_some() && (row + 1 == nr || group_id(row + 1) != group_id(row))
    };

    let candidate = |spec: BorderSpec, origin: EdgeOrigin, row: usize, track: usize| Candidate {
        spec,
        origin,
        row: row as u32,
        track: track as u32,
    };
    let fold = |best: Option<Candidate>, next: Candidate| {
        Some(match best {
            Some(best) => winning_border(best, next),
            None => next,
        })
    };
    let spec_of = |best: Option<Candidate>| best.map_or(BorderSpec::NONE, |c| c.spec);
    let origin_of = |best: Option<Candidate>| best.map_or(EdgeOrigin::Table, |c| c.origin);

    // Horizontal lines: line `i` is between row `i - 1` (above) and row `i` (below).
    out.horizontal.reserve((nr + 1) * nt);
    for line in 0..=nr {
        for t in 0..nt {
            let mut best = None;
            if line > 0 {
                let r = line - 1;
                if let Some(c) = cell_at(r, t) {
                    best = fold(
                        best,
                        candidate(c.edges[BOTTOM], EdgeOrigin::Cell, r, c.track as usize),
                    );
                }
                if let Some(edges) = rows[r].row {
                    best = fold(best, candidate(edges[BOTTOM], EdgeOrigin::Row, r, 0));
                }
                if let (Some(group), true) = (rows[r].group, last_in_group(r)) {
                    best = fold(
                        best,
                        candidate(group.edges[BOTTOM], EdgeOrigin::RowGroup, r, 0),
                    );
                }
            }
            if line < nr {
                let r = line;
                if let Some(c) = cell_at(r, t) {
                    best = fold(
                        best,
                        candidate(c.edges[TOP], EdgeOrigin::Cell, r, c.track as usize),
                    );
                }
                if let Some(edges) = rows[r].row {
                    best = fold(best, candidate(edges[TOP], EdgeOrigin::Row, r, 0));
                }
                if let (Some(group), true) = (rows[r].group, first_in_group(r)) {
                    best = fold(
                        best,
                        candidate(group.edges[TOP], EdgeOrigin::RowGroup, r, 0),
                    );
                }
            }
            if line == 0 {
                best = fold(best, candidate(table[TOP], EdgeOrigin::Table, 0, 0));
            }
            if line == nr {
                best = fold(best, candidate(table[BOTTOM], EdgeOrigin::Table, nr, 0));
            }
            out.horizontal.push(spec_of(best));
            out.horizontal_origin.push(origin_of(best));
        }
    }

    // Vertical lines: line `j` is between track `j - 1` (left) and track `j` (right), one segment per row. Rows and
    // row groups have vertical borders only at the two outer lines.
    out.vertical.reserve(nr * (nt + 1));
    for r in 0..nr {
        for line in 0..=nt {
            let left = (line > 0).then(|| cell_at(r, line - 1)).flatten();
            let right = (line < nt).then(|| cell_at(r, line)).flatten();
            let inside_span = matches!((left, right), (Some(l), Some(rt)) if std::ptr::eq(l, rt));
            let mut best = None;
            if !inside_span {
                if let Some(c) = left {
                    best = fold(
                        best,
                        candidate(c.edges[RIGHT], EdgeOrigin::Cell, r, c.track as usize),
                    );
                }
                if let Some(c) = right {
                    best = fold(
                        best,
                        candidate(c.edges[LEFT], EdgeOrigin::Cell, r, c.track as usize),
                    );
                }
            }
            if line == 0 {
                if let Some(edges) = rows[r].row {
                    best = fold(best, candidate(edges[LEFT], EdgeOrigin::Row, r, 0));
                }
                if let Some(group) = rows[r].group {
                    best = fold(
                        best,
                        candidate(group.edges[LEFT], EdgeOrigin::RowGroup, r, 0),
                    );
                }
                best = fold(best, candidate(table[LEFT], EdgeOrigin::Table, 0, 0));
            }
            if line == nt {
                if let Some(edges) = rows[r].row {
                    best = fold(best, candidate(edges[RIGHT], EdgeOrigin::Row, r, nt));
                }
                if let Some(group) = rows[r].group {
                    best = fold(
                        best,
                        candidate(group.edges[RIGHT], EdgeOrigin::RowGroup, r, nt),
                    );
                }
                best = fold(best, candidate(table[RIGHT], EdgeOrigin::Table, 0, nt));
            }
            out.vertical.push(spec_of(best));
            out.vertical_origin.push(origin_of(best));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLACK: AbsoluteColor = AbsoluteColor::BLACK;
    fn spec(width: f32, style: BorderStyle) -> BorderSpec {
        BorderSpec {
            width,
            style,
            color: BLACK,
        }
    }

    fn solid(width: f32) -> BorderSpec {
        spec(width, BorderStyle::Solid)
    }

    fn red(width: f32) -> BorderSpec {
        BorderSpec {
            width,
            style: BorderStyle::Solid,
            color: AbsoluteColor::srgb_legacy(255, 0, 0, 1.0),
        }
    }

    fn cand(spec: BorderSpec, origin: EdgeOrigin, row: u32, track: u32) -> Candidate {
        Candidate {
            spec,
            origin,
            row,
            track,
        }
    }

    /// The winner is the same whichever way round the arguments are.
    fn wins(a: Candidate, b: Candidate) -> Candidate {
        let (ab, ba) = (winning_border(a, b), winning_border(b, a));
        assert_eq!(ab, ba, "the winner depends on the argument order");
        ab
    }

    const NONE: BorderSpec = BorderSpec::NONE;

    #[test]
    fn hidden_wins_over_everything() {
        let hidden = cand(spec(0.0, BorderStyle::Hidden), EdgeOrigin::Table, 5, 5);
        let wide = cand(solid(10.0), EdgeOrigin::Cell, 0, 0);
        assert_eq!(wins(hidden, wide), hidden);
        // Two hidden ones: either; the result is hidden.
        let other = cand(spec(0.0, BorderStyle::Hidden), EdgeOrigin::Cell, 0, 0);
        assert_eq!(wins(hidden, other).spec.style, BorderStyle::Hidden);
    }

    #[test]
    fn none_has_the_lowest_priority() {
        let none = cand(NONE, EdgeOrigin::Cell, 0, 0);
        let thin = cand(solid(0.5), EdgeOrigin::Table, 9, 9);
        assert_eq!(wins(none, thin), thin);
        // A `none` border is nothing even when it carries a width.
        let none_wide = cand(spec(4.0, BorderStyle::None), EdgeOrigin::Cell, 0, 0);
        assert_eq!(wins(none_wide, thin), thin);
    }

    #[test]
    fn the_wider_border_wins() {
        let wide = cand(solid(4.0), EdgeOrigin::Table, 3, 3);
        let thin = cand(spec(2.0, BorderStyle::Double), EdgeOrigin::Cell, 0, 0);
        assert_eq!(wins(wide, thin), wide);
    }

    #[test]
    fn at_equal_width_the_style_wins_in_the_order_of_the_spec() {
        let order = [
            BorderStyle::Double,
            BorderStyle::Solid,
            BorderStyle::Dashed,
            BorderStyle::Dotted,
            BorderStyle::Ridge,
            BorderStyle::Outset,
            BorderStyle::Groove,
            BorderStyle::Inset,
        ];
        for (i, &better) in order.iter().enumerate() {
            for &worse in &order[i + 1..] {
                // The origin and the position are the opposite of what the style says, so only the style can decide.
                let a = cand(spec(2.0, better), EdgeOrigin::Table, 9, 9);
                let b = cand(spec(2.0, worse), EdgeOrigin::Cell, 0, 0);
                assert_eq!(wins(a, b), a, "{better:?} against {worse:?}");
            }
        }
    }

    #[test]
    fn at_equal_style_the_origin_wins_cell_over_row_over_group_over_table() {
        let of = |origin| cand(solid(2.0), origin, 9, 9);
        let order = [
            EdgeOrigin::Cell,
            EdgeOrigin::Row,
            EdgeOrigin::RowGroup,
            EdgeOrigin::Table,
        ];
        for (i, &better) in order.iter().enumerate() {
            for &worse in &order[i + 1..] {
                assert_eq!(
                    wins(of(better), of(worse)),
                    of(better),
                    "{better:?} against {worse:?}"
                );
            }
        }
    }

    #[test]
    fn between_equal_boxes_the_one_further_up_and_left_wins() {
        let a = cand(red(2.0), EdgeOrigin::Cell, 1, 3);
        let b = cand(solid(2.0), EdgeOrigin::Cell, 1, 4);
        assert_eq!(wins(a, b), a, "left");
        let c = cand(solid(2.0), EdgeOrigin::Cell, 2, 0);
        assert_eq!(wins(a, c), a, "up");
    }

    // ---- resolve -----------------------------------------------------------------------------------------------

    fn no_edges() -> [BorderSpec; 4] {
        [NONE; 4]
    }

    fn edges(top: f32, right: f32, bottom: f32, left: f32) -> [BorderSpec; 4] {
        let side = |w: f32| if w > 0.0 { solid(w) } else { NONE };
        [side(top), side(right), side(bottom), side(left)]
    }

    fn plain_rows(n: usize) -> Vec<RowInput> {
        vec![
            RowInput {
                row: None,
                group: None
            };
            n
        ]
    }

    fn cell(node: usize, row: u32, track: u32, span: u32, edges: [BorderSpec; 4]) -> CellInput {
        CellInput {
            node,
            row,
            track,
            span,
            edges,
        }
    }

    fn widths(b: &CollapsedBorders, horizontal: bool) -> Vec<f32> {
        let v = if horizontal {
            &b.horizontal
        } else {
            &b.vertical
        };
        v.iter().map(|s| s.width).collect()
    }

    #[test]
    fn a_row_rule_is_the_line_under_the_row_and_adds_half_to_each_side() {
        // Two rows of one cell; the first row has `border-bottom: 2px`.
        let mut rows = plain_rows(2);
        rows[0].row = Some(edges(0.0, 0.0, 2.0, 0.0));
        let cells = [cell(10, 0, 0, 1, no_edges()), cell(11, 1, 0, 1, no_edges())];
        let b = resolve(&rows, 1, &cells, no_edges());
        assert_eq!(widths(&b, true), [0.0, 2.0, 0.0]);
        let (upper, lower) = (b.cell_half_widths(0), b.cell_half_widths(1));
        assert_eq!((upper.top, upper.bottom), (0.0, 1.0));
        assert_eq!((lower.top, lower.bottom), (1.0, 0.0));
        assert_eq!((upper.left, upper.right), (0.0, 0.0));
    }

    #[test]
    fn a_shared_cell_border_is_one_line() {
        // 2 x 2 cells with 1 px borders all round: the lines between them are 1 px, so each cell holds half of it.
        let rows = plain_rows(2);
        let cells = [
            cell(0, 0, 0, 1, edges(1.0, 1.0, 1.0, 1.0)),
            cell(1, 0, 1, 1, edges(1.0, 1.0, 1.0, 1.0)),
            cell(2, 1, 0, 1, edges(1.0, 1.0, 1.0, 1.0)),
            cell(3, 1, 1, 1, edges(1.0, 1.0, 1.0, 1.0)),
        ];
        let b = resolve(&rows, 2, &cells, no_edges());
        assert!(b.horizontal.iter().all(|s| s.width == 1.0));
        assert!(b.vertical.iter().all(|s| s.width == 1.0));
        for i in 0..4 {
            let h = b.cell_half_widths(i);
            assert_eq!(
                (h.top, h.right, h.bottom, h.left),
                (0.5, 0.5, 0.5, 0.5),
                "cell {i}"
            );
        }
        let t = b.table_half_widths();
        assert_eq!((t.top, t.right, t.bottom, t.left), (0.5, 0.5, 0.5, 0.5));
    }

    #[test]
    fn the_wider_border_sets_the_line_and_both_cells_hold_half_of_it() {
        // Row rule 4 px against the upper cell's 2 px: 4 px.
        let mut rows = plain_rows(2);
        rows[0].row = Some(edges(0.0, 0.0, 4.0, 0.0));
        let cells = [
            cell(0, 0, 0, 1, edges(0.0, 0.0, 2.0, 0.0)),
            cell(1, 1, 0, 1, no_edges()),
        ];
        let b = resolve(&rows, 1, &cells, no_edges());
        assert_eq!(widths(&b, true), [0.0, 4.0, 0.0]);
        assert_eq!(b.cell_half_widths(0).bottom, 2.0);
        assert_eq!(b.cell_half_widths(1).top, 2.0);
    }

    #[test]
    fn a_row_group_border_top_is_the_first_rows_top_only() {
        // One group of three rows with `border-top: 3px` and `border-left: 1px`.
        let group = GroupInput {
            id: 7,
            edges: edges(3.0, 0.0, 0.0, 1.0),
        };
        let rows = vec![
            RowInput {
                row: None,
                group: Some(group)
            };
            3
        ];
        let cells: Vec<CellInput> = (0..3)
            .map(|r| cell(r as usize, r, 0, 1, no_edges()))
            .collect();
        let b = resolve(&rows, 1, &cells, no_edges());
        assert_eq!(widths(&b, true), [3.0, 0.0, 0.0, 0.0]);
        // The left border is on every row's outer line.
        assert_eq!(widths(&b, false), [1.0, 0.0, 1.0, 0.0, 1.0, 0.0]);
    }

    #[test]
    fn two_groups_do_not_share_their_edges() {
        // thead (1 row, border-bottom 2) then tbody (1 row, border-top 6): one line, 6 px wins.
        let head = GroupInput {
            id: 1,
            edges: edges(0.0, 0.0, 2.0, 0.0),
        };
        let body = GroupInput {
            id: 2,
            edges: edges(6.0, 0.0, 0.0, 0.0),
        };
        let rows = [
            RowInput {
                row: None,
                group: Some(head),
            },
            RowInput {
                row: None,
                group: Some(body),
            },
        ];
        let cells = [cell(0, 0, 0, 1, no_edges()), cell(1, 1, 0, 1, no_edges())];
        let b = resolve(&rows, 1, &cells, no_edges());
        assert_eq!(widths(&b, true), [0.0, 6.0, 0.0]);
    }

    #[test]
    fn an_anonymous_row_has_no_border_of_its_own() {
        // Row 0 is anonymous (no `row` entry), row 1 has a top rule.
        let mut rows = plain_rows(2);
        rows[1].row = Some(edges(2.0, 0.0, 0.0, 0.0));
        let cells = [cell(0, 0, 0, 1, no_edges()), cell(1, 1, 0, 1, no_edges())];
        let b = resolve(&rows, 1, &cells, no_edges());
        assert_eq!(widths(&b, true), [0.0, 2.0, 0.0]);
    }

    #[test]
    fn a_hidden_cell_border_suppresses_the_row_rule() {
        let mut rows = plain_rows(2);
        rows[0].row = Some(edges(0.0, 0.0, 3.0, 0.0));
        let mut cell_edges = no_edges();
        cell_edges[BOTTOM] = spec(0.0, BorderStyle::Hidden);
        let cells = [cell(0, 0, 0, 1, cell_edges), cell(1, 1, 0, 1, no_edges())];
        let b = resolve(&rows, 1, &cells, no_edges());
        assert_eq!(b.horizontal(1, 0).style, BorderStyle::Hidden);
        assert_eq!(b.cell_half_widths(0).bottom, 0.0);
        assert_eq!(b.cell_half_widths(1).top, 0.0);
    }

    #[test]
    fn a_colspan_cell_sees_segments_of_different_widths_and_takes_the_maximum() {
        // Row 0: two cells with `border-bottom` 2 and 6; row 1: one cell spanning both tracks. (Chrome probe `span`:
        // both rows are 3 px taller: half of 6.)
        let rows = plain_rows(2);
        let cells = [
            cell(0, 0, 0, 1, edges(0.0, 0.0, 2.0, 0.0)),
            cell(1, 0, 1, 1, edges(0.0, 0.0, 6.0, 0.0)),
            cell(2, 1, 0, 2, no_edges()),
        ];
        let b = resolve(&rows, 2, &cells, no_edges());
        assert_eq!(widths(&b, true), [0.0, 0.0, 2.0, 6.0, 0.0, 0.0]);
        assert_eq!(b.cell_half_widths(0).bottom, 1.0);
        assert_eq!(b.cell_half_widths(1).bottom, 3.0);
        assert_eq!(b.cell_half_widths(2).top, 3.0);
        // No line inside the spanning cell.
        assert_eq!(b.vertical(1, 1).width, 0.0);
    }

    #[test]
    fn a_spanning_cells_border_covers_every_segment_it_spans() {
        let rows = plain_rows(2);
        let cells = [
            cell(0, 0, 0, 2, edges(0.0, 0.0, 2.0, 0.0)),
            cell(1, 1, 0, 1, no_edges()),
            cell(2, 1, 1, 1, no_edges()),
        ];
        let b = resolve(&rows, 2, &cells, no_edges());
        assert_eq!(widths(&b, true), [0.0, 0.0, 2.0, 2.0, 0.0, 0.0]);
    }

    #[test]
    fn the_table_holds_half_of_the_widest_outer_line_on_every_side() {
        // Chrome probe `spill`: the first row has 1 px cell borders, a cell of the second row has `border-left: 10px`.
        // The table's left border is 5, not 0.5.
        let rows = plain_rows(2);
        let cells = [
            cell(0, 0, 0, 1, edges(1.0, 1.0, 1.0, 1.0)),
            cell(1, 0, 1, 1, edges(1.0, 1.0, 1.0, 1.0)),
            cell(2, 1, 0, 1, edges(0.0, 0.0, 0.0, 10.0)),
            cell(3, 1, 1, 1, no_edges()),
        ];
        let b = resolve(&rows, 2, &cells, no_edges());
        let t = b.table_half_widths();
        assert_eq!((t.top, t.right, t.bottom, t.left), (0.5, 0.5, 0.0, 5.0));
    }

    #[test]
    fn the_tables_own_border_competes_with_the_cells_at_the_outer_line() {
        // Chrome probe `tableborder`: table 6 px, cells 2 px: the line is 6 px, half of it (3) in the table and the
        // cell.
        let rows = plain_rows(1);
        let cells = [
            cell(0, 0, 0, 1, edges(2.0, 2.0, 2.0, 2.0)),
            cell(1, 0, 1, 1, edges(2.0, 2.0, 2.0, 2.0)),
        ];
        let b = resolve(&rows, 2, &cells, edges(6.0, 6.0, 6.0, 6.0));
        let t = b.table_half_widths();
        assert_eq!((t.top, t.right, t.bottom, t.left), (3.0, 3.0, 3.0, 3.0));
        let first = b.cell_half_widths(0);
        assert_eq!(
            (first.top, first.right, first.bottom, first.left),
            (3.0, 1.0, 3.0, 3.0)
        );
    }

    #[test]
    fn a_cell_missing_from_a_row_leaves_its_slot_to_the_row_and_the_neighbours() {
        // Row 1 has one cell for two tracks: the line between row 0 and row 1 under track 1 comes from row 0's cell.
        let rows = plain_rows(2);
        let cells = [
            cell(0, 0, 0, 1, no_edges()),
            cell(1, 0, 1, 1, edges(0.0, 0.0, 2.0, 0.0)),
            cell(2, 1, 0, 1, no_edges()),
        ];
        let b = resolve(&rows, 2, &cells, no_edges());
        assert_eq!(widths(&b, true), [0.0, 0.0, 0.0, 2.0, 0.0, 0.0]);
    }

    #[test]
    fn an_empty_grid_resolves_to_nothing() {
        let b = resolve(&[], 0, &[], edges(1.0, 1.0, 1.0, 1.0));
        assert!(b.horizontal.is_empty() && b.vertical.is_empty());
        let b = resolve(&plain_rows(2), 0, &[], no_edges());
        assert!(b.horizontal.is_empty() && b.vertical.is_empty());
    }

    #[test]
    fn resolving_twice_gives_the_same_result() {
        let mut rows = plain_rows(3);
        rows[1].row = Some(edges(2.0, 0.0, 1.0, 0.0));
        let cells: Vec<CellInput> = (0..6)
            .map(|i| {
                cell(
                    i,
                    (i / 2) as u32,
                    (i % 2) as u32,
                    1,
                    edges(1.0, 1.0, 1.0, 1.0),
                )
            })
            .collect();
        let a = resolve(&rows, 2, &cells, no_edges());
        let b = resolve(&rows, 2, &cells, no_edges());
        assert_eq!(a, b);
    }
}
