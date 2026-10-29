//! White space processing across element boundaries (CSS Text 3 §4.1.1 and §4.1.3).
//!
//! Parley collapses the text of each style span on its own and trims both edges of every span, so the space at the
//! inner edge of an inline element (`<b>Splatnost: </b>2. 11.`) was deleted instead of collapsing with its
//! neighbours. The rule is defined over the whole inline formatting context, which only Blitz knows: it records the
//! context as a list of [`InlineOp`]s, [`collapse_white_space`] applies Phase I and the removal of spaces at the
//! start and the end of a line, and [`replay`] gives Parley the finished text in `WhiteSpaceCollapse::Preserve`.

use std::borrow::Cow;

use parley::{InlineBox, TextStyle, TreeBuilder, WhiteSpaceCollapse};
use style::values::specified::text::TextTransformCase;

use super::construct::{AT_WORD_START, transform_text};
use crate::node::TextBrush;
use crate::stylo_to_parley::stylo;

/// One step of building an inline formatting context.
pub(crate) enum InlineOp<'a> {
    /// An inline element (or a `display: contents` element that changes the text) opens a style span.
    PushSpan(TextStyle<'static, TextBrush>),
    PopSpan,
    /// The text of a text node (or of generated content) with the `white-space-collapse` and the case
    /// transformation of its parent. After [`collapse_white_space`] the text is final and `collapse` is not read
    /// any more; [`replay`] applies `case`.
    Text {
        text: Cow<'a, str>,
        collapse: stylo::WhiteSpaceCollapse,
        case: Option<TextTransformCase>,
    },
    /// The marker of a `list-style-position: inside` item. It is pushed as it is, and a space at its end
    /// collapses with the white space that follows.
    Marker(String),
    /// An atomic inline: `<img>`, an inline-block, a form control. `baseline_aligned` is set for a replaced
    /// element (`<img>`, inline `<svg>`) with `vertical-align: baseline`: Parley then keeps the block's strut
    /// on its line (see `strut_box` in `construct.rs`).
    InlineBox {
        id: u64,
        baseline_aligned: bool,
    },
    /// A `<br>` or a segment break of `white-space: pre-line`.
    ForcedBreak,
    /// An inline box that has no content of its own but keeps its line from being zero-height (CSS 2.1 §9.4.2): the
    /// start of an inline element with non-zero inline-axis margin, padding or border, or a `<wbr>`. It adds nothing
    /// to the text and is transparent for white space; [`line_content_after_last_break`] reads it.
    LineContent,
}

/// Whether the last line of the context, the one that follows its last forced break, holds an [`InlineOp::LineContent`]
/// box: `a<br><span style="padding: 0 4px"></span>`. `ops` is the output of [`collapse_white_space`]. A preserved
/// newline at the end of a text ends the line like a `<br>`.
pub(crate) fn line_content_after_last_break(ops: &[InlineOp<'_>]) -> bool {
    let mut content = false;
    for op in ops {
        match op {
            InlineOp::LineContent => content = true,
            InlineOp::ForcedBreak => content = false,
            InlineOp::Text { text, .. } if text.ends_with(['\n', '\r']) => content = false,
            _ => {}
        }
    }
    content
}

/// What the last thing emitted was, for the purpose of collapsing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Prev {
    /// The start of the context or of a line: collapsible spaces here are removed (§4.1.3).
    LineStart,
    /// A collapsible space: the next one collapses into it (§4.1.1).
    Space,
    /// Anything else: a letter, an atomic inline, a preserved space or an NBSP.
    Other,
}

struct Collapser<'a> {
    out: Vec<InlineOp<'a>>,
    prev: Prev,
    /// The index in `out` of the `Text` op whose last character is the most recently emitted collapsible space, if
    /// nothing but style spans came after it. It is deleted when it turns out to end a line.
    last_space: Option<usize>,
}

impl<'a> Collapser<'a> {
    /// Deletes the collapsible space that ends the line, if there is one.
    fn trim_line_end(&mut self) {
        if let Some(index) = self.last_space.take() {
            if let InlineOp::Text { text, .. } = &mut self.out[index] {
                let text = text.to_mut();
                debug_assert!(text.ends_with(' '));
                text.pop();
            }
        }
    }

    fn push_text(
        &mut self,
        text: Cow<'a, str>,
        collapse: stylo::WhiteSpaceCollapse,
        case: Option<TextTransformCase>,
    ) {
        if text.is_empty() {
            return;
        }
        self.out.push(InlineOp::Text {
            text,
            collapse,
            case,
        });
    }

    /// Collapses runs of white space in `text`, given what came before it. The result ends in a collapsible space
    /// only if the text did and the space was kept. `breaks` makes `\n` a forced break (`pre-line`).
    fn collapsible_text(
        &mut self,
        text: Cow<'a, str>,
        collapse: stylo::WhiteSpaceCollapse,
        case: Option<TextTransformCase>,
    ) {
        if text.is_empty() {
            return;
        }
        let breaks = collapse == stylo::WhiteSpaceCollapse::PreserveBreaks;

        // Most text needs no change: no run of white space, no tab, no newline, nothing at a line start.
        let mut prev = self.prev;
        let unchanged = !breaks
            && text.chars().all(|c| {
                let space = c.is_ascii_whitespace();
                let ok = !space || (c == ' ' && prev == Prev::Other);
                prev = if space { Prev::Space } else { Prev::Other };
                ok
            });
        if unchanged {
            self.prev = prev;
            if prev == Prev::Space {
                self.last_space = Some(self.out.len());
            } else {
                self.last_space = None;
            }
            self.push_text(text, collapse, case);
            return;
        }

        let mut piece = String::with_capacity(text.len());
        for c in text.chars() {
            if c == '\n' && breaks {
                // Spaces before a segment break are removed (§4.1.1), and so are the ones after it (line start).
                if piece.ends_with(' ') {
                    piece.pop();
                } else if piece.is_empty() {
                    self.trim_line_end();
                }
                self.last_space = None;
                let finished = std::mem::take(&mut piece);
                self.push_text(Cow::Owned(finished), collapse, case);
                self.out.push(InlineOp::ForcedBreak);
                self.prev = Prev::LineStart;
            } else if c.is_ascii_whitespace() {
                if self.prev == Prev::Other {
                    piece.push(' ');
                    self.prev = Prev::Space;
                }
            } else {
                piece.push(c);
                self.prev = Prev::Other;
            }
        }
        if piece.is_empty() {
            return;
        }
        self.last_space = if self.prev == Prev::Space {
            Some(self.out.len())
        } else {
            None
        };
        self.push_text(Cow::Owned(piece), collapse, case);
    }
}

/// Phase I of §4.1.1 (collapsing, across element boundaries) and the removal of collapsible spaces at the start and
/// the end of a line (§4.1.3), for the whole inline formatting context. `preserve-breaks` text is split at its
/// newlines into [`InlineOp::ForcedBreak`]s. The ops that come out hold the final text.
pub(crate) fn collapse_white_space<'a>(ops: Vec<InlineOp<'a>>) -> Vec<InlineOp<'a>> {
    let mut c = Collapser {
        out: Vec::with_capacity(ops.len() + 2),
        prev: Prev::LineStart,
        last_space: None,
    };
    for op in ops {
        match op {
            // Spans are transparent: the space inside a span and the one outside it collapse.
            InlineOp::PushSpan(_) | InlineOp::PopSpan | InlineOp::LineContent => c.out.push(op),
            InlineOp::Text {
                text,
                collapse,
                case,
            } => match collapse {
                stylo::WhiteSpaceCollapse::Collapse | stylo::WhiteSpaceCollapse::PreserveBreaks => {
                    c.collapsible_text(text, collapse, case);
                }
                // Preserved text is final. A newline at its end starts a line; otherwise a collapsible space after
                // it is kept. (TODO: `break-spaces` also wraps after preserved spaces.)
                stylo::WhiteSpaceCollapse::Preserve | stylo::WhiteSpaceCollapse::BreakSpaces => {
                    if !text.is_empty() {
                        c.prev = if text.ends_with('\n') {
                            Prev::LineStart
                        } else {
                            Prev::Other
                        };
                        c.last_space = None;
                        c.push_text(text, collapse, case);
                    }
                }
            },
            InlineOp::Marker(text) => {
                c.prev = if text.ends_with(|ch: char| ch.is_ascii_whitespace()) {
                    Prev::Space
                } else {
                    Prev::Other
                };
                c.last_space = None;
                c.out.push(InlineOp::Marker(text));
            }
            InlineOp::InlineBox { .. } => {
                c.prev = Prev::Other;
                c.last_space = None;
                c.out.push(op);
            }
            InlineOp::ForcedBreak => {
                c.trim_line_end();
                c.prev = Prev::LineStart;
                c.out.push(op);
            }
        }
    }
    c.trim_line_end();
    c.out
}

/// Gives the ops to Parley. The text is final, so Parley is told to keep it as it is.
pub(crate) fn replay(ops: Vec<InlineOp<'_>>, builder: &mut TreeBuilder<'_, TextBrush>) {
    builder.set_white_space_mode(WhiteSpaceCollapse::Preserve);
    for op in ops {
        match op {
            InlineOp::PushSpan(style) => builder.push_style_span(style),
            InlineOp::PopSpan => builder.pop_style_span(),
            InlineOp::Text { text, case, .. } => {
                // `text-transform` follows the collapsing (Text 3 §2: the transformation is applied to the text
                // after Phase I).
                builder.push_text(&transform_text(&text, case));
            }
            InlineOp::Marker(text) => builder.push_text(&text),
            InlineOp::InlineBox {
                id,
                baseline_aligned,
            } => builder.push_inline_box(InlineBox {
                id,
                // Overridden by push_inline_box method
                index: 0,
                // Width and height are set during layout
                width: 0.0,
                height: 0.0,
                baseline_aligned,
            }),
            // Nothing for Parley: the box has no text. `line_content_after_last_break` has read it.
            InlineOp::LineContent => {}
            InlineOp::ForcedBreak => {
                // The empty span commits the text before the break on its own, so the break is a run of its own.
                builder.push_style_modification_span(&[]);
                builder.push_text("\n");
                // A forced line break ends the word for `text-transform: capitalize`.
                AT_WORD_START.with(|w| w.set(true));
                builder.pop_style_span();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use stylo::WhiteSpaceCollapse as W;

    fn inline_box() -> InlineOp<'static> {
        InlineOp::InlineBox {
            id: 1,
            baseline_aligned: false,
        }
    }
    fn t(text: &str) -> InlineOp<'_> {
        InlineOp::Text {
            text: Cow::Borrowed(text),
            collapse: W::Collapse,
            case: None,
        }
    }
    fn pre(text: &str) -> InlineOp<'_> {
        InlineOp::Text {
            text: Cow::Borrowed(text),
            collapse: W::Preserve,
            case: None,
        }
    }
    fn line(text: &str) -> InlineOp<'_> {
        InlineOp::Text {
            text: Cow::Borrowed(text),
            collapse: W::PreserveBreaks,
            case: None,
        }
    }
    fn open() -> InlineOp<'static> {
        InlineOp::PushSpan(TextStyle::default())
    }
    fn close() -> InlineOp<'static> {
        InlineOp::PopSpan
    }

    /// `[` and `]` are the spans, `<br>` a forced break, `<box>` an atomic inline, `M(..)` a marker.
    fn show(ops: Vec<InlineOp<'_>>) -> String {
        collapse_white_space(ops)
            .iter()
            .map(|op| match op {
                InlineOp::PushSpan(_) => "[".to_string(),
                InlineOp::PopSpan => "]".to_string(),
                InlineOp::Text { text, .. } => text.to_string(),
                InlineOp::Marker(text) => format!("M({text})"),
                InlineOp::InlineBox { .. } => "<box>".to_string(),
                InlineOp::ForcedBreak => "<br>".to_string(),
                InlineOp::LineContent => "<content>".to_string(),
            })
            .collect()
    }

    #[test]
    fn line_content_is_transparent_for_white_space_and_found_after_the_last_break() {
        // a<br><span style=padding> </span>: the space is collapsed away at the line start, the box stays.
        let ops = vec![
            t("a"),
            InlineOp::ForcedBreak,
            InlineOp::LineContent,
            open(),
            t(" "),
            close(),
        ];
        let collapsed = collapse_white_space(ops);
        assert!(line_content_after_last_break(&collapsed));
        // The box before the break is on the line that the break ends.
        let before = collapse_white_space(vec![
            InlineOp::LineContent,
            open(),
            t("a"),
            InlineOp::ForcedBreak,
            close(),
        ]);
        assert!(!line_content_after_last_break(&before));
        // A preserved newline at the end of a text ends the line like a `<br>`.
        let pre = vec![
            InlineOp::LineContent,
            InlineOp::Text {
                text: "a\n".into(),
                collapse: W::Preserve,
                case: None,
            },
        ];
        assert!(!line_content_after_last_break(&collapse_white_space(pre)));
        assert!(!line_content_after_last_break(&[]));
    }

    #[test]
    fn w1_trailing_space_inside_a_span_is_kept() {
        // a <span>b </span>d
        assert_eq!(
            show(vec![t("a "), open(), t("b "), close(), t("d")]),
            "a [b ]d"
        );
    }

    #[test]
    fn w2_leading_space_inside_a_span_is_kept() {
        // a<span> b</span> d
        assert_eq!(
            show(vec![t("a"), open(), t(" b"), close(), t(" d")]),
            "a[ b] d"
        );
    }

    #[test]
    fn w3_nested_spans() {
        // a <span>b <i>c </i></span>d
        assert_eq!(
            show(vec![
                t("a "),
                open(),
                t("b "),
                open(),
                t("c "),
                close(),
                close(),
                t("d")
            ]),
            "a [b [c ]]d"
        );
    }

    #[test]
    fn w4_label_with_its_space() {
        // <b>Splatnost: </b>2. 11. 2026
        assert_eq!(
            show(vec![open(), t("Splatnost: "), close(), t("2. 11. 2026")]),
            "[Splatnost: ]2. 11. 2026"
        );
    }

    #[test]
    fn w5_the_second_space_is_the_one_that_goes() {
        // a <span> b</span>
        assert_eq!(show(vec![t("a "), open(), t(" b"), close()]), "a [b]");
        // a<span> </span> b: the first space survives, in the span
        assert_eq!(
            show(vec![t("a"), open(), t(" "), close(), t(" b")]),
            "a[ ]b"
        );
        // a <span></span> b
        assert_eq!(show(vec![t("a "), open(), close(), t(" b")]), "a []b");
    }

    #[test]
    fn w6_start_and_end_of_the_context() {
        // <span> a</span> b <span>c </span>
        assert_eq!(
            show(vec![
                open(),
                t(" a"),
                close(),
                t(" b "),
                open(),
                t("c "),
                close()
            ]),
            "[a] b [c]"
        );
        assert_eq!(show(vec![t("  a  ")]), "a");
        assert_eq!(show(vec![t("  \n ")]), "");
        // The trailing space of an earlier text goes when the last text is only a space.
        assert_eq!(show(vec![t("a "), open(), t(" "), close(), t(" ")]), "a[]");
    }

    #[test]
    fn w7_forced_breaks() {
        // a <br> <span> b</span>
        assert_eq!(
            show(vec![
                t("a "),
                InlineOp::ForcedBreak,
                t(" "),
                open(),
                t(" b"),
                close()
            ]),
            "a<br>[b]"
        );
        // a <span>b </span><br>c
        assert_eq!(
            show(vec![
                t("a "),
                open(),
                t("b "),
                close(),
                InlineOp::ForcedBreak,
                t("c")
            ]),
            "a [b]<br>c"
        );
    }

    #[test]
    fn w7_pre_line_splits_at_newlines() {
        // white-space: pre-line;  a <span>b </span>\n  c
        assert_eq!(
            show(vec![line("a "), open(), line("b "), close(), line("\n  c")]),
            "a [b]<br>c"
        );
        assert_eq!(show(vec![line("a \n  c\n\n d ")]), "a<br>c<br><br>d");
        // Spaces around the newline go even when they are in another text.
        assert_eq!(
            show(vec![line("a"), open(), line(" "), close(), line(" \nb")]),
            "a[]<br>b"
        );
    }

    #[test]
    fn w8_a_collapsible_space_after_a_preserved_one_is_kept() {
        // a <span style="white-space:pre">  b  </span> c
        assert_eq!(
            show(vec![t("a "), open(), pre("  b  "), close(), t(" c")]),
            "a [  b  ] c"
        );
        // A preserved newline starts a line.
        assert_eq!(show(vec![pre("a\n"), t(" b")]), "a\nb");
        // Preserved spaces are not removed at the end.
        assert_eq!(show(vec![t("a"), pre("  ")]), "a  ");
    }

    #[test]
    fn w9_atomic_inlines_are_content() {
        // a <img> b
        assert_eq!(show(vec![t("a "), inline_box(), t(" b")]), "a <box> b");
        // <img> at the start, a space at the end of the line
        assert_eq!(show(vec![inline_box(), t(" ")]), "<box>");
        assert_eq!(
            show(vec![inline_box(), t(" "), InlineOp::ForcedBreak]),
            "<box><br>"
        );
    }

    #[test]
    fn w10_generated_content() {
        // ::before { content: "Pred " }  mid  ::after { content: " Po" }
        assert_eq!(
            show(vec![
                open(),
                t("Pred "),
                close(),
                t("mid"),
                open(),
                t(" Po"),
                close()
            ]),
            "[Pred ]mid[ Po]"
        );
    }

    #[test]
    fn w16_a_marker_swallows_the_space_after_it() {
        assert_eq!(
            show(vec![
                InlineOp::Marker("• ".into()),
                t(" "),
                open(),
                t(" x"),
                close()
            ]),
            "M(• )[x]"
        );
        // A marker that does not end in a space does not.
        assert_eq!(
            show(vec![InlineOp::Marker("1.".into()), t(" x")]),
            "M(1.) x"
        );
    }

    #[test]
    fn w17_no_break_spaces_are_content() {
        // <b>12&nbsp;</b>&nbsp;000
        assert_eq!(
            show(vec![open(), t("12\u{a0}"), close(), t("\u{a0}000")]),
            "[12\u{a0}]\u{a0}000"
        );
        assert_eq!(show(vec![t("\u{a0} a \u{a0}")]), "\u{a0} a \u{a0}");
    }

    #[test]
    fn runs_of_white_space_become_one_space() {
        assert_eq!(show(vec![t("a \n\t b")]), "a b");
        assert_eq!(show(vec![t("a"), t("  "), t(" b")]), "a b");
    }
}
