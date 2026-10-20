use core::str;
use std::sync::Arc;

use markup5ever::{QualName, local_name, ns};
use parley::{
    FontContext, InlineBox, LayoutContext, StyleProperty, TreeBuilder, WhiteSpaceCollapse,
};
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

use super::{damage::ALL_DAMAGE, list::collect_list_item_children, table::build_table_context};

use style::values::specified::text::TextTransformCase;

thread_local! {
    /// Whether the next letter pushed into the current inline layout starts a word (for
    /// `text-transform: capitalize`). Reset at the start of every inline layout.
    static AT_WORD_START: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// Applies the case part of `text-transform` (CSS Text 3 §2.1) to a text node's content. Word boundaries
/// for `capitalize` carry over from the previous text node of the same inline layout, so `wo<b>rd</b>`
/// stays one word. Language-specific mappings (Turkish dotted i, Greek accents) are not applied.
fn transform_text(text: &str, case: Option<TextTransformCase>) -> std::borrow::Cow<'_, str> {
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

            match crate::util::parse_svg(outer_html.as_bytes()) {
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
            if let Some(before) = doc.nodes[container_node_id].before {
                layout_children.push(before);
            }
            layout_children.extend_from_slice(&tlayout_children);
            if let Some(after) = doc.nodes[container_node_id].after {
                layout_children.push(after);
            }
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
    // Also hide all-whitespace flexbox children as these should be ignored
    if child_node_kind == NodeKind::Comment || (hide_whitespace && is_whitespace_node) {
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
            let display = doc.nodes[node_id]
                .display_style()
                .unwrap_or(Display::inline());
            let child_is_contents = doc.nodes[child_id]
                .display_style()
                .is_some_and(|d| d.inside() == DisplayInside::Contents);
            let do_recurse = child_is_contents
                || match (display.outside(), display.inside()) {
                    (DisplayOutside::None, DisplayInside::Contents) => true,
                    (DisplayOutside::Inline, DisplayInside::Flow) => true,
                    (_, _) => false,
                };
            if do_recurse {
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

                        if *tag_name == local_name!("img")
                            || *tag_name == local_name!("svg")
                            || *tag_name == local_name!("input")
                            || *tag_name == local_name!("textarea")
                            || *tag_name == local_name!("button")
                        {
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

    // Create a parley tree builder
    let mut builder = layout_ctx.tree_builder(font_ctx, scale, true, &parley_style);
    // `text-transform: capitalize` looks at word boundaries across the whole inline formatting context.
    AT_WORD_START.with(|w| w.set(true));

    // `text-wrap-mode` of the root applies to the whole context (Parley cannot switch it per span).
    text_layout.nowrap = root_node_style
        .as_ref()
        .is_some_and(|s| stylo_to_parley::text_wrap_nowrap(s));

    // Set whitespace collapsing mode
    let collapse_mode = root_node_style
        .map(|s| s.get_inherited_text().white_space_collapse)
        .map(stylo_to_parley::white_space_collapse)
        .unwrap_or(WhiteSpaceCollapse::Collapse);
    builder.set_white_space_mode(collapse_mode);

    // Render position-inside list items
    if let Some(ListItemLayout {
        marker,
        position: ListItemLayoutPosition::Inside,
    }) = root_node
        .element_data()
        .and_then(|el| el.list_item_data.as_deref())
    {
        match marker {
            Marker::Char(char) => builder.push_text(&format!("{char} ")),
            Marker::String(str) => builder.push_text(str),
        }
    };

    if let Some(before_id) = root_node.before {
        build_inline_layout_recursive(
            &mut builder,
            nodes,
            inline_context_root_node_id,
            before_id,
            collapse_mode,
            root_line_height,
        );
    }
    for child_id in root_node.children.iter().copied() {
        build_root_child(
            &mut builder,
            nodes,
            root_node,
            child_id,
            collapse_mode,
            root_line_height,
        );
    }
    if let Some(after_id) = root_node.after {
        build_inline_layout_recursive(
            &mut builder,
            nodes,
            inline_context_root_node_id,
            after_id,
            collapse_mode,
            root_line_height,
        );
    }

    text_layout.text = builder.build_into(&mut text_layout.layout);
    return;

    /// Builds a child of the inline root. The children of an anonymous block can come from inside
    /// `display: contents` elements (they generate no box, so their children were placed in the block): each of
    /// those elements is entered as a style span so that the text keeps the style and the brush of its own
    /// parent and not those of the block.
    fn build_root_child(
        builder: &mut TreeBuilder<TextBrush>,
        nodes: &Slab<Node>,
        root_node: &Node,
        child_id: usize,
        collapse_mode: WhiteSpaceCollapse,
        root_line_height: f32,
    ) {
        let mut wrappers: smallvec::SmallVec<[usize; 4]> = smallvec::SmallVec::new();
        if root_node.data.kind() == NodeKind::AnonymousBlock {
            let mut parent = nodes[child_id].parent;
            while let Some(parent_id) = parent {
                if parent_id == root_node.id || Some(parent_id) == root_node.parent {
                    break;
                }
                let is_contents = nodes[parent_id]
                    .display_style()
                    .is_some_and(|d| d.inside() == DisplayInside::Contents);
                if !is_contents {
                    break;
                }
                wrappers.push(parent_id);
                parent = nodes[parent_id].parent;
            }
        }

        // Outermost first.
        let mut collapse_mode = collapse_mode;
        for &wrapper_id in wrappers.iter().rev() {
            let wrapper = &nodes[wrapper_id];
            builder.push_style_span(inline_span_style(wrapper, root_line_height));
            if let Some(style) = wrapper.primary_styles() {
                collapse_mode = stylo_to_parley::white_space_collapse(
                    style.get_inherited_text().white_space_collapse,
                );
            }
        }
        build_inline_layout_recursive(
            builder,
            nodes,
            root_node.id,
            child_id,
            collapse_mode,
            root_line_height,
        );
        for _ in &wrappers {
            builder.pop_style_span();
        }
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

    fn build_inline_layout_recursive(
        builder: &mut TreeBuilder<TextBrush>,
        nodes: &Slab<Node>,
        parent_id: usize,
        node_id: usize,
        collapse_mode: WhiteSpaceCollapse,
        root_line_height: f32,
    ) {
        let node = &nodes[node_id];

        // Set layout_parent for node.
        node.layout_parent.set(Some(parent_id));

        // Set whitespace collapsing mode
        let collapse_mode = node
            .primary_styles()
            .map(|s| s.get_inherited_text().white_space_collapse)
            .map(stylo_to_parley::white_space_collapse)
            .unwrap_or(collapse_mode);
        builder.set_white_space_mode(collapse_mode);

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
                        builder.push_style_span(inline_span_style(node, root_line_height));
                        for child_id in node
                            .before
                            .into_iter()
                            .chain(node.children.iter().copied())
                            .chain(node.after)
                        {
                            build_inline_layout_recursive(
                                builder,
                                nodes,
                                parent_id,
                                child_id,
                                collapse_mode,
                                root_line_height,
                            );
                        }
                        builder.pop_style_span();
                    }
                    (DisplayOutside::Inline, DisplayInside::Flow) => {
                        let tag_name = &element_data.name.local;

                        if *tag_name == local_name!("img")
                            || *tag_name == local_name!("svg")
                            || *tag_name == local_name!("input")
                            || *tag_name == local_name!("textarea")
                            || *tag_name == local_name!("button")
                        {
                            builder.push_inline_box(InlineBox {
                                id: node_id as u64,
                                // Overridden by push_inline_box method
                                index: 0,
                                // Width and height are set during layout
                                width: 0.0,
                                height: 0.0,
                            });
                        } else if *tag_name == local_name!("br") {
                            // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                            // TODO: update span id for br spans
                            push_forced_break(builder, collapse_mode);
                        } else {
                            // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                            let style = inline_span_style(node, root_line_height);

                            builder.push_style_span(style);

                            if let Some(before_id) = node.before {
                                build_inline_layout_recursive(
                                    builder,
                                    nodes,
                                    node_id,
                                    before_id,
                                    collapse_mode,
                                    root_line_height,
                                );
                            }

                            for child_id in node.children.iter().copied() {
                                build_inline_layout_recursive(
                                    builder,
                                    nodes,
                                    node_id,
                                    child_id,
                                    collapse_mode,
                                    root_line_height,
                                );
                            }
                            if let Some(after_id) = node.after {
                                build_inline_layout_recursive(
                                    builder,
                                    nodes,
                                    node_id,
                                    after_id,
                                    collapse_mode,
                                    root_line_height,
                                );
                            }

                            builder.pop_style_span();
                        }
                    }
                    // Inline box
                    (_, _) => {
                        builder.push_inline_box(InlineBox {
                            id: node_id as u64,
                            // Overridden by push_inline_box method
                            index: 0,
                            // Width and height are set during layout
                            width: 0.0,
                            height: 0.0,
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
                let text = transform_text(&data.content, case);
                // `white-space: pre-line` collapses spaces and tabs but keeps segment breaks. Parley has only
                // the two modes, so the text is split at its newlines: the pieces are collapsed and each
                // newline is pushed as a preserved one, like a `<br>`. Parley's collapse then drops the
                // indentation that follows a newline (it trims the start of text after whitespace).
                let preserve_breaks = parent_style.as_ref().is_some_and(|style| {
                    style.get_inherited_text().white_space_collapse
                        == stylo_to_parley::stylo::WhiteSpaceCollapse::PreserveBreaks
                });
                if preserve_breaks && text.contains('\n') {
                    for (index, piece) in text.split('\n').enumerate() {
                        if index > 0 {
                            push_forced_break(builder, collapse_mode);
                        }
                        builder.push_text(piece);
                    }
                } else {
                    builder.push_text(&text);
                }
            }
            NodeData::Comment => {
                // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            }
            NodeData::Document => unreachable!(),
        }
    }
}

/// Pushes a preserved newline: the text before it is committed with `collapse_mode`, the newline itself is
/// preserved, and `collapse_mode` is active again afterwards.
fn push_forced_break(builder: &mut TreeBuilder<TextBrush>, collapse_mode: WhiteSpaceCollapse) {
    builder.push_style_modification_span(&[]);
    builder.set_white_space_mode(WhiteSpaceCollapse::Preserve);
    builder.push_text("\n");
    // A forced line break ends the word for `text-transform: capitalize`.
    AT_WORD_START.with(|w| w.set(true));
    builder.pop_style_span();
    builder.set_white_space_mode(collapse_mode);
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
