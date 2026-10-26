use crate::node::{Node, NodeData};
use color::{AlphaColor, Srgb};
use style::color::AbsoluteColor;

pub type Color = AlphaColor<Srgb>;

#[cfg(feature = "svg")]
use std::sync::{Arc, OnceLock};
#[cfg(feature = "svg")]
use usvg::fontdb;

/// The host's system fonts, loaded the first time an SVG is parsed without host-supplied options (upstream's
/// behaviour). A host that supplies `DocumentConfig::svg_options` never touches it: no scan, no host font in the output.
#[cfg(feature = "svg")]
static FONT_DB: OnceLock<Arc<fontdb::Database>> = OnceLock::new();

#[cfg(feature = "svg")]
fn system_font_db() -> Arc<fontdb::Database> {
    Arc::clone(FONT_DB.get_or_init(|| {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        Arc::new(db)
    }))
}

#[derive(Clone, Debug)]
pub enum ImageType {
    Image,
    Background(usize),
}

/// A point
#[derive(Clone, Debug, Copy, Eq, PartialEq)]
pub struct Point<T> {
    /// The x coordinate
    pub x: T,
    /// The y coordinate
    pub y: T,
}

impl Point<f64> {
    pub const ZERO: Self = Point { x: 0.0, y: 0.0 };
}

// Debug print an RcDom
pub fn walk_tree(indent: usize, node: &Node) {
    // Skip all-whitespace text nodes entirely
    if let NodeData::Text(data) = &node.data {
        if data.content.chars().all(|c| c.is_ascii_whitespace()) {
            return;
        }
    }

    print!("{}", " ".repeat(indent));
    let id = node.id;
    match &node.data {
        NodeData::Document => println!("#Document {id}"),

        NodeData::Text(data) => {
            if data.content.chars().all(|c| c.is_ascii_whitespace()) {
                println!("{id} #text: <whitespace>");
            } else {
                let content = data.content.trim();
                if content.len() > 10 {
                    println!(
                        "#text {id}: {}...",
                        content
                            .split_at(content.char_indices().take(10).last().unwrap().0)
                            .0
                            .escape_default()
                    )
                } else {
                    println!("#text {id}: {}", data.content.trim().escape_default())
                }
            }
        }

        NodeData::Comment => println!("<!-- COMMENT {id} -->"),

        NodeData::AnonymousBlock(_) => println!("{id} AnonymousBlock"),

        NodeData::Element(data) => {
            print!("<{} {id}", data.name.local);
            for attr in data.attrs.iter() {
                print!(" {}=\"{}\"", attr.name.local, attr.value);
            }
            if !node.children.is_empty() {
                println!(">");
            } else {
                println!("/>");
            }
        } // NodeData::Doctype {
          //     ref name,
          //     ref public_id,
          //     ref system_id,
          // } => println!("<!DOCTYPE {} \"{}\" \"{}\">", name, public_id, system_id),
          // NodeData::ProcessingInstruction { .. } => unreachable!(),
    }

    if !node.children.is_empty() {
        for child_id in node.children.iter() {
            walk_tree(indent + 2, node.with(*child_id));
        }

        if let NodeData::Element(data) = &node.data {
            println!("{}</{}>", " ".repeat(indent), data.name.local);
        }
    }
}

#[cfg(feature = "svg")]
/// Parses an SVG with the host's `options`, or, for `None`, usvg's defaults and the system fonts.
pub(crate) fn parse_svg(
    source: &[u8],
    options: Option<usvg::Options<'static>>,
) -> Result<usvg::Tree, usvg::Error> {
    parse_svg_with(source, options, system_font_db)
}

/// [`parse_svg`] with the system font database passed in: it is asked for only when `options` is `None`.
#[cfg(feature = "svg")]
fn parse_svg_with(
    source: &[u8],
    options: Option<usvg::Options<'static>>,
    system_fonts: impl FnOnce() -> Arc<fontdb::Database>,
) -> Result<usvg::Tree, usvg::Error> {
    let options = options.unwrap_or_else(|| usvg::Options {
        fontdb: system_fonts(),
        ..Default::default()
    });
    let tree = usvg::Tree::from_data(source, &options)?;
    Ok(tree)
}

pub trait ToColorColor {
    /// Converts a color into the `AlphaColor<Srgb>` type from the `color` crate
    fn as_color_color(&self) -> Color;
}
impl ToColorColor for AbsoluteColor {
    fn as_color_color(&self) -> Color {
        Color::new(
            *self
                .to_color_space(style::color::ColorSpace::Srgb)
                .raw_components(),
        )
    }
}

/// Creates an markup5ever::QualName.
/// Given a local name and an optional namespace
#[macro_export]
macro_rules! qual_name {
    ($local:tt $(, $ns:ident)?) => {
        $crate::QualName {
            prefix: None,
            ns: $crate::ns!($($ns)?),
            local: $crate::local_name!($local),
        }
    };
}

#[cfg(all(test, feature = "svg"))]
mod tests {
    use super::*;

    const SVG: &[u8] = b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"4\" height=\"4\"/>";

    #[test]
    fn host_options_never_ask_for_the_system_fonts() {
        parse_svg_with(SVG, Some(usvg::Options::default()), || {
            panic!("the system fonts were loaded although the host supplied the options")
        })
        .expect("parses");
    }

    #[test]
    fn without_options_the_system_fonts_are_asked_for_once() {
        let mut asked = 0;
        parse_svg_with(SVG, None, || {
            asked += 1;
            Arc::new(fontdb::Database::new())
        })
        .expect("parses");
        assert_eq!(asked, 1);
    }
}
