//! Layer (a): the LRC-convention metadata mapping ADR-0021 leaves to #59 --
//! rating, color label, and keywords, read in whatever field/form Lightroom
//! Classic itself writes them in.
//!
//! Field choices (evidence and open questions in
//! `docs/research/scent-xmp-interop.md`):
//!
//! - **`xmp:Rating`**: `-1..=5`. `-1` is the Bridge/exiftool "rejected"
//!   convention (near-universal in practice, not in the core XMP spec).
//!   Absent means unrated -- kept as `None`, distinct from `Some(0)`, because
//!   `docs/research/shed-lrcat-schema.md` found `Adobe_images.rating` is
//!   nullable in the real `.lrcat` (277k/380k rows NULL) and a naive
//!   "default to 0" read would silently invent data.
//! - **LRC's positive "Pick" flag is a *different* concept from the star
//!   rating** (the `.lrcat`'s own `pick` column is separate from `rating`,
//!   per `shed-lrcat-schema.md`) and has no standard XMP field of its own.
//!   Whether LRC writes it to XMP at all, and under what field, is
//!   unverified pending a hands-on LRC pass -- this module does not invent a
//!   mapping for it. It's carried only in Nicti's own `nicti:` namespace
//!   (layer b) until that's confirmed.
//! - **`xmp:Label`**: the label *text* (e.g. `"Red"`), not a numeric id --
//!   LRC's default label set uses color names as the literal string, but a
//!   user-renamed label set changes what text gets written, so this is
//!   read/written as opaque text, never assumed to be one of the 5 defaults.
//! - **`dc:subject`**: flat keyword bag (`rdf:Bag` of `rdf:li`).
//! - **`lr:hierarchicalSubject`**: `lr:hierarchicalSubject` (Lightroom's own
//!   namespace, `http://ns.adobe.com/lightroom/1.0/`) holds `|`-joined
//!   keyword paths -- this is where nested keyword structure actually
//!   lives; `dc:subject` alone loses the hierarchy.
//!
//! Real LRC sidecars mix representations: scalar properties (`Rating`,
//! `Label`) are usually written as plain XML attributes on `rdf:Description`
//! for compactness, but RDF containers (`dc:subject`,
//! `lr:hierarchicalSubject`) can't be attributes -- they're always child
//! elements. This reader accepts *both* the attribute and element form for
//! the scalars (some tools, and hand-edited files, use the element form),
//! matching the same attribute-vs-element pitfall `spikes/calico`'s
//! `xmp_profile.rs` already ran into for `crs:` properties.
//!
//! Namespace prefixes are matched by **local name only**, not resolved via
//! their `xmlns` URI -- the same simplification `spikes/calico` makes. Real
//! files overwhelmingly use the conventional prefixes (`dc`, `xmp`, `lr`),
//! and a from-scratch namespace-URI resolver isn't worth the complexity for
//! a research spike whose job is proving the mapping, not shipping a
//! general-purpose XMP toolkit.

use quick_xml::events::Event;
use quick_xml::reader::Reader;
use quick_xml::XmlVersion;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Unrated (`None`) is distinct from a 0 rating. `-1` is a reject (see
/// module docs); `0..=5` is a star rating.
pub type Rating = Option<i8>;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LrcMeta {
    pub rating: Rating,
    pub label: Option<String>,
    pub keywords: Vec<String>,
    /// Each entry is one `|`-joined path, already split into segments, e.g.
    /// `["Events", "Anthrocon 2025"]` for `lr:hierarchicalSubject`'s
    /// `Events|Anthrocon 2025`.
    pub hierarchical_keywords: Vec<Vec<String>>,
    /// Pick flag. ADR-0059 defers an LRC-side mapping (#187), so this lives
    /// only in nicti's own `nicti:pick` attribute (`"1"` = picked); absent
    /// means not picked.
    pub pick: bool,
}

#[derive(Debug, Error)]
pub enum ReadError {
    #[error("xml parse error: {0}")]
    Xml(#[from] quick_xml::Error),
    #[error("attribute error: {0}")]
    Attr(#[from] quick_xml::events::attributes::AttrError),
    #[error("non-utf8 text content")]
    Utf8,
    #[error("xmp:Rating {0:?} is not an integer in -1..=5")]
    BadRating(String),
    #[error("unresolvable XML entity &{0};")]
    UnknownEntity(String),
}

fn local_name(qname: &[u8]) -> &[u8] {
    match qname.iter().position(|&b| b == b':') {
        Some(i) => &qname[i + 1..],
        None => qname,
    }
}

fn local_eq(qname: &[u8], target: &str) -> bool {
    local_name(qname) == target.as_bytes()
}

/// Parses whatever LRC-convention properties are present in a full XMP
/// packet (sidecar text, or an extracted embedded packet). Missing
/// properties leave their `LrcMeta` field at its default (`None` / empty
/// `Vec`) -- this never assumes an unwritten field means "zero"/"none" in
/// the photographic sense, only "not present in this packet".
pub fn read(xmp: &str) -> Result<LrcMeta, ReadError> {
    let mut reader = Reader::from_str(xmp);
    // Text is accumulated across `Text` and `GeneralRef` events (quick-xml delivers `&amp;` etc. as
    // separate events), so trimming per event would eat the space in `Tom &amp; Jerry`. Values are
    // trimmed once, when their element ends.
    reader.config_mut().trim_text(false);

    let mut meta = LrcMeta::default();
    // Tracks which list property (if any) we're currently inside, so `Text`/
    // `rdf:li` events get routed to the right collection. `rdf:Bag`/`Seq` is
    // itself skipped -- only the parent property name matters.
    let mut in_list: Option<ListKind> = None;
    let mut current_li = String::new();
    // Element-form scalar in progress, e.g. `<xmp:Rating>4</xmp:Rating>`.
    let mut pending_scalar: Option<ScalarKind> = None;
    let mut scalar_buf = String::new();

    loop {
        match reader.read_event()? {
            Event::Eof => break,
            Event::Empty(e) => {
                // A self-closing list container (`<dc:subject/>`, no `rdf:Bag`
                // at all) has no matching `Event::End` -- quick_xml never
                // fires one for `Empty`. Entering `in_list` here the same way
                // `Start` does would leave it stuck forever (nothing ever
                // clears it), silently corrupting every later scalar this
                // reader sees as "inside a list". An empty container has no
                // keywords to collect either way, so this is just a no-op:
                // scalar attributes on it (rare, but XMP allows them on any
                // element) are still read like any other tag.
                read_attrs(&e, &mut meta)?;
            }
            Event::Start(e) => {
                let name = e.name();
                let name = name.as_ref();
                if local_eq(name, "subject") {
                    in_list = Some(ListKind::Subject);
                } else if local_eq(name, "hierarchicalSubject") {
                    in_list = Some(ListKind::Hierarchical);
                } else if local_eq(name, "li") {
                    current_li.clear();
                } else if in_list.is_none() && name == RATING_KEY {
                    pending_scalar = Some(ScalarKind::Rating);
                    scalar_buf.clear();
                } else if in_list.is_none() && name == LABEL_KEY {
                    pending_scalar = Some(ScalarKind::Label);
                    scalar_buf.clear();
                }

                // Attribute form: scalar properties written directly on this
                // element (typically `rdf:Description`).
                read_attrs(&e, &mut meta)?;
            }
            Event::Text(t) => {
                let text = t
                    .xml_content(XmlVersion::Implicit1_0)
                    .map_err(quick_xml::Error::from)?;
                push_text(
                    &text,
                    in_list.is_some(),
                    pending_scalar,
                    &mut current_li,
                    &mut scalar_buf,
                );
            }
            Event::GeneralRef(r) => {
                let resolved: String = match r.resolve_char_ref()? {
                    Some(c) => c.to_string(),
                    None => match r.decode().map_err(quick_xml::Error::from)?.as_ref() {
                        "amp" => "&".into(),
                        "lt" => "<".into(),
                        "gt" => ">".into(),
                        "quot" => "\"".into(),
                        "apos" => "'".into(),
                        // An undeclared entity can't be resolved; dropping it silently would
                        // rename the value, so refuse the packet instead.
                        other => return Err(ReadError::UnknownEntity(other.to_string())),
                    },
                };
                push_text(
                    &resolved,
                    in_list.is_some(),
                    pending_scalar,
                    &mut current_li,
                    &mut scalar_buf,
                );
            }
            Event::End(e) => {
                let name = e.name();
                let name = name.as_ref();
                if local_eq(name, "li") {
                    match in_list {
                        Some(ListKind::Subject) => {
                            let kw = current_li.trim();
                            if !kw.is_empty() {
                                meta.keywords.push(kw.to_string());
                            }
                        }
                        Some(ListKind::Hierarchical) => {
                            let path: Vec<String> = current_li
                                .split('|')
                                .map(|s| s.trim().to_string())
                                .filter(|s| !s.is_empty())
                                .collect();
                            if !path.is_empty() {
                                meta.hierarchical_keywords.push(path);
                            }
                        }
                        None => {}
                    }
                    current_li.clear();
                } else if (local_eq(name, "subject") && matches!(in_list, Some(ListKind::Subject)))
                    || (local_eq(name, "hierarchicalSubject")
                        && matches!(in_list, Some(ListKind::Hierarchical)))
                {
                    in_list = None;
                } else if name == RATING_KEY || name == LABEL_KEY {
                    match pending_scalar.take() {
                        Some(ScalarKind::Rating) => meta.rating = parse_rating(&scalar_buf)?,
                        Some(ScalarKind::Label) => {
                            let trimmed = scalar_buf.trim();
                            if !trimmed.is_empty() {
                                meta.label = Some(trimmed.to_string());
                            }
                        }
                        None => {}
                    }
                    scalar_buf.clear();
                }
            }
            _ => {}
        }
    }

    Ok(meta)
}

/// Exact qualified names, not local names: `MicrosoftPhoto:Rating` (0-100) and other foreign
/// `*:Rating`/`*:Label` properties must never be mistaken for `xmp:Rating`/`xmp:Label`.
const RATING_KEY: &[u8] = b"xmp:Rating";
const LABEL_KEY: &[u8] = b"xmp:Label";
const PICK_KEY: &[u8] = b"nicti:pick";

fn push_text(
    text: &str,
    in_list: bool,
    pending: Option<ScalarKind>,
    li: &mut String,
    scalar: &mut String,
) {
    if in_list {
        li.push_str(text);
    } else if pending.is_some() {
        scalar.push_str(text);
    }
}

/// `-1..=5` only. Anything else (`6`, `200`, `abc`, `3.5`) is an error, not "unrated" -- treating
/// garbage as a missing rating would make a sync silently clear a real catalog rating, and an
/// out-of-range number would violate the catalog's own CHECK constraint.
fn parse_rating(text: &str) -> Result<Rating, ReadError> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    match t.parse::<i8>() {
        Ok(v) if (-1..=5).contains(&v) => Ok(Some(v)),
        _ => Err(ReadError::BadRating(t.to_string())),
    }
}

fn read_attrs(e: &quick_xml::events::BytesStart<'_>, meta: &mut LrcMeta) -> Result<(), ReadError> {
    for attr in e.attributes() {
        let attr = attr?;
        let key = attr.key.as_ref();
        if key == RATING_KEY {
            meta.rating = parse_rating(&attr.normalized_value(XmlVersion::Implicit1_0)?)?;
        } else if key == LABEL_KEY {
            let val = attr.normalized_value(XmlVersion::Implicit1_0)?;
            let val = val.trim();
            if !val.is_empty() {
                meta.label = Some(val.to_string());
            }
        } else if key == PICK_KEY {
            meta.pick = attr.value.as_ref() == b"1";
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListKind {
    Subject,
    Hierarchical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScalarKind {
    Rating,
    Label,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_form_rating_and_label() {
        let xmp = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="4" xmp:Label="Red"/></rdf:RDF></x:xmpmeta>"#;
        let meta = read(xmp).unwrap();
        assert_eq!(meta.rating, Some(4));
        assert_eq!(meta.label.as_deref(), Some("Red"));
    }

    #[test]
    fn absent_rating_is_none_not_zero() {
        let xmp = r#"<rdf:Description xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"/>"#;
        let meta = read(xmp).unwrap();
        assert_eq!(meta.rating, None, "unrated must stay None, not Some(0)");
    }

    #[test]
    fn explicit_zero_rating_is_some_zero() {
        let xmp = r#"<rdf:Description xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="0"/>"#;
        let meta = read(xmp).unwrap();
        assert_eq!(meta.rating, Some(0));
    }

    #[test]
    fn reject_is_negative_one() {
        let xmp = r#"<rdf:Description xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="-1"/>"#;
        let meta = read(xmp).unwrap();
        assert_eq!(meta.rating, Some(-1));
    }

    #[test]
    fn dc_subject_bag_and_hierarchical_subject() {
        let xmp = r#"<rdf:Description xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:lr="http://ns.adobe.com/lightroom/1.0/">
<dc:subject><rdf:Bag><rdf:li>Anthrocon</rdf:li><rdf:li>2025</rdf:li></rdf:Bag></dc:subject>
<lr:hierarchicalSubject><rdf:Bag><rdf:li>Events|Anthrocon 2025</rdf:li><rdf:li>Species|Fox</rdf:li></rdf:Bag></lr:hierarchicalSubject>
</rdf:Description>"#;
        let meta = read(xmp).unwrap();
        assert_eq!(meta.keywords, vec!["Anthrocon", "2025"]);
        assert_eq!(
            meta.hierarchical_keywords,
            vec![
                vec!["Events".to_string(), "Anthrocon 2025".to_string()],
                vec!["Species".to_string(), "Fox".to_string()],
            ]
        );
    }

    #[test]
    fn element_form_rating_and_label_are_also_accepted() {
        // Rarer, but some tools (and hand-edited files) write scalars as
        // child elements rather than attributes -- same pitfall
        // spikes/calico's xmp_profile.rs documents for crs: properties.
        let xmp = r#"<rdf:Description xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:xmp="http://ns.adobe.com/xap/1.0/"><xmp:Rating>5</xmp:Rating><xmp:Label>Green</xmp:Label></rdf:Description>"#;
        let meta = read(xmp).unwrap();
        assert_eq!(meta.rating, Some(5));
        assert_eq!(meta.label.as_deref(), Some("Green"));
    }

    #[test]
    fn keyword_text_inside_a_list_is_never_mistaken_for_a_scalar() {
        // `pending_scalar` must not leak across a list boundary: an `rdf:li`
        // text node here must never be read as `Rating`/`Label` content.
        let xmp = r#"<rdf:Description xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:subject><rdf:Bag><rdf:li>5</rdf:li></rdf:Bag></dc:subject></rdf:Description>"#;
        let meta = read(xmp).unwrap();
        assert_eq!(meta.rating, None);
        assert_eq!(meta.keywords, vec!["5"]);
    }

    #[test]
    fn self_closing_empty_list_container_does_not_leak_list_state() {
        // A self-closing `<dc:subject/>` (no `rdf:Bag`, e.g. an empty
        // keyword list some tool wrote out explicitly) fires `Event::Empty`,
        // which has no matching `Event::End` -- `in_list` must never be left
        // set afterwards, or every later scalar in the document would be
        // silently misread as "inside a list" and dropped.
        let xmp = r#"<rdf:Description xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:xmp="http://ns.adobe.com/xap/1.0/"><dc:subject/><xmp:Rating>4</xmp:Rating></rdf:Description>"#;
        let meta = read(xmp).unwrap();
        assert_eq!(
            meta.rating,
            Some(4),
            "Rating after an empty list container must still be read"
        );
        assert!(meta.keywords.is_empty());
    }

    const WRAP_OPEN: &str = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:dc="http://purl.org/dc/elements/1.1/">"#;

    #[test]
    fn entities_in_keywords_and_labels_are_decoded_without_losing_spaces() {
        let xmp = format!(
            r#"{WRAP_OPEN}<rdf:Description xmp:Label="Tom &amp; Jerry"><dc:subject><rdf:Bag><rdf:li>Tom &amp; Jerry</rdf:li><rdf:li>a&lt;b</rdf:li><rdf:li>it&apos;s &quot;q&quot; &#38; &#x26;</rdf:li></rdf:Bag></dc:subject></rdf:Description></rdf:RDF>"#
        );
        let meta = read(&xmp).unwrap();
        assert_eq!(meta.label.as_deref(), Some("Tom & Jerry"));
        assert_eq!(meta.keywords, vec!["Tom & Jerry", "a<b", "it's \"q\" & &"]);
    }

    #[test]
    fn element_form_label_decodes_entities() {
        let xmp = format!(
            r#"{WRAP_OPEN}<rdf:Description><xmp:Label>Tom &amp; Jerry</xmp:Label><xmp:Rating>3</xmp:Rating></rdf:Description></rdf:RDF>"#
        );
        let meta = read(&xmp).unwrap();
        assert_eq!(meta.label.as_deref(), Some("Tom & Jerry"));
        assert_eq!(meta.rating, Some(3));
    }

    #[test]
    fn an_undeclared_entity_is_refused_not_dropped() {
        let xmp = format!(
            r#"{WRAP_OPEN}<rdf:Description><dc:subject><rdf:Bag><rdf:li>x&nbsp;y</rdf:li></rdf:Bag></dc:subject></rdf:Description></rdf:RDF>"#
        );
        assert!(matches!(read(&xmp), Err(ReadError::UnknownEntity(_))));
    }

    #[test]
    fn out_of_range_or_garbage_ratings_are_errors_not_unrated() {
        for bad in ["6", "-2", "127", "200", "abc", "3.5"] {
            let xmp = format!(r#"{WRAP_OPEN}<rdf:Description xmp:Rating="{bad}"/></rdf:RDF>"#);
            assert!(
                matches!(read(&xmp), Err(ReadError::BadRating(_))),
                "{bad} should be rejected"
            );
        }
        let ok = format!(r#"{WRAP_OPEN}<rdf:Description xmp:Rating=""/></rdf:RDF>"#);
        assert_eq!(read(&ok).unwrap().rating, None);
    }

    #[test]
    fn foreign_rating_and_label_properties_are_ignored() {
        let xmp = format!(
            r#"{WRAP_OPEN}<rdf:Description xmlns:MicrosoftPhoto="http://ns.microsoft.com/photo/1.0/" MicrosoftPhoto:Rating="75" MicrosoftPhoto:Label="x"/></rdf:RDF>"#
        );
        let meta = read(&xmp).unwrap();
        assert_eq!((meta.rating, meta.label), (None, None));
    }
}
