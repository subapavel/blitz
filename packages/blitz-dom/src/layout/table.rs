use std::{ops::Range, sync::Arc};

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
        columns: Vec::new(),
    };
    collect_table_children(doc, table_root_node_id, &children, Level::Table, &mut grid);
    let GridBuilder {
        row,
        col,
        items,
        columns: mut column_sizes,
        ..
    } = grid;
    column_sizes.resize(col as usize, style_helpers::auto());

    style.grid_template_columns = column_sizes
        .into_iter()
        .map(|dim| TrackSizingFunction::from(dim).into())
        .collect();
    style.grid_template_rows = vec![style_helpers::auto(); row as usize];

    let layout_children = items
        .iter()
        .filter(|item| item.kind == TableItemKind::Cell)
        .map(|cell| cell.node_id)
        .collect();
    let root_node = &mut doc.nodes[table_root_node_id];
    root_node.children = children;

    (TableContext { style, items }, layout_children)
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

/// Adds the table cell `node_id` (a `display: table-cell` element, or an anonymous cell) to the grid: its column
/// track from the first row's widths (or a wider `width` below it), its grid position, and its style as a grid item.
fn push_cell_item(doc: &BaseDocument, node_id: usize, grid: &mut GridBuilder) {
    let node = &doc.nodes[node_id];
    let stylo_style = &node.primary_styles().unwrap();
    let colspan: u16 = node
        .attr(local_name!("colspan"))
        .and_then(|val| val.parse().ok())
        .unwrap_or(1);
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
        grid.columns.push(column);
    } else if !grid.is_fixed
        && (grid.col as usize) < grid.columns.len()
        && taffy::CompactLength::LENGTH_TAG == style.size.width.tag()
    {
        let new_len = cell_border_box_width(&style);
        let tag = grid.columns[grid.col as usize].tag();
        let value = grid.columns[grid.col as usize].value();
        grid.columns[grid.col as usize] = match tag {
            taffy::CompactLength::LENGTH_TAG => style_helpers::length(value.max(new_len)),
            taffy::CompactLength::AUTO_TAG => style_helpers::length(new_len),
            taffy::CompactLength::PERCENT_TAG => style_helpers::percent(value),
            _ => unreachable!(),
        }
    }

    style.grid_column = taffy::Line {
        start: style_helpers::line((grid.col + 1) as i16),
        end: style_helpers::span(colspan),
    };
    style.grid_row = taffy::Line {
        start: style_helpers::line(grid.row as i16),
        end: style_helpers::span(1),
    };
    style.size.width = style_helpers::auto();
    height_as_table_minimum(&mut style);
    grid.items.push(TableItem {
        kind: TableItemKind::Cell,
        node_id,
        style,
    });

    grid.col += colspan;
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

/// The grid being built from a table's parts: the position of the next cell and what has been collected.
struct GridBuilder {
    is_fixed: bool,
    row: u16,
    col: u16,
    items: Vec<TableItem>,
    columns: Vec<Dimension>,
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
                let node_id = taffy::NodeId::from(cell.node_id);
                self.doc.compute_child_layout(node_id, inputs)
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
}
