use core::str;
use std::sync::Arc;

use markup5ever::{QualName, local_name, ns};
use parley::{FontContext, LayoutContext, StyleProperty};
use slab::Slab;
use style::{
    data::ElementData as StyloElementData,
    selector_parser::RestyleDamage,
    shared_lock::StylesheetGuards,
    values::{
        computed::{Content, ContentItem, Display},
        specified::box_::{DisplayInside, DisplayOutside},
    },
};

use crate::{
    BaseDocument, ElementData, Node, NodeData,
    layout::damage::{CONSTRUCT_BOX, CONSTRUCT_DESCENDENT, CONSTRUCT_FC},
    node::{
        ListItemLayout, ListItemLayoutPosition, Marker, NodeFlags, NodeKind, SpecialElementData,
        TextBrush, TextInputData, TextLayout,
    },
    qual_name, stylo_to_parley,
};

use super::{
    damage::ALL_DAMAGE,
    list::collect_list_item_children,
    table::build_table_context,
    whitespace::{InlineOp, collapse_white_space, line_content_after_last_break, replay},
};

use style::values::specified::text::TextTransformCase;

thread_local! {
    /// Whether the next letter pushed into the current inline layout starts a word (for
    /// `text-transform: capitalize`). Reset at the start of every inline layout.
    pub(super) static AT_WORD_START: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// Applies the case part of `text-transform` (CSS Text 3 §2.1) to a text node's content. Word boundaries
/// for `capitalize` carry over from the previous text node of the same inline layout, so `wo<b>rd</b>`
/// stays one word. Language-specific mappings (Turkish dotted i, Greek accents) are not applied.
pub(super) fn transform_text(
    text: &str,
    case: Option<TextTransformCase>,
) -> std::borrow::Cow<'_, str> {
    AT_WORD_START.with(|at_word_start| {
        let mut start = at_word_start.get();
        let out = match case {
            Some(TextTransformCase::Uppercase) => std::borrow::Cow::Owned(text.to_uppercase()),
            Some(TextTransformCase::Lowercase) => std::borrow::Cow::Owned(text.to_lowercase()),
            Some(TextTransformCase::Capitalize) => {
                let mut out = String::with_capacity(text.len());
                for c in text.chars() {
                    if start && c.is_alphabetic() {
                        out.extend(c.to_uppercase());
                    } else {
                        out.push(c);
                    }
                    start = next_at_word_start(start, c);
                }
                at_word_start.set(start);
                return std::borrow::Cow::Owned(out);
            }
            _ => std::borrow::Cow::Borrowed(text),
        };
        for c in text.chars() {
            start = next_at_word_start(start, c);
        }
        at_word_start.set(start);
        out
    })
}

/// Letters, digits, combining marks and soft hyphens continue a word; an apostrophe does too, but only inside
/// one (`don't`, not `'quote`).
fn next_at_word_start(at_word_start: bool, c: char) -> bool {
    if c.is_alphanumeric()
        || is_combining_mark(c)
        || matches!(c, '\u{AD}' | '\u{200C}' | '\u{200D}')
    {
        false
    } else if matches!(c, '\'' | '\u{2019}') {
        at_word_start
    } else {
        true
    }
}

const DUMMY_NAME: QualName = qual_name!("div", html);

#[derive(Clone)]
pub(crate) struct ConstructionTask {
    pub(crate) node_id: usize,
    pub(crate) data: ConstructionTaskData,
}

pub(crate) struct ConstructionTaskResult {
    pub(crate) node_id: usize,
    pub(crate) data: ConstructionTaskResultData,
}

#[derive(Clone)]
pub(crate) enum ConstructionTaskData {
    InlineLayout(Box<TextLayout>),
}

pub(crate) enum ConstructionTaskResultData {
    InlineLayout(Box<TextLayout>),
}

fn push_children_and_pseudos(layout_children: &mut Vec<usize>, node: &Node) {
    if let Some(before) = node.before {
        layout_children.push(before);
    }
    layout_children.extend_from_slice(&node.children);
    if let Some(after) = node.after {
        layout_children.push(after);
    }
}

/// Convert a relative line height to an absolute one
fn resolve_line_height(line_height: parley::LineHeight, font_size: f32) -> f32 {
    match line_height {
        parley::LineHeight::FontSizeRelative(relative) => relative * font_size,
        parley::LineHeight::Absolute(absolute) => absolute,
        parley::LineHeight::MetricsRelative(relative) => relative * font_size, //unreachable!(),
    }
}

pub(crate) fn collect_layout_children(
    doc: &mut BaseDocument,
    container_node_id: usize,
    layout_children: &mut Vec<usize>,
    anonymous_block_id: &mut Option<usize>,
) {
    // Reset construction flags
    // TODO: make incremental and only remove this if the element is no longer an inline root
    doc.nodes[container_node_id]
        .flags
        .reset_construction_flags();
    if let Some(element_data) = doc.nodes[container_node_id].element_data_mut() {
        element_data.take_inline_layout();
    }

    flush_pseudo_elements(doc, container_node_id);

    if let Some(el) = doc.nodes[container_node_id].data.downcast_element() {
        // Handle text inputs
        let tag_name = el.name.local.as_ref();
        if matches!(tag_name, "input" | "textarea") {
            let type_attr: Option<&str> = doc.nodes[container_node_id]
                .data
                .downcast_element()
                .and_then(|el| el.attr(local_name!("type")));
            if tag_name == "textarea" {
                create_text_editor(doc, container_node_id, true);
                return;
            } else if matches!(
                type_attr,
                None | Some("text" | "password" | "email" | "number" | "search" | "tel" | "url")
            ) {
                create_text_editor(doc, container_node_id, false);
                return;
            } else if matches!(type_attr, Some("checkbox" | "radio")) {
                create_checkbox_input(doc, container_node_id);
                return;
            }
        }

        #[cfg(feature = "svg")]
        if matches!(tag_name, "svg") {
            let mut outer_html = doc.get_node(container_node_id).unwrap().outer_html();

            // HACK: usvg fails to parse SVGs that don't have the SVG xmlns set. So inject it
            // if the generated source doesn't have it.
            if !outer_html.contains("xmlns") {
                outer_html =
                    outer_html.replace("<svg", "<svg xmlns=\"http://www.w3.org/2000/svg\"");
            }

            // Remove contruction damage from subtree
            doc.iter_subtree_mut(container_node_id, |id: usize, doc: &mut BaseDocument| {
                doc.nodes[id].remove_damage(CONSTRUCT_BOX | CONSTRUCT_DESCENDENT | CONSTRUCT_FC);
            });

            let options = doc
                .svg_options
                .as_ref()
                .map(|factory| factory(container_node_id));
            match crate::util::parse_svg(outer_html.as_bytes(), options) {
                Ok(svg) => {
                    doc.get_node_mut(container_node_id)
                        .unwrap()
                        .element_data_mut()
                        .unwrap()
                        .special_data = SpecialElementData::Image(Box::new(svg.into()));
                }
                Err(err) => {
                    // The SVG is dropped: content the author wrote is lost, so this is a warning. The markup can
                    // be large; it is only at `debug`.
                    log::warn!("inline SVG (node {container_node_id}) is not rendered: {err}");
                    log::debug!("rejected SVG (node {container_node_id}): {outer_html}");
                }
            };
            return;
        }

        //Only ol tags have start and reversed attributes
        let (mut index, reversed) = if tag_name == "ol" {
            (
                el.attr_parsed(local_name!("start"))
                    .map(|start: usize| start - 1)
                    .unwrap_or(0),
                el.attr_parsed(local_name!("reversed")).unwrap_or(false),
            )
        } else {
            (1, false)
        };
        collect_list_item_children(doc, &mut index, reversed, container_node_id);
    }

    // Skip further construction if the node has no children or psuedo-children
    {
        let node = &doc.nodes[container_node_id];
        if node.children.is_empty() && node.before.is_none() && node.after.is_none() {
            return;
        }
    }

    let container_display = doc.nodes[container_node_id].display_style().unwrap_or(
        match doc.nodes[container_node_id].data.kind() {
            NodeKind::AnonymousBlock => Display::Block,
            _ => Display::Inline,
        },
    );

    match container_display.inside() {
        DisplayInside::None => {}
        DisplayInside::Contents => {
            // Only reached by defensive paths (the root is blockified): the element generates no box and
            // its children take its place.
            doc.nodes[container_node_id]
                .remove_damage(CONSTRUCT_BOX | CONSTRUCT_DESCENDENT | CONSTRUCT_FC);
            let flat = flattened_box_children(doc, container_node_id, true);
            layout_children.extend_from_slice(&flat);
        }
        DisplayInside::Flow | DisplayInside::FlowRoot | DisplayInside::TableCell => {
            // `display: contents` children generate no box: their children (and pseudo-elements) take part
            // in the classification as if they were children of this container (css-display-3 §2.5).
            let has_contents = has_contents_child(doc, container_node_id);
            let (all_block, all_inline) = if has_contents {
                let flat = flattened_box_children(doc, container_node_id, false);
                classify_children(doc, &flat)
            } else {
                classify_children(doc, &doc.nodes[container_node_id].children)
            };

            if all_inline {
                let existing_layout = doc.nodes[container_node_id]
                    .element_data_mut()
                    .and_then(|el| el.inline_layout_data.take());
                let layout = existing_layout.unwrap_or_else(|| Box::new(TextLayout::new()));

                // Queue node for inline layout construction. Deferring construction of inline layouts to a
                // dedicated phase allows us to multithread the expensive text shaping step.
                doc.deferred_construction_nodes.push(ConstructionTask {
                    node_id: container_node_id,
                    data: ConstructionTaskData::InlineLayout(layout),
                });
                doc.nodes[container_node_id]
                    .flags
                    .insert(NodeFlags::IS_INLINE_ROOT);
                find_inline_layout_embedded_boxes(doc, container_node_id, layout_children);
                return;
            }

            // If the children are either all inline or all block then simply return the regular children
            // as the layout children
            if (all_block | all_inline) & !has_contents {
                return push_children_and_pseudos(layout_children, &doc.nodes[container_node_id]);
            }

            fn block_item_needs_wrap(
                child_node_kind: NodeKind,
                display_outside: DisplayOutside,
            ) -> bool {
                child_node_kind == NodeKind::Text || display_outside == DisplayOutside::Inline
            }
            collect_complex_layout_children(
                doc,
                container_node_id,
                layout_children,
                anonymous_block_id,
                false,
                block_item_needs_wrap,
            );
        }
        DisplayInside::Flex | DisplayInside::Grid => {
            let has_text_node_or_contents = doc.nodes[container_node_id]
                .children
                .iter()
                .copied()
                .map(|child_id| &doc.nodes[child_id])
                .any(|child| {
                    let display = child.display_style().unwrap_or(Display::inline());
                    let node_kind = child.data.kind();
                    display.inside() == DisplayInside::Contents || node_kind == NodeKind::Text
                });

            if !has_text_node_or_contents {
                return push_children_and_pseudos(layout_children, &doc.nodes[container_node_id]);
            }

            fn flex_or_grid_item_needs_wrap(
                child_node_kind: NodeKind,
                _display_outside: DisplayOutside,
            ) -> bool {
                child_node_kind == NodeKind::Text
            }
            collect_complex_layout_children(
                doc,
                container_node_id,
                layout_children,
                anonymous_block_id,
                true,
                flex_or_grid_item_needs_wrap,
            );
        }

        DisplayInside::Table => {
            let (table_context, tlayout_children) = build_table_context(doc, container_node_id);
            #[allow(clippy::arc_with_non_send_sync)]
            let data = SpecialElementData::TableRoot(Arc::new(table_context));
            doc.nodes[container_node_id]
                .flags
                .insert(NodeFlags::IS_TABLE_ROOT);
            doc.nodes[container_node_id]
                .data
                .downcast_element_mut()
                .unwrap()
                .special_data = data;
            // The table's `::before` and `::after` boxes are table children like the DOM ones (see
            // `table_children_with_pseudos`): they are laid out as cells, rows or the content of an anonymous
            // cell, and reach the layout children of the table that way.
            layout_children.extend_from_slice(&tlayout_children);
        }

        _ => {
            if has_contents_child(doc, container_node_id) {
                let flat = flattened_box_children(doc, container_node_id, true);
                layout_children.extend_from_slice(&flat);
            } else {
                push_children_and_pseudos(layout_children, &doc.nodes[container_node_id]);
            }
        }
    }
}

/// Whether any child of `node_id` is a `display: contents` element.
fn has_contents_child(doc: &BaseDocument, node_id: usize) -> bool {
    doc.nodes[node_id].children.iter().any(|&child_id| {
        doc.nodes[child_id]
            .display_style()
            .is_some_and(|display| display.inside() == DisplayInside::Contents)
    })
}

/// The children of `node_id` that generate boxes, with every `display: contents` element replaced by its own
/// `::before`, children and `::after` (recursively, css-display-3 §2.5). The pseudo-elements of the contents
/// elements are created or refreshed on the way. The pseudo-elements of `node_id` itself are included when
/// `with_own_pseudos` is set (the caller has flushed them).
fn flattened_box_children(
    doc: &mut BaseDocument,
    node_id: usize,
    with_own_pseudos: bool,
) -> Vec<usize> {
    fn flatten(doc: &mut BaseDocument, node_id: usize, with_pseudos: bool, out: &mut Vec<usize>) {
        if with_pseudos {
            out.extend(doc.nodes[node_id].before);
        }
        for i in 0..doc.nodes[node_id].children.len() {
            let child_id = doc.nodes[node_id].children[i];
            let is_contents = doc.nodes[child_id]
                .display_style()
                .is_some_and(|display| display.inside() == DisplayInside::Contents);
            if is_contents {
                flush_pseudo_elements(doc, child_id);
                doc.nodes[child_id]
                    .remove_damage(CONSTRUCT_BOX | CONSTRUCT_DESCENDENT | CONSTRUCT_FC);
                flatten(doc, child_id, true, out);
            } else {
                out.push(child_id);
            }
        }
        if with_pseudos {
            out.extend(doc.nodes[node_id].after);
        }
    }

    let mut out = Vec::new();
    flatten(doc, node_id, with_own_pseudos, &mut out);
    out
}

/// `(all_block, all_inline)` for the box-generating children `ids` of a block container.
fn classify_children(doc: &BaseDocument, ids: &[usize]) -> (bool, bool) {
    let mut all_block = true;
    let mut all_inline = true;
    for child in ids.iter().map(|&child_id| &doc.nodes[child_id]) {
        // Unwraps on Text and SVG nodes
        let display = child.display_style().unwrap_or(Display::inline());
        match display.outside() {
            DisplayOutside::None => {}
            DisplayOutside::Block
            | DisplayOutside::TableCaption
            | DisplayOutside::InternalTable => all_inline = false,
            DisplayOutside::Inline => {
                all_block = false;

                // We need the "complex" tree fixing when an inline contains a block
                if child.is_or_contains_block() {
                    all_inline = false;
                }
            }
        }
    }
    (all_block, all_inline)
}

fn flush_pseudo_elements(doc: &mut BaseDocument, node_id: usize) {
    let (before_style, after_style, before_node_id, after_node_id) = {
        let node = &doc.nodes[node_id];

        let before_node_id = node.before;
        let after_node_id = node.after;

        // Note: yes these are kinda backwards
        let style_data = node.stylo_element_data.borrow();
        let before_style = style_data
            .as_ref()
            .and_then(|d| d.styles.pseudos.as_array()[1].clone());
        let after_style = style_data
            .as_ref()
            .and_then(|d| d.styles.pseudos.as_array()[0].clone());

        (before_style, after_style, before_node_id, after_node_id)
    };

    // Sync pseudo element
    // TODO: Make incremental
    for (idx, pe_style, pe_node_id) in [
        (1, before_style, before_node_id),
        (0, after_style, after_node_id),
    ] {
        // Delete psuedo element if it exists but shouldn't
        if let (Some(pe_node_id), None) = (pe_node_id, &pe_style) {
            doc.remove_and_drop_pe(pe_node_id);
            let node = &mut doc.nodes[node_id];
            node.set_pe_by_index(idx, None);
            node.insert_damage(ALL_DAMAGE);
        }

        // Create pseudo element if it should exist but doesn't
        if let (None, Some(pe_style)) = (pe_node_id, &pe_style) {
            let new_node_id = doc.create_node(NodeData::AnonymousBlock(ElementData::new(
                DUMMY_NAME,
                Vec::new(),
            )));
            doc.nodes[new_node_id].parent = Some(node_id);
            doc.nodes[new_node_id].layout_parent.set(Some(node_id));

            let content = &pe_style.as_ref().get_counters().content;
            if let Content::Items(item_data) = content {
                let items = &item_data.items[0..item_data.alt_start];
                match &items[0] {
                    ContentItem::String(owned_str) => {
                        let text_node_id = doc.create_text_node(owned_str);
                        doc.nodes[new_node_id].children.push(text_node_id);
                    }
                    _ => {
                        // TODO: other types of content
                    }
                }
            }

            let mut element_data = StyloElementData::default();
            element_data.styles.primary = Some(pe_style.clone());
            element_data.set_restyled();
            element_data.damage = RestyleDamage::all();
            *doc.nodes[new_node_id].stylo_element_data.borrow_mut() = Some(element_data);

            let node = &mut doc.nodes[node_id];
            node.set_pe_by_index(idx, Some(new_node_id));
            node.insert_damage(ALL_DAMAGE);
        }

        // Else: Update psuedo element
        if let (Some(pe_node_id), Some(pe_style)) = (pe_node_id, pe_style) {
            // TODO: Update content

            let mut node_styles = doc.nodes[pe_node_id].stylo_element_data.borrow_mut();
            let node_styles = &mut node_styles.as_mut().unwrap();
            node_styles.damage.insert(RestyleDamage::all());
            let primary_styles = &mut node_styles.styles.primary;

            if !std::ptr::eq(&**primary_styles.as_ref().unwrap(), &*pe_style) {
                *primary_styles = Some(pe_style);
                node_styles.set_restyled();
            }
        }
    }
}

/// Handles the cases where there are text nodes or inline nodes that need to be wrapped in an anonymous block node
fn collect_complex_layout_children(
    doc: &mut BaseDocument,
    container_node_id: usize,
    layout_children: &mut Vec<usize>,
    anonymous_block_id: &mut Option<usize>,
    hide_whitespace: bool,
    needs_wrap: impl Fn(NodeKind, DisplayOutside) -> bool,
) {
    doc.iter_children_and_pseudos_mut(container_node_id, |child_id, doc| {
        place_box_child(
            doc,
            container_node_id,
            child_id,
            layout_children,
            anonymous_block_id,
            hide_whitespace,
            &needs_wrap,
        );
    });

    // If anonymous block node only contains whitespace then delete it
    close_anonymous_block(doc, layout_children, anonymous_block_id);
}

fn block_is_only_whitespace(doc: &BaseDocument, node_id: usize) -> bool {
    for child_id in doc.nodes[node_id].children.iter().copied() {
        let child = &doc.nodes[child_id];
        if child
            .text_data()
            .is_none_or(|text_data| !text_data.content.chars().all(|c| c.is_ascii_whitespace()))
        {
            return false;
        }
    }

    true
}

/// Closes the open anonymous block, if any. A block that holds only whitespace generates no box and is freed.
///
/// The open block is always the last entry of `layout_children`: every push of a box first closes it. It is
/// removed by identity all the same, so a broken invariant cannot free a block that is still listed or drop another
/// child (F150).
fn close_anonymous_block(
    doc: &mut BaseDocument,
    layout_children: &mut Vec<usize>,
    anonymous_block_id: &mut Option<usize>,
) {
    if let Some(anon_id) = anonymous_block_id.take() {
        if block_is_only_whitespace(doc, anon_id) {
            let position = layout_children.iter().rposition(|&id| id == anon_id);
            debug_assert_eq!(
                position,
                layout_children.len().checked_sub(1),
                "the open anonymous block must be the last layout child"
            );
            if let Some(position) = position {
                layout_children.remove(position);
            }
            doc.nodes.remove(anon_id);
        }
    }
}

/// Places one child (or pseudo-element) of a block, flex or grid container: wraps it in the open anonymous block,
/// or closes that block and pushes the child. A `display: contents` child generates no box; its `::before`,
/// children and `::after` are placed the same way, as if they were children of the container (css-display-3 §2.5),
/// so every push goes through here and the open anonymous block stays the last layout child.
fn place_box_child(
    doc: &mut BaseDocument,
    container_node_id: usize,
    child_id: usize,
    layout_children: &mut Vec<usize>,
    anonymous_block_id: &mut Option<usize>,
    hide_whitespace: bool,
    needs_wrap: &dyn Fn(NodeKind, DisplayOutside) -> bool,
) {
    // Get node kind (text, element, comment, etc)
    let child_node_kind = doc.nodes[child_id].data.kind();

    // Get Display style. Default to inline because nodes without styles are probably text nodes
    let child_display = doc.nodes[child_id]
        .display_style()
        .unwrap_or(Display::inline());
    let display_inside = child_display.inside();

    let is_whitespace_node = match &doc.nodes[child_id].data {
        NodeData::Text(data) => data.content.chars().all(|c| c.is_ascii_whitespace()),
        _ => false,
    };

    // Skip comment nodes. Note that we do *not* skip `Display::None` nodes as they may need to be hidden.
    // Taffy knows how to deal with `Display::None` children.
    //
    // Also hide all-whitespace flexbox children as these should be ignored. Whitespace inside a run of text is
    // part of the run (css-flexbox-1 §4, css-grid-2 §6): the run can reach across the boundary of a
    // `display: contents` element (`a<s> </s>b`), so only whitespace that would start a run is hidden.
    if child_node_kind == NodeKind::Comment
        || (hide_whitespace && is_whitespace_node && anonymous_block_id.is_none())
    {
        // return;
    }
    // A `display: contents` node is replaced by its pseudo-elements and children
    else if display_inside == DisplayInside::Contents {
        flush_pseudo_elements(doc, child_id);
        doc.nodes[child_id].remove_damage(CONSTRUCT_BOX | CONSTRUCT_DESCENDENT | CONSTRUCT_FC);
        doc.iter_children_and_pseudos_mut(child_id, |grandchild_id, doc| {
            place_box_child(
                doc,
                container_node_id,
                grandchild_id,
                layout_children,
                anonymous_block_id,
                hide_whitespace,
                needs_wrap,
            );
        });
    } else {
        let display_outside = if doc.nodes[child_id].is_or_contains_block() {
            DisplayOutside::Block
        } else {
            child_display.outside()
        };

        // Push nodes that need wrapping into the current "anonymous block container".
        // If there is not an open one then we create one.
        if needs_wrap(child_node_kind, display_outside) {
            if anonymous_block_id.is_none() {
                let node_id = create_anonymous_block(doc, container_node_id);
                layout_children.push(node_id);
                *anonymous_block_id = Some(node_id);
            }

            doc.nodes[anonymous_block_id.unwrap()]
                .children
                .push(child_id);
        }
        // Else push the child directly (and close any open "anonymous block container")
        else {
            close_anonymous_block(doc, layout_children, anonymous_block_id);
            layout_children.push(child_id);
        }
    }
}

/// Creates an anonymous block box whose style inherits from `style_parent` (`ServoAnonymousBox`, so its computed
/// `display` is the initial `inline`) and whose `parent` and `layout_parent` are `style_parent`. The caller
/// pushes it into the layout children of its parent and fills its `children`.
pub(crate) fn create_anonymous_block(doc: &mut BaseDocument, style_parent: usize) -> usize {
    use style::selector_parser::PseudoElement;

    const NAME: QualName = QualName {
        prefix: None,
        ns: ns!(html),
        local: local_name!("div"),
    };
    let node_id = doc.create_node(NodeData::AnonymousBlock(ElementData::new(NAME, Vec::new())));

    // Set style data
    let parent_style = doc.nodes[style_parent].primary_styles().unwrap();
    let read_guard = doc.guard.read();
    let guards = StylesheetGuards::same(&read_guard);
    let style = doc.stylist.style_for_anonymous::<&Node>(
        &guards,
        &PseudoElement::ServoAnonymousBox,
        &parent_style,
    );
    let mut stylo_element_data = StyloElementData {
        damage: ALL_DAMAGE,
        ..Default::default()
    };
    drop(parent_style);

    stylo_element_data.styles.primary = Some(style);
    stylo_element_data.set_restyled();
    *doc.nodes[node_id].stylo_element_data.borrow_mut() = Some(stylo_element_data);
    doc.nodes[node_id].parent = Some(style_parent);
    doc.nodes[node_id].layout_parent.set(Some(style_parent));
    node_id
}

fn create_text_editor(doc: &mut BaseDocument, input_element_id: usize, is_multiline: bool) {
    let node = &mut doc.nodes[input_element_id];
    let parley_style = node
        .primary_styles()
        .as_ref()
        .map(|s| stylo_to_parley::style(node.id, s))
        .unwrap_or_default();

    let element = &mut node.data.downcast_element_mut().unwrap();
    if !matches!(element.special_data, SpecialElementData::TextInput(_)) {
        let mut text_input_data = TextInputData::new(is_multiline);
        let editor = &mut text_input_data.editor;

        editor.set_text(element.attr(local_name!("value")).unwrap_or(" "));
        editor.set_scale(doc.viewport.scale_f64() as f32);
        editor.set_width(None);

        let styles = editor.edit_styles();
        styles.insert(StyleProperty::FontSize(parley_style.font_size));
        styles.insert(StyleProperty::LineHeight(parley_style.line_height));
        styles.insert(StyleProperty::Brush(parley_style.brush));

        editor.refresh_layout(&mut doc.font_ctx.lock().unwrap(), &mut doc.layout_ctx);

        element.special_data = SpecialElementData::TextInput(text_input_data);
    }
}

fn create_checkbox_input(doc: &mut BaseDocument, input_element_id: usize) {
    let node = &mut doc.nodes[input_element_id];

    let element = &mut node.data.downcast_element_mut().unwrap();
    if !matches!(element.special_data, SpecialElementData::CheckboxInput(_)) {
        let checked = element.has_attr(local_name!("checked"));
        element.special_data = SpecialElementData::CheckboxInput(checked);
    }
}

/// Find and return the "layout_children" (inline boxes) for an inline layout
/// without actually constructing the layout. This allows us to defer the expensive
/// construction of the Parley layout (which invokes text shaping) to a paralell phase.
pub(crate) fn find_inline_layout_embedded_boxes(
    doc: &mut BaseDocument,
    inline_context_root_node_id: usize,
    layout_children: &mut Vec<usize>,
) {
    flush_inline_pseudos_recursive(doc, inline_context_root_node_id);

    let root_node = &doc.nodes[inline_context_root_node_id];
    if let Some(before_id) = root_node.before {
        find_inline_layout_embedded_boxes_recursive(
            &doc.nodes,
            inline_context_root_node_id,
            before_id,
            layout_children,
        );
    }
    for child_id in root_node.children.iter().copied() {
        find_inline_layout_embedded_boxes_recursive(
            &doc.nodes,
            inline_context_root_node_id,
            child_id,
            layout_children,
        );
    }
    if let Some(after_id) = root_node.after {
        find_inline_layout_embedded_boxes_recursive(
            &doc.nodes,
            inline_context_root_node_id,
            after_id,
            layout_children,
        );
    }

    fn flush_inline_pseudos_recursive(doc: &mut BaseDocument, node_id: usize) {
        doc.iter_children_mut(node_id, |child_id, doc| {
            flush_pseudo_elements(doc, child_id);
            // The pseudo-elements of an element are created where the walks below descend into it, whatever
            // `node_id` is: a nested inline element is reached through its parent, however deep (html-to-pdf F66i).
            if inline_walk_descends(&doc.nodes[child_id]) {
                flush_inline_pseudos_recursive(doc, child_id);
            }
        });
    }

    fn find_inline_layout_embedded_boxes_recursive(
        nodes: &Slab<Node>,
        parent_id: usize,
        node_id: usize,
        layout_children: &mut Vec<usize>,
    ) {
        let node = &nodes[node_id];

        // Set layout_parent for node.
        node.layout_parent.set(Some(parent_id));

        match &node.data {
            NodeData::Element(element_data) | NodeData::AnonymousBlock(element_data) => {
                // if the input type is hidden, hide it
                if *element_data.name.local == *"input" {
                    if let Some("hidden") = element_data.attr(local_name!("type")) {
                        return;
                    }
                }

                let display = node.display_style().unwrap_or(Display::inline());

                match (display.outside(), display.inside()) {
                    (DisplayOutside::None, DisplayInside::None) => {
                        node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                    }
                    (DisplayOutside::None, DisplayInside::Contents) => {
                        node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                        // The element generates no box; its pseudo-elements and children take its place.
                        let before = node.before;
                        let after = node.after;
                        for child_id in before
                            .into_iter()
                            .chain(node.children.iter().copied())
                            .chain(after)
                        {
                            find_inline_layout_embedded_boxes_recursive(
                                nodes,
                                parent_id,
                                child_id,
                                layout_children,
                            );
                        }
                    }
                    (DisplayOutside::Inline, DisplayInside::Flow) => {
                        let tag_name = &element_data.name.local;

                        if is_atomic_inline_tag(tag_name) {
                            layout_children.push(node_id);
                        } else if *tag_name == local_name!("br") {
                            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                        } else {
                            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);

                            if let Some(before_id) = node.before {
                                find_inline_layout_embedded_boxes_recursive(
                                    nodes,
                                    node_id,
                                    before_id,
                                    layout_children,
                                );
                            }
                            for child_id in node.children.iter().copied() {
                                find_inline_layout_embedded_boxes_recursive(
                                    nodes,
                                    node_id,
                                    child_id,
                                    layout_children,
                                );
                            }
                            if let Some(after_id) = node.after {
                                find_inline_layout_embedded_boxes_recursive(
                                    nodes,
                                    node_id,
                                    after_id,
                                    layout_children,
                                );
                            }
                        }
                    }
                    // Inline box
                    (_, _) => {
                        layout_children.push(node_id);
                    }
                };
            }
            NodeData::Comment | NodeData::Text(_) => {
                node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            }
            NodeData::Document => unreachable!(),
        }
    }
}

pub(crate) fn build_inline_layout_into(
    nodes: &Slab<Node>,
    layout_ctx: &mut LayoutContext<TextBrush>,
    font_ctx: &mut FontContext,
    text_layout: &mut TextLayout,
    scale: f32,
    inline_context_root_node_id: usize,
) {
    // Get the inline context's root node's text styles
    let root_node = &nodes[inline_context_root_node_id];
    let root_node_style = root_node.primary_styles().or_else(|| {
        root_node
            .parent
            .and_then(|parent_id| nodes[parent_id].primary_styles())
    });

    let parley_style = root_node_style
        .as_ref()
        .map(|s| stylo_to_parley::style(inline_context_root_node_id, s))
        .unwrap_or_default();

    let root_line_height = resolve_line_height(parley_style.line_height, parley_style.font_size);
    let line_height_normal = root_node_style
        .as_ref()
        .is_some_and(|s| stylo_to_parley::line_height_is_normal(s));

    // The inline formatting context is recorded as a list of ops first: white space is collapsed across the
    // boundaries of its elements (`whitespace.rs`) before Parley sees the text.
    let mut ops: Vec<InlineOp<'_>> = Vec::new();

    // `text-wrap-mode` of the root applies to the whole context (Parley cannot switch it per span).
    text_layout.nowrap = root_node_style
        .as_ref()
        .is_some_and(|s| stylo_to_parley::text_wrap_nowrap(s));

    // The whitespace collapsing mode of the root
    let collapse_mode = root_node_style
        .map(|s| s.get_inherited_text().white_space_collapse)
        .unwrap_or(stylo_to_parley::stylo::WhiteSpaceCollapse::Collapse);

    // Render position-inside list items
    if let Some(ListItemLayout {
        marker,
        position: ListItemLayoutPosition::Inside,
    }) = root_node
        .element_data()
        .and_then(|el| el.list_item_data.as_deref())
    {
        match marker {
            Marker::Char(char) => ops.push(InlineOp::Marker(format!("{char} "))),
            Marker::String(str) => ops.push(InlineOp::Marker(str.clone())),
        }
    };

    if let Some(before_id) = root_node.before {
        build_inline_layout_recursive(
            &mut ops,
            nodes,
            inline_context_root_node_id,
            before_id,
            collapse_mode,
            root_line_height,
        );
    }
    // The children of an anonymous block can come from inside `display: contents` elements (they generate no
    // box, so their children were placed in the block). Each element that matters for the text (see
    // `contents_needs_span`) stays open as a style span while its children are built, so the text keeps the
    // style and the brush of its own parent and not those of the block.
    let mut open: smallvec::SmallVec<[usize; 4]> = smallvec::SmallVec::new();
    for child_id in root_node.children.iter().copied() {
        let mut chain: smallvec::SmallVec<[usize; 4]> = smallvec::SmallVec::new();
        if root_node.data.kind() == NodeKind::AnonymousBlock {
            let mut parent = nodes[child_id].parent;
            while let Some(parent_id) = parent {
                if parent_id == root_node.id || Some(parent_id) == root_node.parent {
                    break;
                }
                let wrapper = &nodes[parent_id];
                if !wrapper
                    .display_style()
                    .is_some_and(|d| d.inside() == DisplayInside::Contents)
                {
                    break;
                }
                if contents_needs_span(nodes, wrapper) {
                    chain.push(parent_id);
                }
                parent = wrapper.parent;
            }
            // Outermost first.
            chain.reverse();
        }

        let common = open.iter().zip(&chain).take_while(|(a, b)| a == b).count();
        while open.len() > common {
            ops.push(InlineOp::PopSpan);
            open.pop();
        }
        for &wrapper_id in &chain[common..] {
            ops.push(InlineOp::PushSpan(inline_span_style(
                &nodes[wrapper_id],
                root_line_height,
            )));
            open.push(wrapper_id);
        }
        let collapse_mode = open
            .last()
            .and_then(|&id| nodes[id].primary_styles())
            .map(|s| s.get_inherited_text().white_space_collapse)
            .unwrap_or(collapse_mode);
        build_inline_layout_recursive(
            &mut ops,
            nodes,
            inline_context_root_node_id,
            child_id,
            collapse_mode,
            root_line_height,
        );
    }
    while open.pop().is_some() {
        ops.push(InlineOp::PopSpan);
    }
    if let Some(after_id) = root_node.after {
        build_inline_layout_recursive(
            &mut ops,
            nodes,
            inline_context_root_node_id,
            after_id,
            collapse_mode,
            root_line_height,
        );
    }

    // The strut of the block (CSS 2.1 section 10.8.1), only for a context with a baseline-aligned box: Parley keeps
    // it on the lines that hold one. Other contexts do not need it, and their lines are computed as before.
    //
    // Not inside an atomic inline (`inline-block`, `inline-flex`, `<button>` ...): the outer line puts the bottom of
    // such a box on its baseline instead of the baseline of its last line (html-to-pdf F183b), so the image would be
    // drawn the strut's descent too low. Until that is fixed those contexts keep the old line computation.
    let needs_strut = ops.iter().any(|op| {
        matches!(
            op,
            InlineOp::InlineBox {
                baseline_aligned: true,
                ..
            }
        )
    }) && !inside_atomic_inline(nodes, inline_context_root_node_id);
    let strut = if needs_strut {
        root_strut(
            font_ctx,
            &parley_style,
            root_line_height,
            line_height_normal,
            scale,
        )
    } else {
        None
    };

    // Create a parley tree builder
    let mut builder = layout_ctx.tree_builder(font_ctx, scale, true, &parley_style);
    // `text-transform: capitalize` looks at word boundaries across the whole inline formatting context.
    AT_WORD_START.with(|w| w.set(true));
    let mut ops = collapse_white_space(ops);
    let mut content_after_last_break = line_content_after_last_break(&ops);
    // A block that ends in a forced break can be one part of a box that Blitz lays out as a block because a block is among
    // its children (css-inline-3 §3: the box is split around it), or have a sibling that has a height in Chrome and none
    // here. What Chrome puts on the line after the break from them is not in `ops`: `trailing_break_context` reads it
    // from the tree.
    if !content_after_last_break && ends_in_forced_break(&ops) {
        let context = trailing_break_context(nodes, root_node);
        if context.end_edge {
            ops.push(InlineOp::EndEdge);
        }
        content_after_last_break = context.keep_line || line_content_after_last_break(&ops);
    }
    text_layout.content_after_last_break = content_after_last_break;
    replay(ops, &mut builder);

    text_layout.text = builder.build_into(&mut text_layout.layout);
    text_layout.layout.set_strut(strut);
    return;

    /// Whether a `display: contents` element needs a style span of its own in an inline layout. It does not when
    /// its text style, colour and white-space mode equal those of its parent, it draws no text decoration and it is
    /// not a link: its children then build exactly as they would without it, and a span would only add a style run
    /// (white space collapses across the boundary either way, `whitespace.rs`).
    fn contents_needs_span(nodes: &Slab<Node>, node: &Node) -> bool {
        let (Some(styles), Some(parent_styles)) = (
            node.primary_styles(),
            node.parent
                .and_then(|parent| nodes[parent].primary_styles()),
        ) else {
            return true;
        };
        if node
            .element_data()
            .is_some_and(|el| el.name.local == local_name!("a"))
            || !styles.clone_text_decoration_line().is_empty()
            || styles.clone_color() != parent_styles.clone_color()
            || styles.get_inherited_text().white_space_collapse
                != parent_styles.get_inherited_text().white_space_collapse
        {
            return true;
        }
        let mut own = stylo_to_parley::style(node.id, &styles);
        let mut parent = stylo_to_parley::style(node.id, &parent_styles);
        own.brush = parent.brush;
        parent.brush = own.brush;
        own != parent
    }

    /// The Parley style of the span of an inline element (or `display: contents` element).
    fn inline_span_style(
        node: &Node,
        root_line_height: f32,
    ) -> parley::TextStyle<'static, TextBrush> {
        let mut style = node
            .primary_styles()
            .map(|s| stylo_to_parley::style(node.id, &s))
            .unwrap_or_default();

        let font_size = style.font_size;

        // Floor the line-height of the span by the line-height of the inline context
        // See https://www.w3.org/TR/CSS21/visudet.html#line-height
        style.line_height = parley::LineHeight::Absolute(
            resolve_line_height(style.line_height, font_size).max(root_line_height),
        );
        style
    }

    fn build_inline_layout_recursive<'a>(
        ops: &mut Vec<InlineOp<'a>>,
        nodes: &'a Slab<Node>,
        parent_id: usize,
        node_id: usize,
        collapse_mode: stylo_to_parley::stylo::WhiteSpaceCollapse,
        root_line_height: f32,
    ) {
        let node = &nodes[node_id];

        // Set layout_parent for node.
        node.layout_parent.set(Some(parent_id));

        // Set whitespace collapsing mode
        let collapse_mode = node
            .primary_styles()
            .map(|s| s.get_inherited_text().white_space_collapse)
            .unwrap_or(collapse_mode);

        match &node.data {
            NodeData::Element(element_data) | NodeData::AnonymousBlock(element_data) => {
                // if the input type is hidden, hide it
                if *element_data.name.local == *"input" {
                    if let Some("hidden") = element_data.attr(local_name!("type")) {
                        return;
                    }
                }

                let display = node.display_style().unwrap_or(Display::inline());

                match (display.outside(), display.inside()) {
                    (DisplayOutside::None, DisplayInside::None) => {
                        // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                    }
                    (DisplayOutside::None, DisplayInside::Contents) => {
                        // The element generates no box, but its text style is inherited by its children and
                        // its brush identifies them (colour, decoration, link) in the paint.
                        let span = contents_needs_span(nodes, node);
                        if span {
                            ops.push(InlineOp::PushSpan(inline_span_style(
                                node,
                                root_line_height,
                            )));
                        }
                        for child_id in node
                            .before
                            .into_iter()
                            .chain(node.children.iter().copied())
                            .chain(node.after)
                        {
                            build_inline_layout_recursive(
                                ops,
                                nodes,
                                parent_id,
                                child_id,
                                collapse_mode,
                                root_line_height,
                            );
                        }
                        if span {
                            ops.push(InlineOp::PopSpan);
                        }
                    }
                    (DisplayOutside::Inline, DisplayInside::Flow) => {
                        let tag_name = &element_data.name.local;

                        if is_atomic_inline_tag(tag_name) {
                            ops.push(InlineOp::InlineBox {
                                id: node_id as u64,
                                baseline_aligned: strut_box(node),
                            });
                        } else if *tag_name == local_name!("br") {
                            // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                            // TODO: update span id for br spans
                            ops.push(InlineOp::ForcedBreak);
                        } else {
                            // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                            let style = inline_span_style(node, root_line_height);

                            let (start_edge, end_edge) = inline_axis_edges(node);
                            // `<wbr>` and the form controls Chrome lays out as atomic inlines (Blitz has no box for
                            // them) are content of the line they are on.
                            ops.push(InlineOp::BoxStart {
                                edge: start_edge
                                    || *tag_name == local_name!("wbr")
                                    || is_unlaid_form_control(tag_name),
                            });
                            ops.push(InlineOp::PushSpan(style));

                            if let Some(before_id) = node.before {
                                build_inline_layout_recursive(
                                    ops,
                                    nodes,
                                    node_id,
                                    before_id,
                                    collapse_mode,
                                    root_line_height,
                                );
                            }

                            for child_id in node.children.iter().copied() {
                                build_inline_layout_recursive(
                                    ops,
                                    nodes,
                                    node_id,
                                    child_id,
                                    collapse_mode,
                                    root_line_height,
                                );
                            }
                            if let Some(after_id) = node.after {
                                build_inline_layout_recursive(
                                    ops,
                                    nodes,
                                    node_id,
                                    after_id,
                                    collapse_mode,
                                    root_line_height,
                                );
                            }

                            if end_edge {
                                ops.push(InlineOp::EndEdge);
                            }
                            ops.push(InlineOp::PopSpan);
                        }
                    }
                    // Inline box
                    (_, _) => {
                        ops.push(InlineOp::InlineBox {
                            id: node_id as u64,
                            baseline_aligned: strut_box(node),
                        });
                    }
                };
            }
            NodeData::Text(data) => {
                // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                // dbg!(&data.content);
                // Text nodes have no style of their own: `text-transform` comes from the parent element.
                let parent_style = node
                    .parent
                    .and_then(|parent| nodes[parent].primary_styles());
                let case = parent_style
                    .as_ref()
                    .map(|style| style.clone_text_transform().case());
                // The text goes in as it is. `collapse_white_space` collapses it with its neighbours (across the
                // boundaries of elements) and splits `white-space: pre-line` text at its newlines, and `replay`
                // applies `text-transform` to what is left.
                ops.push(InlineOp::Text {
                    text: std::borrow::Cow::Borrowed(data.content.as_str()),
                    collapse: collapse_mode,
                    case,
                });
            }
            NodeData::Comment => {
                // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            }
            NodeData::Document => unreachable!(),
        }
    }
}

/// Whether the inline formatting context rooted at `root_id` sits inside an atomic inline-level box: the root or one
/// of its ancestors has an inline-level outside display with an inside other than `flow` (`inline-block`,
/// `inline-flex`, `inline-grid`, `inline-table`), or is an inline-level `<button>` / `<textarea>` / `<input>`, which
/// the inline layout above treats as an atomic inline whatever their `display` inside is.
fn inside_atomic_inline(nodes: &Slab<Node>, root_id: usize) -> bool {
    let mut current = Some(root_id);
    while let Some(id) = current {
        let node = &nodes[id];
        if let Some(display) = node.display_style() {
            if display.outside() == DisplayOutside::Inline
                && (display.inside() != DisplayInside::Flow
                    || node.element_data().is_some_and(|el| {
                        el.name.local == local_name!("button")
                            || el.name.local == local_name!("textarea")
                            || el.name.local == local_name!("input")
                    }))
            {
                return true;
            }
        }
        current = node.parent;
    }
    false
}

/// The elements the inline walks hand to the layout as one atomic inline box whatever their `display` inside is: replaced
/// elements and form controls.
fn is_atomic_inline_tag(tag_name: &markup5ever::LocalName) -> bool {
    *tag_name == local_name!("img")
        || *tag_name == local_name!("svg")
        || *tag_name == local_name!("input")
        || *tag_name == local_name!("textarea")
        || *tag_name == local_name!("button")
}

/// Whether the inline walks (`flush_inline_pseudos_recursive`, `find_inline_layout_embedded_boxes_recursive`,
/// `build_inline_layout_recursive`) descend into `node`'s pseudo-elements and children: an inline-level `flow` element that is
/// not laid out as an atomic inline and is not `<br>`, or a `display: contents` element. One predicate for all three, so that
/// the pseudo-elements that are created and the ones that are read cannot diverge again (html-to-pdf F66i, F159(d)).
fn inline_walk_descends(node: &Node) -> bool {
    let (NodeData::Element(element_data) | NodeData::AnonymousBlock(element_data)) = &node.data
    else {
        return false;
    };
    let display = node.display_style().unwrap_or(Display::inline());
    match (display.outside(), display.inside()) {
        (DisplayOutside::None, DisplayInside::Contents) => true,
        (DisplayOutside::Inline, DisplayInside::Flow) => {
            let tag_name = &element_data.name.local;
            !is_atomic_inline_tag(tag_name) && *tag_name != local_name!("br")
        }
        (_, _) => false,
    }
}

/// `<select>`, `<meter>` and `<progress>`: atomic inlines with a height in Chrome (html-to-pdf F66f round 3). Blitz has no
/// display rule for them, so they are empty inline elements, which leaves the line after a forced break empty.
fn is_unlaid_form_control(tag_name: &markup5ever::LocalName) -> bool {
    *tag_name == local_name!("select")
        || *tag_name == local_name!("meter")
        || *tag_name == local_name!("progress")
}

/// Whether the context ends in a forced break: a `<br>` or a preserved newline, with nothing but style ops after it.
fn ends_in_forced_break(ops: &[InlineOp<'_>]) -> bool {
    ops.iter().any(|op| match op {
        InlineOp::ForcedBreak => true,
        InlineOp::Text { text, .. } => text.ends_with(['\n', '\r']),
        _ => false,
    })
}

/// What the line after the trailing forced break of a block takes from its surroundings.
#[derive(Default)]
struct TrailingBreakContext {
    /// The line is kept whatever the block holds: Chrome has a line there that Blitz does not lay out.
    keep_line: bool,
    /// The end edge of an open box lands on the last line of the block (the box ends with it), so it counts as content
    /// when a box has started after the break (`EndEdge`).
    end_edge: bool,
}

/// What the siblings on one side of a node hold, as far as the line after a forced break is concerned.
#[derive(Default)]
struct SiblingSide {
    /// Text, a `<br>` or an atomic inline: a line of its own, which carries the edges of the boxes that end around it.
    inline_content: bool,
    /// A block-level box in flow, a float excluded.
    block: bool,
    /// A float: it takes no line of its own, but it is not text either.
    float: bool,
    /// A box that has a height in Chrome and none here, see [`unlaid_height`].
    unlaid_height: bool,
}

/// Whether `node` holds nothing but white space and comments.
fn is_blank(nodes: &Slab<Node>, node: &Node) -> bool {
    node.before.is_none()
        && node.after.is_none()
        && node.children.iter().all(|&id| match &nodes[id].data {
            NodeData::Text(text) => text.content.chars().all(|c| c.is_ascii_whitespace()),
            NodeData::Comment => true,
            _ => false,
        })
}

/// Whether `node` is an empty box that has a height in Chrome and none here: `<details>` (Chrome shows the default
/// summary), `display: list-item` with an outside marker (a line for the marker). Blitz lays out neither, so the line
/// after a forced break before or after it stands in for the line. With content of its own the box has a height here too.
fn unlaid_height(nodes: &Slab<Node>, node: &Node) -> bool {
    let Some(element) = node.element_data() else {
        return false;
    };
    if !is_blank(nodes, node) {
        return false;
    }
    element.name.local == local_name!("details")
        || (node.display_style().is_some_and(|d| d.is_list_item())
            && matches!(
                element.list_item_data.as_deref(),
                Some(ListItemLayout {
                    position: ListItemLayoutPosition::Outside(_),
                    ..
                })
            ))
}

fn scan_siblings(nodes: &Slab<Node>, ids: &[usize], side: &mut SiblingSide) {
    use style::computed_values::{float::T as Float, position::T as Position};

    for &id in ids {
        let node = &nodes[id];
        match &node.data {
            NodeData::Text(text) => {
                // A no-break space is not white space here (it is not ASCII), a collapsible space is nothing.
                side.inline_content |= text.content.chars().any(|c| !c.is_ascii_whitespace());
            }
            NodeData::Element(element) | NodeData::AnonymousBlock(element) => {
                let (Some(display), Some(styles)) = (node.display_style(), node.primary_styles())
                else {
                    continue;
                };
                if display.outside() == DisplayOutside::None
                    && display.inside() != DisplayInside::Contents
                {
                    continue;
                }
                if display.inside() == DisplayInside::Contents {
                    scan_siblings(nodes, &node.children, side);
                    continue;
                }
                if matches!(
                    styles.clone_position(),
                    Position::Absolute | Position::Fixed
                ) {
                    continue;
                }
                let tag = &element.name.local;
                if node.is_or_contains_block() {
                    if styles.clone_float() == Float::None {
                        side.block = true;
                    } else if is_blank(nodes, node) {
                        side.float = true;
                    } else {
                        // A float with content is a line here: it adds its height to the box that holds it.
                        side.inline_content = true;
                    }
                    side.unlaid_height |= unlaid_height(nodes, node);
                } else if *tag == local_name!("br")
                    || *tag == local_name!("img")
                    || *tag == local_name!("svg")
                    || *tag == local_name!("input")
                    || *tag == local_name!("textarea")
                    || *tag == local_name!("button")
                    || display.inside() != DisplayInside::Flow
                    || node.before.is_some()
                    || node.after.is_some()
                {
                    side.inline_content = true;
                } else if is_unlaid_form_control(tag) {
                    // With options (or fallback text) it has text here, which is its line.
                    side.inline_content = true;
                    side.unlaid_height |= is_blank(nodes, node);
                } else {
                    scan_siblings(nodes, &node.children, side);
                }
            }
            NodeData::Comment | NodeData::Document => {}
        }
    }
}

/// The position of the child of `parent` that holds `node` (or is it), if `node` is in its subtree.
fn child_holding(nodes: &Slab<Node>, parent: usize, mut node: usize) -> Option<usize> {
    loop {
        let up = nodes[node].parent?;
        if up == parent {
            return nodes[parent].children.iter().position(|&id| id == node);
        }
        node = up;
    }
}

/// Reads the surroundings of the block `root` (an anonymous block, mostly) for the line after its trailing forced break
/// (html-to-pdf F66f, round 3). Chrome and Blitz differ in two ways:
///
/// - A box with a block in it (`is_or_contains_block`, a float is one) is laid out as a block here, with its text in
///   anonymous blocks. Chrome splits the box around the block (css-inline-3 §3): the part before keeps the start
///   edge, the part after the end edge, and a part with nothing but an edge is a line (`<span style="padding-right:4px">
///   a<br><div>blk</div></span>` is a, blk and an empty line). The line is not there in Blitz, so the line after the
///   break stands in for it: `keep_line`. When the break is in the last part, the end edge lands on its last line
///   like it does inline (`end_edge`). Both walk up through the boxes that are laid out as blocks only for that reason
///   (inline outside, flow inside); an edge counts once, at the level where nothing but blocks (or floats, for the
///   end) lies between the anonymous block and it. A float takes no line, so it opens none before the text.
/// - A form control, an empty `<details>` or `display: list-item` has a height in Chrome and none here; with one of
///   those next to the block (in the same container) the line is kept.
fn trailing_break_context(nodes: &Slab<Node>, root: &Node) -> TrailingBreakContext {
    let mut context = TrailingBreakContext::default();
    // An anonymous block stands for a run of its parent's children, which may sit inside `display: contents` elements; any
    // other root is one child of its parent.
    let (first, last) = if root.data.kind() == NodeKind::AnonymousBlock {
        let (Some(&first), Some(&last)) = (root.children.first(), root.children.last()) else {
            return context;
        };
        (first, last)
    } else {
        // Only a block in flow has siblings in the sense of the text above: not an atomic inline, a float or an
        // out-of-flow box.
        let in_flow_block = root
            .display_style()
            .is_some_and(|d| d.outside() != DisplayOutside::Inline)
            && root.primary_styles().is_some_and(|styles| {
                use style::computed_values::{float::T as Float, position::T as Position};
                styles.clone_float() == Float::None
                    && !matches!(
                        styles.clone_position(),
                        Position::Absolute | Position::Fixed
                    )
            });
        if !in_flow_block {
            return context;
        }
        (root.id, root.id)
    };
    let (mut first, mut last) = (first, last);
    let mut parent = root.parent;
    let mut level = 0;
    // A block or a float seen on the way: the edges above are on a line of their own. `start_done` / `end_done`: text or an
    // atomic inline lies between the anonymous block and the edges above, they are on its line.
    let (mut block_before, mut block_after) = (false, false);
    let (mut start_done, mut end_done) = (false, false);
    while let Some(parent_id) = parent {
        let container = &nodes[parent_id];
        let (Some(first_at), Some(last_at)) = (
            child_holding(nodes, parent_id, first),
            child_holding(nodes, parent_id, last),
        ) else {
            break;
        };
        let (mut before, mut after) = (SiblingSide::default(), SiblingSide::default());
        scan_siblings(nodes, &container.children[..first_at], &mut before);
        scan_siblings(nodes, &container.children[last_at + 1..], &mut after);
        before.inline_content |= container.before.is_some();
        after.inline_content |= container.after.is_some();

        let display = container.display_style();
        let block_container = display.is_none_or(|d| {
            matches!(
                d.inside(),
                DisplayInside::Flow | DisplayInside::FlowRoot | DisplayInside::TableCell
            )
        });
        if level == 0 && block_container && (before.unlaid_height || after.unlaid_height) {
            context.keep_line = true;
        }

        let blockified_inline = matches!(container.data, NodeData::Element(_))
            && display.is_some_and(|d| {
                d.outside() == DisplayOutside::Inline && d.inside() == DisplayInside::Flow
            });
        if !blockified_inline {
            break;
        }
        let (start, end) = inline_axis_edges(container);
        if !start_done {
            if before.inline_content {
                start_done = true;
            } else {
                block_before |= before.block;
                if start && block_before {
                    context.keep_line = true;
                }
            }
        }
        if !end_done {
            if after.inline_content {
                end_done = true;
            } else {
                block_after |= after.block || after.float;
                if end {
                    if block_after {
                        context.keep_line = true;
                    } else {
                        context.end_edge = true;
                    }
                }
            }
        }
        if start_done && end_done {
            break;
        }
        (first, last) = (parent_id, parent_id);
        parent = container.parent;
        level += 1;
    }
    context
}

/// Whether an inline element has a non-zero margin, padding or border on its inline-start side and on its inline-end side
/// (CSS 2.1 §9.4.2: the reason for a line box that holds nothing else not to be zero-height). Vertical ones do not count,
/// nor an `auto` margin (zero on an inline box); a percentage or `calc()` counts as non-zero. The start side is the left
/// one for `direction: ltr` and the right one for `rtl`, the direction being that of the box itself (Chrome).
fn inline_axis_edges(node: &Node) -> (bool, bool) {
    fn non_zero(len: taffy::CompactLength) -> bool {
        match len.tag() {
            taffy::CompactLength::LENGTH_TAG | taffy::CompactLength::PERCENT_TAG => {
                len.value() != 0.0
            }
            taffy::CompactLength::AUTO_TAG => false,
            _ => true,
        }
    }
    let Some(styles) = node.primary_styles() else {
        return (false, false);
    };
    let (margin, padding, border) = (
        styles.get_margin(),
        styles.get_padding(),
        styles.get_border(),
    );
    let length = |px: f32| taffy::LengthPercentage::length(px).into_raw();
    let left = [
        stylo_taffy::convert::margin(&margin.margin_left).into_raw(),
        stylo_taffy::convert::length_percentage(&padding.padding_left.0).into_raw(),
        length(border.border_left_width.to_f32_px()),
    ]
    .into_iter()
    .any(non_zero);
    let right = [
        stylo_taffy::convert::margin(&margin.margin_right).into_raw(),
        stylo_taffy::convert::length_percentage(&padding.padding_right.0).into_raw(),
        length(border.border_right_width.to_f32_px()),
    ]
    .into_iter()
    .any(non_zero);
    if styles.writing_mode.is_bidi_ltr() {
        (left, right)
    } else {
        (right, left)
    }
}

/// Whether `node` is a replaced element that Parley lays out as a baseline-aligned inline box (CSS 2.1 section
/// 10.8.1: its bottom margin edge sits on the baseline, the strut of the block stays below it): an `<img>` or an
/// inline `<svg>` with an inline-level `display` (an out-of-flow or floated box is blockified, so `abspos` and
/// `float` are excluded by it) and `vertical-align: baseline`. The other values (`top`, `middle`, a length ...)
/// and other atomic inlines (inline-blocks, form controls) keep the old line computation.
fn strut_box(node: &Node) -> bool {
    use style::values::generics::box_::{VerticalAlign, VerticalAlignKeyword};

    let Some(element) = node.element_data() else {
        return false;
    };
    if !(element.name.local == local_name!("img") || element.name.local == local_name!("svg")) {
        return false;
    }
    node.primary_styles().is_some_and(|styles| {
        styles.clone_display().outside() == DisplayOutside::Inline
            && matches!(
                styles.clone_vertical_align(),
                VerticalAlign::Keyword(VerticalAlignKeyword::Baseline)
            )
    })
}

/// The strut of an inline formatting context's root (CSS 2.1 section 10.8.1, css-inline-3 section 2): the first
/// available font of the root's `font-family` (css-fonts-4 section 5) at the root's `font-size`, with the root's
/// line height. The font is found like Parley finds it for text, and the metrics come from the same source as the
/// ones of a text run (`skrifa` at the font size; no variations). `line-height: normal` is Chrome's: the ascent, the
/// descent and the line gap each rounded to whole pixels, then added. All values are in layout units (`scale`).
///
/// Returns `None` when no font of the stack is available (the lines keep their old height then).
fn root_strut(
    font_ctx: &mut FontContext,
    style: &parley::TextStyle<'static, TextBrush>,
    line_height: f32,
    line_height_normal: bool,
    scale: f32,
) -> Option<parley::Strut> {
    use parley::fontique::{Attributes, QueryFamily, QueryStatus};
    use parley::{FontFamily, FontStack};

    let families: Vec<FontFamily<'_>> = match &style.font_stack {
        FontStack::Source(source) => FontFamily::parse_list(source).collect(),
        FontStack::Single(family) => vec![family.clone()],
        FontStack::List(list) => list.to_vec(),
    };
    let mut query = font_ctx.collection.query(&mut font_ctx.source_cache);
    query.set_families(families.iter().map(|family| match family {
        FontFamily::Named(name) => QueryFamily::Named(name),
        FontFamily::Generic(generic) => QueryFamily::Generic(*generic),
    }));
    query.set_attributes(Attributes {
        width: style.font_width,
        weight: style.font_weight,
        style: style.font_style,
    });
    let mut found = None;
    query.matches_with(|font| {
        found = Some((font.blob.clone(), font.index));
        QueryStatus::Stop
    });
    let (blob, index) = found?;

    let font = skrifa::FontRef::from_index(blob.as_ref(), index).ok()?;
    let font_size = style.font_size * scale;
    let metrics = skrifa::metrics::Metrics::new(
        &font,
        skrifa::prelude::Size::new(font_size),
        skrifa::instance::LocationRef::default(),
    );
    let (ascent, descent) = (metrics.ascent, -metrics.descent);
    let line_height = if line_height_normal {
        ascent.round() + descent.round() + metrics.leading.round()
    } else {
        line_height * scale
    };
    Some(parley::Strut {
        ascent,
        descent,
        line_height,
    })
}

/// The combining diacritical mark blocks (enough for decomposed Latin, Greek and Cyrillic text).
fn is_combining_mark(c: char) -> bool {
    matches!(c,
        '\u{0300}'..='\u{036F}'
            | '\u{1AB0}'..='\u{1AFF}'
            | '\u{1DC0}'..='\u{1DFF}'
            | '\u{20D0}'..='\u{20FF}'
            | '\u{FE20}'..='\u{FE2F}')
}

#[cfg(all(test, feature = "svg"))]
mod svg_log_tests {
    use std::sync::Mutex;

    use crate::{Attribute, BaseDocument, DocumentConfig, QualName, ns};

    static RECORDS: Mutex<Vec<(log::Level, String, String)>> = Mutex::new(Vec::new());

    struct Capture;

    impl log::Log for Capture {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }
        fn log(&self, r: &log::Record<'_>) {
            RECORDS
                .lock()
                .unwrap()
                .push((r.level(), r.target().to_string(), r.args().to_string()));
        }
        fn flush(&self) {}
    }

    fn html_el(name: &str) -> QualName {
        QualName::new(None, ns!(html), name.into())
    }

    /// An inline `<svg>` that usvg rejects is dropped with one `warn` record (the markup only at `debug`), and
    /// nothing is written to stdout. One test only: the logger is global to the process.
    #[test]
    fn a_rejected_inline_svg_is_a_warning() {
        log::set_logger(&Capture).unwrap();
        log::set_max_level(log::LevelFilter::Debug);

        let mut doc = BaseDocument::new(DocumentConfig::default());
        {
            let mut m = doc.mutate();
            let html = m.create_element(html_el("html"), vec![]);
            let body = m.create_element(html_el("body"), vec![]);
            let attr = |name: &str, value: &str| Attribute {
                name: QualName::new(None, ns!(), name.into()),
                value: value.to_string(),
            };
            // A zero size is `usvg::Error::InvalidSize`.
            let svg = m.create_element(
                QualName::new(None, ns!(svg), "svg".into()),
                vec![attr("width", "0"), attr("height", "0")],
            );
            m.append_children(0, &[html]);
            m.append_children(html, &[body]);
            m.append_children(body, &[svg]);
        }
        doc.resolve(0.0);

        let records = std::mem::take(&mut *RECORDS.lock().unwrap());
        let warns: Vec<_> = records
            .iter()
            .filter(|(l, ..)| *l == log::Level::Warn)
            .collect();
        assert_eq!(warns.len(), 1, "{records:?}");
        assert!(warns[0].2.contains("inline SVG"), "{warns:?}");
        assert!(warns[0].1.starts_with("blitz_dom"), "{warns:?}");
        assert!(
            records
                .iter()
                .any(|(l, _, m)| *l == log::Level::Debug && m.contains("<svg")),
            "{records:?}"
        );
    }
}

#[cfg(test)]
mod contents_tests {
    use std::collections::HashMap;

    use crate::{Attribute, BaseDocument, DocumentConfig, QualName, ns};

    /// A tiny tree description: an element with an `id`, a `style` attribute and children, or a text node with a
    /// name.
    enum N {
        E(&'static str, &'static str, &'static str, Vec<N>),
        T(&'static str, &'static str),
    }
    use N::{E, T};

    const C: &str = "display:contents";

    fn build(
        m: &mut crate::DocumentMutator<'_>,
        n: &N,
        ids: &mut HashMap<&'static str, usize>,
    ) -> usize {
        match n {
            T(name, text) => {
                let id = m.create_text_node(text);
                ids.insert(name, id);
                id
            }
            E(tag, name, style, kids) => {
                let attr = |n: &str, v: &str| Attribute {
                    name: QualName::new(None, ns!(), n.into()),
                    value: v.to_string(),
                };
                let mut attrs = vec![attr("id", name)];
                if !style.is_empty() {
                    attrs.push(attr("style", style));
                }
                let id = m.create_element(QualName::new(None, ns!(html), (*tag).into()), attrs);
                ids.insert(name, id);
                let kids: Vec<usize> = kids.iter().map(|k| build(m, k, ids)).collect();
                m.append_children(id, &kids);
                id
            }
        }
    }

    /// Resolves `<html><body>{body}</body></html>` and returns the document and the ids by name.
    fn resolved(body: Vec<N>) -> (BaseDocument, HashMap<&'static str, usize>) {
        let mut doc = BaseDocument::new(DocumentConfig::default());
        let mut ids = HashMap::new();
        {
            let mut m = doc.mutate();
            let html = build(
                &mut m,
                &E("html", "html", "", vec![E("body", "body", "", body)]),
                &mut ids,
            );
            m.append_children(0, &[html]);
        }
        doc.resolve(0.0);
        (doc, ids)
    }

    fn layout_children(doc: &BaseDocument, id: usize) -> Vec<usize> {
        doc.nodes[id]
            .layout_children
            .borrow()
            .clone()
            .unwrap_or_default()
    }

    /// Every layout child, recursively, is a live node and has its parent as `layout_parent`.
    fn assert_live(doc: &BaseDocument, id: usize) {
        for child in layout_children(doc, id) {
            assert!(
                doc.nodes.contains(child),
                "node {id} lists freed node {child}"
            );
            assert_eq!(doc.nodes[child].layout_parent.get(), Some(id));
            assert_live(doc, child);
        }
    }

    fn anon_children(doc: &BaseDocument, id: usize) -> Vec<usize> {
        doc.nodes[id].children.clone()
    }

    fn p(name: &'static str, text_name: &'static str, text: &'static str) -> N {
        E("p", name, "", vec![T(text_name, text)])
    }

    #[test]
    fn c1_blocks_with_inline_content_are_placed() {
        let (doc, i) = resolved(vec![
            E(
                "div",
                "w",
                C,
                vec![p("p1", "t1", "Faktura"), p("p2", "t2", "Celkem")],
            ),
            p("end", "t3", "end"),
        ]);
        assert_eq!(
            layout_children(&doc, i["body"]),
            vec![i["p1"], i["p2"], i["end"]]
        );
        for name in ["p1", "p2", "end"] {
            assert_eq!(
                layout_children(&doc, i[name]),
                Vec::<usize>::new(),
                "{name}"
            );
            assert!(
                doc.nodes[i[name]].flags.is_inline_root(),
                "{name} is an inline root"
            );
        }
        assert_live(&doc, 0);
    }

    #[test]
    fn c2_contents_after_a_block() {
        let (doc, i) = resolved(vec![
            p("start", "t0", "start"),
            E(
                "div",
                "w",
                C,
                vec![p("p1", "t1", "Faktura"), p("p2", "t2", "Celkem")],
            ),
        ]);
        assert_eq!(
            layout_children(&doc, i["body"]),
            vec![i["start"], i["p1"], i["p2"]]
        );
        assert_live(&doc, 0);
    }

    #[test]
    fn c3_inline_content_of_a_wrapper_joins_one_anonymous_block() {
        let (doc, i) = resolved(vec![E(
            "div",
            "o",
            "",
            vec![
                E(
                    "div",
                    "w",
                    C,
                    vec![
                        E("span", "s", "", vec![T("t1", "Faktura")]),
                        T("sp", " "),
                        E("b", "b", "", vec![T("t2", "Celkem")]),
                    ],
                ),
                p("y", "t3", "y"),
            ],
        )]);
        let lc = layout_children(&doc, i["o"]);
        assert_eq!(lc.len(), 2, "{lc:?}");
        assert_eq!(lc[1], i["y"]);
        assert_eq!(anon_children(&doc, lc[0]), vec![i["s"], i["sp"], i["b"]]);
        assert_live(&doc, 0);
    }

    #[test]
    fn c4_a_run_of_inline_content_crosses_the_boundary() {
        let (doc, i) = resolved(vec![E(
            "div",
            "o",
            "",
            vec![
                T("a", "a "),
                E(
                    "span",
                    "s",
                    C,
                    vec![T("b", "b "), E("i", "i", "", vec![T("c", "c")])],
                ),
                T("d", " d"),
                p("e", "te", "e"),
            ],
        )]);
        let lc = layout_children(&doc, i["o"]);
        assert_eq!(lc.len(), 2, "{lc:?}");
        assert_eq!(lc[1], i["e"]);
        assert_eq!(
            anon_children(&doc, lc[0]),
            vec![i["a"], i["b"], i["i"], i["d"]]
        );
        assert_live(&doc, 0);
    }

    #[test]
    fn c7_flex_items_and_text_through_a_wrapper() {
        let (doc, i) = resolved(vec![E(
            "div",
            "f",
            "display:flex",
            vec![
                E(
                    "div",
                    "w",
                    C,
                    vec![
                        E("div", "i1", "", vec![T("t1", "1")]),
                        E("div", "i2", "", vec![T("t2", "2")]),
                    ],
                ),
                T("tx", "text"),
                E("div", "i3", "", vec![T("t3", "3")]),
            ],
        )]);
        let lc = layout_children(&doc, i["f"]);
        assert_eq!(lc.len(), 4, "{lc:?}");
        assert_eq!((lc[0], lc[1], lc[3]), (i["i1"], i["i2"], i["i3"]));
        assert_eq!(anon_children(&doc, lc[2]), vec![i["tx"]]);
        assert_live(&doc, 0);
    }

    /// Whitespace between two texts is part of their run, also when the texts are on either side of a contents
    /// element; whitespace that would start a run is hidden in a flex container.
    #[test]
    fn flex_whitespace_inside_a_text_run_is_kept() {
        let (doc, i) = resolved(vec![E(
            "div",
            "f",
            "display:flex",
            vec![
                T("ws0", " "),
                T("a", "a"),
                E("span", "w", C, vec![T("sp", " ")]),
                T("b", "b"),
                E("div", "d", "", vec![T("t", "d")]),
                E("span", "w2", C, vec![T("ws1", " ")]),
            ],
        )]);
        let lc = layout_children(&doc, i["f"]);
        assert_eq!(lc.len(), 2, "{lc:?}");
        assert_eq!(anon_children(&doc, lc[0]), vec![i["a"], i["sp"], i["b"]]);
        assert_eq!(lc[1], i["d"]);
        assert_live(&doc, 0);
    }

    #[test]
    fn c10_list_items_inside_a_wrapper_are_numbered_by_the_list() {
        let (doc, i) = resolved(vec![E(
            "ol",
            "ol",
            "",
            vec![
                E(
                    "div",
                    "w",
                    C,
                    vec![
                        E("li", "a", "", vec![T("t1", "a")]),
                        E("li", "b", "", vec![T("t2", "b")]),
                    ],
                ),
                E("li", "c", "", vec![T("t3", "c")]),
            ],
        )]);
        assert_eq!(layout_children(&doc, i["ol"]), vec![i["a"], i["b"], i["c"]]);
        for name in ["a", "b", "c"] {
            let item = doc.nodes[i[name]]
                .element_data()
                .unwrap()
                .list_item_data
                .is_some();
            assert!(item, "{name} has a marker");
        }
        assert_live(&doc, 0);
    }

    /// F150: a whitespace-only anonymous block followed by a contents element whose subtree pushes children.
    #[test]
    fn f150_whitespace_block_before_a_contents_subtree() {
        let nested = || {
            E(
                "div",
                "w",
                C,
                vec![E("div", "d", "", vec![E("p", "q", "", vec![])])],
            )
        };
        // The end-of-loop cleanup.
        let (doc, i) = resolved(vec![E("p", "p0", "", vec![]), T("ws", " "), nested()]);
        assert_eq!(layout_children(&doc, i["body"]), vec![i["p0"], i["d"]]);
        assert_live(&doc, 0);
        // The cleanup before a block sibling.
        let (doc, i) = resolved(vec![E(
            "div",
            "o",
            "",
            vec![T("ws", " "), nested(), E("p", "p1", "", vec![])],
        )]);
        assert_eq!(layout_children(&doc, i["o"]), vec![i["d"], i["p1"]]);
        assert_live(&doc, 0);
        // An orphan table inside a wrapper.
        let (doc, i) = resolved(vec![
            E("p", "p0", "", vec![]),
            T("ws", " "),
            E("table", "t", C, vec![E("tr", "r", "", vec![])]),
        ]);
        assert_live(&doc, 0);
        assert!(doc.nodes.contains(i["r"]));
    }

    fn inline_text(doc: &BaseDocument, id: usize) -> String {
        doc.nodes[id]
            .element_data()
            .and_then(|el| el.inline_layout_data.as_ref())
            .map(|layout| layout.text.clone())
            .unwrap_or_default()
    }

    /// The ids of the brushes of the glyph runs of an inline layout, in order.
    fn brush_ids(doc: &BaseDocument, id: usize) -> Vec<usize> {
        let layout = doc.nodes[id]
            .element_data()
            .and_then(|el| el.inline_layout_data.as_ref())
            .unwrap();
        let mut ids = Vec::new();
        for line in layout.layout.lines() {
            for item in line.items() {
                if let parley::PositionedLayoutItem::GlyphRun(run) = item {
                    ids.push(run.style().brush.id);
                }
            }
        }
        ids
    }

    /// A wrapper that changes nothing for the text adds no span, so the whitespace around it collapses as if it
    /// were not there (a span would trim the space at its end).
    #[test]
    fn a_transparent_wrapper_does_not_change_the_text() {
        let (doc, i) = resolved(vec![E(
            "div",
            "o",
            "",
            vec![
                T("a", "a "),
                E(
                    "span",
                    "s",
                    C,
                    vec![T("b", "b "), E("i", "i", "", vec![T("c", "c")])],
                ),
                T("d", " d"),
                E("p", "e", "", vec![T("te", "e")]),
            ],
        )]);
        let anon = layout_children(&doc, i["o"])[0];
        assert_eq!(inline_text(&doc, anon), "a b c d");
        // The same through an inline root (no block sibling).
        let (doc, i) = resolved(vec![E(
            "div",
            "o",
            "",
            vec![
                T("a", "a "),
                E(
                    "span",
                    "s",
                    C,
                    vec![T("b", "b "), E("i", "i", "", vec![T("c", "c")])],
                ),
                T("d", " d"),
            ],
        )]);
        assert_eq!(inline_text(&doc, i["o"]), "a b c d");
    }

    /// A wrapper that sets its own style is a span: the text inside it takes the wrapper's brush, in an anonymous
    /// block and in an inline root, however deep the wrappers are nested.
    #[test]
    fn a_styled_wrapper_gives_its_text_its_own_brush() {
        let red = "display:contents;color:red";
        let (doc, i) = resolved(vec![E(
            "div",
            "o",
            "",
            vec![
                T("a", "a"),
                E(
                    "span",
                    "w1",
                    C,
                    vec![E("span", "w2", red, vec![T("b", "b")])],
                ),
                E("p", "e", "", vec![T("te", "e")]),
            ],
        )]);
        let anon = layout_children(&doc, i["o"])[0];
        let brushes = brush_ids(&doc, anon);
        assert_eq!(brushes.len(), 2, "{brushes:?}");
        assert_ne!(brushes[0], i["w2"]);
        assert_eq!(brushes[1], i["w2"]);

        let (doc, i) = resolved(vec![E(
            "div",
            "o",
            "",
            vec![T("a", "a"), E("span", "w2", red, vec![T("b", "b")])],
        )]);
        let brushes = brush_ids(&doc, i["o"]);
        assert_eq!(brushes.len(), 2, "{brushes:?}");
        assert_eq!(brushes[1], i["w2"]);
    }

    #[test]
    fn resolving_twice_gives_the_same_tree() {
        let (mut doc, i) = resolved(vec![E(
            "div",
            "o",
            "",
            vec![
                T("a", "a "),
                E("span", "s", C, vec![T("b", "b "), p("p1", "tp", "x")]),
                T("ws", " "),
                E(
                    "div",
                    "w",
                    C,
                    vec![E("div", "d", "", vec![p("q", "tq", "q")])],
                ),
            ],
        )]);
        let first = layout_children(&doc, i["o"]);
        let shape = |doc: &BaseDocument| {
            let lc = layout_children(doc, i["o"]);
            lc.iter()
                .map(|&id| anon_children(doc, id).len())
                .collect::<Vec<_>>()
        };
        let before = shape(&doc);
        doc.resolve(0.0);
        assert_live(&doc, 0);
        assert_eq!(shape(&doc), before);
        assert_eq!(layout_children(&doc, i["o"]).len(), first.len());
    }
}

/// The text of inline formatting contexts after white space collapsing across element boundaries (`whitespace.rs`,
/// CSS Text 3 §4.1.1), as it reaches Parley.
#[cfg(test)]
mod whitespace_tests {
    use std::collections::HashMap;

    use crate::{Attribute, BaseDocument, DocumentConfig, QualName, ns};

    enum N {
        E(&'static str, &'static str, &'static str, Vec<N>),
        T(&'static str),
    }
    use N::{E, T};

    fn build(
        m: &mut crate::DocumentMutator<'_>,
        n: &N,
        ids: &mut HashMap<&'static str, usize>,
    ) -> usize {
        match n {
            T(text) => m.create_text_node(text),
            E(tag, name, style, kids) => {
                let attr = |n: &str, v: &str| Attribute {
                    name: QualName::new(None, ns!(), n.into()),
                    value: v.to_string(),
                };
                let mut attrs = vec![attr("id", name)];
                if !style.is_empty() {
                    attrs.push(attr("style", style));
                }
                let id = m.create_element(QualName::new(None, ns!(html), (*tag).into()), attrs);
                ids.insert(name, id);
                let kids: Vec<usize> = kids.iter().map(|k| build(m, k, ids)).collect();
                m.append_children(id, &kids);
                id
            }
        }
    }

    /// Resolves `<html><body>{body}</body></html>` and returns the text of the inline layout of the element `root`.
    fn text(root: &'static str, body: Vec<N>) -> String {
        let mut doc = BaseDocument::new(DocumentConfig::default());
        let mut ids = HashMap::new();
        {
            let mut m = doc.mutate();
            let html = build(
                &mut m,
                &E("html", "html", "", vec![E("body", "body", "", body)]),
                &mut ids,
            );
            m.append_children(0, &[html]);
        }
        doc.resolve(0.0);
        doc.nodes[ids[root]]
            .element_data()
            .and_then(|el| el.inline_layout_data.as_ref())
            .map(|layout| layout.text.clone())
            .unwrap_or_else(|| panic!("{root} has no inline layout"))
    }

    fn p(style: &'static str, kids: Vec<N>) -> Vec<N> {
        vec![E("p", "p", style, kids)]
    }

    fn span(kids: Vec<N>) -> N {
        E("span", "s", "", kids)
    }

    #[test]
    fn a_space_inside_a_span_is_kept() {
        // a <span>b </span>d
        assert_eq!(
            text("p", p("", vec![T("a "), span(vec![T("b ")]), T("d")])),
            "a b d"
        );
        // a<span> b</span> d
        assert_eq!(
            text("p", p("", vec![T("a"), span(vec![T(" b")]), T(" d")])),
            "a b d"
        );
        // a <span>b <i>c </i></span>d
        let inner = span(vec![T("b "), E("i", "i", "", vec![T("c ")])]);
        assert_eq!(text("p", p("", vec![T("a "), inner, T("d")])), "a b c d");
    }

    #[test]
    fn two_spaces_collapse_to_the_first() {
        assert_eq!(text("p", p("", vec![T("a "), span(vec![T(" b")])])), "a b");
        assert_eq!(
            text("p", p("", vec![T("a"), span(vec![T(" ")]), T(" b")])),
            "a b"
        );
    }

    #[test]
    fn spaces_at_the_ends_of_the_context_are_removed() {
        let kids = vec![span(vec![T(" a")]), T(" b "), span(vec![T("c ")])];
        assert_eq!(text("p", p("", kids)), "a b c");
    }

    #[test]
    fn spaces_around_a_forced_break_are_removed() {
        // a <br> <span> b</span>
        let kids = vec![
            T("a "),
            E("br", "br", "", vec![]),
            T(" "),
            span(vec![T(" b")]),
        ];
        assert_eq!(text("p", p("", kids)), "a\nb");
        // pre-line: a <span>b </span>\n  c
        let kids = vec![T("a "), span(vec![T("b ")]), T("\n  c")];
        assert_eq!(text("p", p("white-space:pre-line", kids)), "a b\nc");
    }

    #[test]
    fn a_collapsible_space_after_a_preserved_one_is_kept() {
        let pre = E("span", "s", "white-space:pre", vec![T("  b  ")]);
        assert_eq!(text("p", p("", vec![T("a "), pre, T(" c")])), "a   b   c");
    }

    #[test]
    fn atomic_inlines_are_content() {
        let block = E("span", "s", "display:inline-block", vec![T("X")]);
        // The text has no character for the box; the two spaces are the ones before and after it.
        assert_eq!(text("p", p("", vec![T("a "), block, T(" b")])), "a  b");
    }

    #[test]
    fn a_contents_element_with_a_style_keeps_its_space() {
        let wrapper = E("span", "w", "display:contents;color:red", vec![T("b ")]);
        assert_eq!(text("p", p("", vec![T("a "), wrapper, T("d")])), "a b d");
    }

    #[test]
    fn capitalize_follows_the_collapsing() {
        let kids = vec![T("a "), span(vec![T("b ")]), T("d")];
        assert_eq!(text("p", p("text-transform:capitalize", kids)), "A B D");
    }

    #[test]
    fn no_break_spaces_are_never_collapsed() {
        let kids = vec![E("b", "b", "", vec![T("12\u{a0}")]), T("\u{a0}000")];
        assert_eq!(text("p", p("", kids)), "12\u{a0}\u{a0}000");
    }

    #[test]
    fn the_body_is_an_inline_root_too() {
        let mut doc = BaseDocument::new(DocumentConfig::default());
        let mut ids = HashMap::new();
        {
            let mut m = doc.mutate();
            let kids = vec![T("Faktura "), E("b", "b", "", vec![T("č. ")]), T("2026")];
            let html = build(
                &mut m,
                &E("html", "html", "", vec![E("body", "body", "", kids)]),
                &mut ids,
            );
            m.append_children(0, &[html]);
        }
        doc.resolve(0.0);
        let body = doc.nodes[ids["body"]]
            .element_data()
            .and_then(|el| el.inline_layout_data.as_ref())
            .expect("the body is an inline root");
        assert_eq!(body.text, "Faktura č. 2026");
    }

    #[test]
    fn an_inside_marker_swallows_the_leading_spaces() {
        let list = vec![E(
            "ul",
            "u",
            "list-style-position:inside",
            vec![E("li", "li", "", vec![T(" "), span(vec![T(" x")])])],
        )];
        assert_eq!(text("li", list), "• x");
    }
}

/// An inline `<svg>` is parsed with the `usvg::Options` the document's factory makes: its image resolver and its fonts.
#[cfg(all(test, feature = "svg"))]
mod svg_options_tests {
    use std::sync::{Arc, Mutex};

    use crate::node::{ImageData, SpecialElementData};
    use crate::{Attribute, BaseDocument, DocumentConfig, QualName, SvgOptionsFactory, ns};

    /// A 1 x 1 PNG.
    const PNG: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    /// A font that has a glyph for U+2022 (the bullet) and nothing else worth naming.
    const BULLET_FONT: &[u8] = include_bytes!("../../assets/moz-bullet-font.otf");

    fn attr(name: &str, value: &str) -> Attribute {
        Attribute {
            name: QualName::new(None, ns!(), name.into()),
            value: value.to_string(),
        }
    }

    /// `<html><body><svg>...</svg>...</body></html>` (`svgs` of them, each built by `fill`); returns the document and
    /// the ids of the `<svg>` elements.
    fn document_with(
        config: DocumentConfig,
        svgs: usize,
        fill: impl Fn(&mut crate::DocumentMutator<'_>, usize),
    ) -> (BaseDocument, Vec<usize>) {
        let mut doc = BaseDocument::new(config);
        let mut ids = Vec::new();
        {
            let mut m = doc.mutate();
            let html = m.create_element(QualName::new(None, ns!(html), "html".into()), vec![]);
            let body = m.create_element(QualName::new(None, ns!(html), "body".into()), vec![]);
            for _ in 0..svgs {
                let svg = m.create_element(
                    QualName::new(None, ns!(svg), "svg".into()),
                    vec![attr("width", "10"), attr("height", "10")],
                );
                fill(&mut m, svg);
                m.append_children(body, &[svg]);
                ids.push(svg);
            }
            m.append_children(html, &[body]);
            m.append_children(0, &[html]);
        }
        doc.resolve(0.0);
        (doc, ids)
    }

    /// `<svg><image href=PNG></svg>`
    fn document(config: DocumentConfig, svgs: usize) -> (BaseDocument, Vec<usize>) {
        document_with(config, svgs, |m, svg| {
            let image = m.create_element(
                QualName::new(None, ns!(svg), "image".into()),
                vec![attr("href", PNG), attr("width", "10"), attr("height", "10")],
            );
            m.append_children(svg, &[image]);
        })
    }

    /// `<svg><text font-family=family>&bull;</text></svg>`
    fn text_document(config: DocumentConfig, family: &str) -> (BaseDocument, usize) {
        let (doc, ids) = document_with(config, 1, |m, svg| {
            let text = m.create_element(
                QualName::new(None, ns!(svg), "text".into()),
                vec![
                    attr("x", "1"),
                    attr("y", "8"),
                    attr("font-size", "8"),
                    attr("font-family", family),
                ],
            );
            let content = m.create_text_node("\u{2022}");
            m.append_children(text, &[content]);
            m.append_children(svg, &[text]);
        });
        (doc, ids[0])
    }

    fn tree(doc: &BaseDocument, svg: usize) -> &usvg::Tree {
        match doc.nodes[svg].element_data().map(|e| &e.special_data) {
            Some(SpecialElementData::Image(data)) => match &**data {
                ImageData::Svg(tree) => tree,
                _ => panic!("the <svg> is not parsed"),
            },
            _ => panic!("the <svg> has no tree"),
        }
    }

    fn count(doc: &BaseDocument, svg: usize, f: fn(&usvg::Node) -> bool) -> usize {
        fn walk(group: &usvg::Group, f: fn(&usvg::Node) -> bool) -> usize {
            group
                .children()
                .iter()
                .map(|node| {
                    usize::from(f(node))
                        + match node {
                            usvg::Node::Group(g) => walk(g, f),
                            usvg::Node::Text(t) => walk(t.flattened(), f),
                            _ => 0,
                        }
                })
                .sum()
        }
        walk(tree(doc, svg).root(), f)
    }

    fn images(doc: &BaseDocument, svg: usize) -> usize {
        count(doc, svg, |n| matches!(n, usvg::Node::Image(_)))
    }

    fn texts(doc: &BaseDocument, svg: usize) -> usize {
        count(doc, svg, |n| matches!(n, usvg::Node::Text(_)))
    }

    fn factory(
        f: impl Fn(usize) -> usvg::Options<'static> + Send + Sync + 'static,
    ) -> DocumentConfig {
        DocumentConfig {
            svg_options: Some(Arc::new(f) as SvgOptionsFactory),
            ..DocumentConfig::default()
        }
    }

    /// A font database that holds only the bullet font, and the font's family name.
    fn bullet_db() -> (Arc<usvg::fontdb::Database>, String) {
        let mut db = usvg::fontdb::Database::new();
        db.load_font_data(BULLET_FONT.to_vec());
        let family = db.faces().next().expect("the bullet font loads").families[0]
            .0
            .clone();
        (Arc::new(db), family)
    }

    #[test]
    fn without_a_factory_usvgs_resolver_is_used() {
        let (doc, ids) = document(DocumentConfig::default(), 1);
        assert_eq!(images(&doc, ids[0]), 1);
    }

    #[test]
    fn a_resolver_that_refuses_leaves_no_image_node() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let config = factory(move |node_id| {
            log.lock().unwrap().push(node_id);
            usvg::Options {
                image_href_resolver: usvg::ImageHrefResolver {
                    resolve_data: Box::new(|_, _, _| None),
                    resolve_string: Box::new(|_, _| None),
                },
                ..usvg::Options::default()
            }
        });
        let (doc, ids) = document(config, 2);
        // The factory is called with the id of the <svg> being parsed, and the SVG is still a tree.
        assert_eq!(*seen.lock().unwrap(), ids);
        assert_eq!(images(&doc, ids[0]), 0);
        assert_eq!(images(&doc, ids[1]), 0);
    }

    #[test]
    fn default_options_from_the_factory_give_the_same_tree() {
        let config = factory(|_| usvg::Options::default());
        let (doc, ids) = document(config, 1);
        assert_eq!(images(&doc, ids[0]), 1);
    }

    #[test]
    fn the_factorys_font_database_is_what_text_is_shaped_with() {
        let (db, family) = bullet_db();
        let config = factory(move |_| usvg::Options {
            fontdb: Arc::clone(&db),
            ..usvg::Options::default()
        });
        let (doc, svg) = text_document(config, &family);
        assert_eq!(texts(&doc, svg), 1, "the bundled face draws the text");

        // The same text with an empty database (usvg's default) finds no face: no text node, no host font.
        let (doc, svg) = text_document(factory(|_| usvg::Options::default()), &family);
        assert_eq!(texts(&doc, svg), 0);
    }

    #[test]
    fn the_factorys_font_resolver_picks_the_face() {
        let (db, _) = bullet_db();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&asked);
        let config = factory(move |_| {
            let log = Arc::clone(&log);
            let default = usvg::FontResolver::default_font_selector();
            usvg::Options {
                fontdb: Arc::clone(&db),
                font_resolver: usvg::FontResolver {
                    select_font: Box::new(move |font, db| {
                        log.lock().unwrap().push(format!("{:?}", font.families()));
                        default(font, db)
                    }),
                    select_fallback: usvg::FontResolver::default_fallback_selector(),
                },
                ..usvg::Options::default()
            }
        });
        // A family nobody has: the resolver is asked, answers through the default selector, and finds nothing.
        let (doc, svg) = text_document(config, "Nobody Has This");
        let asked = asked.lock().unwrap();
        assert!(!asked.is_empty(), "the resolver is asked");
        assert!(asked[0].contains("Nobody Has This"), "{asked:?}");
        assert_eq!(texts(&doc, svg), 0);
    }
}

/// A replaced inline element with `vertical-align: baseline` sits on the baseline of a line that keeps the strut of its
/// block (CSS 2.1 section 10.8.1): `<div><img height=60></div>` is 60 px plus the strut's descent tall, not 60.
#[cfg(test)]
mod strut_tests {
    use parley::FontContext;
    use parley::fontique::Blob;

    use crate::{Attribute, BaseDocument, DocumentConfig, QualName, ns};

    /// A font that has a glyph for U+2022 (the bullet) and nothing else worth naming; its metrics are the strut's.
    const FONT: &[u8] = include_bytes!("../../assets/moz-bullet-font.otf");

    fn attr(name: &str, value: &str) -> Attribute {
        Attribute {
            name: QualName::new(None, ns!(), name.into()),
            value: value.to_string(),
        }
    }

    /// The registered family of `FONT`.
    fn font_context() -> (FontContext, String) {
        let mut font_ctx = FontContext::default();
        let registered = font_ctx
            .collection
            .register_fonts(Blob::from(FONT.to_vec()), None);
        let name = font_ctx
            .collection
            .family_name(registered[0].0)
            .expect("the family has a name")
            .to_string();
        (font_ctx, name)
    }

    /// The strut's descent below the baseline for `line-height: normal` at `size` px: Chrome's rounded ascent,
    /// descent and line gap are added up, so the leading is zero and the descent stays.
    fn expected_descent(size: f32) -> f32 {
        let font = skrifa::FontRef::new(FONT).unwrap();
        let metrics = skrifa::metrics::Metrics::new(
            &font,
            skrifa::prelude::Size::new(size),
            skrifa::instance::LocationRef::default(),
        );
        (-metrics.descent).round()
    }

    /// `<div id="d" style="font: {size}px {family}; {div_style}">{inside}</div>` in `<body>`; `inside` builds the
    /// children of the div. Returns the document and the ids of the div and of its first `<img>`/`<span>` child.
    fn laid_out(
        div_style: &str,
        text: Option<&str>,
        child_tag: &str,
        child_style: &str,
    ) -> (BaseDocument, usize, usize) {
        let (font_ctx, family) = font_context();
        let mut doc = BaseDocument::new(DocumentConfig {
            font_ctx: Some(font_ctx),
            ..Default::default()
        });
        let (div, child);
        {
            let mut m = doc.mutate();
            let html_name = |n: &str| QualName::new(None, ns!(html), n.into());
            let html = m.create_element(html_name("html"), vec![]);
            let body =
                m.create_element(html_name("body"), vec![attr("style", "margin:0;padding:0")]);
            div = m.create_element(
                html_name("div"),
                vec![attr(
                    "style",
                    &format!("font:16px '{family}';width:400px;{div_style}"),
                )],
            );
            let mut attrs = vec![attr("width", "60"), attr("height", "60")];
            if !child_style.is_empty() {
                attrs.push(attr("style", child_style));
            }
            child = m.create_element(html_name(child_tag), attrs);
            let mut kids = Vec::new();
            if let Some(text) = text {
                kids.push(m.create_text_node(text));
            }
            kids.push(child);
            m.append_children(div, &kids);
            m.append_children(body, &[div]);
            m.append_children(html, &[body]);
            m.append_children(0, &[html]);
        }
        doc.resolve(0.0);
        (doc, div, child)
    }

    fn height(doc: &BaseDocument, id: usize) -> f32 {
        doc.nodes[id].final_layout.size.height
    }

    #[test]
    fn an_inline_img_keeps_the_strut_below_the_baseline() {
        let (doc, div, img) = laid_out("", None, "img", "");
        let descent = expected_descent(16.0);
        assert!(descent >= 1.0, "the test font has a descent: {descent}");
        assert_eq!(height(&doc, div), 60.0 + descent);
        assert_eq!(doc.nodes[img].final_layout.location.y, 0.0);
    }

    #[test]
    fn the_other_vertical_aligns_and_block_images_have_no_strut() {
        for style in [
            "vertical-align:middle",
            "vertical-align:top",
            "vertical-align:bottom",
            "display:block",
        ] {
            let (doc, div, _) = laid_out("", None, "img", style);
            assert_eq!(height(&doc, div), 60.0, "{style}");
        }
    }

    #[test]
    fn an_inline_block_is_not_a_strut_box() {
        let (doc, div, _) = laid_out(
            "",
            None,
            "span",
            "display:inline-block;width:60px;height:60px",
        );
        assert_eq!(height(&doc, div), 60.0);
    }

    #[test]
    fn an_img_inside_an_inline_block_has_no_strut_until_f183b() {
        // The outer line puts the bottom of an inline-block on its baseline, not the baseline of its last line, so a
        // strut inside would draw the image its descent too low: the context keeps the old line computation.
        for div_style in ["display:inline-block", "display:inline-block;line-height:0"] {
            let (doc, div, img) = laid_out(div_style, None, "img", "");
            assert_eq!(height(&doc, div), 60.0, "{div_style}");
            assert_eq!(doc.nodes[img].final_layout.location.y, 0.0, "{div_style}");
        }
    }

    #[test]
    fn an_img_with_text_starts_at_the_line_top() {
        // Before the fix the strut's half-leading was added on top of the box: the image started above the line.
        let (doc, div, img) = laid_out("", Some("Dodavatel"), "img", "");
        assert_eq!(doc.nodes[img].final_layout.location.y, 0.0);
        // The text falls back to another font (the test font has no letters): its descent may be larger than the
        // strut's, never smaller.
        assert!(height(&doc, div) >= 60.0 + expected_descent(16.0));
    }

    #[test]
    fn an_absolutely_positioned_img_adds_no_line() {
        let (doc, div, _) = laid_out("", None, "img", "position:absolute;left:100px;top:100px");
        assert_eq!(height(&doc, div), 0.0);
    }
}

/// The `::before` / `::after` of an inline element nested in another inline element are boxes of the line layout like
/// any other (CSS 2.1 section 12.1): the pseudo-element of a host two or more levels below the root of the line layout is
/// created, and its text and boxes are in the root's layout (html-to-pdf F66i).
#[cfg(test)]
mod nested_inline_pseudo_tests {
    use std::collections::HashMap;

    use crate::{Attribute, BaseDocument, DocumentConfig, QualName, ns};

    enum N {
        E(&'static str, &'static str, &'static str, Vec<N>),
        T(&'static str),
    }
    use N::{E, T};

    fn build(
        m: &mut crate::DocumentMutator<'_>,
        n: &N,
        ids: &mut HashMap<&'static str, usize>,
    ) -> usize {
        match n {
            T(text) => m.create_text_node(text),
            E(tag, name, style, kids) => {
                let attr = |n: &str, v: &str| Attribute {
                    name: QualName::new(None, ns!(), n.into()),
                    value: v.to_string(),
                };
                let mut attrs = vec![attr("id", name)];
                if !style.is_empty() {
                    attrs.push(attr("style", style));
                }
                let id = m.create_element(QualName::new(None, ns!(html), (*tag).into()), attrs);
                ids.insert(name, id);
                let kids: Vec<usize> = kids.iter().map(|k| build(m, k, ids)).collect();
                m.append_children(id, &kids);
                id
            }
        }
    }

    /// Resolves `<html><body>{body}</body></html>` with the user-agent rules `css`.
    fn resolved(css: &str, body: Vec<N>) -> (BaseDocument, HashMap<&'static str, usize>) {
        let mut doc = BaseDocument::new(DocumentConfig::default());
        doc.add_user_agent_stylesheet(css);
        let mut ids = HashMap::new();
        {
            let mut m = doc.mutate();
            let html = build(
                &mut m,
                &E("html", "html", "", vec![E("body", "body", "", body)]),
                &mut ids,
            );
            m.append_children(0, &[html]);
        }
        doc.resolve(0.0);
        (doc, ids)
    }

    fn text_of(doc: &BaseDocument, root: usize) -> String {
        doc.nodes[root]
            .element_data()
            .and_then(|el| el.inline_layout_data.as_ref())
            .map(|layout| layout.text.clone())
            .unwrap_or_else(|| panic!("node {root} has no inline layout"))
    }

    /// The nodes `a <span id=h>{host_text}</span> w`, wrapped in `depth - 1` spans (a depth of 1 is the three nodes).
    fn run(depth: usize, host_text: &'static str) -> Vec<N> {
        let host = E(
            "span",
            "h",
            "",
            if host_text.is_empty() {
                vec![]
            } else {
                vec![T(host_text)]
            },
        );
        let mut nodes = vec![T("a "), host, T(" w")];
        for _ in 1..depth {
            nodes = vec![E("span", "w", "", nodes)];
        }
        nodes
    }

    const BEFORE: &str = "#h::before { content: \"tx\" }";
    const AFTER: &str = "#h::after { content: \"ty\" }";

    #[test]
    fn the_pseudo_of_a_nested_host_is_in_the_layout_of_a_block_root() {
        for depth in 1..=4 {
            for (css, wanted) in [(BEFORE, "tx"), (AFTER, "ty")] {
                for host_text in ["", "h"] {
                    let (doc, ids) = resolved(css, vec![E("div", "r", "", run(depth, host_text))]);
                    let text = text_of(&doc, ids["r"]);
                    assert!(
                        text.contains(wanted),
                        "depth {depth}, {css}, host text {host_text:?}: layout text {text:?}"
                    );
                    let host = &doc.nodes[ids["h"]];
                    assert!(
                        host.before.is_some() || host.after.is_some(),
                        "depth {depth}, {css}: the host has a pseudo-element"
                    );
                }
            }
        }
    }

    #[test]
    fn the_pseudo_of_a_nested_host_is_in_the_layout_of_any_root_kind() {
        for depth in 2..=3 {
            let roots: [(&str, N); 4] = [
                ("p", E("p", "r", "", run(depth, ""))),
                (
                    "table cell",
                    E(
                        "table",
                        "tbl",
                        "",
                        vec![E("tr", "tr", "", vec![E("td", "r", "", run(depth, ""))])],
                    ),
                ),
                (
                    "inline-block",
                    E(
                        "div",
                        "o",
                        "",
                        vec![E("span", "r", "display:inline-block", run(depth, ""))],
                    ),
                ),
                (
                    "flex item",
                    E(
                        "div",
                        "o",
                        "display:flex",
                        vec![E("div", "r", "", run(depth, ""))],
                    ),
                ),
            ];
            for (name, root) in roots {
                let (doc, ids) = resolved(BEFORE, vec![root]);
                let text = text_of(&doc, ids["r"]);
                assert!(
                    text.contains("tx"),
                    "{name} root, depth {depth}: layout text {text:?}"
                );
            }
        }
    }

    #[test]
    fn a_nested_inline_block_pseudo_is_a_layout_child_of_the_root() {
        for depth in 1..=3 {
            let (doc, ids) = resolved(
                "#h::before { content: \"\"; display: inline-block; width: 6px; height: 6px }",
                vec![E("div", "r", "", run(depth, ""))],
            );
            let before = doc.nodes[ids["h"]].before.expect("the host has ::before");
            let children = doc.nodes[ids["r"]]
                .layout_children
                .borrow()
                .clone()
                .unwrap_or_default();
            assert!(
                children.contains(&before),
                "depth {depth}: the pseudo-element {before} is among the layout children {children:?} of the root"
            );
        }
    }

    #[test]
    fn an_anonymous_block_root_and_a_contents_chain_still_print_the_pseudo() {
        // The inline run sits next to a block: its root is an anonymous block.
        let (doc, ids) = resolved(
            BEFORE,
            vec![E(
                "div",
                "r",
                "",
                vec![
                    E("span", "w", "", run(1, "")),
                    E("div", "blk", "", vec![T("blk")]),
                ],
            )],
        );
        let anon_root = doc.nodes[ids["w"]]
            .layout_parent
            .get()
            .expect("layout parent");
        assert!(
            text_of(&doc, anon_root).contains("tx"),
            "anonymous block root: {:?}",
            text_of(&doc, anon_root)
        );
        // `display: contents` between the root and the host.
        let (doc, ids) = resolved(
            BEFORE,
            vec![E(
                "div",
                "r",
                "",
                vec![E(
                    "span",
                    "c1",
                    "display:contents",
                    vec![E("span", "c2", "display:contents", run(1, ""))],
                )],
            )],
        );
        assert!(text_of(&doc, ids["r"]).contains("tx"));
    }

    #[test]
    fn a_pseudo_under_display_none_inside_an_anonymous_block_is_not_read() {
        let (doc, ids) = resolved(
            BEFORE,
            vec![E(
                "div",
                "r",
                "",
                vec![
                    E(
                        "span",
                        "w",
                        "",
                        vec![
                            T("a "),
                            E(
                                "span",
                                "n",
                                "display:none",
                                vec![E("span", "h", "", vec![])],
                            ),
                            T(" w"),
                        ],
                    ),
                    E("div", "blk", "", vec![T("blk")]),
                ],
            )],
        );
        let anon_root = doc.nodes[ids["w"]]
            .layout_parent
            .get()
            .expect("layout parent");
        assert!(
            !text_of(&doc, anon_root).contains("tx"),
            "{:?}",
            text_of(&doc, anon_root)
        );
    }
}
