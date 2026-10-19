use std::{collections::BTreeMap, ops::Range, sync::Arc};

use markup5ever::local_name;
use style::values::generics::box_::{VerticalAlign, VerticalAlignKeyword};
use style::values::specified::box_::{DisplayInside, DisplayOutside};
use style::{Atom, computed_values::table_layout::T as TableLayout};
use taffy::{
    Dimension, LayoutInput, LayoutOutput, LayoutPartialTree as _, NodeId, Position, ResolveOrZero,
    RunMode, Size, TrackSizingFunction, compute_leaf_layout, style_helpers,
};

use crate::BaseDocument;
use crate::node::{Node, NodeData};

use super::compute_node_layout;
use super::construct::create_anonymous_block;
use super::damage::{CONSTRUCT_BOX, CONSTRUCT_DESCENDENT, CONSTRUCT_FC};
use super::resolve_calc_value;

pub struct TableTreeWrapper<'doc> {
    pub(crate) doc: &'doc mut BaseDocument,
    pub(crate) ctx: Arc<TableContext>,
}

#[derive(Debug, Clone)]
pub struct TableContext {
    style: taffy::Style<Atom>,
    items: Vec<TableItem>,
    /// The columns of an automatic layout that have an explicit length `width`: they are never narrower than their
    /// cells' min-content widths, see [`widen_explicit_columns`].
    explicit_columns: Vec<ExplicitColumn>,
}

/// A grid track with an explicit length or percentage width (`push_cell_item`) and the items of the cells that start in
/// it and span only it.
#[derive(Debug, Clone)]
struct ExplicitColumn {
    track: usize,
    width: ColumnWidth,
    items: Vec<usize>,
}

#[derive(Debug, Clone, Copy)]
enum ColumnWidth {
    /// A border-box width in px.
    Length(f32),
    /// A fraction of the table's content-box width (`0.1` for `10%`).
    Percent(f32),
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TableItemKind {
    Row,
    Cell,
}

#[derive(Debug, Clone)]
pub struct TableItem {
    kind: TableItemKind,
    node_id: usize,
    style: taffy::Style<Atom>,
}

pub(crate) fn build_table_context(
    doc: &mut BaseDocument,
    table_root_node_id: usize,
) -> (TableContext, Vec<usize>) {
    let root_node = &mut doc.nodes[table_root_node_id];

    let children = std::mem::take(&mut root_node.children);

    let Some(stylo_styles) = root_node.primary_styles() else {
        panic!("Ignoring table because it has no styles");
    };

    let mut style = stylo_taffy::to_taffy_style(&stylo_styles);
    style.grid_auto_columns = Vec::new();
    style.grid_auto_rows = Vec::new();
    // A table is at least as tall as its rows (CSS 2.1 §17.5.3): a fixed `height` must not clip them.
    height_as_table_minimum(&mut style);
    grid_container_min_height(&mut style);

    let is_fixed = match stylo_styles.clone_table_layout() {
        TableLayout::Fixed => true,
        TableLayout::Auto => false,
    };

    drop(stylo_styles);

    let mut grid = GridBuilder {
        is_fixed,
        row: 0,
        col: 0,
        items: Vec::new(),
        columns: BTreeMap::new(),
        first_row_width: 0,
        placements: Vec::new(),
    };
    collect_table_children(doc, table_root_node_id, &children, Level::Table, &mut grid);
    let GridBuilder {
        row,
        mut items,
        columns,
        placements,
        ..
    } = grid;

    // The tracks are the columns of the table and each cell is placed on the ones it spans, now that every row
    // is known (see `track_columns`).
    let fixed = uses_fixed_algorithm(is_fixed, &style);
    let tracks = track_columns(&placements, fixed);
    for placement in &placements {
        let (start, span) = grid_column(&tracks, placement.start, placement.span);
        items[placement.item].style.grid_column = taffy::Line {
            start: style_helpers::line(start),
            end: style_helpers::span(span),
        };
    }
    style.grid_template_columns = tracks
        .iter()
        .map(|column| {
            let width = columns
                .get(column)
                .copied()
                .unwrap_or_else(style_helpers::auto);
            TrackSizingFunction::from(width).into()
        })
        .collect();
    style.grid_template_rows = vec![style_helpers::auto(); row as usize];

    // In automatic layout an explicit length is the column's preferred width, not a cap: see
    // `widen_explicit_columns`.
    let mut explicit_columns: Vec<ExplicitColumn> = Vec::new();
    if !fixed {
        for (track, column) in tracks.iter().enumerate() {
            let Some(width) = columns.get(column) else {
                continue;
            };
            let width = match width.tag() {
                taffy::CompactLength::LENGTH_TAG => ColumnWidth::Length(width.value()),
                taffy::CompactLength::PERCENT_TAG => ColumnWidth::Percent(width.value()),
                _ => continue,
            };
            let items = placements
                .iter()
                .filter(|p| grid_column(&tracks, p.start, p.span) == (track as i16 + 1, 1))
                .map(|p| p.item)
                .collect();
            explicit_columns.push(ExplicitColumn {
                track,
                width,
                items,
            });
        }
    }

    let layout_children = items
        .iter()
        .filter(|item| item.kind == TableItemKind::Cell)
        .map(|cell| cell.node_id)
        .collect();
    let root_node = &mut doc.nodes[table_root_node_id];
    root_node.children = children;

    (
        TableContext {
            style,
            items,
            explicit_columns,
        },
        layout_children,
    )
}

/// A column of an automatic table layout is never narrower than the largest min-content width of its cells (CSS 2.1
/// §17.5.2.2), so an explicit cell `width` is the column's preferred width and not a cap: a `nowrap` price in a
/// `width: 50px` cell widens its column instead of spilling into the next cell. The grid takes the explicit length as
/// a fixed track, so the tracks that have one are measured first (the cells' own `width` is ignored while measuring, see
/// `TableTreeWrapper::compute_child_layout`) and replaced by a wider fixed track where a cell needs it. The fixed
/// algorithm (§17.5.2.1) has no such columns: its content overflows.
///
/// A percentage width is the same kind of preference: `10%` of the table's content width, when that is definite (with an
/// indefinite one the grid takes the percentage as `auto` anyway), and never less than the min-content.
///
/// `inputs` are those of the table's own layout, which decide what a percentage is a percentage of.
///
/// Returns the context to lay the table out with: `context` itself when no column needs widening.
pub(crate) fn widen_explicit_columns(
    doc: &mut BaseDocument,
    context: Arc<TableContext>,
    inputs: &LayoutInput,
) -> Arc<TableContext> {
    if context.explicit_columns.is_empty() {
        return context;
    }
    let basis = table_content_width(&context.style, inputs);
    let mut widened = Vec::new();
    {
        let mut wrapper = TableTreeWrapper {
            doc,
            ctx: Arc::clone(&context),
        };
        for column in &context.explicit_columns {
            let preferred = match column.width {
                ColumnWidth::Length(width) => width,
                ColumnWidth::Percent(fraction) => match basis {
                    Some(basis) => fraction * basis,
                    None => continue,
                },
            };
            let mut width = preferred;
            for &item in &column.items {
                let output = taffy::LayoutPartialTree::compute_child_layout(
                    &mut wrapper,
                    NodeId::from(item),
                    LayoutInput {
                        run_mode: RunMode::ComputeSize,
                        sizing_mode: taffy::SizingMode::InherentSize,
                        axis: taffy::RequestedAxis::Horizontal,
                        known_dimensions: Size::NONE,
                        parent_size: Size::NONE,
                        available_space: Size {
                            width: taffy::AvailableSpace::MinContent,
                            height: taffy::AvailableSpace::MaxContent,
                        },
                        vertical_margins_are_collapsible: taffy::Line::FALSE,
                    },
                );
                width = width.max(output.size.width);
            }
            if width > preferred {
                widened.push((column.track, width));
            }
        }
    }
    if widened.is_empty() {
        return context;
    }
    let mut context = (*context).clone();
    for (track, width) in widened {
        context.style.grid_template_columns[track] =
            TrackSizingFunction::from(style_helpers::length::<f32, Dimension>(width)).into();
    }
    #[allow(clippy::arc_with_non_send_sync)]
    Arc::new(context)
}

/// The width the grid resolves a percentage track against: the content box of the table, from the same inputs Taffy's
/// grid layout uses (`known_dimensions`, else the preferred size when it is the inherent size, clamped by `min-width` /
/// `max-width`), or `None` when it is not definite.
fn table_content_width(style: &taffy::Style<Atom>, inputs: &LayoutInput) -> Option<f32> {
    use taffy::{MaybeMath as _, MaybeResolve as _};
    let parent = inputs.parent_size.width;
    let padding_border = (style.padding.resolve_or_zero(parent, resolve_calc_value)
        + style.border.resolve_or_zero(parent, resolve_calc_value))
    .horizontal_components()
    .sum();
    let adjustment = if style.box_sizing == taffy::BoxSizing::ContentBox {
        padding_border
    } else {
        0.0
    };
    let resolve = |dimension: Dimension| {
        dimension
            .maybe_resolve(parent, resolve_calc_value)
            .map(|w| w + adjustment)
    };
    let preferred = if inputs.sizing_mode == taffy::SizingMode::InherentSize {
        resolve(style.size.width)
    } else {
        None
    };
    let outer = inputs
        .known_dimensions
        .width
        .or(preferred)
        .maybe_clamp(resolve(style.min_size.width), resolve(style.max_size.width))
        .maybe_max(padding_border);
    outer.map(|outer| outer - padding_border)
}

/// The border-box width of a cell with an explicit length `width`, which is what its column track needs.
/// Under `content-box` the horizontal padding and border are added; under `border-box` the width already
/// includes them, but never shrinks below them (css-sizing-3 §5.1).
fn cell_border_box_width(style: &taffy::Style<Atom>) -> f32 {
    let len = style.size.width.value();
    let padding = style.padding.resolve_or_zero(None, resolve_calc_value);
    let border = style.border.resolve_or_zero(None, resolve_calc_value);
    let padding_border = padding.left + padding.right + border.left + border.right;
    if style.box_sizing == taffy::BoxSizing::BorderBox {
        len.max(padding_border)
    } else {
        len + padding_border
    }
}

/// A table cell's or table's length `height` is a minimum (CSS 2.1 §17.5.3: a row is as tall as its cells'
/// `height` and content need, a table's non-`auto` `height` is a minimum), and `max-height` is undefined on
/// them (§10.7); content wins, so it is ignored. Express the `height` as `min-height` so that the box is never
/// clamped below its content and the grid stretches the cell (`vertical-align` applies to it like to its
/// siblings). Left alone when the `height` is not a plain length (percentage, `calc()`) or the box has another
/// kind of `min-height`.
pub(crate) fn height_as_table_minimum(style: &mut taffy::Style<Atom>) {
    style.max_size.height = taffy::Dimension::auto();
    if !style.size.height.into_raw().is_calc()
        && style.size.height.tag() == taffy::CompactLength::LENGTH_TAG
        && !style.min_size.height.into_raw().is_calc()
    {
        let height = style.size.height.value();
        let min = style.min_size.height;
        let merged = match min.tag() {
            taffy::CompactLength::AUTO_TAG => Some(height),
            taffy::CompactLength::LENGTH_TAG => Some(min.value().max(height)),
            _ => None,
        };
        if let Some(merged) = merged {
            style.min_size.height = style_helpers::length(merged);
            style.size.height = style_helpers::auto();
        }
    }
}

/// Re-expresses the minimum height of the table's grid container for Taffy, which stretches the grid's auto
/// rows to a container `min-height` as if it were the size of the content box and only then adds the container's
/// padding and border (taffy 0.9.2 `compute/grid/mod.rs`; under `content-box` it has also added them to the
/// minimum before). A table's `height` is a total in Chrome under `border-box` (CSS 2.1 §17.5.3: the table is as
/// tall as its `height` says, a minimum) and the content box under `content-box`; both come out right when the
/// minimum handed to Taffy is `height - padding - border`: the rows are stretched to the content box
/// (`border-box`: the total minus the frame; `content-box`: Taffy adds the frame back to the minimum, which is
/// the `height` itself) and the container is the rows plus the frame. A percentage padding counts as zero here.
/// Only for the grid style of the table root, not for its own node style (which the flex and grid algorithms read).
pub(crate) fn grid_container_min_height(style: &mut taffy::Style<Atom>) {
    if style.min_size.height.into_raw().is_calc()
        || style.min_size.height.tag() != taffy::CompactLength::LENGTH_TAG
    {
        return;
    }
    let padding = style.padding.resolve_or_zero(None, resolve_calc_value);
    let border = style.border.resolve_or_zero(None, resolve_calc_value);
    let frame = padding.top + padding.bottom + border.top + border.bottom;
    style.min_size.height = style_helpers::length(style.min_size.height.value() - frame);
}

/// The largest `colspan` HTML allows (HTML §4.9.11: a value above 1000 is clamped to 1000).
const MAX_COLSPAN: u32 = 1000;

/// Taffy grid lines are `i16`, and a placement past them panics. A table has at most this many column tracks;
/// cells that start past the last one share it (laid out and painted, overlapping) instead of failing the whole
/// document. Far beyond any real table (Chrome caps a span at 1000 as well).
const MAX_COLUMNS: u32 = 16_384;

/// A `colspan` attribute by HTML's rules for parsing non-negative integers (HTML §2.3.4.2): leading ASCII
/// whitespace and one `+` skipped, then the leading digits (`"2.5"` and `"2px"` are 2). A missing, unparsable or
/// zero value is 1; a value above [`MAX_COLSPAN`] is [`MAX_COLSPAN`] (HTML §4.9.11). Chrome agrees on every case.
fn parse_colspan(value: Option<&str>) -> u32 {
    let Some(value) = value else { return 1 };
    let value = value.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let value = value.strip_prefix('+').unwrap_or(value);
    let mut span: u32 = 0;
    for digit in value.bytes().take_while(u8::is_ascii_digit) {
        // Saturate early: only "above the maximum" matters, not the exact value.
        span = (span * 10 + u32::from(digit - b'0')).min(MAX_COLSPAN + 1);
    }
    if span == 0 { 1 } else { span.min(MAX_COLSPAN) }
}

/// Adds the table cell `node_id` (a `display: table-cell` element, or an anonymous cell) to the grid: its column
/// width from the first row (or a wider `width` below it), its style as a grid item and its [`CellPlacement`]. Its
/// grid position is set by [`build_table_context`] once every row is known.
fn push_cell_item(doc: &BaseDocument, node_id: usize, grid: &mut GridBuilder) {
    let node = &doc.nodes[node_id];
    let stylo_style = &node.primary_styles().unwrap();
    let start = grid.col;
    let colspan = parse_colspan(node.attr(local_name!("colspan")));
    let mut style = stylo_taffy::to_taffy_style(stylo_style);

    // TODO: account for padding/border/margin
    if grid.row == 1 {
        // A `calc()` that survives Stylo's simplification mixes a percentage and a non-zero length;
        // on a table cell it MUST be treated as `auto` (css-values-4 § Computed Value of math
        // expressions). `tag()` of a calc value is not a valid tag, so test it first.
        let column = if style.size.width.into_raw().is_calc() {
            style_helpers::auto()
        } else {
            match style.size.width.tag() {
                taffy::CompactLength::LENGTH_TAG => {
                    style_helpers::length(cell_border_box_width(&style))
                }
                taffy::CompactLength::PERCENT_TAG => {
                    style_helpers::percent(style.size.width.value())
                }
                taffy::CompactLength::AUTO_TAG => style_helpers::auto(),
                _ => unreachable!(),
            }
        };
        grid.columns.insert(start, column);
        grid.first_row_width = start.saturating_add(colspan);
    } else if !grid.is_fixed
        && start < grid.first_row_width
        && taffy::CompactLength::LENGTH_TAG == style.size.width.tag()
    {
        let new_len = cell_border_box_width(&style);
        let column = grid
            .columns
            .entry(start)
            .or_insert_with(style_helpers::auto);
        *column = match column.tag() {
            taffy::CompactLength::LENGTH_TAG => style_helpers::length(column.value().max(new_len)),
            taffy::CompactLength::AUTO_TAG => style_helpers::length(new_len),
            taffy::CompactLength::PERCENT_TAG => style_helpers::percent(column.value()),
            _ => unreachable!(),
        };
    }

    style.grid_row = taffy::Line {
        start: style_helpers::line(grid.row as i16),
        end: style_helpers::span(1),
    };
    style.size.width = style_helpers::auto();
    height_as_table_minimum(&mut style);
    grid.placements.push(CellPlacement {
        item: grid.items.len(),
        start,
        span: colspan,
    });
    grid.items.push(TableItem {
        kind: TableItemKind::Cell,
        node_id,
        style,
    });

    grid.col = start.saturating_add(colspan);
}

/// Where a cell starts and how far it reaches, in the raw columns its row gives it (the sum of the `colspan`s before
/// it, and its own); `item` indexes the cell's [`TableItem`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CellPlacement {
    item: usize,
    start: u32,
    span: u32,
}

/// Whether the table's columns are laid out by the fixed table algorithm (CSS 2.1 §17.5.2.1: every column the widest
/// row has is a column of the table, whatever its content). CSS Tables 3 applies it only when `table-layout` is
/// `fixed` *and* the table's `width` is not `auto`; Chrome lays out `table-layout: fixed; width: auto` automatically.
fn uses_fixed_algorithm(is_fixed: bool, style: &taffy::Style<Atom>) -> bool {
    let width = style.size.width;
    let auto = !width.into_raw().is_calc() && width.tag() == taffy::CompactLength::AUTO_TAG;
    is_fixed && !auto
}

/// The raw columns that are column tracks of the grid, ascending. In automatic layout a column in which no cell
/// starts has no width (Chrome; HTML calls it a table model error and renders nothing for it), so the tracks are the
/// columns some cell starts in: a `colspan` wider than the table creates none. In fixed layout every column up to
/// the widest row is a track (CSS 2.1 §17.5.2.1: "any remaining columns equally divide the remaining horizontal
/// table space"). At most [`MAX_COLUMNS`].
fn track_columns(placements: &[CellPlacement], fixed: bool) -> Vec<u32> {
    if fixed {
        let widest = placements
            .iter()
            .map(|p| p.start.saturating_add(p.span))
            .max()
            .unwrap_or(0);
        return (0..widest.min(MAX_COLUMNS)).collect();
    }
    let mut columns: Vec<u32> = placements.iter().map(|p| p.start).collect();
    columns.sort_unstable();
    columns.dedup();
    columns.truncate(MAX_COLUMNS as usize);
    columns
}

/// The grid placement of a cell that starts in raw column `start` and spans `span` raw columns: its 1-based start
/// line and the number of tracks it covers (the tracks whose columns lie in `[start, start + span)`, at least one).
/// A cell that starts past the last track (only when there are more than [`MAX_COLUMNS`] of them) sits on the last
/// one. `tracks` must not be empty.
fn grid_column(tracks: &[u32], start: u32, span: u32) -> (i16, u16) {
    let first = tracks.partition_point(|&column| column < start);
    if first >= tracks.len() {
        return (tracks.len() as i16, 1);
    }
    let end = tracks.partition_point(|&column| column < start.saturating_add(span));
    ((first + 1) as i16, (end - first).max(1) as u16)
}

/// What a child of a table, a row group or a row is, by its computed `display` and not its tag (css-display-3 §2.4:
/// the table role comes from `display`; HTML §15.3.8's `td { display: table-cell }` is overridden by author CSS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TablePart {
    /// `table-cell`.
    Cell,
    /// `table-row`.
    Row,
    /// `table-row-group`, `table-header-group`, `table-footer-group`.
    RowGroup,
    /// `table-column`, `table-column-group`: no box in the grid, skipped.
    Column,
    /// `table-caption`: skipped (fulgur moves a `<caption>` out of the table before Blitz sees it).
    Caption,
    /// `display: none` (or an element without styles): no box, skipped, never opens a run.
    Hidden,
    /// Whitespace-only text and comments: never opens a run (CSS 2.1 §17.2.1 rule 1), kept inside one.
    Whitespace,
    /// Everything else: a block, flex, grid, inline or list-item box, a nested table, a table part of the wrong
    /// kind for its parent, non-whitespace text. It is wrapped in an anonymous table object.
    Other,
}

/// The kind of container whose children are planned: the table itself or a row group. (A row is planned by
/// [`plan_row`].)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    Table,
    RowGroup,
}

/// What becomes of the children of a row: a child that is a cell, or a run of consecutive children that are
/// not (one anonymous cell). Indices are into the flattened child list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RowSlot {
    Cell(usize),
    AnonymousCell(Range<usize>),
}

/// What becomes of the children of a table or a row group: a child that is a row (or, in a table, a row
/// group), or a run of consecutive children that are neither (one anonymous row, with the slots of its cells).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Slot {
    Keep(usize),
    AnonymousRow(Vec<RowSlot>),
}

/// Classifies `node` by its computed `display`. See [`TablePart`].
fn table_part_of(node: &Node) -> TablePart {
    match &node.data {
        NodeData::Text(text) => {
            if text.content.chars().all(|c| c.is_ascii_whitespace()) {
                TablePart::Whitespace
            } else {
                TablePart::Other
            }
        }
        NodeData::Comment => TablePart::Whitespace,
        NodeData::Element(_) | NodeData::AnonymousBlock(_) => {
            let Some(display) = node.primary_styles().map(|s| s.clone_display()) else {
                println!("Ignoring table descendent because it has no styles");
                return TablePart::Hidden;
            };
            if display.outside() == DisplayOutside::None {
                return TablePart::Hidden;
            }
            if display.outside() == DisplayOutside::TableCaption {
                return TablePart::Caption;
            }
            match display.inside() {
                DisplayInside::TableCell => TablePart::Cell,
                DisplayInside::TableRow => TablePart::Row,
                DisplayInside::TableRowGroup
                | DisplayInside::TableHeaderGroup
                | DisplayInside::TableFooterGroup => TablePart::RowGroup,
                DisplayInside::TableColumnGroup | DisplayInside::TableColumn => TablePart::Column,
                _ => TablePart::Other,
            }
        }
        _ => TablePart::Hidden,
    }
}

/// `ids` with every `display: contents` element replaced by its children (recursively): they take part in the
/// parent's table fixup as if the element were not there (css-display-3 §2.5).
fn flatten_contents(doc: &BaseDocument, ids: &[usize], out: &mut Vec<usize>) {
    for &id in ids {
        let node = &doc.nodes[id];
        let is_contents = node.is_element()
            && node
                .primary_styles()
                .is_some_and(|s| s.clone_display().inside() == DisplayInside::Contents);
        if is_contents {
            flatten_contents(doc, &node.children, out);
        } else {
            out.push(id);
        }
    }
}

/// Plans the children `parts[range]` of one row (CSS 2.1 §17.2.1 rule 2, "missing wrappers"): a cell stays a
/// cell, and each run of consecutive children that are not cells gets one anonymous cell. A run starts at a
/// child that is not whitespace, hidden, a column or a caption; hidden boxes and whitespace inside it stay in
/// it, the ones at its end are left out. A column or caption ends a run and is skipped.
fn plan_row(parts: &[TablePart], range: Range<usize>) -> Vec<RowSlot> {
    fn close(run: &mut Option<Range<usize>>, slots: &mut Vec<RowSlot>) {
        if let Some(run) = run.take() {
            slots.push(RowSlot::AnonymousCell(run));
        }
    }
    let mut slots = Vec::new();
    let mut run: Option<Range<usize>> = None;
    for i in range {
        match parts[i] {
            TablePart::Cell => {
                close(&mut run, &mut slots);
                slots.push(RowSlot::Cell(i));
            }
            TablePart::Column | TablePart::Caption => close(&mut run, &mut slots),
            TablePart::Hidden | TablePart::Whitespace => {}
            TablePart::Row | TablePart::RowGroup | TablePart::Other => match &mut run {
                Some(run) => run.end = i + 1,
                None => run = Some(i..i + 1),
            },
        }
    }
    close(&mut run, &mut slots);
    slots
}

/// Plans the children of a table (`Level::Table`) or a row group (`Level::RowGroup`) (CSS 2.1 §17.2.1 rule 2):
/// a row stays (so does a row group in a table), and each run of consecutive children that are not rows (cells,
/// boxes of any other display, text) gets one anonymous row, whose own children are planned like a row's. Hidden
/// boxes, whitespace, columns and captions are handled as in [`plan_row`].
pub(crate) fn plan_table_children(level: Level, parts: &[TablePart]) -> Vec<Slot> {
    fn close(parts: &[TablePart], run: &mut Option<Range<usize>>, slots: &mut Vec<Slot>) {
        if let Some(run) = run.take() {
            slots.push(Slot::AnonymousRow(plan_row(parts, run)));
        }
    }
    let mut slots = Vec::new();
    let mut run: Option<Range<usize>> = None;
    for (i, part) in parts.iter().enumerate() {
        match part {
            TablePart::Row => {
                close(parts, &mut run, &mut slots);
                slots.push(Slot::Keep(i));
            }
            TablePart::RowGroup if level == Level::Table => {
                close(parts, &mut run, &mut slots);
                slots.push(Slot::Keep(i));
            }
            TablePart::Column | TablePart::Caption => close(parts, &mut run, &mut slots),
            TablePart::Hidden | TablePart::Whitespace => {}
            TablePart::Cell | TablePart::RowGroup | TablePart::Other => match &mut run {
                Some(run) => run.end = i + 1,
                None => run = Some(i..i + 1),
            },
        }
    }
    close(parts, &mut run, &mut slots);
    slots
}

/// The role of a row group that decides where it is placed in the table (CSS 2.1 §17.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GroupRole {
    /// `table-header-group`: "always displayed before all other rows and row groups".
    Header,
    /// `table-footer-group`: "always displayed after all other rows and row groups".
    Footer,
}

/// The [`GroupRole`] of `node`, by its computed `display` and not its tag (css-display-3 §2.4). `None` for anything
/// that is not a header or footer group, and for a box that generates none (`display: none`).
fn group_role_of(node: &Node) -> Option<GroupRole> {
    if !node.is_element() {
        return None;
    }
    let display = node.primary_styles()?.clone_display();
    if display.outside() == DisplayOutside::None {
        return None;
    }
    group_role_of_display(display.inside())
}

fn group_role_of_display(inside: DisplayInside) -> Option<GroupRole> {
    match inside {
        DisplayInside::TableHeaderGroup => Some(GroupRole::Header),
        DisplayInside::TableFooterGroup => Some(GroupRole::Footer),
        _ => None,
    }
}

/// Puts the planned children of a table in visual order (CSS 2.1 §17.2): the first header group first and the first
/// footer group last, wherever they were written. "If a table contains multiple elements with
/// `display: table-header-group`, only the first is rendered as a header; the others are treated as if they had
/// `display: table-row-group`" (the same for footers), so every other slot keeps its relative order: further header
/// and footer groups, body groups, rows and anonymous rows. `roles` is indexed like the `Slot::Keep` indices.
pub(crate) fn order_table_slots(slots: Vec<Slot>, roles: &[Option<GroupRole>]) -> Vec<Slot> {
    let role_of = |slot: &Slot| match slot {
        Slot::Keep(i) => roles.get(*i).copied().flatten(),
        Slot::AnonymousRow(_) => None,
    };
    let header = slots
        .iter()
        .position(|slot| role_of(slot) == Some(GroupRole::Header));
    let footer = slots
        .iter()
        .position(|slot| role_of(slot) == Some(GroupRole::Footer));
    if header.is_none_or(|h| h == 0) && footer.is_none_or(|f| f == slots.len() - 1) {
        return slots;
    }
    let mut head = None;
    let mut foot = None;
    let mut middle = Vec::with_capacity(slots.len());
    for (i, slot) in slots.into_iter().enumerate() {
        if Some(i) == header {
            head = Some(slot);
        } else if Some(i) == footer {
            foot = Some(slot);
        } else {
            middle.push(slot);
        }
    }
    head.into_iter().chain(middle).chain(foot).collect()
}

/// The grid being built from a table's parts: the position of the next cell and what has been collected.
struct GridBuilder {
    is_fixed: bool,
    row: u16,
    /// The raw column the next cell of the row starts in.
    col: u32,
    items: Vec<TableItem>,
    /// The width of the column each first-row cell starts in (or a wider one below it), by raw column.
    columns: BTreeMap<u32, Dimension>,
    /// The raw columns of the first row; a later row's cell widens a column only if it starts within them.
    first_row_width: u32,
    placements: Vec<CellPlacement>,
}

/// Hidden boxes, columns and captions are not constructed: they have no box in the table.
fn release_skipped(doc: &BaseDocument, ids: &[usize], parts: &[TablePart]) {
    for (&id, part) in ids.iter().zip(parts) {
        if matches!(
            part,
            TablePart::Hidden | TablePart::Column | TablePart::Caption
        ) {
            doc.nodes[id].remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
        }
    }
}

/// Collects the children `children` of the table (`parent_id`, whose own `children` were taken by the caller) or
/// of a row group into `grid`. Returns the layout children of the container (its rows and the cells of its
/// anonymous rows, in DOM order) and whether there is an anonymous row among them.
fn collect_table_children(
    doc: &mut BaseDocument,
    parent_id: usize,
    children: &[usize],
    level: Level,
    grid: &mut GridBuilder,
) -> (Vec<usize>, bool) {
    let mut ids = Vec::with_capacity(children.len());
    flatten_contents(doc, children, &mut ids);
    let parts: Vec<TablePart> = ids
        .iter()
        .map(|&id| table_part_of(&doc.nodes[id]))
        .collect();
    release_skipped(doc, &ids, &parts);

    let mut layout_children = Vec::new();
    let mut has_anonymous = false;
    for slot in plan_table_children(level, &parts) {
        match slot {
            Slot::Keep(i) => {
                let id = ids[i];
                if parts[i] == TablePart::Row {
                    collect_row(doc, id, grid);
                    layout_children.push(id);
                } else {
                    collect_row_group(doc, id, grid);
                }
            }
            Slot::AnonymousRow(slots) => {
                grid.row += 1;
                grid.col = 0;
                has_anonymous = true;
                layout_children.extend(push_row_slots(doc, parent_id, &ids, &slots, grid));
            }
        }
    }
    (layout_children, has_anonymous)
}

fn collect_row_group(doc: &mut BaseDocument, group_id: usize, grid: &mut GridBuilder) {
    let children = std::mem::take(&mut doc.nodes[group_id].children);
    let (layout_children, has_anonymous) =
        collect_table_children(doc, group_id, &children, Level::RowGroup, grid);
    let group = &mut doc.nodes[group_id];
    group.children = children;
    // Rows and the cells of anonymous rows, so that a walk over the layout children finds them all.
    *group.layout_children.borrow_mut() = has_anonymous.then_some(layout_children);
}

fn collect_row(doc: &mut BaseDocument, row_id: usize, grid: &mut GridBuilder) {
    let node = &doc.nodes[row_id];
    node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
    grid.row += 1;
    grid.col = 0;

    {
        let stylo_style = &node.primary_styles().unwrap();
        let mut style = stylo_taffy::to_taffy_style(stylo_style);
        style.grid_column = taffy::Line {
            start: style_helpers::line(0),
            end: style_helpers::line(-1),
        };
        style.grid_row = taffy::Line {
            start: style_helpers::line(grid.row as i16),
            end: style_helpers::span(1),
        };
        grid.items.push(TableItem {
            kind: TableItemKind::Row,
            node_id: row_id,
            style,
        });
    }

    let children = std::mem::take(&mut doc.nodes[row_id].children);
    let mut ids = Vec::with_capacity(children.len());
    flatten_contents(doc, &children, &mut ids);
    let parts: Vec<TablePart> = ids
        .iter()
        .map(|&id| table_part_of(&doc.nodes[id]))
        .collect();
    release_skipped(doc, &ids, &parts);
    let slots = plan_row(&parts, 0..ids.len());
    let has_anonymous = slots
        .iter()
        .any(|slot| matches!(slot, RowSlot::AnonymousCell(_)));
    let layout_children = push_row_slots(doc, row_id, &ids, &slots, grid);
    let row = &mut doc.nodes[row_id];
    row.children = children;
    // The cells in DOM order, the anonymous ones included; a row without one keeps walking its `children`.
    *row.layout_children.borrow_mut() = has_anonymous.then_some(layout_children);
}

/// Adds the cells of `slots` to the grid in order and returns their node ids. An anonymous cell is a new
/// anonymous block that adopts the run `ids[range]` as its children; it inherits from `style_parent` (the row,
/// or the container of an anonymous row) and has no padding, border or width of its own.
fn push_row_slots(
    doc: &mut BaseDocument,
    style_parent: usize,
    ids: &[usize],
    slots: &[RowSlot],
    grid: &mut GridBuilder,
) -> Vec<usize> {
    let mut cells = Vec::with_capacity(slots.len());
    for slot in slots {
        let cell_id = match slot {
            RowSlot::Cell(i) => ids[*i],
            RowSlot::AnonymousCell(run) => {
                let anonymous = create_anonymous_block(doc, style_parent);
                doc.nodes[anonymous].children = ids[run.clone()].to_vec();
                anonymous
            }
        };
        push_cell_item(doc, cell_id, grid);
        cells.push(cell_id);
    }
    cells
}

/// How much of a table cell's free vertical space goes above its content, from the cell's
/// `vertical-align` (CSS 2.1 §17.5.3): `middle` 1/2, `bottom` 1, everything else 0. `top` is the start;
/// `baseline` and the remaining values act as `baseline`, which for the cells of one row is the top as
/// long as every cell starts with a line of the same height (real baseline alignment is not done).
fn cell_align_factor(node: &Node) -> f32 {
    let Some(styles) = node.primary_styles() else {
        return 0.0;
    };
    match styles.clone_vertical_align() {
        VerticalAlign::Keyword(VerticalAlignKeyword::Middle) => 0.5,
        VerticalAlign::Keyword(VerticalAlignKeyword::Bottom) => 1.0,
        _ => 0.0,
    }
}

/// Whether this layout request is the final layout of a table cell that `vertical-align` moves: a cell
/// of a table root, stretched to a known row height, with `middle` or `bottom` alignment. Measuring a
/// cell (`ComputeSize`) never moves content, and a cell without a known height is not stretched.
pub(crate) fn is_aligned_table_cell(
    doc: &BaseDocument,
    node_id: NodeId,
    inputs: &LayoutInput,
) -> bool {
    if inputs.run_mode != RunMode::PerformLayout || inputs.known_dimensions.height.is_none() {
        return false;
    }
    let node = &doc.nodes[usize::from(node_id)];
    let Some(parent) = node.layout_parent.get() else {
        return false;
    };
    doc.nodes[parent].flags.is_table_root()
        && node.display_constructed_as.inside() == DisplayInside::TableCell
        && cell_align_factor(node) > 0.0
}

/// Lays out a stretched table cell and moves its content down by its share of the free space. The cell
/// box keeps the row's height (backgrounds and borders still fill the row); only what is inside moves,
/// as in CSS Tables 3 §3 "intrinsic padding". The offset is stored in `Node::table_cell_offset`;
/// [`TableTreeWrapper::set_unrounded_layout`] adds it to the top padding of the cell's layout, which is
/// where inline content (laid out as one paragraph, no child boxes) is placed from.
///
/// The content is laid out at its natural height, as if the cell had `height: auto` (and no
/// `height` or `min-height`, which only set the row's minimum), so a percentage height inside the cell is `auto` as
/// in Chrome and never fills the stretched cell. The reported size is then the row's height.
pub(crate) fn compute_table_cell_layout(
    doc: &mut BaseDocument,
    node_id: NodeId,
    inputs: LayoutInput,
) -> LayoutOutput {
    let id = usize::from(node_id);
    let Some(cell_height) = inputs.known_dimensions.height else {
        return compute_node_layout(doc, node_id, inputs);
    };
    let factor = cell_align_factor(&doc.nodes[id]);

    // `height` and `min-height` of the cell made it as tall as its row (`collect_table_cells` hands them
    // to the grid as a minimum); they are not part of its content height.
    let (height, min_height) = {
        let style = &mut doc.nodes[id].style;
        (
            std::mem::replace(&mut style.size.height, style_helpers::auto()),
            std::mem::replace(&mut style.min_size.height, style_helpers::auto()),
        )
    };
    let mut output = compute_node_layout(
        doc,
        node_id,
        LayoutInput {
            known_dimensions: Size {
                width: inputs.known_dimensions.width,
                height: None,
            },
            ..inputs
        },
    );
    {
        let style = &mut doc.nodes[id].style;
        style.size.height = height;
        style.min_size.height = min_height;
    }
    let natural = output.size.height;
    output.size.height = cell_height;

    let mut offset = (cell_height - natural).max(0.0) * factor;
    // Float noise in `natural == cell_height` must not move a cell.
    if offset < 0.001 {
        offset = 0.0;
    }
    doc.nodes[id].table_cell_offset = offset;
    if offset > 0.0 {
        let children = doc.nodes[id].layout_children.borrow().clone();
        for child_id in children.into_iter().flatten() {
            let child = &mut doc.nodes[child_id];
            // Out-of-flow boxes are placed relative to their containing block, not to the content.
            if child.style.position != Position::Absolute {
                child.unrounded_layout.location.y += offset;
            }
        }
    }
    output
}

pub struct RangeIter(Range<usize>);

impl Iterator for RangeIter {
    type Item = taffy::NodeId;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(taffy::NodeId::from)
    }
}

impl taffy::TraversePartialTree for TableTreeWrapper<'_> {
    type ChildIter<'a>
        = RangeIter
    where
        Self: 'a;

    #[inline(always)]
    fn child_ids(&self, _node_id: taffy::NodeId) -> Self::ChildIter<'_> {
        RangeIter(0..self.ctx.items.len())
    }

    #[inline(always)]
    fn child_count(&self, node_id: taffy::NodeId) -> usize {
        self.doc.child_count(node_id)
    }

    #[inline(always)]
    fn get_child_id(&self, _node_id: taffy::NodeId, index: usize) -> taffy::NodeId {
        index.into()
    }
}
impl taffy::TraverseTree for TableTreeWrapper<'_> {}

impl taffy::LayoutPartialTree for TableTreeWrapper<'_> {
    type CoreContainerStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    type CustomIdent = Atom;

    fn get_core_container_style(&self, _node_id: taffy::NodeId) -> &taffy::Style<Atom> {
        &self.ctx.style
    }

    fn resolve_calc_value(&self, calc_ptr: *const (), parent_size: f32) -> f32 {
        resolve_calc_value(calc_ptr, parent_size)
    }

    fn set_unrounded_layout(&mut self, node_id: taffy::NodeId, layout: &taffy::Layout) {
        let item = &self.ctx.items[usize::from(node_id)];
        let node_id = taffy::NodeId::from(item.node_id);
        let offset = match item.kind {
            TableItemKind::Cell => self.doc.nodes[item.node_id].table_cell_offset,
            TableItemKind::Row => 0.0,
        };
        if offset > 0.0 {
            // Inline content of the cell is placed from its top padding edge: move that edge down.
            let mut layout = *layout;
            layout.padding.top += offset;
            self.doc.set_unrounded_layout(node_id, &layout)
        } else {
            self.doc.set_unrounded_layout(node_id, layout)
        }
    }

    fn compute_child_layout(
        &mut self,
        node_id: taffy::NodeId,
        inputs: taffy::tree::LayoutInput,
    ) -> taffy::LayoutOutput {
        let cell = &self.ctx.items[usize::from(node_id)];
        match cell.kind {
            TableItemKind::Row => {
                compute_leaf_layout(inputs, &cell.style, resolve_calc_value, |_, _| {
                    taffy::Size::ZERO
                })
            }
            TableItemKind::Cell => {
                let id = cell.node_id;
                // The cell's explicit length `width` is the column's business (`push_cell_item` gave it to the
                // track). Left on the cell it would also be the cell's min-content size, so a `nowrap` text in a
                // `width: 50px` cell could not widen its column and would spill into the next cell. (Percentages
                // are left alone: they resolve against the table, not to a length of their own.)
                let length_width =
                    self.doc.nodes[id].style.size.width.tag() == taffy::CompactLength::LENGTH_TAG;
                let width = length_width.then(|| {
                    std::mem::replace(
                        &mut self.doc.nodes[id].style.size.width,
                        style_helpers::auto(),
                    )
                });
                let output = self
                    .doc
                    .compute_child_layout(taffy::NodeId::from(id), inputs);
                if let Some(width) = width {
                    self.doc.nodes[id].style.size.width = width;
                }
                output
            }
        }
    }
}

impl taffy::LayoutGridContainer for TableTreeWrapper<'_> {
    type GridContainerStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    type GridItemStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    fn get_grid_container_style(&self, node_id: taffy::NodeId) -> Self::GridContainerStyle<'_> {
        self.get_core_container_style(node_id)
    }

    fn get_grid_child_style(&self, child_node_id: taffy::NodeId) -> Self::GridItemStyle<'_> {
        &self.ctx.items[usize::from(child_node_id)].style
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colspan_is_parsed_like_html() {
        for (value, span) in [
            (None, 1),
            (Some(""), 1),
            (Some("0"), 1),
            (Some("00"), 1),
            (Some("-1"), 1),
            (Some("abc"), 1),
            (Some("+1"), 1),
            (Some("2"), 2),
            (Some(" 2"), 2),
            (Some("\t\n2"), 2),
            (Some("2.5"), 2),
            (Some("2px"), 2),
            (Some("02"), 2),
            (Some("1000"), 1000),
            (Some("1001"), 1000),
            (Some("65536"), 1000),
            (Some("99999999999999999999"), 1000),
        ] {
            assert_eq!(parse_colspan(value), span, "{value:?}");
        }
    }
    use taffy::style_helpers::{auto, percent};

    fn length(v: f32) -> taffy::Dimension {
        taffy::style_helpers::length(v)
    }

    fn styled(
        height: taffy::Dimension,
        min: taffy::Dimension,
        max: taffy::Dimension,
    ) -> taffy::Style<Atom> {
        let mut style = taffy::Style::<Atom>::default();
        style.size.height = height;
        style.min_size.height = min;
        style.max_size.height = max;
        style
    }

    #[test]
    fn a_length_height_becomes_the_minimum() {
        let mut style = styled(length(40.0), auto(), auto());
        height_as_table_minimum(&mut style);
        assert_eq!(style.size.height, auto());
        assert_eq!(style.min_size.height, length(40.0));
    }

    #[test]
    fn the_larger_of_height_and_min_height_wins() {
        let mut style = styled(length(40.0), length(60.0), auto());
        height_as_table_minimum(&mut style);
        assert_eq!(style.size.height, auto());
        assert_eq!(style.min_size.height, length(60.0));
        let mut style = styled(length(80.0), length(60.0), auto());
        height_as_table_minimum(&mut style);
        assert_eq!(style.min_size.height, length(80.0));
    }

    #[test]
    fn percentage_heights_and_percentage_minimums_are_left_alone() {
        let mut style = styled(percent(0.5_f32), auto(), auto());
        height_as_table_minimum(&mut style);
        assert_eq!(style.size.height, percent(0.5_f32));
        assert_eq!(style.min_size.height, auto());
        let mut style = styled(length(40.0), percent(0.5_f32), auto());
        height_as_table_minimum(&mut style);
        assert_eq!(style.size.height, length(40.0));
        assert_eq!(style.min_size.height, percent(0.5_f32));
    }

    #[test]
    fn max_height_is_ignored() {
        let mut style = styled(auto(), auto(), length(40.0));
        height_as_table_minimum(&mut style);
        assert_eq!(style.max_size.height, auto());
        let mut style = styled(length(20.0), length(10.0), length(15.0));
        height_as_table_minimum(&mut style);
        assert_eq!(style.max_size.height, auto());
        assert_eq!(style.min_size.height, length(20.0));
    }

    fn framed(min: taffy::Dimension, box_sizing: taffy::BoxSizing) -> taffy::Style<Atom> {
        let mut style = styled(auto(), min, auto());
        style.box_sizing = box_sizing;
        style.padding.top = taffy::LengthPercentage::length(10.0);
        style.padding.bottom = taffy::LengthPercentage::length(10.0);
        style.border.top = taffy::LengthPercentage::length(6.0);
        style.border.bottom = taffy::LengthPercentage::length(6.0);
        style
    }

    #[test]
    fn the_grid_minimum_is_the_height_less_the_frame_in_both_box_sizings() {
        for bs in [taffy::BoxSizing::BorderBox, taffy::BoxSizing::ContentBox] {
            let mut style = framed(length(100.0), bs);
            grid_container_min_height(&mut style);
            assert_eq!(style.min_size.height, length(68.0), "{bs:?}");
        }
    }

    #[test]
    fn the_grid_minimum_leaves_auto_and_percentages_alone() {
        for min in [auto(), percent(0.5_f32)] {
            let mut style = framed(min, taffy::BoxSizing::BorderBox);
            grid_container_min_height(&mut style);
            assert_eq!(style.min_size.height, min);
        }
    }

    /// Placements of rows given as the `colspan`s of their cells.
    fn placed(rows: &[&[u32]]) -> Vec<CellPlacement> {
        let mut out = Vec::new();
        for spans in rows {
            let mut start = 0;
            for &span in *spans {
                out.push(CellPlacement {
                    item: out.len(),
                    start,
                    span,
                });
                start += span;
            }
        }
        out
    }

    /// The `(line, span)` of every cell, by [`grid_column`] over the tracks of `rows`.
    fn grid_of(rows: &[&[u32]], fixed: bool) -> (Vec<u32>, Vec<(i16, u16)>) {
        let placements = placed(rows);
        let tracks = track_columns(&placements, fixed);
        let cells = placements
            .iter()
            .map(|p| grid_column(&tracks, p.start, p.span))
            .collect();
        (tracks, cells)
    }

    #[test]
    fn a_span_past_the_end_of_the_table_covers_the_real_columns_only() {
        // `A B C` over one cell spanning 99: the cell covers the three tracks, not 99.
        let (tracks, cells) = grid_of(&[&[1, 1, 1], &[99]], false);
        assert_eq!(tracks, [0, 1, 2]);
        assert_eq!(cells, [(1, 1), (2, 1), (3, 1), (1, 3)]);
        // Exact and far-too-wide spans are the same grid.
        assert_eq!(grid_of(&[&[1, 1, 1], &[3]], false).1, cells);
        assert_eq!(grid_of(&[&[1, 1, 1], &[1000]], false).1, cells);
        // So is a spanning cell above the cells it spans.
        let (_, above) = grid_of(&[&[99], &[1, 1, 1]], false);
        assert_eq!(above, [(1, 3), (1, 1), (2, 1), (3, 1)]);
    }

    #[test]
    fn a_column_only_spans_cover_is_not_a_track() {
        // Every row is `X` over two columns, then `Y`: column 1 starts no cell.
        let (tracks, cells) = grid_of(&[&[2, 1], &[2, 1]], false);
        assert_eq!(tracks, [0, 2]);
        assert_eq!(cells, [(1, 1), (2, 1), (1, 1), (2, 1)]);
        // `A`, `B` over two columns, `D` / `X` over three, `Y`: the tracks are 0, 1 and 3.
        let (tracks, cells) = grid_of(&[&[1, 2, 1], &[3, 1]], false);
        assert_eq!(tracks, [0, 1, 3]);
        assert_eq!(cells, [(1, 1), (2, 1), (3, 1), (1, 2), (3, 1)]);
    }

    #[test]
    fn a_spanning_first_row_leaves_the_tracks_to_the_rows_below() {
        let (tracks, cells) = grid_of(&[&[2, 1], &[1, 1, 1]], false);
        assert_eq!(tracks, [0, 1, 2]);
        assert_eq!(cells, [(1, 2), (3, 1), (1, 1), (2, 1), (3, 1)]);
    }

    #[test]
    fn rows_that_all_span_form_one_track() {
        let (tracks, cells) = grid_of(&[&[5], &[3]], false);
        assert_eq!(tracks, [0]);
        assert_eq!(cells, [(1, 1), (1, 1)]);
        let (tracks, cells) = grid_of(&[&[1000]], false);
        assert_eq!(tracks, [0]);
        assert_eq!(cells, [(1, 1)]);
    }

    #[test]
    fn rows_of_spans_of_5_3_and_4_plus_1_have_two_tracks() {
        let (tracks, cells) = grid_of(&[&[5], &[3], &[4, 1]], false);
        assert_eq!(tracks, [0, 4]);
        assert_eq!(cells, [(1, 2), (1, 1), (1, 1), (2, 1)]);
    }

    #[test]
    fn fixed_layout_keeps_every_column_up_to_the_widest_row() {
        let (tracks, cells) = grid_of(&[&[1, 1, 1], &[99], &[1]], true);
        assert_eq!(tracks.len(), 99);
        assert_eq!(cells[0..3], [(1, 1), (2, 1), (3, 1)]);
        assert_eq!(cells[3], (1, 99));
        // The same table in automatic layout has three.
        assert_eq!(grid_of(&[&[1, 1, 1], &[99], &[1]], false).0.len(), 3);
        // A row ending in a cell after the span is placed after it.
        let (tracks, cells) = grid_of(&[&[1, 1, 1], &[99, 1]], true);
        assert_eq!(tracks.len(), 100);
        assert_eq!(cells[4], (100, 1));
    }

    #[test]
    fn twenty_cells_of_colspan_1000_are_twenty_tracks() {
        let row = [1000; 20];
        let (tracks, cells) = grid_of(&[&row, &row], false);
        assert_eq!(tracks.len(), 20);
        let lines: Vec<i16> = cells.iter().take(20).map(|c| c.0).collect();
        assert_eq!(lines, (1..=20).collect::<Vec<i16>>());
        assert!(cells.iter().all(|c| c.1 == 1));
    }

    #[test]
    fn a_table_without_cells_has_no_tracks() {
        assert!(track_columns(&[], false).is_empty());
        assert!(track_columns(&[], true).is_empty());
    }

    #[test]
    fn more_columns_than_grid_lines_share_the_last_track() {
        let row = vec![1; MAX_COLUMNS as usize + 600];
        for fixed in [false, true] {
            let (tracks, cells) = grid_of(&[&row], fixed);
            assert_eq!(tracks.len(), MAX_COLUMNS as usize, "fixed: {fixed}");
            assert_eq!(cells[0], (1, 1));
            assert_eq!(cells[MAX_COLUMNS as usize - 1], (MAX_COLUMNS as i16, 1));
            // Past the last track: on it, never beyond Taffy's `i16` grid lines.
            assert_eq!(cells[MAX_COLUMNS as usize + 500], (MAX_COLUMNS as i16, 1));
        }
        // A span reaching past the last track stops at it.
        let tracks: Vec<u32> = (0..MAX_COLUMNS).collect();
        assert_eq!(
            grid_column(&tracks, MAX_COLUMNS - 3, MAX_COLSPAN),
            (MAX_COLUMNS as i16 - 2, 3)
        );
    }

    #[test]
    fn the_fixed_algorithm_needs_a_table_width() {
        let mut style = taffy::Style::<Atom>::default();
        assert!(!uses_fixed_algorithm(true, &style));
        style.size.width = length(600.0);
        assert!(uses_fixed_algorithm(true, &style));
        style.size.width = percent(1.0_f32);
        assert!(uses_fixed_algorithm(true, &style));
        assert!(!uses_fixed_algorithm(false, &style));
    }

    use TablePart::{Caption, Cell, Column, Hidden, Other, Row, RowGroup, Whitespace};

    #[test]
    fn plan_row_keeps_cells_and_wraps_each_run_of_other_children() {
        // cell, run of two, cell, run of one
        let parts = [Cell, Other, Other, Cell, Other];
        assert_eq!(
            plan_row(&parts, 0..parts.len()),
            vec![
                RowSlot::Cell(0),
                RowSlot::AnonymousCell(1..3),
                RowSlot::Cell(3),
                RowSlot::AnonymousCell(4..5),
            ]
        );
        assert_eq!(
            plan_row(&[Cell, Cell], 0..2),
            vec![RowSlot::Cell(0), RowSlot::Cell(1)]
        );
        assert_eq!(plan_row(&[], 0..0), vec![]);
    }

    #[test]
    fn plan_row_trims_whitespace_and_hidden_boxes_at_the_ends_of_a_run_only() {
        // Whitespace never opens a run, stays inside one, and trailing ones are dropped.
        let parts = [
            Whitespace, Cell, Whitespace, Other, Whitespace, Hidden, Other, Whitespace, Hidden,
            Cell,
        ];
        assert_eq!(
            plan_row(&parts, 0..parts.len()),
            vec![
                RowSlot::Cell(1),
                RowSlot::AnonymousCell(3..7),
                RowSlot::Cell(9),
            ]
        );
        // Nothing but whitespace and hidden boxes: no cell at all.
        assert_eq!(plan_row(&[Whitespace, Hidden, Whitespace], 0..3), vec![]);
    }

    #[test]
    fn plan_row_skips_columns_and_captions_and_they_end_a_run() {
        let parts = [Other, Column, Other, Caption, Cell];
        assert_eq!(
            plan_row(&parts, 0..parts.len()),
            vec![
                RowSlot::AnonymousCell(0..1),
                RowSlot::AnonymousCell(2..3),
                RowSlot::Cell(4),
            ]
        );
    }

    #[test]
    fn plan_row_wraps_a_misplaced_row_or_row_group_like_any_other_box() {
        let parts = [Row, RowGroup, Cell];
        assert_eq!(
            plan_row(&parts, 0..3),
            vec![RowSlot::AnonymousCell(0..2), RowSlot::Cell(2)]
        );
    }

    #[test]
    fn plan_row_plans_only_its_range() {
        let parts = [Other, Cell, Other, Other];
        assert_eq!(plan_row(&parts, 2..4), vec![RowSlot::AnonymousCell(2..4)]);
    }

    #[test]
    fn a_table_keeps_rows_and_row_groups() {
        let parts = [Whitespace, RowGroup, Whitespace, Row, Whitespace, RowGroup];
        assert_eq!(
            plan_table_children(Level::Table, &parts),
            vec![Slot::Keep(1), Slot::Keep(3), Slot::Keep(5)]
        );
    }

    #[test]
    fn a_table_wraps_a_run_of_cells_in_one_anonymous_row() {
        // Two bare cells, then a row: the cells share an anonymous row, the row follows.
        let parts = [Cell, Cell, Row, Cell];
        assert_eq!(
            plan_table_children(Level::Table, &parts),
            vec![
                Slot::AnonymousRow(vec![RowSlot::Cell(0), RowSlot::Cell(1)]),
                Slot::Keep(2),
                Slot::AnonymousRow(vec![RowSlot::Cell(3)]),
            ]
        );
    }

    #[test]
    fn a_table_wraps_other_boxes_in_an_anonymous_row_and_an_anonymous_cell() {
        // Missing wrappers apply twice: a `tbody { display: block }` is not a row, so it gets an anonymous row;
        // it is not a cell either, so that row gets an anonymous cell. Cells in the same run stay cells.
        let parts = [Row, Other, Whitespace, Other, Cell, Whitespace, Row];
        assert_eq!(
            plan_table_children(Level::Table, &parts),
            vec![
                Slot::Keep(0),
                Slot::AnonymousRow(vec![RowSlot::AnonymousCell(1..4), RowSlot::Cell(4)]),
                Slot::Keep(6),
            ]
        );
    }

    #[test]
    fn a_row_group_wraps_a_nested_row_group_but_a_table_does_not() {
        let parts = [Row, RowGroup, Row];
        assert_eq!(
            plan_table_children(Level::RowGroup, &parts),
            vec![
                Slot::Keep(0),
                Slot::AnonymousRow(vec![RowSlot::AnonymousCell(1..2)]),
                Slot::Keep(2),
            ]
        );
        assert_eq!(
            plan_table_children(Level::Table, &parts),
            vec![Slot::Keep(0), Slot::Keep(1), Slot::Keep(2)]
        );
    }

    #[test]
    fn a_table_skips_captions_columns_and_hidden_boxes() {
        let parts = [Caption, Column, Hidden, Row, Hidden, Whitespace];
        assert_eq!(
            plan_table_children(Level::Table, &parts),
            vec![Slot::Keep(3)]
        );
        // They end a run: two boxes around a column are two anonymous rows.
        let parts = [Other, Column, Other];
        assert_eq!(
            plan_table_children(Level::Table, &parts),
            vec![
                Slot::AnonymousRow(vec![RowSlot::AnonymousCell(0..1)]),
                Slot::AnonymousRow(vec![RowSlot::AnonymousCell(2..3)]),
            ]
        );
    }

    #[test]
    fn a_table_without_misparented_children_plans_no_anonymous_row() {
        // The common case: whitespace between rows and groups is dropped, nothing is wrapped.
        let parts = [Whitespace, RowGroup, Whitespace, RowGroup, Whitespace];
        assert!(
            plan_table_children(Level::Table, &parts)
                .iter()
                .all(|slot| matches!(slot, Slot::Keep(_)))
        );
    }

    #[test]
    fn a_table_without_group_roles_keeps_its_order() {
        let slots = vec![
            Slot::Keep(0),
            Slot::AnonymousRow(vec![RowSlot::Cell(1)]),
            Slot::Keep(2),
        ];
        assert_eq!(order_table_slots(slots.clone(), &[None, None, None]), slots);
        assert_eq!(order_table_slots(Vec::new(), &[]), Vec::new());
    }

    #[test]
    fn a_footer_written_first_and_a_header_written_last_swap_places() {
        use GroupRole::{Footer, Header};
        // [F, B, H] is displayed as [H, B, F].
        let roles = [Some(Footer), None, Some(Header)];
        let slots = vec![Slot::Keep(0), Slot::Keep(1), Slot::Keep(2)];
        assert_eq!(
            order_table_slots(slots, &roles),
            vec![Slot::Keep(2), Slot::Keep(1), Slot::Keep(0)]
        );
        // [H, F, B] is displayed as [H, B, F].
        let roles = [Some(Header), Some(Footer), None];
        let slots = vec![Slot::Keep(0), Slot::Keep(1), Slot::Keep(2)];
        assert_eq!(
            order_table_slots(slots, &roles),
            vec![Slot::Keep(0), Slot::Keep(2), Slot::Keep(1)]
        );
    }

    #[test]
    fn only_the_first_footer_and_the_first_header_move() {
        use GroupRole::{Footer, Header};
        // [F1, B, F2] is displayed as [B, F2, F1]: F2 is a plain row group.
        let roles = [Some(Footer), None, Some(Footer)];
        let slots = vec![Slot::Keep(0), Slot::Keep(1), Slot::Keep(2)];
        assert_eq!(
            order_table_slots(slots, &roles),
            vec![Slot::Keep(1), Slot::Keep(2), Slot::Keep(0)]
        );
        // [B, H1, H2] is displayed as [H1, B, H2].
        let roles = [None, Some(Header), Some(Header)];
        let slots = vec![Slot::Keep(0), Slot::Keep(1), Slot::Keep(2)];
        assert_eq!(
            order_table_slots(slots, &roles),
            vec![Slot::Keep(1), Slot::Keep(0), Slot::Keep(2)]
        );
    }

    #[test]
    fn the_modern_order_is_not_touched() {
        use GroupRole::{Footer, Header};
        let roles = [Some(Header), None, None, Some(Footer)];
        let slots = vec![Slot::Keep(0), Slot::Keep(1), Slot::Keep(2), Slot::Keep(3)];
        assert_eq!(order_table_slots(slots.clone(), &roles), slots);
        // Two headers written first: the second one is a body group and stays behind the first.
        let roles = [Some(Header), Some(Header), None];
        let slots = vec![Slot::Keep(0), Slot::Keep(1), Slot::Keep(2)];
        assert_eq!(order_table_slots(slots.clone(), &roles), slots);
    }

    #[test]
    fn bare_and_anonymous_rows_keep_their_place_among_the_bodies() {
        use GroupRole::{Footer, Header};
        // Slots: footer, bare row, anonymous row, body group, header.
        let roles = [Some(Footer), None, None, None, Some(Header)];
        let slots = vec![
            Slot::Keep(0),
            Slot::Keep(1),
            Slot::AnonymousRow(vec![RowSlot::AnonymousCell(2..3)]),
            Slot::Keep(3),
            Slot::Keep(4),
        ];
        assert_eq!(
            order_table_slots(slots, &roles),
            vec![
                Slot::Keep(4),
                Slot::Keep(1),
                Slot::AnonymousRow(vec![RowSlot::AnonymousCell(2..3)]),
                Slot::Keep(3),
                Slot::Keep(0),
            ]
        );
    }

    #[test]
    fn a_group_role_is_decided_by_display() {
        assert_eq!(
            group_role_of_display(DisplayInside::TableHeaderGroup),
            Some(GroupRole::Header)
        );
        assert_eq!(
            group_role_of_display(DisplayInside::TableFooterGroup),
            Some(GroupRole::Footer)
        );
        for inside in [
            DisplayInside::TableRowGroup,
            DisplayInside::TableRow,
            DisplayInside::TableCell,
            DisplayInside::Flow,
            DisplayInside::Table,
        ] {
            assert_eq!(group_role_of_display(inside), None, "{inside:?}");
        }
    }
}
