//! Reads and patches a full XMP packet, preserving everything this spike
//! doesn't own -- `crs:` mask corrections, `exif:`/`aux:` camera metadata,
//! any other tool's namespace -- byte-for-byte, per ADR-0002's "XMP is an
//! interop layer, not the source of truth for what Nicti doesn't already
//! know about" framing.
//!
//! **Method: event-copy-and-patch, not a fresh serializer.** The whole
//! packet is parsed into a flat list of `quick_xml` events; everything
//! outside the fields this module owns is re-emitted completely unchanged.
//! Only the first `rdf:Description`'s owned attributes are rewritten, and
//! only its `dc:subject`/`lr:hierarchicalSubject` child blocks are replaced
//! wholesale when a write touches keywords. This is what makes the
//! byte-diff guard in `embedded::jpeg` meaningful: a patch that doesn't
//! touch keywords leaves every other byte of the packet identical.
//!
//! **Simplifying assumption, stated up front**: exactly one `rdf:Description`
//! carries the properties this module reads/writes -- true for every real
//! LRC-written sidecar this spike measured (`docs/research/scent-xmp-interop.md`).
//! XMP technically allows multiple `rdf:Description` elements for the same
//! subject with properties split across them; this module only ever touches
//! the first one found and leaves any others alone.
//!
//! The lossless `nicti:` recovery layer (ADR-0002 layer b) rides in this
//! same packet as a single `nicti:editDocument` attribute, opaque
//! base64+JSON blob -- same shape `spikes/pawprint/src/xmp.rs` proved,
//! now hosted on a real packet instead of a throwaway wrapper string.

use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::name::QName;
use quick_xml::reader::Reader;
use quick_xml::writer::Writer;
use thiserror::Error;

use crate::lrc_fields::Rating;

#[derive(Debug, Error)]
pub enum PatchError {
    #[error("xml error: {0}")]
    Xml(#[from] quick_xml::Error),
    #[error("attribute error: {0}")]
    Attr(#[from] quick_xml::events::attributes::AttrError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("no rdf:Description element found in this packet")]
    NoDescription,
    #[error(
        "malformed or truncated xmp: no matching closing tag found (e.g. a half-written sidecar)"
    )]
    Unbalanced,
}

/// What to change on the next write. Each field is "leave untouched" (`None`)
/// vs. "set to this" (`Some(..)`) -- the inner `Option`/`Vec` carries the
/// actual value, including the "clear this property" case (`Some(None)` for
/// a scalar, `Some(vec![])` for a list).
#[derive(Debug, Clone, Default)]
pub struct Patch {
    pub rating: Option<Rating>,
    pub label: Option<Option<String>>,
    pub keywords: Option<Vec<String>>,
    pub hierarchical_keywords: Option<Vec<Vec<String>>>,
    pub nicti_edit_document: Option<Option<String>>,
}

const NICTI_NS: &str = "https://nicti.dev/xmp/1.0/";

fn local_name(qname: QName) -> Vec<u8> {
    match qname.as_ref().iter().position(|&b| b == b':') {
        Some(i) => qname.as_ref()[i + 1..].to_vec(),
        None => qname.as_ref().to_vec(),
    }
}

fn is_local(qname: QName, target: &str) -> bool {
    local_name(qname) == target.as_bytes()
}

/// Finds the index of the matching `Event::End` for the `Event::Start` at
/// `start_idx`, by tracking nesting depth over every intervening
/// Start/End event regardless of element name. Returns `start_idx` itself
/// if that event is `Event::Empty` (self-closing -- no matching End exists).
///
/// Returns `Err(PatchError::Unbalanced)`, never panics, if no matching End
/// is found before EOF -- `quick_xml`'s reader doesn't require balanced
/// nesting and will happily run to `Event::Eof` on a half-written sidecar
/// (a real possibility: an LRC crash or a disk-full mid-save, not just an
/// adversarial input), so this is a real, reachable error path, not just a
/// defensive check.
fn matching_end(events: &[Event<'static>], start_idx: usize) -> Result<usize, PatchError> {
    if matches!(events[start_idx], Event::Empty(_)) {
        return Ok(start_idx);
    }
    let mut depth = 1i32;
    for (i, ev) in events.iter().enumerate().skip(start_idx + 1) {
        match ev {
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    return Ok(i);
                }
            }
            _ => {}
        }
    }
    Err(PatchError::Unbalanced)
}

/// Applies `patch` to `xmp`, returning the full patched packet text.
pub fn apply(xmp: &str, patch: &Patch) -> Result<String, PatchError> {
    let mut reader = Reader::from_str(xmp);
    reader.config_mut().trim_text(false);

    let mut events: Vec<Event<'static>> = Vec::new();
    loop {
        let ev = reader.read_event()?.into_owned();
        let is_eof = matches!(ev, Event::Eof);
        events.push(ev);
        if is_eof {
            break;
        }
    }

    let desc_idx = events
        .iter()
        .position(
            |e| matches!(e, Event::Start(s) | Event::Empty(s) if is_local(s.name(), "Description")),
        )
        .ok_or(PatchError::NoDescription)?;
    let desc_end_idx = matching_end(&events, desc_idx)?;

    // 1. Rewrite the Description tag's own attributes (Rating/Label/nicti:editDocument).
    let new_desc_start = {
        let (was_start, orig) = match &events[desc_idx] {
            Event::Start(s) => (true, s.clone()),
            Event::Empty(s) => (false, s.clone()),
            _ => unreachable!(),
        };
        let mut new_tag =
            BytesStart::new(String::from_utf8_lossy(orig.name().as_ref()).into_owned());
        let mut saw_nicti_ns = false;
        for attr in orig.attributes() {
            let attr = attr?;
            let key = attr.key;
            if is_local(key, "Rating") && patch.rating.is_some() {
                continue;
            }
            if is_local(key, "Label") && patch.label.is_some() {
                continue;
            }
            if is_local(key, "editDocument") && patch.nicti_edit_document.is_some() {
                continue;
            }
            if key.as_ref() == b"xmlns:nicti" {
                saw_nicti_ns = true;
            }
            new_tag.push_attribute((key.as_ref(), attr.value.as_ref()));
        }
        if let Some(Some(v)) = &patch.rating {
            new_tag.push_attribute(("xmp:Rating", v.to_string().as_str()));
        }
        if let Some(Some(v)) = &patch.label {
            new_tag.push_attribute(("xmp:Label", v.as_str()));
        }
        if let Some(Some(v)) = &patch.nicti_edit_document {
            if !saw_nicti_ns {
                new_tag.push_attribute(("xmlns:nicti", NICTI_NS));
            }
            new_tag.push_attribute(("nicti:editDocument", v.as_str()));
        }
        (was_start, new_tag)
    };

    // 2. Find and drop any existing dc:subject / lr:hierarchicalSubject
    //    child blocks, if this write touches that list.
    let mut skip: Vec<bool> = vec![false; events.len()];
    let touches_keywords = patch.keywords.is_some() || patch.hierarchical_keywords.is_some();
    if touches_keywords && new_desc_start.0 {
        let mut i = desc_idx + 1;
        while i < desc_end_idx {
            let is_subject = matches!(&events[i], Event::Start(s) | Event::Empty(s) if is_local(s.name(), "subject"));
            let is_hier = matches!(&events[i], Event::Start(s) | Event::Empty(s) if is_local(s.name(), "hierarchicalSubject"));
            if (is_subject && patch.keywords.is_some())
                || (is_hier && patch.hierarchical_keywords.is_some())
            {
                let block_end = matching_end(&events, i)?;
                for s in skip.iter_mut().take(block_end + 1).skip(i) {
                    *s = true;
                }
                i = block_end + 1;
            } else {
                i += 1;
            }
        }
    }

    // 3. Serialize: everything up to Description unchanged, patched
    //    Description tag, preserved (non-skipped) children, freshly built
    //    keyword blocks, Description close, everything after unchanged.
    let mut writer = Writer::new(Vec::new());
    for ev in events.iter().take(desc_idx) {
        writer.write_event(ev.clone())?;
    }

    let needs_children = new_desc_start.0
        || (touches_keywords
            && (!patch.keywords.as_deref().unwrap_or(&[]).is_empty()
                || !patch
                    .hierarchical_keywords
                    .as_deref()
                    .unwrap_or(&[])
                    .is_empty()));

    if needs_children {
        writer.write_event(Event::Start(new_desc_start.1.clone()))?;
        if new_desc_start.0 {
            for i in (desc_idx + 1)..desc_end_idx {
                if !skip[i] {
                    writer.write_event(events[i].clone())?;
                }
            }
        }
        // Skip writing a container at all when the patch clears a list to
        // empty (`Some(vec![])`) -- inserting `<dc:subject><rdf:Bag/></dc:subject>`
        // for "no keywords" would be a needless empty container never asked
        // for, not a faithful "leave nothing here" clear.
        if let Some(kws) = patch.keywords.as_deref().filter(|k| !k.is_empty()) {
            write_bag(&mut writer, "dc:subject", kws)?;
        }
        if let Some(paths) = patch
            .hierarchical_keywords
            .as_deref()
            .filter(|p| !p.is_empty())
        {
            let joined: Vec<String> = paths.iter().map(|p| p.join("|")).collect();
            write_bag(&mut writer, "lr:hierarchicalSubject", &joined)?;
        }
        writer.write_event(Event::End(new_desc_start.1.to_end().into_owned()))?;
    } else {
        writer.write_event(Event::Empty(new_desc_start.1))?;
    }

    for ev in events.iter().skip(desc_end_idx + 1) {
        writer.write_event(ev.clone())?;
    }

    Ok(String::from_utf8(writer.into_inner()).expect("xmp is always valid utf-8"))
}

fn write_bag<W: std::io::Write>(
    writer: &mut Writer<W>,
    tag: &str,
    items: &[String],
) -> Result<(), quick_xml::Error> {
    writer.write_event(Event::Start(BytesStart::new(tag)))?;
    writer.write_event(Event::Start(BytesStart::new("rdf:Bag")))?;
    for item in items {
        writer.write_event(Event::Start(BytesStart::new("rdf:li")))?;
        writer.write_event(Event::Text(BytesText::new(item)))?;
        writer.write_event(Event::End(BytesEnd::new("rdf:li")))?;
    }
    writer.write_event(Event::End(BytesEnd::new("rdf:Bag")))?;
    writer.write_event(Event::End(BytesEnd::new(tag)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lrc_fields;

    const SAMPLE: &str = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/" xmp:Rating="3" crs:WhiteBalance="Custom"/></rdf:RDF></x:xmpmeta>"#;

    #[test]
    fn patching_rating_preserves_unrelated_namespaces() {
        let patched = apply(
            SAMPLE,
            &Patch {
                rating: Some(Some(5)),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(patched.contains("crs:WhiteBalance=\"Custom\""), "{patched}");
        assert!(patched.contains("xmp:Rating=\"5\""), "{patched}");
        let meta = lrc_fields::read(&patched).unwrap();
        assert_eq!(meta.rating, Some(5));
    }

    #[test]
    fn patching_one_attribute_preserves_every_other_attributes_exact_value() {
        // Stronger than the substring check above: the Description tag is
        // rebuilt attribute-by-attribute (not byte-spliced), so this proves
        // every *value* this patch doesn't own survives exactly, including
        // one containing characters (`&`, `"`) that a careless rebuild could
        // re-escape differently. It does not assert byte-identical XML
        // serialization of the tag as a whole (quoting/attribute order can
        // still be normalized by the writer) -- see the module's `apply`
        // docs and ADR-0059 for that distinction.
        let xmp = r#"<rdf:Description xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/" xmp:Rating="3" xmlns:xmp="http://ns.adobe.com/xap/1.0/" crs:Note="Tom &amp; Jerry said &quot;hi&quot;"/>"#;
        let patched = apply(
            xmp,
            &Patch {
                rating: Some(Some(5)),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            patched.contains("Tom &amp; Jerry said &quot;hi&quot;")
                || patched.contains("Tom &amp; Jerry said &#34;hi&#34;"),
            "unrelated attribute value was not preserved exactly: {patched}"
        );
    }

    #[test]
    fn malformed_truncated_xmp_returns_an_error_not_a_panic() {
        // A half-written sidecar (LRC crash, disk full mid-save) can leave
        // an unbalanced document -- quick_xml's reader doesn't require
        // balanced nesting and just runs to Eof. This must surface as
        // `PatchError::Unbalanced`, never panic.
        let truncated = r#"<rdf:Description xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:xmp="http://ns.adobe.com/xap/1.0/"><xmp:Rating>4</xmp:Rating>"#;
        let err = apply(
            truncated,
            &Patch {
                rating: Some(Some(5)),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, PatchError::Unbalanced), "{err:?}");
    }

    #[test]
    fn clearing_keywords_to_empty_removes_the_container_entirely() {
        // Some(vec![]) means "no keywords", not "an empty rdf:Bag" -- the
        // container should disappear, not be replaced with a needless
        // empty one.
        let with_kw = apply(
            SAMPLE,
            &Patch {
                keywords: Some(vec!["Old".into()]),
                ..Default::default()
            },
        )
        .unwrap();
        let cleared = apply(
            &with_kw,
            &Patch {
                keywords: Some(vec![]),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            !cleared.contains("dc:subject"),
            "clearing to empty should remove the container, not insert an empty one: {cleared}"
        );
        let meta = lrc_fields::read(&cleared).unwrap();
        assert!(meta.keywords.is_empty());
    }

    #[test]
    fn patch_touching_only_rating_leaves_label_absent() {
        let patched = apply(
            SAMPLE,
            &Patch {
                rating: Some(Some(2)),
                ..Default::default()
            },
        )
        .unwrap();
        let meta = lrc_fields::read(&patched).unwrap();
        assert_eq!(meta.label, None);
    }

    #[test]
    fn writing_keywords_round_trips() {
        let patched = apply(
            SAMPLE,
            &Patch {
                keywords: Some(vec!["Anthrocon".into(), "2025".into()]),
                hierarchical_keywords: Some(vec![vec!["Events".into(), "Anthrocon 2025".into()]]),
                ..Default::default()
            },
        )
        .unwrap();
        let meta = lrc_fields::read(&patched).unwrap();
        assert_eq!(meta.keywords, vec!["Anthrocon", "2025"]);
        assert_eq!(
            meta.hierarchical_keywords,
            vec![vec!["Events".to_string(), "Anthrocon 2025".to_string()]]
        );
        assert!(patched.contains("crs:WhiteBalance=\"Custom\""), "{patched}");
    }

    #[test]
    fn replacing_existing_keywords_drops_the_old_ones() {
        let with_kw = apply(
            SAMPLE,
            &Patch {
                keywords: Some(vec!["Old".into()]),
                ..Default::default()
            },
        )
        .unwrap();
        let replaced = apply(
            &with_kw,
            &Patch {
                keywords: Some(vec!["New".into()]),
                ..Default::default()
            },
        )
        .unwrap();
        let meta = lrc_fields::read(&replaced).unwrap();
        assert_eq!(meta.keywords, vec!["New"]);
    }

    #[test]
    fn nicti_edit_document_round_trips_through_the_real_packet() {
        let patched = apply(
            SAMPLE,
            &Patch {
                nicti_edit_document: Some(Some("eyJmb28iOjF9".into())),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            patched.contains("nicti:editDocument=\"eyJmb28iOjF9\""),
            "{patched}"
        );
        assert!(patched.contains("crs:WhiteBalance=\"Custom\""), "{patched}");
    }
}
