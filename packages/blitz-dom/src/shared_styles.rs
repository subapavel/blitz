//! Style state that several documents of one owner can share.
//!
//! A [`BaseDocument`](crate::BaseDocument) normally creates its own Stylo `SharedRwLock` and parses Blitz's user
//! agent sheet (and every author sheet) again. An owner that builds many small documents from the same sheets
//! (fulgur renders one document per `@page` margin box and page) builds one [`SharedStyles`] and hands a clone to
//! each document through [`DocumentConfig::shared_styles`](crate::DocumentConfig::shared_styles).
//!
//! What a [`SharedStyles`] holds is immutable after construction:
//!
//! * the lock is `Arc<RwLock<()>>`: it carries no data, it only gives guards an identity, and Stylo asserts that a
//!   sheet and a node are read under the same lock. Every document built from one `SharedStyles` therefore uses
//!   the lock of the `SharedStyles`;
//! * the sheets are `Arc<Stylesheet>`. A document changes the *set* of sheets in its own `Stylist`, never a sheet.
//!
//! Do not keep a `SharedStyles` beyond the owner's unit of work (one render): it is not process-wide state.

use crate::DEFAULT_CSS;
use crate::url::DocumentUrl;
use selectors::matching::QuirksMode;
use std::str::FromStr;
use std::sync::Arc;
use style::media_queries::MediaList;
use style::servo_arc::Arc as ServoArc;
use style::shared_lock::SharedRwLock;
use style::stylesheets::{AllowImportRules, DocumentStyleSheet, Origin, Stylesheet};

/// A lock, the user agent sheets and (optionally) author sheets that several documents share.
///
/// Cloning is cheap (a reference count), so one value can be handed to every document of a render.
#[derive(Clone)]
pub struct SharedStyles {
    inner: Arc<Inner>,
}

#[derive(Clone)]
struct Inner {
    lock: SharedRwLock,
    base_url: String,
    /// `(css text, sheet)`, in the order they are appended to a document's `Stylist`.
    ua: Vec<(String, DocumentStyleSheet)>,
    /// Author sheets, appended after the user agent sheets and before any sheet of the document itself.
    author: Vec<DocumentStyleSheet>,
}

impl SharedStyles {
    /// A new lock, with Blitz's default user agent sheet parsed once under it. `base_url` must be the
    /// `base_url` of every document built from this value.
    pub fn new(base_url: &str) -> Self {
        let lock = SharedRwLock::new();
        let ua = vec![(
            DEFAULT_CSS.to_string(),
            parse_sheet(&lock, base_url, DEFAULT_CSS, Origin::UserAgent),
        )];
        Self {
            inner: Arc::new(Inner {
                lock,
                base_url: base_url.to_string(),
                ua,
                author: Vec::new(),
            }),
        }
    }

    /// Add an author sheet that every document starts with, in front of the sheets of its own `<style>` and
    /// `<link>` elements (the order a `<style>` in `<head>` gets today).
    ///
    /// `sheet` must have been parsed under [`SharedStyles::lock`]: take it from a document built from this value
    /// with [`BaseDocument::stylesheet_of_node`](crate::BaseDocument::stylesheet_of_node). Its text is then the
    /// text html5ever and the entity decoder make of the element, which is what a `<style>` in the document would
    /// have produced. Stylo panics if a sheet of another lock is read.
    pub fn with_author_sheet(mut self, sheet: DocumentStyleSheet) -> Self {
        Arc::make_mut(&mut self.inner).author.push(sheet);
        self
    }

    /// The lock every document of this value uses.
    pub fn lock(&self) -> &SharedRwLock {
        &self.inner.lock
    }

    /// The base URL the shared sheets were parsed with.
    pub fn base_url(&self) -> &str {
        &self.inner.base_url
    }

    pub(crate) fn ua_sheets(&self) -> &[(String, DocumentStyleSheet)] {
        &self.inner.ua
    }

    pub(crate) fn author_sheets(&self) -> &[DocumentStyleSheet] {
        &self.inner.author
    }
}

fn parse_sheet(
    lock: &SharedRwLock,
    base_url: &str,
    css: &str,
    origin: Origin,
) -> DocumentStyleSheet {
    let url = DocumentUrl::from_str(base_url).unwrap_or_default();
    let data = Stylesheet::from_str(
        css,
        url.url_extra_data(),
        origin,
        ServoArc::new(lock.wrap(MediaList::empty())),
        lock.clone(),
        None,
        None,
        QuirksMode::NoQuirks,
        AllowImportRules::Yes,
    );
    DocumentStyleSheet(ServoArc::new(data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Attribute, BaseDocument, DocumentConfig, QualName, ns};
    use std::panic::{AssertUnwindSafe, catch_unwind};

    const BASE: &str = "file:///";

    fn qual(name: &str) -> QualName {
        QualName::new(None, ns!(html), name.into())
    }

    fn attr(name: &str, value: &str) -> Attribute {
        Attribute {
            name: QualName::new(None, ns!(), name.into()),
            value: value.to_string(),
        }
    }

    fn config(shared: Option<&SharedStyles>) -> DocumentConfig {
        DocumentConfig {
            base_url: Some(BASE.to_string()),
            shared_styles: shared.cloned(),
            ..DocumentConfig::default()
        }
    }

    /// `html > (head > style*, body > div.x)`; every `<style>` text is one entry of `styles`. Returns the
    /// document, the `div` and the `<style>` nodes.
    fn page(config: DocumentConfig, styles: &[&str]) -> (BaseDocument, usize, Vec<usize>) {
        let mut doc = BaseDocument::new(config);
        let (div, style_ids);
        {
            let mut m = doc.mutate();
            let html = m.create_element(qual("html"), vec![]);
            let head = m.create_element(qual("head"), vec![]);
            let body = m.create_element(qual("body"), vec![]);
            div = m.create_element(qual("div"), vec![attr("class", "x")]);
            m.append_children(0, &[html]);
            m.append_children(html, &[head, body]);
            m.append_children(body, &[div]);
            let mut ids = Vec::new();
            for css in styles {
                let style = m.create_element(qual("style"), vec![]);
                let text = m.create_text_node(css);
                m.append_children(style, &[text]);
                m.append_children(head, &[style]);
                ids.push(style);
            }
            style_ids = ids;
        }
        (doc, div, style_ids)
    }

    fn width_of(doc: &mut BaseDocument, node: usize) -> f32 {
        doc.resolve(0.0);
        doc.get_node(node).unwrap().final_layout.size.width
    }

    fn rule_count(doc: &BaseDocument, sheet: &DocumentStyleSheet) -> usize {
        sheet.0.contents.rules(&doc.guard().read()).len()
    }

    #[test]
    fn documents_without_shared_styles_keep_their_own_lock_and_ua_sheet() {
        let a = BaseDocument::new(config(None));
        let b = BaseDocument::new(config(None));
        let (sa, sb) = (
            &a.ua_stylesheets[DEFAULT_CSS],
            &b.ua_stylesheets[DEFAULT_CSS],
        );
        assert!(
            !ServoArc::ptr_eq(&sa.0, &sb.0),
            "each document parses its own UA sheet"
        );
        assert!(rule_count(&a, sa) > 0);
        // Reading a sheet under another document's lock is a Stylo assertion.
        let other_lock = catch_unwind(AssertUnwindSafe(|| rule_count(&b, sa)));
        assert!(
            other_lock.is_err(),
            "two plain documents must not share a lock"
        );
    }

    #[test]
    fn two_documents_of_one_shared_styles_share_lock_and_ua_sheet() {
        let shared = SharedStyles::new(BASE);
        let a = BaseDocument::new(config(Some(&shared)));
        let b = BaseDocument::new(config(Some(&shared)));
        let (sa, sb) = (
            &a.ua_stylesheets[DEFAULT_CSS],
            &b.ua_stylesheets[DEFAULT_CSS],
        );
        assert!(
            ServoArc::ptr_eq(&sa.0, &sb.0),
            "the UA sheet is parsed once"
        );
        // Same lock: a sheet of one document is readable under the guard of the other, and under the shared one.
        assert_eq!(rule_count(&b, sa), rule_count(&a, sa));
        assert_eq!(
            sa.0.contents.rules(&shared.lock().read()).len(),
            rule_count(&a, sa)
        );
        // A second `SharedStyles` has its own lock.
        let c = BaseDocument::new(config(Some(&SharedStyles::new(BASE))));
        let foreign = catch_unwind(AssertUnwindSafe(|| rule_count(&c, sa)));
        assert!(
            foreign.is_err(),
            "another SharedStyles must have another lock"
        );
    }

    #[test]
    fn shared_documents_resolve_like_plain_ones() {
        // Same page, once with Blitz's own UA sheet and once with the shared one: same layout.
        let css = ".x { width: 123px; }";
        let (mut plain, div, _) = page(config(None), &[css]);
        let shared = SharedStyles::new(BASE);
        let (mut shared_doc, div2, _) = page(config(Some(&shared)), &[css]);
        assert_eq!(div, div2);
        assert_eq!(width_of(&mut plain, div), 123.0);
        assert_eq!(width_of(&mut shared_doc, div2), 123.0);
        // The UA sheet really applies: `body` has the UA margin, so the block is not at x = 0.
        let body_x = |d: &BaseDocument| {
            let body = d.get_node(div).unwrap().parent.unwrap();
            d.get_node(body).unwrap().final_layout.location.x
        };
        assert!(body_x(&plain) > 0.0, "the UA margin on body applies");
        assert_eq!(body_x(&plain), body_x(&shared_doc));
    }

    #[test]
    fn author_sheet_comes_before_the_documents_own_sheets() {
        // Build the shared author sheet from a probe document, as fulgur does.
        let bare = SharedStyles::new(BASE);
        let (probe, _, style_ids) =
            page(config(Some(&bare)), &[".x { width: 111px; height: 7px; }"]);
        let sheet = probe
            .stylesheet_of_node(style_ids[0])
            .expect("the probe's <style> has a sheet");
        let shared = bare.with_author_sheet(sheet.clone());

        // Without a sheet of its own the document gets the shared one.
        let (mut only, div, ids) = page(config(Some(&shared)), &[]);
        assert!(ids.is_empty());
        assert_eq!(width_of(&mut only, div), 111.0);
        assert_eq!(only.get_node(div).unwrap().final_layout.size.height, 7.0);

        // A `<style>` of the document is later in the cascade, as a `<style>` in `<head>` would have been.
        let (mut own, div, ids) = page(config(Some(&shared)), &[".x { width: 222px; }"]);
        assert_eq!(width_of(&mut own, div), 222.0);
        assert_eq!(
            own.get_node(div).unwrap().final_layout.size.height,
            7.0,
            "the shared rules still apply"
        );
        // The sheet of the document's own `<style>` is not the shared one.
        let own_sheet = own.stylesheet_of_node(ids[0]).unwrap();
        assert!(!ServoArc::ptr_eq(&own_sheet.0, &sheet.0));
        assert!(rule_count(&own, &own_sheet) > 0);
    }

    #[test]
    fn a_document_without_shared_styles_has_no_author_sheet_it_did_not_ask_for() {
        let (mut doc, div, _) = page(config(None), &[]);
        assert_ne!(width_of(&mut doc, div), 111.0);
        assert!(doc.stylesheet_of_node(div).is_none());
    }
}
