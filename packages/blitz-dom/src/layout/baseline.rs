//! The alignment baseline of an atomic inline (html-to-pdf F183c).
//!
//! CSS 2.1 §10.8.1: the baseline of an `inline-block` is the baseline of its last line box in the normal flow. When it has no
//! in-flow line box, or its `overflow` computes to something that makes it a scroll container (`hidden`, `scroll`, `auto`), its
//! bottom margin edge is the baseline. `overflow: clip` does not make a scroll container (css-overflow-3 §3.1) and Chrome
//! gives such a box the baseline of its last line like `visible` (measured, m153).
//!
//! Parley places an inline box by `InlineBox::baseline`, a distance from the top of its margin box. This module computes that
//! distance from the layout of the box's subtree, so the subtree must have been laid out (`RunMode::PerformLayout`) before.

use taffy::{Overflow, Position, ResolveOrZero as _};

use super::resolve_calc_value;
use crate::BaseDocument;

/// Deeper than any document that matters; the walk gives up (no baseline) beyond it.
const MAX_DEPTH: usize = 512;

/// The distance from the border-box top of the atomic inline `node_id` to its alignment baseline, in CSS px, or `None` when the
/// bottom margin edge is the baseline (no in-flow line box, a scroll container, a replaced element).
///
/// `parent_width` resolves percentage paddings of the box itself, as the placement of the box does.
pub(crate) fn atomic_inline_baseline(
    doc: &BaseDocument,
    node_id: usize,
    parent_width: Option<f32>,
) -> Option<f32> {
    let node = &doc.nodes[node_id];
    if makes_scroll_container(&node.style) {
        return None;
    }
    let padding = node
        .style
        .padding
        .resolve_or_zero(parent_width, resolve_calc_value);
    let border = node
        .style
        .border
        .resolve_or_zero(parent_width, resolve_calc_value);
    last_line_baseline(doc, node_id, border.top + padding.top, 0)
}

/// Whether the box lays its content out in a box that clips and scrolls it, which sets its baseline to the bottom edge.
/// Taffy has no `auto`: Blitz converts it to `Scroll`.
fn makes_scroll_container(style: &taffy::Style<style::Atom>) -> bool {
    let scrolls = |o: Overflow| matches!(o, Overflow::Hidden | Overflow::Scroll);
    scrolls(style.overflow.x) || scrolls(style.overflow.y)
}

/// Whether the box is out of the normal flow and so has no say in the baseline of its container.
fn out_of_flow(doc: &BaseDocument, node_id: usize) -> bool {
    use style::computed_values::float::T as Float;

    let node = &doc.nodes[node_id];
    if node.style.position == Position::Absolute {
        return true;
    }
    node.primary_styles()
        .is_some_and(|s| s.clone_float() != Float::None)
}

/// The baseline of the last line box in the normal flow of `node_id`, from its border-box top, in CSS px.
///
/// `top_inset` is the node's border-top plus padding-top. An inline root gives that plus the baseline of its last line. Any other
/// node walks its in-flow layout children from the last: the first one that has a baseline gives its own offset plus that
/// baseline. A float or an absolutely positioned child is skipped.
pub(crate) fn last_line_baseline(
    doc: &BaseDocument,
    node_id: usize,
    top_inset: f32,
    depth: usize,
) -> Option<f32> {
    if depth >= MAX_DEPTH {
        return None;
    }
    let node = &doc.nodes[node_id];
    // An inline root: the baseline of its last line. Parley's `baseline` is measured from the top of the layout.
    if let Some(text) = node
        .element_data()
        .and_then(|el| el.inline_layout_data.as_ref())
    {
        if let Some(line) = text.layout.lines().last() {
            return Some(top_inset + line.metrics().baseline / doc.viewport.scale());
        }
    }
    // Blitz's `layout_children` is authoritative about what is in flow: an explicit empty list means none.
    let layout_children = node.layout_children.borrow();
    let children: &[usize] = layout_children.as_deref().unwrap_or(&node.children);
    for &child_id in children.iter().rev() {
        if out_of_flow(doc, child_id) {
            continue;
        }
        // Only the box that is itself the atomic inline loses its baseline to `overflow` (CSS 2.1 §10.8.1); a nested block
        // keeps the baseline of its lines, as it did when fulgur walked the tree.
        let layout = &doc.nodes[child_id].unrounded_layout;
        let inset = layout.border.top + layout.padding.top;
        if let Some(inner) = last_line_baseline(doc, child_id, inset, depth + 1) {
            return Some(layout.location.y + inner);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use parley::fontique::{Blob, Collection, CollectionOptions, SourceCache};
    use taffy::{
        AvailableSpace, LayoutInput, LayoutPartialTree as _, NodeId, RunMode, Size, SizingMode,
    };

    use crate::{Attribute, BaseDocument, DocumentConfig, FontContext, QualName, ns};

    enum N {
        /// `tag#name`: the element `tag`, found in the id map as `name` (or `tag` without a `#`).
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
                let (tag, name) = tag.split_once('#').unwrap_or((tag, tag));
                let id = m.create_element(QualName::new(None, ns!(html), tag.into()), attrs);
                ids.insert(name.to_string(), id);
                let kids: Vec<usize> = kids.iter().map(|k| build(m, k, ids)).collect();
                m.append_children(id, &kids);
                id
            }
        }
    }

    /// `<body style="margin:0">` with `kids`, laid out; every glyph of the bundled font is an em-wide bullet, the font is the
    /// only one (no host fonts). Text is 20px/20px.
    fn laid_out(kids: Vec<N>) -> (BaseDocument, HashMap<String, usize>) {
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
                    vec![E(
                        "body",
                        "margin:0;font-family:Bullet;font-size:20px;line-height:20px;width:400px",
                        kids,
                    )],
                ),
                &mut ids,
            );
            m.append_children(0, &[html]);
        }
        doc.resolve(0.0);
        (doc, ids)
    }

    const IB: &str = "display:inline-block;width:100px;";

    /// The baseline of the box `name`, as the inline root that holds it was given it.
    fn baseline_of(doc: &BaseDocument, id: usize) -> Option<f32> {
        super::atomic_inline_baseline(doc, id, Some(400.0))
    }

    fn ib(style: &'static str, kids: Vec<N>) -> N {
        E("div#box", style, kids)
    }

    #[test]
    fn the_baseline_is_that_of_the_last_line() {
        // Two lines of 20px: the last line's baseline is below the first line's.
        let (doc, ids) = laid_out(vec![ib(
            "display:inline-block;width:100px;",
            vec![T("a"), E("br", "", vec![]), T("b")],
        )]);
        let one = laid_out(vec![ib(IB, vec![T("a")])]);
        let two = baseline_of(&doc, ids["box"]).expect("a baseline");
        let first = baseline_of(&one.0, one.1["box"]).expect("a baseline");
        assert!((two - first - 20.0).abs() < 0.01, "{two} vs {first}");
    }

    #[test]
    fn border_and_padding_top_move_the_baseline_down() {
        let plain = laid_out(vec![ib(IB, vec![T("a")])]);
        let padded = laid_out(vec![ib(
            "display:inline-block;width:100px;padding-top:7px;border-top:3px solid #000;",
            vec![T("a")],
        )]);
        let a = baseline_of(&plain.0, plain.1["box"]).unwrap();
        let b = baseline_of(&padded.0, padded.1["box"]).unwrap();
        assert!((b - a - 10.0).abs() < 0.01, "{a} {b}");
    }

    #[test]
    fn an_empty_box_has_no_baseline() {
        let (doc, ids) = laid_out(vec![ib(
            "display:inline-block;width:100px;height:48px;",
            vec![],
        )]);
        assert_eq!(baseline_of(&doc, ids["box"]), None);
    }

    #[test]
    fn hidden_scroll_and_auto_give_the_bottom_edge_but_clip_and_visible_a_line() {
        for (overflow, has) in [
            ("visible", true),
            ("clip", true),
            ("overflow-x:clip;overflow-y:clip", true),
            ("hidden", false),
            ("scroll", false),
            ("auto", false),
            ("overflow-x:hidden", false),
            ("overflow-y:auto", false),
        ] {
            let style: &'static str = Box::leak(
                format!("display:inline-block;width:100px;height:60px;{}", {
                    if overflow.contains(':') {
                        overflow.to_string()
                    } else {
                        format!("overflow:{overflow}")
                    }
                })
                .into_boxed_str(),
            );
            let (doc, ids) = laid_out(vec![ib(style, vec![T("a")])]);
            assert_eq!(
                baseline_of(&doc, ids["box"]).is_some(),
                has,
                "overflow {overflow}"
            );
        }
    }

    #[test]
    fn the_last_block_child_with_a_line_gives_the_baseline() {
        // `box > div#a "a", div#b "b"`: the baseline is the second block's line, one line (20px) below the first.
        let (doc, ids) = laid_out(vec![ib(
            IB,
            vec![E("div#a", "", vec![T("a")]), E("div#b", "", vec![T("b")])],
        )]);
        let one = laid_out(vec![ib(IB, vec![T("a")])]);
        let first = baseline_of(&one.0, one.1["box"]).unwrap();
        let got = baseline_of(&doc, ids["box"]).unwrap();
        assert!((got - first - 20.0).abs() < 0.01, "{got} {first}");
    }

    #[test]
    fn a_childless_last_block_has_no_line_and_is_walked_past() {
        let (doc, ids) = laid_out(vec![ib(
            IB,
            vec![
                E("div#a", "", vec![T("a")]),
                E("div#empty", "height:30px", vec![]),
            ],
        )]);
        let one = laid_out(vec![ib(IB, vec![T("a")])]);
        let first = baseline_of(&one.0, one.1["box"]).unwrap();
        assert_eq!(baseline_of(&doc, ids["box"]), Some(first));
    }

    #[test]
    fn a_float_or_absolutely_positioned_last_child_is_skipped() {
        let one = laid_out(vec![ib(IB, vec![T("a")])]);
        let first = baseline_of(&one.0, one.1["box"]).unwrap();
        for last in ["float:left", "position:absolute", "position:fixed"] {
            let style: &'static str = Box::leak(format!("{last};height:30px").into_boxed_str());
            let (doc, ids) = laid_out(vec![ib(
                IB,
                vec![
                    E("div#a", "", vec![T("a")]),
                    E("div#last", style, vec![T("z")]),
                ],
            )]);
            assert_eq!(baseline_of(&doc, ids["box"]), Some(first), "{last}");
        }
    }

    #[test]
    fn the_walk_stops_at_the_depth_limit() {
        let (doc, ids) = laid_out(vec![ib(IB, vec![T("a")])]);
        assert_eq!(
            super::last_line_baseline(&doc, ids["box"], 0.0, super::MAX_DEPTH),
            None
        );
    }

    /// Parley seats the box with its baseline on the line's: the box top plus its baseline is the baseline of the line.
    #[test]
    fn the_box_sits_by_its_baseline_on_the_line_of_the_text_around_it() {
        let (doc, ids) = laid_out(vec![E(
            "p#p",
            "margin:0",
            vec![
                T("ab "),
                ib(
                    "display:inline-block;width:100px;height:60px;",
                    vec![T("x")],
                ),
                T(" cd"),
            ],
        )]);
        let p = &doc.nodes[ids["p"]];
        let line = p
            .element_data()
            .unwrap()
            .inline_layout_data
            .as_ref()
            .unwrap()
            .layout
            .lines()
            .next()
            .unwrap();
        let line_baseline = line.metrics().baseline / doc.viewport.scale();
        let inner = baseline_of(&doc, ids["box"]).unwrap();
        let top = doc.nodes[ids["box"]].unrounded_layout.location.y;
        assert!(
            (top + inner - line_baseline).abs() < 0.01,
            "box top {top} + baseline {inner} vs line baseline {line_baseline}"
        );
        // The text is at the box's top: the box hangs below the line's text, it is not sitting on it.
        assert!(top < line_baseline - 5.0, "{top} {line_baseline}");
    }

    /// The height of the inline root that holds a 60px box with a line at its top, measured (as a grid item, a table cell, a
    /// flex item are) and laid out, is the same.
    #[test]
    fn measured_and_laid_out_height_of_the_root_agree_in_grid_items_and_table_cells() {
        let content = || {
            vec![
                T("ab "),
                ib(
                    "display:inline-block;width:100px;height:60px;",
                    vec![T("x"), E("br", "", vec![]), T("y")],
                ),
                T(" cd"),
            ]
        };
        let (block, ids) = laid_out(vec![E("p#p", "margin:0", content())]);
        let in_block = block.nodes[ids["p"]].final_layout.size.height;
        let (grid, ids) = laid_out(vec![E(
            "div#g",
            "display:grid;grid-template-columns:200px 200px",
            vec![
                E("p#p", "margin:0", content()),
                E("p", "margin:0", vec![T("z")]),
            ],
        )]);
        let in_grid = grid.nodes[ids["p"]].final_layout.size.height;
        let (table, ids) = laid_out(vec![E(
            "div#t",
            "display:table",
            vec![E(
                "div",
                "display:table-row",
                vec![E("div#p", "display:table-cell", content())],
            )],
        )]);
        let in_cell = table.nodes[ids["p"]].final_layout.size.height;
        assert!(in_block >= 60.0, "{in_block}");
        assert_eq!(in_grid, in_block, "grid item");
        assert_eq!(in_cell, in_block, "table cell");

        // Measured: the same root asked for its size alone gives the height it is laid out with.
        let p = NodeId::from(ids["p"]);
        let (mut doc, ids) = laid_out(vec![E("p#p", "margin:0", content())]);
        let measured = doc.compute_child_layout(
            NodeId::from(ids["p"]),
            LayoutInput {
                known_dimensions: Size::NONE,
                parent_size: Size {
                    width: Some(400.0),
                    height: None,
                },
                available_space: Size {
                    width: AvailableSpace::Definite(400.0),
                    height: AvailableSpace::MaxContent,
                },
                sizing_mode: SizingMode::InherentSize,
                axis: taffy::RequestedAxis::Both,
                run_mode: RunMode::ComputeSize,
                vertical_margins_are_collapsible: taffy::Line::FALSE,
            },
        );
        let _ = p;
        assert_eq!(measured.size.height, in_block, "measured");
    }

    /// A measuring pass at another width lays the box out at that width: its lines wrap differently there, and the baseline
    /// (so the height of the root) is that of the measured layout, not that of the earlier layout of the document.
    #[test]
    fn a_measuring_pass_at_another_width_reads_the_baseline_of_the_box_at_that_width() {
        let content = || {
            vec![
                T("ab "),
                ib("display:inline-block;height:20px;", vec![T("a b c d")]),
                T(" cd"),
            ]
        };
        // The truth: the root laid out at 30px (the box wraps to one word per line).
        let (narrow, ids) = laid_out(vec![E("p#p", "margin:0;width:30px", content())]);
        let truth = narrow.nodes[ids["p"]].final_layout.size.height;
        // The same root laid out wide, then measured at 30px.
        let (mut doc, ids) = laid_out(vec![E("p#p", "margin:0", content())]);
        let wide = doc.nodes[ids["p"]].final_layout.size.height;
        let measured = doc.compute_child_layout(
            NodeId::from(ids["p"]),
            LayoutInput {
                known_dimensions: Size {
                    width: Some(30.0),
                    height: None,
                },
                parent_size: Size {
                    width: Some(400.0),
                    height: None,
                },
                available_space: Size {
                    width: AvailableSpace::Definite(30.0),
                    height: AvailableSpace::MaxContent,
                },
                sizing_mode: SizingMode::InherentSize,
                axis: taffy::RequestedAxis::Both,
                run_mode: RunMode::ComputeSize,
                vertical_margins_are_collapsible: taffy::Line::FALSE,
            },
        );
        assert_ne!(truth, wide, "the probe must wrap differently");
        assert_eq!(measured.size.height, truth, "measured at 30px");
    }
}
