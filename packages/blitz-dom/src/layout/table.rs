use std::{ops::Range, sync::Arc};

use markup5ever::local_name;
use style::values::generics::box_::{VerticalAlign, VerticalAlignKeyword};
use style::values::specified::box_::{DisplayInside, DisplayOutside};
use style::{Atom, computed_values::table_layout::T as TableLayout};
use taffy::{
    Dimension, LayoutInput, LayoutOutput, LayoutPartialTree as _, NodeId, Position, RequestedAxis,
    ResolveOrZero, RunMode, Size, TrackSizingFunction, compute_leaf_layout, style_helpers,
};

use crate::BaseDocument;
use crate::node::Node;

use super::compute_node_layout;
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
    let mut items: Vec<TableItem> = Vec::new();
    let mut row = 0u16;
    let mut col = 0u16;

    let root_node = &mut doc.nodes[table_root_node_id];

    let children = std::mem::take(&mut root_node.children);

    let Some(stylo_styles) = root_node.primary_styles() else {
        panic!("Ignoring table because it has no styles");
    };

    let mut style = stylo_taffy::to_taffy_style(&stylo_styles);
    style.grid_auto_columns = Vec::new();
    style.grid_auto_rows = Vec::new();

    let is_fixed = match stylo_styles.clone_table_layout() {
        TableLayout::Fixed => true,
        TableLayout::Auto => false,
    };

    drop(stylo_styles);

    let mut column_sizes: Vec<taffy::Dimension> = Vec::new();
    for child_id in children.iter().copied() {
        collect_table_cells(
            doc,
            child_id,
            is_fixed,
            &mut row,
            &mut col,
            &mut items,
            &mut column_sizes,
        );
    }
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

pub(crate) fn collect_table_cells(
    doc: &mut BaseDocument,
    node_id: usize,
    is_fixed: bool,
    row: &mut u16,
    col: &mut u16,
    cells: &mut Vec<TableItem>,
    columns: &mut Vec<Dimension>,
) {
    let node = &doc.nodes[node_id];

    if !node.is_element() {
        return;
    }

    let Some(display) = node.primary_styles().map(|s| s.clone_display()) else {
        println!("Ignoring table descendent because it has no styles");
        return;
    };

    if display.outside() == DisplayOutside::None {
        node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
        return;
    }

    match display.inside() {
        DisplayInside::TableRowGroup
        | DisplayInside::TableHeaderGroup
        | DisplayInside::TableFooterGroup
        | DisplayInside::Contents => {
            let children = std::mem::take(&mut doc.nodes[node_id].children);
            for child_id in children.iter().copied() {
                doc.nodes[child_id]
                    .remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                collect_table_cells(doc, child_id, is_fixed, row, col, cells, columns);
            }
            doc.nodes[node_id].children = children;
        }
        DisplayInside::TableRow => {
            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            *row += 1;
            *col = 0;

            {
                let stylo_style = &node.primary_styles().unwrap();
                let mut style = stylo_taffy::to_taffy_style(stylo_style);
                style.grid_column = taffy::Line {
                    start: style_helpers::line(0),
                    end: style_helpers::line(-1),
                };
                style.grid_row = taffy::Line {
                    start: style_helpers::line(*row as i16),
                    end: style_helpers::span(1),
                };
                cells.push(TableItem {
                    kind: TableItemKind::Row,
                    node_id,
                    style,
                });
            }

            let children = std::mem::take(&mut doc.nodes[node_id].children);
            for child_id in children.iter().copied() {
                collect_table_cells(doc, child_id, is_fixed, row, col, cells, columns);
            }
            doc.nodes[node_id].children = children;
        }
        DisplayInside::TableCell => {
            // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            let stylo_style = &node.primary_styles().unwrap();
            let colspan: u16 = node
                .attr(local_name!("colspan"))
                .and_then(|val| val.parse().ok())
                .unwrap_or(1);
            let mut style = stylo_taffy::to_taffy_style(stylo_style);

            // TODO: account for padding/border/margin
            if *row == 1 {
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
                columns.push(column);
            } else if !is_fixed
                && (*col as usize) < columns.len()
                && taffy::CompactLength::LENGTH_TAG == style.size.width.tag()
            {
                let new_len = cell_border_box_width(&style);
                let tag = columns[*col as usize].tag();
                let value = columns[*col as usize].value();
                columns[*col as usize] = match tag {
                    taffy::CompactLength::LENGTH_TAG => style_helpers::length(value.max(new_len)),
                    taffy::CompactLength::AUTO_TAG => style_helpers::length(new_len),
                    taffy::CompactLength::PERCENT_TAG => style_helpers::percent(value),
                    _ => unreachable!(),
                }
            }

            style.grid_column = taffy::Line {
                start: style_helpers::line((*col + 1) as i16),
                end: style_helpers::span(colspan),
            };
            style.grid_row = taffy::Line {
                start: style_helpers::line(*row as i16),
                end: style_helpers::span(1),
            };
            style.size.width = style_helpers::auto();
            cells.push(TableItem {
                kind: TableItemKind::Cell,
                node_id,
                style,
            });

            *col += colspan;
        }
        DisplayInside::Flow
        | DisplayInside::FlowRoot
        | DisplayInside::Flex
        | DisplayInside::Grid => {
            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            // Probably a table caption: ignore
            // println!(
            //     "Warning: ignoring non-table typed descendent of table ({:?})",
            //     display.inside()
            // );
        }
        DisplayInside::TableColumnGroup | DisplayInside::TableColumn | DisplayInside::Table => {
            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            //Ignore
        }
        DisplayInside::None => {
            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            // Ignore
        }
    }
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

    // The natural (content) height at the same width. Measured first: Taffy's block `ComputeSize` also
    // writes child positions, which the layout below must overwrite, not the other way round.
    let natural = doc
        .compute_child_layout(
            node_id,
            LayoutInput {
                run_mode: RunMode::ComputeSize,
                axis: RequestedAxis::Both,
                known_dimensions: Size {
                    width: inputs.known_dimensions.width,
                    height: None,
                },
                ..inputs
            },
        )
        .size
        .height;

    let output = compute_node_layout(doc, node_id, inputs);

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
