use crate::NON_INCREMENTAL;
use crate::node::NodeFlags;
use crate::{BaseDocument, net::ImageHandler, node::BackgroundImageData, util::ImageType};
use blitz_traits::net::Request;
use markup5ever::local_name;
use style::properties::ComputedValues;
use style::properties::generated::longhands::position::computed_value::T as Position;
use style::selector_parser::RestyleDamage;
use style::servo::url::ComputedUrl;
use style::values::generics::image::Image as StyloImage;
use style::values::specified::align::AlignFlags;
use style::values::specified::box_::DisplayInside;
use style::values::specified::box_::DisplayOutside;

pub(crate) const CONSTRUCT_BOX: RestyleDamage =
    RestyleDamage::from_bits_retain(0b_0000_0000_0001_0000);
pub(crate) const CONSTRUCT_FC: RestyleDamage =
    RestyleDamage::from_bits_retain(0b_0000_0000_0010_0000);
pub(crate) const CONSTRUCT_DESCENDENT: RestyleDamage =
    RestyleDamage::from_bits_retain(0b_0000_0000_0100_0000);

pub(crate) const ONLY_RELAYOUT: RestyleDamage =
    RestyleDamage::from_bits_retain(0b_0000_0000_0000_1000);

pub(crate) const ALL_DAMAGE: RestyleDamage =
    RestyleDamage::from_bits_retain(0b_0000_0000_0111_1111);

impl BaseDocument {
    #[cfg(feature = "incremental")]
    pub(crate) fn propagate_damage_flags(
        &mut self,
        node_id: usize,
        damage_from_parent: RestyleDamage,
    ) -> RestyleDamage {
        let Some(mut damage) = self.nodes[node_id].damage() else {
            return RestyleDamage::empty();
        };
        damage |= damage_from_parent;

        let damage_for_children = RestyleDamage::empty();
        let children = std::mem::take(&mut self.nodes[node_id].children);
        let layout_children = std::mem::take(self.nodes[node_id].layout_children.get_mut());
        let use_layout_children = self.nodes[node_id].should_traverse_layout_children();
        if use_layout_children {
            let layout_children = layout_children.as_ref().unwrap();
            for child in layout_children.iter() {
                damage |= self.propagate_damage_flags(*child, damage_for_children);
            }
        } else {
            for child in children.iter() {
                damage |= self.propagate_damage_flags(*child, damage_for_children);
            }
            if let Some(before_id) = self.nodes[node_id].before {
                damage |= self.propagate_damage_flags(before_id, damage_for_children);
            }
            if let Some(after_id) = self.nodes[node_id].after {
                damage |= self.propagate_damage_flags(after_id, damage_for_children);
            }
        }

        let node = &mut self.nodes[node_id];

        // Put children back
        node.children = children;
        *node.layout_children.get_mut() = layout_children;

        if damage.contains(CONSTRUCT_BOX) {
            damage.insert(RestyleDamage::RELAYOUT);
        }

        // Compute damage to propagate to parent
        let damage_for_parent = damage; // & RestyleDamage::RELAYOUT;

        // If the node or any of it's children have been mutated or their layout styles
        // have changed, then we should clear it's layout cache.
        if damage.intersects(ONLY_RELAYOUT | CONSTRUCT_BOX) {
            node.cache.clear();
            if let Some(inline_layout) = node
                .data
                .downcast_element_mut()
                .and_then(|el| el.inline_layout_data.as_mut())
            {
                inline_layout.content_widths = None;
            }
            damage.remove(ONLY_RELAYOUT);
        }

        // Store damage for current node
        node.set_damage(damage);

        // let _is_fc_root = node
        //     .primary_styles()
        //     .map(|s| is_fc_root(&s))
        //     .unwrap_or(false);

        // if damage.contains(CONSTRUCT_BOX) {
        //     // damage_for_parent.insert(CONSTRUCT_FC | CONSTRUCT_DESCENDENT);
        //     damage_for_parent.insert(CONSTRUCT_BOX);
        // }

        // if damage.contains(CONSTRUCT_FC) {
        //     damage_for_parent.insert(CONSTRUCT_DESCENDENT);
        //     // if !is_fc_root {
        //     damage_for_parent.insert(CONSTRUCT_FC);
        //     // }
        // }

        // Propagate damage to parent
        damage_for_parent
    }
}

// #[cfg(feature = "incremental")]
// fn is_fc_root(style: &ComputedValues) -> bool {
//     let display = style.clone_display();
//     let display_inside = display.inside();

//     match display_inside {
//         DisplayInside::Flow => {
//             // Depends on parent context
//             false
//         }

//         DisplayInside::None => true,
//         DisplayInside::FlowRoot => true,
//         DisplayInside::Flex => true,
//         DisplayInside::Grid => true,
//         DisplayInside::Table => true,
//         DisplayInside::TableCell => true,

//         DisplayInside::Contents => false,
//         DisplayInside::TableRowGroup => false,
//         DisplayInside::TableColumn => false,
//         DisplayInside::TableColumnGroup => false,
//         DisplayInside::TableHeaderGroup => false,
//         DisplayInside::TableFooterGroup => false,
//         DisplayInside::TableRow => false,
//     }
// }

pub(crate) fn compute_layout_damage(old: &ComputedValues, new: &ComputedValues) -> RestyleDamage {
    let box_tree_needs_rebuild = || {
        let old_box = old.get_box();
        let new_box = new.get_box();

        if old_box.display != new_box.display
            || old_box.float != new_box.float
            || old_box.position != new_box.position
        {
            return true;
        }

        if old.get_font() != new.get_font() {
            return true;
        }

        if new_box.display.outside() == DisplayOutside::Block
            && new_box.display.inside() == DisplayInside::Flow
        {
            let alignment_establishes_new_block_formatting_context = |style: &ComputedValues| {
                style.get_position().align_content.0.primary() != AlignFlags::NORMAL
            };

            let old_column = old.get_column();
            let new_column = new.get_column();
            if old_box.overflow_x.is_scrollable() != new_box.overflow_x.is_scrollable()
                || old_column.is_multicol() != new_column.is_multicol()
                || old_column.column_span != new_column.column_span
                || alignment_establishes_new_block_formatting_context(old)
                    != alignment_establishes_new_block_formatting_context(new)
            {
                return true;
            }
        }

        if old_box.display.is_list_item() {
            let old_list = old.get_list();
            let new_list = new.get_list();
            if old_list.list_style_position != new_list.list_style_position
                || old_list.list_style_image != new_list.list_style_image
                || (new_list.list_style_image == StyloImage::None
                    && old_list.list_style_type != new_list.list_style_type)
            {
                return true;
            }
        }

        if new.is_pseudo_style() && old.get_counters().content != new.get_counters().content {
            return true;
        }

        false
    };

    let text_shaping_needs_recollect = || {
        if old.clone_direction() != new.clone_direction()
            || old.clone_unicode_bidi() != new.clone_unicode_bidi()
        {
            return true;
        }

        let old_text = old.get_inherited_text();
        let new_text = new.get_inherited_text();
        if !std::ptr::eq(old_text, new_text)
            && (old_text.white_space_collapse != new_text.white_space_collapse
                || old_text.text_transform != new_text.text_transform
                || old_text.word_break != new_text.word_break
                || old_text.overflow_wrap != new_text.overflow_wrap
                || old_text.letter_spacing != new_text.letter_spacing
                || old_text.word_spacing != new_text.word_spacing
                || old_text.text_rendering != new_text.text_rendering)
        {
            return true;
        }

        false
    };

    #[allow(
        clippy::if_same_then_else,
        reason = "these branches will soon be different"
    )]
    if box_tree_needs_rebuild() {
        ALL_DAMAGE
    } else if text_shaping_needs_recollect() {
        ALL_DAMAGE
    } else {
        // This element needs to be laid out again, but does not have any damage to
        // its box. In the future, we will distinguish between types of damage to the
        // fragment as well.
        RestyleDamage::RELAYOUT
    }
}

impl BaseDocument {
    pub(crate) fn invalidate_inline_contexts(&mut self) {
        let scale = self.viewport.scale();

        let font_ctx = &self.font_ctx;
        let layout_ctx = &mut self.layout_ctx;

        for (_, node) in self.nodes.iter_mut() {
            if !(node.flags.contains(NodeFlags::IS_IN_DOCUMENT)) {
                continue;
            }
            let Some(element) = node.data.downcast_element_mut() else {
                continue;
            };

            if element.inline_layout_data.is_some() {
                node.insert_damage(ALL_DAMAGE);
            } else if let Some(input) = element.text_input_data_mut() {
                input.editor.set_scale(scale);
                let mut font_ctx = font_ctx.lock().unwrap();
                input.editor.refresh_layout(&mut font_ctx, layout_ctx);
                node.insert_damage(ONLY_RELAYOUT);
            }
        }
    }

    /// Walk the whole tree, converting styles to layout
    pub fn flush_styles_to_layout(&mut self, node_id: usize) {
        let doc_id = self.id();

        // Whether the grid container this node is an item of leaves `justify-items` / `align-items` at
        // `normal` (stylo_taffy turns both `normal` and `stretch` into `Stretch`, so the computed style
        // is read here).
        let grid_parent_normal = self.nodes[node_id]
            .layout_parent
            .get()
            .map(|parent| &self.nodes[parent])
            .filter(|parent| parent.style.display == taffy::Display::Grid)
            .and_then(|parent| {
                let styles = parent.primary_styles()?;
                let position = styles.get_position();
                Some((
                    position.justify_items.computed.0.value() == AlignFlags::NORMAL,
                    position.align_items.0.value() == AlignFlags::NORMAL,
                ))
            });

        let display = {
            let node = self.nodes.get_mut(node_id).unwrap();
            let _damage = node.damage().unwrap_or(ALL_DAMAGE);
            let stylo_element_data = node.stylo_element_data.borrow();
            let primary_styles = stylo_element_data
                .as_ref()
                .and_then(|data| data.styles.get_primary());

            let Some(style) = primary_styles else {
                return;
            };

            // if damage.intersects(RestyleDamage::RELAYOUT | CONSTRUCT_BOX) {
            node.style = stylo_taffy::to_taffy_style(style);
            // A table cell's `calc()` width that mixes a percentage and a length counts as `auto`
            // (css-values-4, see `layout/table.rs`); otherwise it would still size the cell's content.
            if style.clone_display().inside() == DisplayInside::TableCell
                && node.style.size.width.into_raw().is_calc()
            {
                node.style.size.width = taffy::Dimension::auto();
            }
            // A cell's length `height` is a minimum and its `max-height` is ignored (see
            // `layout/table.rs`): the cell is measured and laid out through this style, so it must not be
            // clamped below its content.
            // The same holds for the table itself when its own style is read, as a flex or grid item's is.
            if matches!(
                style.clone_display().inside(),
                DisplayInside::TableCell | DisplayInside::Table
            ) {
                super::table::height_as_table_minimum(&mut node.style);
            }
            // A table and its cells in the collapsing border model have half of the collapsed borders on each side,
            // which Stylo does not know (see `layout/table.rs`, `collapse_geometry`); a table has no padding.
            if let Some(border) = node.collapsed_border {
                let length = taffy::LengthPercentage::length;
                node.style.border = taffy::Rect {
                    left: length(border.left),
                    right: length(border.right),
                    top: length(border.top),
                    bottom: length(border.bottom),
                };
                if style.clone_display().inside() == DisplayInside::Table {
                    node.style.padding = taffy::Rect {
                        left: length(0.0),
                        right: length(0.0),
                        top: length(0.0),
                        bottom: length(0.0),
                    };
                }
            }
            if let Some(element) = node.data.downcast_element() {
                let tag = element.name.local.as_ref();
                // HTML §15.4.3: the `width` / `height` attributes of an <img> are presentational hints,
                // that is `width` / `height` declarations of the lowest priority. Taffy only looks at the
                // style, and a definite size decides whether a flex item is stretched across the cross
                // axis (css-flexbox §9.4) and whether a grid item has a size of its own.
                if tag == "img" {
                    let hint = |name| {
                        element
                            .attr(name)
                            .and_then(|val| val.parse::<f32>().ok())
                            .filter(|len| *len >= 0.0)
                    };
                    if node.style.size.width.is_auto() {
                        if let Some(width) = hint(local_name!("width")) {
                            node.style.size.width = taffy::Dimension::length(width);
                        }
                    }
                    if node.style.size.height.is_auto() {
                        if let Some(height) = hint(local_name!("height")) {
                            node.style.size.height = taffy::Dimension::length(height);
                        }
                    }
                }
                // css-grid-1 §6.2 / css-align-3 §6.1: `normal` self-alignment of a grid item with a
                // preferred aspect ratio, which every replaced element has, behaves as `start` rather
                // than `stretch`. An explicit `stretch` (on the item or through `justify-items` /
                // `align-items` on the container) is kept.
                if let Some((justify_normal, align_normal)) = grid_parent_normal {
                    if matches!(tag, "img" | "svg" | "canvas" | "video") {
                        if justify_normal && node.style.justify_self.is_none() {
                            node.style.justify_self = Some(taffy::AlignSelf::Start);
                        }
                        if align_normal && node.style.align_self.is_none() {
                            node.style.align_self = Some(taffy::AlignSelf::Start);
                        }
                    }
                }
            }
            node.display_constructed_as = style.clone_display();
            // }

            // Flush background image from style to dedicated storage on the node
            // TODO: handle multiple background images
            if let Some(elem) = node.data.downcast_element_mut() {
                let style_bgs = &style.get_background().background_image.0;
                let elem_bgs = &mut elem.background_images;

                let len = style_bgs.len();
                elem_bgs.resize_with(len, || None);

                for idx in 0..len {
                    let background_image = &style_bgs[idx];
                    let new_bg_image = match background_image {
                        StyloImage::Url(ComputedUrl::Valid(new_url)) => {
                            let old_bg_image = elem_bgs[idx].as_ref();
                            let old_bg_image_url = old_bg_image.map(|data| &data.url);
                            if old_bg_image_url.is_some_and(|old_url| **new_url == **old_url) {
                                break;
                            }

                            self.net_provider.fetch(
                                doc_id,
                                Request::get((**new_url).clone()),
                                Box::new(ImageHandler::new(node_id, ImageType::Background(idx))),
                            );

                            let bg_image_data = BackgroundImageData::new(new_url.clone());
                            Some(bg_image_data)
                        }
                        _ => None,
                    };

                    // Element will always exist due to resize_with above
                    elem_bgs[idx] = new_bg_image;
                }
            }

            // In non-incremental mode we unconditionally clear the Taffy cache.
            // In incremental mode this is handled as part of damage propagation.
            if NON_INCREMENTAL {
                node.cache.clear();
                if let Some(inline_layout) = node
                    .data
                    .downcast_element_mut()
                    .and_then(|el| el.inline_layout_data.as_mut())
                {
                    inline_layout.content_widths = None;
                }
            }

            node.style.display
        };

        // If the node has children, then take those children and...
        let children = self.nodes[node_id].layout_children.borrow_mut().take();
        if let Some(mut children) = children {
            // Recursively call flush_styles_to_layout on each child
            for child in children.iter() {
                self.flush_styles_to_layout(*child);
            }

            // If the node is a Flexbox or Grid node then sort by css order property
            if matches!(display, taffy::Display::Flex | taffy::Display::Grid) {
                children.sort_by(|left, right| {
                    let left_node = self.nodes.get(*left).unwrap();
                    let right_node = self.nodes.get(*right).unwrap();
                    left_node.order().cmp(&right_node.order())
                });
            }

            // Put children back
            *self.nodes[node_id].layout_children.borrow_mut() = Some(children);

            // Sort paint_children in place
            self.nodes[node_id]
                .paint_children
                .borrow_mut()
                .as_mut()
                .unwrap()
                .sort_by(|left, right| {
                    let left_node = self.nodes.get(*left).unwrap();
                    let right_node = self.nodes.get(*right).unwrap();
                    left_node
                        .z_index()
                        .cmp(&right_node.z_index())
                        .then_with(|| {
                            fn position_to_order(pos: Position) -> u8 {
                                match pos {
                                    Position::Static | Position::Relative | Position::Sticky => 0,
                                    Position::Absolute | Position::Fixed => 1,
                                }
                            }
                            let left_position = left_node
                                .primary_styles()
                                .map(|s| position_to_order(s.clone_position()))
                                .unwrap_or(0);
                            let right_position = right_node
                                .primary_styles()
                                .map(|s| position_to_order(s.clone_position()))
                                .unwrap_or(0);

                            left_position.cmp(&right_position)
                        })
                })
        }
    }
}
