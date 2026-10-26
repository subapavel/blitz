use crate::{HtmlParserProvider, SharedStyles, net::Resource};
use blitz_traits::{
    navigation::NavigationProvider,
    net::NetProvider,
    shell::{ShellProvider, Viewport},
};
use parley::FontContext;
use std::sync::Arc;

/// Makes the `<image>` href resolver of one inline `<svg>`; called with the id of the `<svg>` node each time the
/// element is parsed. With it, a host decides which images an SVG may load (and can refuse, say, a PNG it cannot
/// decode) instead of leaving that to usvg's default resolver, which only looks at the MIME type or the magic bytes.
#[cfg(feature = "svg")]
pub type SvgImageResolverFactory =
    Arc<dyn Fn(usize) -> usvg::ImageHrefResolver<'static> + Send + Sync>;

/// Options used when constructing a [`BaseDocument`](crate::BaseDocument)
#[derive(Default)]
pub struct DocumentConfig {
    /// The initial `Viewport`
    pub viewport: Option<Viewport>,
    /// The base url which relative URLs are resolved against
    pub base_url: Option<String>,
    /// User Agent stylesheets
    pub ua_stylesheets: Option<Vec<String>>,
    /// Net provider to handle network requests for resources
    pub net_provider: Option<Arc<dyn NetProvider<Resource>>>,
    /// Navigation provider to handle link clicks and form submissions
    pub navigation_provider: Option<Arc<dyn NavigationProvider>>,
    /// Shell provider to redraw requests, clipboard, etc
    pub shell_provider: Option<Arc<dyn ShellProvider>>,
    /// HTML parser provider. Used to parse HTML for setInnerHTML
    pub html_parser_provider: Option<Arc<dyn HtmlParserProvider>>,
    /// Parley `FontContext`
    pub font_ctx: Option<FontContext>,
    /// A style lock, user agent sheets and author sheets shared with other documents. With `Some`, the document
    /// uses this lock instead of a new one and starts with these sheets instead of parsing Blitz's own;
    /// `ua_stylesheets` must be `None` and `base_url` must equal the one of the `SharedStyles`.
    pub shared_styles: Option<SharedStyles>,
    /// Resolver for the `<image>` elements of inline `<svg>` elements. `None` keeps usvg's default resolver.
    #[cfg(feature = "svg")]
    pub svg_image_resolver: Option<SvgImageResolverFactory>,
}
