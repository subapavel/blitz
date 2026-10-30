use parley::AlignmentOptions;
use taffy::{
    AvailableSpace, LayoutPartialTree as _, MaybeMath as _, MaybeResolve as _, NodeId, Position,
    ResolveOrZero as _, Size, compute_leaf_layout,
};

use super::resolve_calc_value;
use crate::BaseDocument;

/// Breaks `layout` into lines no wider than `width`: [`parley::Layout::break_all_lines`] without the
/// empty line Parley 0.6 adds after an inline box that overflows the line and ends the paragraph.
///
/// A box that does not fit at the start of a line is committed on that line anyway ("consume it and
/// accept the overflow", CSS 2.1 §9.4.2: an unbreakable inline box wider than the line box overflows
/// it). Parley's `greedy.rs` then takes one more `break_next` step, which finds no items left and
/// commits an empty line with the previous line's metrics. That line has no content, but it doubles the
/// paragraph's height, and a paragraph of two lines is fragmented differently from one of one line. It
/// is recognised here as a final line of zero advance directly after an overflowing one, and taken back
/// with [`parley::BreakLines::revert`]; a real trailing empty line (after a preserved newline) follows a
/// line holding the newline, which does not overflow.
///
/// The same empty line after a forced break at the end of the text is taken back too: a `<br>` or a preserved newline
/// ends its line and creates no line of its own when nothing follows it in the block (CSS 2.1 §9.4.2: a line box with no
/// text, no preserved white space and no other in-flow content is treated as zero-height; html-to-pdf F66f, F37d). The
/// text then ends in a newline cluster, the breaker is done and the line has no advance. Anything on that line keeps it:
/// preserved spaces (`pre`, `pre-wrap`) and a no-break space have an advance, an inline box (even one without width) sits
/// at the end of the text. A block of only `<br>` keeps the line that holds the newline.
///
/// With `nowrap` (`text-wrap-mode: nowrap`) the lines are broken at an unbounded width, as Parley's own
/// `break_all_lines(None)` does: only forced breaks (`<br>`, a preserved newline) end a line, and a line wider
/// than `width` overflows it (CSS Text 3 §3, §7.1). The line is still aligned against the box, see the caller.
fn break_lines(
    layout: &mut parley::Layout<crate::node::TextBrush>,
    text: &str,
    width: f32,
    nowrap: bool,
    content_after_last_break: bool,
) {
    // Parley's `Whitespace::Newline`: `\n` and `\r` (a `\r\n` pair is one cluster).
    let ends_in_newline = text.ends_with(['\n', '\r']);
    let box_after_text = layout.inline_boxes().iter().any(|b| b.index >= text.len());
    let width = if nowrap { f32::MAX } else { width };
    let mut breaker = layout.break_lines();
    let mut previous_overflowed = false;
    while let Some((advance, _)) = breaker.break_next(width) {
        if breaker.is_done()
            && advance == 0.0
            && (previous_overflowed
                || (ends_in_newline && !box_after_text && !content_after_last_break))
        {
            breaker.revert();
            break;
        }
        previous_overflowed = advance > width;
    }
    breaker.finish();
}

impl BaseDocument {
    pub(crate) fn compute_inline_layout(
        &mut self,
        node_id: usize,
        inputs: taffy::tree::LayoutInput,
    ) -> taffy::LayoutOutput {
        let scale = self.viewport.scale();

        // Take inline layout to satisfy borrow checker
        let mut inline_layout = self.nodes[node_id]
            .data
            .downcast_element_mut()
            .unwrap()
            .take_inline_layout()
            .unwrap();

        // TODO: eliminate clone
        let style = self.nodes[node_id].style.clone();

        let output = compute_leaf_layout(
            inputs,
            &style,
            resolve_calc_value,
            |_known_dimensions, available_space| {
                // Short circuit if inline context contains no text or inline boxes
                if inline_layout.text.is_empty() && inline_layout.layout.inline_boxes().is_empty() {
                    return Size::ZERO;
                }

                // Compute size of inline boxes
                let child_inputs = taffy::tree::LayoutInput {
                    known_dimensions: Size::NONE,
                    available_space,
                    parent_size: available_space.into_options(),
                    ..inputs
                };
                for ibox in inline_layout.layout.inline_boxes_mut() {
                    let style = &self.nodes[ibox.id as usize].style;
                    let margin = style
                        .margin
                        .resolve_or_zero(inputs.parent_size, resolve_calc_value);

                    if style.position == Position::Absolute {
                        ibox.width = 0.0;
                        ibox.height = 0.0;
                    } else {
                        let output = self.compute_child_layout(NodeId::from(ibox.id), child_inputs);
                        ibox.width = (margin.left + margin.right + output.size.width) * scale;
                        ibox.height = (margin.top + margin.bottom + output.size.height) * scale;
                    }
                }

                // Determine width
                let padding = style
                    .padding
                    .resolve_or_zero(inputs.parent_size, resolve_calc_value);
                let border = style
                    .border
                    .resolve_or_zero(inputs.parent_size, resolve_calc_value);
                let container_pb = padding + border;
                let pbw = container_pb.horizontal_components().sum() * scale;

                let width = inputs
                    .known_dimensions
                    .width
                    .map(|w| (w * scale) - pbw)
                    .unwrap_or_else(|| {
                        // TODO: Cache content widths.
                        //
                        // This is a little tricky as the size of the inline boxes may depend on whether we are sizing under
                        // and a min-content or max-content constraint. So if we want to compute both widths in one pass then
                        // we need to store both a min-content and max-content size on each box.
                        let mut content_sizes = inline_layout.layout.calculate_content_widths();
                        if inline_layout.nowrap {
                            // No soft wrap opportunity: the context cannot be narrower than its widest line
                            // (CSS Sizing 3 §5).
                            content_sizes.min = content_sizes.max;
                        }
                        let computed_width = match available_space.width {
                            AvailableSpace::MinContent => content_sizes.min,
                            AvailableSpace::MaxContent => content_sizes.max,
                            AvailableSpace::Definite(limit) => (limit * scale)
                                .min(content_sizes.max)
                                .max(content_sizes.min),
                        }
                        .ceil();
                        let style_width = style
                            .size
                            .width
                            .maybe_resolve(inputs.parent_size.width, resolve_calc_value)
                            .map(|w| w * scale);
                        let min_width = style
                            .min_size
                            .width
                            .maybe_resolve(inputs.parent_size.width, resolve_calc_value)
                            .map(|w| w * scale);
                        let max_width = style
                            .max_size
                            .width
                            .maybe_resolve(inputs.parent_size.width, resolve_calc_value)
                            .map(|w| w * scale);

                        (style_width)
                            .unwrap_or(computed_width + pbw)
                            .max(computed_width)
                            .maybe_clamp(min_width, max_width)
                            - pbw
                    });

                if inputs.run_mode == taffy::RunMode::ComputeSize {
                    // Height SHOULD be ignored if RequestedAxis is Horizontal, but currently that doesn't
                    // always seem to be the case. So we perform layout to obtain a height every time. We
                    // perform layout on a clone of the Layout to avoid clobbering the actual layout which
                    // was causing https://github.com/DioxusLabs/blitz/pull/247#issuecomment-3235111617
                    //
                    // Doing this does seem to be as slow as one might expect, and if it enables correct
                    // incremental layout then that is overall a big performance win.
                    //
                    // FIXME: avoid the need to clone the layout each time
                    let mut layout = inline_layout.clone();
                    break_lines(
                        &mut layout.layout,
                        &layout.text,
                        width,
                        layout.nowrap,
                        layout.content_after_last_break,
                    );

                    return taffy::Size {
                        width: width.ceil() / scale,
                        height: layout.layout.height() / scale,
                    };
                }

                // Perform inline layout
                break_lines(
                    &mut inline_layout.layout,
                    &inline_layout.text,
                    width,
                    inline_layout.nowrap,
                    inline_layout.content_after_last_break,
                );

                let alignment = self.nodes[node_id]
                    .primary_styles()
                    .map(|s| {
                        use parley::layout::Alignment;
                        use style::values::specified::TextAlignKeyword;

                        match s.clone_text_align() {
                            TextAlignKeyword::Start => Alignment::Start,
                            TextAlignKeyword::Left => Alignment::Left,
                            TextAlignKeyword::Right => Alignment::Right,
                            TextAlignKeyword::Center => Alignment::Center,
                            TextAlignKeyword::Justify => Alignment::Justify,
                            TextAlignKeyword::End => Alignment::End,
                            TextAlignKeyword::MozCenter => Alignment::Center,
                            TextAlignKeyword::MozLeft => Alignment::Left,
                            TextAlignKeyword::MozRight => Alignment::Right,
                        }
                    })
                    .unwrap_or(parley::layout::Alignment::Start);

                inline_layout.layout.align(
                    Some(width),
                    alignment,
                    AlignmentOptions {
                        align_when_overflowing: false,
                    },
                );

                // Store sizes and positions of inline boxes
                for line in inline_layout.layout.lines() {
                    for item in line.items() {
                        if let parley::layout::PositionedLayoutItem::InlineBox(ibox) = item {
                            let node = &mut self.nodes[ibox.id as usize];
                            let padding = node
                                .style
                                .padding
                                .resolve_or_zero(child_inputs.parent_size, resolve_calc_value);
                            let border = node
                                .style
                                .border
                                .resolve_or_zero(child_inputs.parent_size, resolve_calc_value);
                            let margin = node
                                .style
                                .margin
                                .resolve_or_zero(child_inputs.parent_size, resolve_calc_value);

                            // Resolve inset
                            let left = node
                                .style
                                .inset
                                .left
                                .maybe_resolve(child_inputs.parent_size.width, resolve_calc_value);
                            let right = node
                                .style
                                .inset
                                .right
                                .maybe_resolve(child_inputs.parent_size.width, resolve_calc_value);
                            let top = node
                                .style
                                .inset
                                .top
                                .maybe_resolve(child_inputs.parent_size.height, resolve_calc_value);
                            let bottom = node
                                .style
                                .inset
                                .bottom
                                .maybe_resolve(child_inputs.parent_size.height, resolve_calc_value);

                            if node.style.position == Position::Absolute {
                                let output =
                                    self.compute_child_layout(NodeId::from(ibox.id), child_inputs);

                                let layout = &mut self.nodes[ibox.id as usize].unrounded_layout;
                                layout.size = output.size;

                                // TODO: Implement absolute positioning
                                layout.location.x = left
                                    .or_else(|| {
                                        child_inputs
                                            .parent_size
                                            .width
                                            .zip(right)
                                            .map(|(w, r)| w - r)
                                    })
                                    .unwrap_or((ibox.x / scale) + margin.left + container_pb.left);
                                layout.location.y = top
                                    .or_else(|| {
                                        child_inputs
                                            .parent_size
                                            .height
                                            .zip(bottom)
                                            .map(|(w, r)| w - r)
                                    })
                                    .unwrap_or((ibox.y / scale) + margin.top + container_pb.top);

                                layout.padding = padding; //.map(|p| p / scale);
                                layout.border = border; //.map(|p| p / scale);
                            } else {
                                let layout = &mut node.unrounded_layout;
                                layout.size.width =
                                    (ibox.width / scale) - margin.left - margin.right;
                                layout.size.height =
                                    (ibox.height / scale) - margin.top - margin.bottom;
                                layout.location.x =
                                    (ibox.x / scale) + margin.left + container_pb.left;
                                layout.location.y =
                                    (ibox.y / scale) + margin.top + container_pb.top;
                                layout.padding = padding; //.map(|p| p / scale);
                                layout.border = border; //.map(|p| p / scale);
                            }
                        }
                    }
                }

                // println!("INLINE LAYOUT FOR {:?}. max_advance: {:?}", node_id, max_advance);
                // dbg!(&inline_layout.text);
                // println!("Computed: w: {} h: {}", inline_layout.layout.width(), inline_layout.layout.height());
                // println!("known_dimensions: w: {:?} h: {:?}", inputs.known_dimensions.width, inputs.known_dimensions.height);
                // println!("\n");

                inputs.known_dimensions.unwrap_or(taffy::Size {
                    width: inline_layout.layout.width().ceil() / scale,
                    height: inline_layout.layout.height() / scale,
                })
            },
        );

        // Put layout back
        self.nodes[node_id]
            .data
            .downcast_element_mut()
            .unwrap()
            .inline_layout_data = Some(inline_layout);

        output
    }
}

/// The lines of an inline formatting context after layout: a forced break at the end of a block ends its line and
/// creates no line of its own (CSS 2.1 §9.4.2, CSS Text 3 §4.1.3), where Parley commits an empty one.
#[cfg(test)]
mod trailing_forced_break_tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use parley::fontique::{Blob, Collection, CollectionOptions, SourceCache};

    use crate::{Attribute, BaseDocument, DocumentConfig, FontContext, QualName, ns};

    enum N {
        E(&'static str, &'static str, Vec<N>),
        T(&'static str),
    }
    use N::{E, T};

    fn build(m: &mut crate::DocumentMutator<'_>, n: &N, ids: &mut HashMap<String, usize>) -> usize {
        match n {
            T(text) => m.create_text_node(text),
            E(tag, style, kids) => {
                let mut attrs = Vec::new();
                if !style.is_empty() {
                    attrs.push(Attribute {
                        name: QualName::new(None, ns!(), "style".into()),
                        value: style.to_string(),
                    });
                }
                let id = m.create_element(QualName::new(None, ns!(html), (*tag).into()), attrs);
                ids.insert((*tag).to_string(), id);
                let kids: Vec<usize> = kids.iter().map(|k| build(m, k, ids)).collect();
                m.append_children(id, &kids);
                id
            }
        }
    }

    /// The document of `<div style="font: 20px/20px Bullet; white-space: {white_space}">{kids}</div>` laid out, with the
    /// node ids by tag name (the last element of a tag wins). The bundled bullet face is the only font: its glyphs do not
    /// matter, the clusters (and the newline among them) do.
    fn laid_out(white_space: &str, kids: Vec<N>) -> (BaseDocument, HashMap<String, usize>) {
        let style: &'static str = Box::leak(
            format!("font-family:Bullet;font-size:20px;line-height:20px;width:400px;white-space:{white_space}")
                .into_boxed_str(),
        );
        let mut ctx = FontContext {
            collection: Collection::new(CollectionOptions {
                system_fonts: false,
                ..Default::default()
            }),
            source_cache: SourceCache::new(Default::default()),
        };
        ctx.collection
            .register_fonts(Blob::new(Arc::new(crate::BULLET_FONT) as _), None);
        let mut doc = BaseDocument::new(DocumentConfig {
            font_ctx: Some(ctx),
            ..DocumentConfig::default()
        });
        let mut ids = HashMap::new();
        {
            let mut m = doc.mutate();
            let html = build(
                &mut m,
                &E(
                    "html",
                    "",
                    vec![E("body", "margin:0", vec![E("div", style, kids)])],
                ),
                &mut ids,
            );
            m.append_children(0, &[html]);
        }
        doc.resolve(0.0);
        (doc, ids)
    }

    /// Lines and height (px) of `<div style="font: 20px/20px Bullet; white-space: {white_space}">{kids}</div>`.
    fn lines(white_space: &str, kids: Vec<N>) -> (usize, f32) {
        let (doc, ids) = laid_out(white_space, kids);
        let div = &doc.nodes[ids["div"]];
        let n = div
            .element_data()
            .and_then(|el| el.inline_layout_data.as_ref())
            .expect("the div is an inline root")
            .layout
            .lines()
            .count();
        (n, div.final_layout.size.height)
    }

    /// Height (px) of the outer `<div>` when its content has blocks in it (it is no inline root then), in lines of 20 px.
    fn height_in_lines(kids: Vec<N>) -> f32 {
        let (doc, ids) = laid_out("normal", kids);
        let outer = doc.nodes[ids["body"]].children[0];
        doc.nodes[outer].final_layout.size.height / 20.0
    }

    fn br() -> N {
        E("br", "", vec![])
    }

    fn check(white_space: &str, kids: Vec<N>, expect: usize) {
        let (n, h) = lines(white_space, kids);
        assert_eq!(n, expect, "lines");
        assert_eq!(h, 20.0 * expect as f32, "height");
    }

    #[test]
    fn a_br_at_the_end_makes_no_line() {
        check("normal", vec![T("a"), br(), T("b"), br()], 2);
        check("normal", vec![T("a"), br()], 1);
    }

    #[test]
    fn two_brs_at_the_end_keep_two_lines() {
        check("normal", vec![T("a"), br(), br()], 2);
        check("normal", vec![T("a"), br(), T("b"), br(), br(), br()], 4);
    }

    #[test]
    fn a_block_of_only_brs_has_a_line_per_br() {
        check("normal", vec![br()], 1);
        check("normal", vec![br(), br(), br()], 3);
    }

    #[test]
    fn a_trailing_newline_under_pre_makes_no_line() {
        check("pre", vec![T("a\nb\n")], 2);
        check("pre", vec![T("a\n\n")], 2);
        check("pre", vec![T("\n")], 1);
        check("pre-wrap", vec![T("a   \n")], 1);
        check("pre-line", vec![T("\n    Firma\n    Ulice 1\n")], 3);
    }

    #[test]
    fn an_empty_line_in_the_middle_stays() {
        check("pre", vec![T("a\n\nb")], 3);
        check("pre-line", vec![T("\nx")], 2);
    }

    #[test]
    fn anything_on_the_last_line_keeps_it() {
        // Preserved spaces, a no-break space.
        check("pre", vec![T("a\n  ")], 2);
        check("pre-wrap", vec![T("a\n   ")], 2);
        check("normal", vec![T("a"), br(), T("\u{a0}")], 2);
        // An inline box, even with no width.
        let ib = || E("span", "display:inline-block;width:0;height:30px", vec![]);
        let (n, _) = lines("normal", vec![T("a"), br(), ib()]);
        assert_eq!(n, 2);
    }

    #[test]
    fn an_inline_box_with_inline_axis_margin_padding_or_border_after_the_break_keeps_the_line() {
        // CSS 2.1 §9.4.2: a line with an inline element with non-zero margin, padding or border is not zero-height.
        for style in [
            "padding:0 4px",
            "padding-left:3px",
            "border-left:1px solid #000",
            "border-right:2px solid #000",
            "margin:0 4px",
            "margin-right:5px",
            "padding:1%",
        ] {
            let span = || E("span", style, vec![]);
            let (n, h) = lines("normal", vec![T("a"), br(), span()]);
            assert_eq!((n, h), (2, 40.0), "<span style=\"{style}\"> after the br");
            // White space in it collapses away; the box still holds the line.
            let (n, _) = lines("normal", vec![T("a"), br(), E("span", style, vec![T(" ")])]);
            assert_eq!(n, 2, "<span style=\"{style}\"> </span> after the br");
        }
        // Under `pre` the preserved newline ends the line the same way.
        let (n, _) = lines("pre", vec![T("a\n"), E("span", "padding:0 4px", vec![])]);
        assert_eq!(n, 2, "pre");
        // `<wbr>`, one or several.
        let (n, _) = lines("normal", vec![T("a"), br(), E("wbr", "", vec![])]);
        assert_eq!(n, 2, "wbr");
        let (n, _) = lines(
            "normal",
            vec![T("a"), br(), E("wbr", "", vec![]), E("wbr", "", vec![])],
        );
        assert_eq!(n, 2, "two wbr");
    }

    #[test]
    fn an_inline_box_without_inline_axis_margin_padding_or_border_after_the_break_is_no_line() {
        for style in [
            "",
            "padding:4px 0",
            "margin:4px 0",
            "margin:0 auto",
            "padding:0",
            "border-left:0 solid #000",
            "border-left:3px none #000",
        ] {
            let (n, _) = lines("normal", vec![T("a"), br(), E("span", style, vec![])]);
            assert_eq!(n, 1, "<span style=\"{style}\"> after the br");
        }
        // The closing edge of a padded span that ends in the break is not a line either (Chrome).
        let (n, _) = lines(
            "normal",
            vec![T("a"), br(), E("span", "padding:0 4px", vec![T("x"), br()])],
        );
        assert_eq!(n, 2);
        // The box before the break is on the line the break ends.
        let (n, _) = lines(
            "normal",
            vec![T("a"), E("span", "padding:0 4px", vec![]), br()],
        );
        assert_eq!(n, 1);
    }

    #[test]
    fn an_open_inline_box_with_an_end_edge_keeps_the_line_when_a_box_starts_after_the_break() {
        // Chrome (m141, measured): the end edge of `<span style="padding-right:4px">a<br><span></span></span>` lands on
        // the line after the break, where it is inline content, so that line is not zero-height. Only the end side of
        // the box counts, and the end side follows the direction of the box itself.
        for style in [
            "padding-right:4px",
            "margin-right:4px",
            "border-right:1px solid #000",
            "margin-right:-4px",
            "padding-right:1%",
            "direction:rtl;padding-left:4px",
            "direction:rtl;margin-left:4px",
        ] {
            let mut cases: Vec<(&str, &str, Vec<N>)> = vec![
                (
                    "normal",
                    "empty span",
                    vec![T("a"), br(), E("span", "", vec![])],
                ),
                (
                    "normal",
                    "empty <i>",
                    vec![T("a"), br(), E("i", "", vec![])],
                ),
                (
                    "normal",
                    "span with a nested empty span",
                    vec![T("a"), br(), E("span", "", vec![E("b", "", vec![])])],
                ),
                (
                    "normal",
                    "white space around the empty span",
                    vec![T("a"), br(), T(" "), E("span", "", vec![]), T(" ")],
                ),
                (
                    "normal",
                    "two breaks",
                    vec![T("a"), br(), br(), E("span", "", vec![])],
                ),
            ];
            cases.push((
                "pre",
                "preserved newline",
                vec![T("a\n"), E("span", "", vec![])],
            ));
            for (white_space, name, kids) in cases {
                let (n, h) = lines(white_space, vec![E("span", style, kids)]);
                let expected = if name == "two breaks" { 3 } else { 2 };
                assert_eq!(
                    (n, h),
                    (expected, 20.0 * expected as f32),
                    "<span style=\"{style}\"> a<br> {name}"
                );
            }
            // The box that holds the end edge need not be the one the break is in.
            let (n, _) = lines(
                "normal",
                vec![E(
                    "span",
                    style,
                    vec![E("b", "", vec![T("a"), br()]), E("span", "", vec![])],
                )],
            );
            assert_eq!(n, 2, "<span style=\"{style}\"><b>a<br></b><span></span>");
            let (n, _) = lines(
                "normal",
                vec![E(
                    "span",
                    style,
                    vec![E("b", "", vec![T("a"), br(), E("span", "", vec![])])],
                )],
            );
            assert_eq!(n, 2, "<span style=\"{style}\"><b>a<br><span></span></b>");
        }
    }

    #[test]
    fn an_end_edge_without_a_box_that_starts_after_the_break_is_no_line() {
        // The close tags that follow the break directly belong to the line it ends (Chrome: no line).
        let pad = "padding-right:4px";
        let (n, _) = lines("normal", vec![E("span", pad, vec![T("a"), br()])]);
        assert_eq!(n, 1, "<span pad>a<br></span>");
        let (n, _) = lines(
            "normal",
            vec![E("span", "", vec![E("span", pad, vec![T("a"), br()])])],
        );
        assert_eq!(n, 1, "<span><span pad>a<br></span></span>");
        // A box that starts after the end edge closed.
        let (n, _) = lines(
            "normal",
            vec![E("span", pad, vec![T("a"), br()]), E("span", "", vec![])],
        );
        assert_eq!(n, 1, "<span pad>a<br></span><span></span>");
        let (n, _) = lines(
            "normal",
            vec![E(
                "span",
                "",
                vec![E("span", pad, vec![T("a"), br()]), E("span", "", vec![])],
            )],
        );
        assert_eq!(n, 1, "<span><span pad>a<br></span><span></span></span>");
        // White space that collapses away is no box.
        let (n, _) = lines("normal", vec![E("span", pad, vec![T("a"), br(), T(" ")])]);
        assert_eq!(n, 1, "<span pad>a<br> </span>");
    }

    #[test]
    fn only_the_end_side_of_an_open_box_keeps_the_line_and_only_a_box_that_exists() {
        for style in [
            "",
            "padding-left:4px",
            "margin-left:4px",
            "border-left:1px solid #000",
            "padding-top:4px",
            "padding:4px 0",
            "margin-right:auto",
            "padding-right:0",
            "direction:rtl;padding-right:4px",
            "direction:rtl;margin-right:4px",
        ] {
            let (n, _) = lines(
                "normal",
                vec![E("span", style, vec![T("a"), br(), E("span", "", vec![])])],
            );
            assert_eq!(n, 1, "<span style=\"{style}\">a<br><span></span></span>");
        }
        // Nothing after the break is a box: `display: none`, `display: contents`, a comment-like empty text.
        let pad = "padding-right:4px";
        for gone in ["display:none", "display:contents"] {
            let (n, _) = lines(
                "normal",
                vec![E("span", pad, vec![T("a"), br(), E("span", gone, vec![])])],
            );
            assert_eq!(n, 1, "<span pad>a<br><span style=\"{gone}\"></span></span>");
        }
    }

    #[test]
    fn collapsible_white_space_after_the_break_is_no_line() {
        check("normal", vec![T("a"), br(), T(" ")], 1);
        check("pre-line", vec![T("a\n   ")], 1);
        check("normal", vec![E("b", "", vec![T("x"), br()])], 1);
    }

    fn span(style: &'static str, kids: Vec<N>) -> N {
        E("span", style, kids)
    }

    fn block(text: &'static str) -> N {
        E(
            "div",
            "",
            if text.is_empty() {
                vec![]
            } else {
                vec![T(text)]
            },
        )
    }

    fn float() -> N {
        E("span", "float:left", vec![])
    }

    #[test]
    fn the_end_edge_of_a_box_with_a_block_in_it_makes_a_line_after_the_block() {
        // Chrome (m141): `<span style="padding-right:4px">a<br><div>blk</div></span>` splits the span around the block;
        // the part after the block is empty but has the end edge, so it is a line: a, blk, and that line. Blitz lays the
        // span out as a block, so the line after the block is not there; the line after the break stands in for it.
        let pad = "padding-right:4px";
        let cases: Vec<(&str, N, f32)> = vec![
            ("block", span(pad, vec![T("a"), br(), block("blk")]), 3.0),
            ("empty block", span(pad, vec![T("a"), br(), block("")]), 2.0),
            (
                "two blocks",
                span(pad, vec![T("a"), br(), block("blk"), block("b2")]),
                4.0,
            ),
            (
                "white space after the block",
                span(pad, vec![T("a"), br(), block("blk"), T("  ")]),
                3.0,
            ),
            (
                "empty box after the block",
                span(pad, vec![T("a"), br(), block("blk"), span("", vec![])]),
                3.0,
            ),
            (
                "inner box without an edge",
                span(pad, vec![span("", vec![T("a"), br(), block("blk")])]),
                3.0,
            ),
            (
                "both boxes with an edge",
                span(pad, vec![span(pad, vec![T("a"), br(), block("blk")])]),
                3.0,
            ),
            (
                "two lines of text, blocks between",
                span(
                    pad,
                    vec![T("a"), br(), block("blk"), T("b"), br(), block("b2")],
                ),
                5.0,
            ),
            ("float", span(pad, vec![T("a"), br(), float()]), 2.0),
            (
                "float after a block",
                span(pad, vec![T("a"), br(), block("blk"), float()]),
                3.0,
            ),
            (
                "inner box closed, float after it",
                span(pad, vec![span("", vec![T("a"), br()]), float()]),
                2.0,
            ),
            (
                "the edge on the left, rtl",
                span(
                    "direction:rtl;padding-left:4px",
                    vec![T("a"), br(), block("blk")],
                ),
                3.0,
            ),
        ];
        for (name, kids, lines) in cases {
            assert_eq!(height_in_lines(vec![kids]), lines, "{name}");
        }
    }

    #[test]
    fn a_box_with_a_block_in_it_and_the_break_in_its_last_part_follows_the_open_end_edge_rule() {
        // The part after the last block is an anonymous block of its own: the end edge of the box lands on it, and keeps
        // the line after the break when a box starts after it (the rule of the inline case above).
        let pad = "padding-right:4px";
        let cases: Vec<(&str, N, f32)> = vec![
            (
                "empty box after the break",
                span(pad, vec![block("blk"), T("a"), br(), span("", vec![])]),
                3.0,
            ),
            (
                "inner box without an edge",
                span(
                    pad,
                    vec![block("blk"), span("", vec![T("a"), br(), span("", vec![])])],
                ),
                3.0,
            ),
            (
                "box with an edge, nothing after the break",
                span(pad, vec![block("blk"), T("a"), br()]),
                2.0,
            ),
            (
                "the edge on the start side opens a line before the block",
                span("padding-left:4px", vec![block("blk"), T("a"), br()]),
                3.0,
            ),
            (
                "the same with a box after the break",
                span(
                    "padding-left:4px",
                    vec![block("blk"), T("a"), br(), span("", vec![])],
                ),
                3.0,
            ),
            (
                "a float before it is no line",
                span("padding-left:4px", vec![float(), T("a"), br()]),
                1.0,
            ),
        ];
        for (name, kids, lines) in cases {
            assert_eq!(height_in_lines(vec![kids]), lines, "{name}");
        }
    }

    #[test]
    fn a_box_with_a_block_in_it_and_no_edge_after_the_break_makes_no_line() {
        // Guards: these were right before the change and stay.
        let cases: Vec<(&str, N, f32)> = vec![
            ("no edge", span("", vec![T("a"), br(), block("blk")]), 2.0),
            (
                "start edge only, the line of the text",
                span("padding-left:4px", vec![T("a"), br(), block("blk")]),
                2.0,
            ),
            (
                "text after the block carries the edge",
                span(
                    "padding-right:4px",
                    vec![T("a"), br(), block("blk"), T("w")],
                ),
                3.0,
            ),
            (
                "a block container: nothing to carry",
                block_with_br_then_block(),
                2.0,
            ),
        ];
        for (name, kids, lines) in cases {
            assert_eq!(height_in_lines(vec![kids]), lines, "{name}");
        }
    }

    fn block_with_br_then_block() -> N {
        E("p", "margin:0", vec![T("a"), br(), block("blk")])
    }

    #[test]
    fn a_form_control_after_the_break_is_content() {
        // Chrome: `<select>`, `<meter>` and `<progress>` are atomic inlines with a height, so the line after the break is
        // not empty. Blitz lays them out as empty inline elements.
        for tag in ["select", "meter", "progress"] {
            check("normal", vec![T("a"), br(), E(tag, "", vec![])], 2);
            check(
                "normal",
                vec![
                    span("padding-right:4px", vec![T("a"), br()]),
                    E(tag, "", vec![]),
                ],
                2,
            );
        }
        // An empty `<span>` is not.
        check("normal", vec![T("a"), br(), E("span", "", vec![])], 1);
    }

    #[test]
    fn a_box_with_a_height_blitz_does_not_lay_out_keeps_the_line_before_it() {
        // Chrome: an empty `<details>` shows its default summary, an empty `display: list-item` with a marker has a line
        // for the marker. Neither has a height here, so the line after the break stands in for it. Where Blitz does lay
        // the content out (a summary, text, no marker) it is not added a second time.
        let li = |style: &'static str, kids: Vec<N>| E("span", style, kids);
        let cases: Vec<(&str, N, f32)> = vec![
            ("empty details", E("details", "", vec![]), 2.0),
            (
                "details with a summary",
                E("details", "", vec![E("summary", "", vec![T("s")])]),
                2.0,
            ),
            ("empty list item", li("display:list-item", vec![]), 2.0),
            (
                "list item without a marker",
                li("display:list-item;list-style:none", vec![]),
                1.0,
            ),
            (
                "list item with text",
                li("display:list-item", vec![T("t")]),
                2.0,
            ),
            ("empty block", block(""), 1.0),
        ];
        for (name, after, lines) in cases {
            let after_br = height_in_lines(vec![T("a"), br(), after]);
            assert_eq!(after_br, lines, "{name} after a<br>");
        }
        // Also when it is a few siblings later, and before the text.
        assert_eq!(
            height_in_lines(vec![T("a"), br(), block("blk"), E("details", "", vec![])]),
            3.0,
            "a<br><div>blk</div><details>"
        );
        assert_eq!(
            height_in_lines(vec![E("details", "", vec![]), T("a"), br()]),
            2.0,
            "<details></details>a<br>"
        );
    }
}
