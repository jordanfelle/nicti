//! Reads and patches a full XMP packet, preserving everything this spike
//! doesn't own -- `crs:` mask corrections, `exif:`/`aux:` camera metadata,
//! any other tool's namespace -- byte-for-byte, per ADR-0021's "XMP is an
//! interop layer, not the source of truth for what Nicti doesn't already
//! know about" framing.
//!
//! **Method: event-copy-and-patch, not a fresh serializer.** The whole
//! packet is parsed into a flat list of `quick_xml` events; everything
//! outside the fields this module owns is re-emitted completely unchanged.
//! Only the first `rdf:Description`'s owned attributes are rewritten, and
//! only its `dc:subject`/`lr:hierarchicalSubject` child blocks are replaced
//! wholesale when a write touches keywords. Any *later* `rdf:Description`
//! has the properties this patch owns stripped (and nothing else touched), so
//! a stale value there can't override the patched one on read (#375). This is what makes the
//! byte-diff guard in `embedded::jpeg` meaningful: a patch that doesn't
//! touch keywords leaves every other byte of the packet identical.
//!
//! **Simplifying assumption, stated up front**: exactly one `rdf:Description`
//! carries the properties this module reads/writes -- true for every real
//! LRC-written sidecar this spike measured (`docs/research/scent-xmp-interop.md`).
//! XMP technically allows multiple `rdf:Description` elements for the same
//! subject with properties split across them; this module writes the owned
//! properties into the first one found and only removes them from the others.
//!
//! The lossless `nicti:` recovery layer (ADR-0021 layer b) rides in this
//! same packet as a single `nicti:editDocument` attribute, opaque
//! base64+JSON blob -- same shape `spikes/pawprint/src/xmp.rs` proved,
//! now hosted on a real packet instead of a throwaway wrapper string.

use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::name::QName;
use quick_xml::reader::Reader;
use quick_xml::writer::Writer;
use thiserror::Error;

use crate::gpx::Gps;
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
    #[error("the `exif` prefix is bound to a namespace other than the exif one; refusing to write GPS under it")]
    ExifPrefixConflict,
    #[error("GPS position out of range or not finite")]
    InvalidGps,
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
    /// `Some(true)` writes `nicti:pick="1"`; `Some(false)` removes it.
    pub nicti_pick: Option<bool>,
    /// `Some(Some(g))` writes the `exif:GPS*` properties; `Some(None)` removes them (#431).
    pub gps: Option<Option<Gps>>,
}

/// The exif-namespace spellings in scope at `events[idx]` (a Description): every prefix bound to
/// [`EXIF_NS`] there or on an ancestor (`""` = the default namespace), plus `exif` when nothing
/// binds that prefix. The bool is true when `exif` is bound to some *other* namespace.
fn exif_scope(events: &[Event<'static>], idx: usize) -> Result<(Vec<String>, bool), PatchError> {
    let mut frames: Vec<Vec<(String, String)>> = Vec::new();
    for (i, ev) in events.iter().enumerate().take(idx + 1) {
        match ev {
            Event::Start(t) | Event::Empty(t) => {
                let mut frame = Vec::new();
                for attr in t.attributes() {
                    let attr = attr?;
                    let key = attr.key.as_ref();
                    let uri = String::from_utf8_lossy(attr.value.as_bytes()).into_owned();
                    if key == "xmlns" {
                        frame.push((String::new(), uri));
                    } else if let Some(p) = key.strip_prefix("xmlns:") {
                        frame.push((p.to_owned(), uri));
                    }
                }
                frames.push(frame);
                if matches!(ev, Event::Empty(_)) && i != idx {
                    frames.pop();
                }
            }
            Event::End(_) => {
                frames.pop();
            }
            _ => {}
        }
    }
    let mut latest: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
    for (p, u) in frames.iter().flatten() {
        latest.insert(p, u);
    }
    let mut v: Vec<String> = latest
        .iter()
        .filter(|(_, u)| **u == EXIF_NS)
        .map(|(p, _)| (*p).to_owned())
        .collect();
    let conflict = latest.get("exif").is_some_and(|u| *u != EXIF_NS);
    if !latest.contains_key("exif") {
        v.push("exif".to_owned());
    }
    Ok((v, conflict))
}

/// Whether `name` is any `GPS*` property of the exif namespace under one of `prefixes`.
fn is_gps_name(name: &str, prefixes: &[String]) -> bool {
    let (p, l) = name.split_once(':').unwrap_or(("", name));
    l.starts_with("GPS") && prefixes.iter().any(|x| x == p)
}

/// XMP GPS coordinate: `DDD,MM.mmmmmmK` (degrees, decimal minutes, hemisphere letter).
fn xmp_coord(v: f64, pos: char, neg: char) -> String {
    let hemi = if v < 0.0 { neg } else { pos };
    let a = v.abs();
    let mut deg = a.floor();
    let mut min = ((a - deg) * 60.0 * 1e6).round() / 1e6;
    if min >= 60.0 {
        deg += 1.0;
        min = 0.0;
    }
    format!("{},{:.6}{}", deg as u32, min, hemi)
}

/// The `exif:GPS*` attribute pairs for `g`.
fn gps_attrs(g: &Gps) -> Vec<(&'static str, String)> {
    let mut v = vec![
        ("exif:GPSVersionID", "2.2.0.0".to_owned()),
        ("exif:GPSLatitude", xmp_coord(g.latitude, 'N', 'S')),
        ("exif:GPSLongitude", xmp_coord(g.longitude, 'E', 'W')),
        ("exif:GPSMapDatum", "WGS-84".to_owned()),
    ];
    if let Some(cm) = g.altitude.map(|a| (a * 100.0).round() as i64) {
        // rational with centimetre precision; Ref 1 = below sea level
        v.push(("exif:GPSAltitude", format!("{}/100", cm.abs())));
        v.push((
            "exif:GPSAltitudeRef",
            if cm < 0 { "1" } else { "0" }.to_owned(),
        ));
    }
    v
}

const NICTI_NS: &str = "https://nicti.dev/xmp/1.0/";
const XMP_NS: &str = "http://ns.adobe.com/xap/1.0/";
const DC_NS: &str = "http://purl.org/dc/elements/1.1/";
const EXIF_NS: &str = "http://ns.adobe.com/exif/1.0/";
const LR_NS: &str = "http://ns.adobe.com/lightroom/1.0/";

fn local_name(qname: QName) -> String {
    let name: &str = qname.as_ref();
    match name.find(':') {
        Some(i) => name[i + 1..].to_owned(),
        None => name.to_owned(),
    }
}

fn is_local(qname: QName, target: &str) -> bool {
    local_name(qname) == target
}

fn has_crs_prefix(qname: QName) -> bool {
    qname.as_ref().starts_with("crs:")
}

/// Whether this packet already carries any `crs:`-namespaced property --
/// element or attribute -- regardless of who wrote it. Used by the `crs:`
/// write gate's first-write case: a sidecar with no prior Nicti write
/// recorded (`last_written_hash: None`) might still already hold real
/// `crs:` data LRC itself wrote, which `should_write_crs` must not treat as
/// "safe to overwrite" just because Nicti has never touched it before.
pub fn has_crs_content(xmp: &str) -> Result<bool, PatchError> {
    let mut reader = Reader::from_str(xmp);
    reader.config_mut().trim_text(false);
    loop {
        match reader.read_event()? {
            Event::Eof => return Ok(false),
            Event::Start(e) | Event::Empty(e) => {
                if has_crs_prefix(e.name()) {
                    return Ok(true);
                }
                for attr in e.attributes() {
                    if has_crs_prefix(attr?.key) {
                        return Ok(true);
                    }
                }
            }
            _ => {}
        }
    }
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

/// The `rdf:about` of a Description tag; absent counts as the empty string.
fn about_of(tag: &BytesStart) -> Result<String, PatchError> {
    for attr in tag.attributes() {
        let attr = attr?;
        if attr.key.as_ref() == "rdf:about" {
            return Ok(attr.value.into_owned());
        }
    }
    Ok(String::new())
}

/// Whether `patch` owns the attribute `key` (so a rewrite drops its old value).
fn patch_owns_attr(key: QName, patch: &Patch, gps_pfx: &[String]) -> bool {
    (key.as_ref() == "xmp:Rating" && patch.rating.is_some())
        || (key.as_ref() == "xmp:Label" && patch.label.is_some())
        || (is_local(key, "editDocument") && patch.nicti_edit_document.is_some())
        || (key.as_ref() == "nicti:pick" && patch.nicti_pick.is_some())
        || (patch.gps.is_some() && is_gps_name(key.as_ref(), gps_pfx))
}

/// Whether the child element at `ev` is a block `patch` owns.
fn patch_owns_child(ev: &Event<'static>, patch: &Patch, gps_pfx: &[String], direct: bool) -> bool {
    let (Event::Start(s) | Event::Empty(s)) = ev else {
        return false;
    };
    let name = s.name();
    (is_local(name, "subject") && patch.keywords.is_some())
        || (is_local(name, "hierarchicalSubject") && patch.hierarchical_keywords.is_some())
        || (name.as_ref() == "xmp:Rating" && patch.rating.is_some())
        || (name.as_ref() == "xmp:Label" && patch.label.is_some())
        || (direct && patch.gps.is_some() && is_gps_name(name.as_ref(), gps_pfx))
}

/// Applies `patch` to `xmp`, returning the full patched packet text.
pub fn apply(xmp: &str, patch: &Patch) -> Result<String, PatchError> {
    if matches!(&patch.gps, Some(Some(g)) if !g.is_valid()) {
        return Err(PatchError::InvalidGps);
    }
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
    let gps_pfx = if patch.gps.is_some() {
        let (v, conflict) = exif_scope(&events, desc_idx)?;
        if conflict && matches!(&patch.gps, Some(Some(_))) {
            return Err(PatchError::ExifPrefixConflict);
        }
        v
    } else {
        Vec::new()
    };
    let new_desc_start = {
        let (was_start, orig) = match &events[desc_idx] {
            Event::Start(s) => (true, s.clone()),
            Event::Empty(s) => (false, s.clone()),
            _ => unreachable!(),
        };
        let mut new_tag = BytesStart::new(orig.name().as_ref().to_owned());
        let (mut saw_nicti_ns, mut saw_xmp_ns, mut saw_dc_ns, mut saw_lr_ns, mut saw_exif_ns) =
            (false, false, false, false, false);
        for attr in orig.attributes() {
            let attr = attr?;
            let key = attr.key;
            if key.as_ref() == "xmp:Rating" && patch.rating.is_some() {
                continue;
            }
            if key.as_ref() == "xmp:Label" && patch.label.is_some() {
                continue;
            }
            if is_local(key, "editDocument") && patch.nicti_edit_document.is_some() {
                continue;
            }
            if key.as_ref() == "nicti:pick" && patch.nicti_pick.is_some() {
                continue;
            }
            if patch.gps.is_some() && is_gps_name(key.as_ref(), &gps_pfx) {
                continue;
            }
            match key.as_ref() {
                "xmlns:xmp" => saw_xmp_ns = true,
                "xmlns:dc" => saw_dc_ns = true,
                "xmlns:lr" => saw_lr_ns = true,
                "xmlns:exif" => saw_exif_ns = true,
                _ => {}
            }
            if key.as_ref() == "xmlns:nicti" {
                saw_nicti_ns = true;
            }
            // `Attribute` itself, not a `(&str, &str)` tuple: the tuple form escapes the value, and
            // `attr.value` is already the raw (escaped) text, so it would double-escape.
            // push_attribute always wraps in double quotes, so a literal `"` from a
            // single-quoted source attribute must become `&quot;` or the output is malformed.
            new_tag.push_attribute(quick_xml::events::attributes::Attribute {
                key: attr.key,
                value: attr.value.replace('"', "&quot;").into(),
            });
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
        if patch.nicti_pick == Some(true) {
            if !saw_nicti_ns && !matches!(&patch.nicti_edit_document, Some(Some(_))) {
                new_tag.push_attribute(("xmlns:nicti", NICTI_NS));
            }
            new_tag.push_attribute(("nicti:pick", "1"));
        }
        if let Some(Some(g)) = &patch.gps {
            if !saw_exif_ns {
                new_tag.push_attribute(("xmlns:exif", EXIF_NS));
            }
            for (k, v) in gps_attrs(g) {
                new_tag.push_attribute((k, v.as_str()));
            }
        }
        // Every prefix this write emits must be bound. A rating-only packet (the shape LRC and the
        // in-repo SAMPLE use) may declare `xmp` but not `dc`/`lr`; redeclaring the standard URI on
        // the Description is harmless if an ancestor already binds it.
        let writes_scalar =
            matches!(&patch.rating, Some(Some(_))) || matches!(&patch.label, Some(Some(_)));
        if writes_scalar && !saw_xmp_ns {
            new_tag.push_attribute(("xmlns:xmp", XMP_NS));
        }
        if patch.keywords.as_deref().is_some_and(|k| !k.is_empty()) && !saw_dc_ns {
            new_tag.push_attribute(("xmlns:dc", DC_NS));
        }
        if patch
            .hierarchical_keywords
            .as_deref()
            .is_some_and(|k| !k.is_empty())
            && !saw_lr_ns
        {
            new_tag.push_attribute(("xmlns:lr", LR_NS));
        }
        (was_start, new_tag)
    };

    // 2. Find and drop any existing dc:subject / lr:hierarchicalSubject /
    //    element-form Rating / Label child blocks, for whichever fields
    //    this write actually touches. Without this, a packet carrying a
    //    *child-element* Rating/Label (lrc_fields::read accepts both forms
    //    -- see that module's docs) would have its patched attribute value
    //    silently overridden right back by the untouched old child element
    //    on the next read, since `read()` processes the child element after
    //    the Description's own attributes.
    let mut skip: Vec<bool> = vec![false; events.len()];
    let touches_keywords = patch.keywords.is_some() || patch.hierarchical_keywords.is_some();
    let touches_children =
        touches_keywords || patch.rating.is_some() || patch.label.is_some() || patch.gps.is_some();
    if touches_children && new_desc_start.0 {
        let mut i = desc_idx + 1;
        let mut depth = 0usize;
        while i < desc_end_idx {
            if matches!(events[i], Event::End(_)) {
                depth = depth.saturating_sub(1);
            }
            let is_subject = matches!(&events[i], Event::Start(s) | Event::Empty(s) if is_local(s.name(), "subject"));
            let is_hier = matches!(&events[i], Event::Start(s) | Event::Empty(s) if is_local(s.name(), "hierarchicalSubject"));
            let is_rating = matches!(&events[i], Event::Start(s) | Event::Empty(s) if s.name().as_ref() == "xmp:Rating");
            let is_label = matches!(&events[i], Event::Start(s) | Event::Empty(s) if s.name().as_ref() == "xmp:Label");
            if (is_subject && patch.keywords.is_some())
                || (is_hier && patch.hierarchical_keywords.is_some())
                || (is_rating && patch.rating.is_some())
                || (is_label && patch.label.is_some())
                || patch_owns_child(&events[i], patch, &gps_pfx, depth == 0)
            {
                let block_end = matching_end(&events, i)?;
                for s in skip.iter_mut().take(block_end + 1).skip(i) {
                    *s = true;
                }
                i = block_end + 1;
            } else {
                if matches!(events[i], Event::Start(_)) {
                    depth += 1;
                }
                i += 1;
            }
        }
    }

    // 2b. Later `rdf:Description` elements: strip what this patch owns so it can't win on read.
    let first_about = match &events[desc_idx] {
        Event::Start(s) | Event::Empty(s) => about_of(s)?,
        _ => unreachable!(),
    };
    let mut replaced: std::collections::HashMap<usize, Event<'static>> =
        std::collections::HashMap::new();
    let mut j = desc_end_idx + 1;
    while j < events.len() {
        let (is_desc, was_start) = match &events[j] {
            Event::Start(s) if is_local(s.name(), "Description") => (true, true),
            Event::Empty(s) if is_local(s.name(), "Description") => (true, false),
            _ => (false, false),
        };
        if !is_desc {
            j += 1;
            continue;
        }
        let end = matching_end(&events, j)?;
        let (Event::Start(orig) | Event::Empty(orig)) = &events[j] else {
            unreachable!()
        };
        // Only a Description about the same resource as the first is ours to strip; another
        // resource's properties are none of this patch's business.
        if about_of(orig)? != first_about {
            j = end + 1;
            continue;
        }
        let mut tag = BytesStart::new(orig.name().as_ref().to_owned());
        let mut changed = false;
        let later_pfx = if patch.gps.is_some() {
            exif_scope(&events, j)?.0
        } else {
            Vec::new()
        };
        for attr in orig.attributes() {
            let attr = attr?;
            if patch_owns_attr(attr.key, patch, &later_pfx) {
                changed = true;
                continue;
            }
            tag.push_attribute(quick_xml::events::attributes::Attribute {
                key: attr.key,
                value: attr.value.replace('"', "&quot;").into(),
            });
        }
        if changed {
            replaced.insert(
                j,
                if was_start {
                    Event::Start(tag)
                } else {
                    Event::Empty(tag)
                },
            );
        }
        if was_start {
            let mut i = j + 1;
            let mut depth = 0usize;
            while i < end {
                if matches!(events[i], Event::End(_)) {
                    depth = depth.saturating_sub(1);
                }
                if patch_owns_child(&events[i], patch, &later_pfx, depth == 0) {
                    let block_end = matching_end(&events, i)?;
                    for s in skip.iter_mut().take(block_end + 1).skip(i) {
                        *s = true;
                    }
                    i = block_end + 1;
                } else {
                    if matches!(events[i], Event::Start(_)) {
                        depth += 1;
                    }
                    i += 1;
                }
            }
        }
        j = end + 1;
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

    for (i, ev) in events.iter().enumerate().skip(desc_end_idx + 1) {
        if skip[i] {
            continue;
        }
        writer.write_event(replaced.get(&i).unwrap_or(ev).clone())?;
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
    fn gps_coordinates_format_as_xmp_degrees_decimal_minutes() {
        assert_eq!(xmp_coord(46.0005, 'N', 'S'), "46,0.030000N");
        assert_eq!(xmp_coord(-33.9, 'N', 'S'), "33,54.000000S");
        assert_eq!(xmp_coord(-0.5, 'E', 'W'), "0,30.000000W");
        // minutes that round up to 60 carry into the degrees
        assert_eq!(xmp_coord(10.99999999999, 'N', 'S'), "11,0.000000N");
    }

    #[test]
    fn patching_gps_writes_replaces_and_clears() {
        let g = Gps {
            latitude: 46.0005,
            longitude: -7.001,
            altitude: Some(-12.345),
        };
        let p = apply(
            SAMPLE,
            &Patch {
                gps: Some(Some(g)),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            p.contains("xmlns:exif=\"http://ns.adobe.com/exif/1.0/\""),
            "{p}"
        );
        assert!(p.contains("exif:GPSLatitude=\"46,0.030000N\""), "{p}");
        assert!(p.contains("exif:GPSLongitude=\"7,0.060000W\""), "{p}");
        assert!(
            p.contains("exif:GPSAltitude=\"1235/100\"") && p.contains("exif:GPSAltitudeRef=\"1\""),
            "{p}"
        );
        assert!(
            p.contains("crs:WhiteBalance=\"Custom\"") && p.contains("xmp:Rating=\"3\""),
            "{p}"
        );
        // re-patching replaces rather than duplicating, and keeps the one namespace binding
        let g2 = Gps {
            latitude: 1.0,
            longitude: 2.0,
            altitude: None,
        };
        let p2 = apply(
            &p,
            &Patch {
                gps: Some(Some(g2)),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(p2.matches("exif:GPSLatitude=").count(), 1, "{p2}");
        assert_eq!(p2.matches("xmlns:exif=").count(), 1, "{p2}");
        assert!(
            !p2.contains("GPSAltitude"),
            "stale altitude must not survive: {p2}"
        );
        let p3 = apply(
            &p2,
            &Patch {
                gps: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!p3.contains("GPS"), "{p3}");
        assert!(p3.contains("xmp:Rating=\"3\""), "{p3}");
    }

    #[test]
    fn patching_gps_replaces_element_form_properties() {
        let xmp = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:exif="http://ns.adobe.com/exif/1.0/"><exif:GPSLatitude>10,0.000000N</exif:GPSLatitude><exif:DateTimeOriginal>2026-05-01T10:00:00</exif:DateTimeOriginal></rdf:Description></rdf:RDF>"#;
        let g = Gps {
            latitude: 20.0,
            longitude: 30.0,
            altitude: None,
        };
        let p = apply(
            xmp,
            &Patch {
                gps: Some(Some(g)),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!p.contains("10,0.000000N"), "{p}");
        assert!(p.contains("exif:GPSLatitude=\"20,0.000000N\""), "{p}");
        assert!(
            p.contains("DateTimeOriginal"),
            "other exif: properties are preserved: {p}"
        );
    }

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
    fn single_quoted_attribute_with_literal_double_quote_stays_well_formed() {
        let xmp = r#"<rdf:Description xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/" xmlns:xmp="http://ns.adobe.com/xap/1.0/" crs:Note='said "hi"'/>"#;
        let patched = apply(
            xmp,
            &Patch {
                rating: Some(Some(5)),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            patched.contains("crs:Note=\"said &quot;hi&quot;\""),
            "{patched}"
        );
        assert!(apply(&patched, &Patch::default()).is_ok());
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
    fn patching_rating_removes_a_stale_element_form_child_that_would_otherwise_win_on_read() {
        // lrc_fields::read processes the Description's own attributes first,
        // then any child Rating/Label element -- so a leftover element-form
        // child would silently override the just-patched attribute value on
        // the next read, unless apply() also removes it.
        let xmp = r#"<rdf:Description xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="4"><xmp:Rating>4</xmp:Rating></rdf:Description>"#;
        let patched = apply(
            xmp,
            &Patch {
                rating: Some(Some(5)),
                ..Default::default()
            },
        )
        .unwrap();
        let meta = lrc_fields::read(&patched).unwrap();
        assert_eq!(
            meta.rating,
            Some(5),
            "a stale element-form Rating child overrode the patched attribute value: {patched}"
        );
        assert!(
            !patched.contains("<xmp:Rating>"),
            "stale element-form Rating child was not removed: {patched}"
        );
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

    #[test]
    fn pick_round_trips_and_clears() {
        let picked = apply(
            SAMPLE,
            &Patch {
                nicti_pick: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(picked.contains("xmlns:nicti="), "{picked}");
        assert!(picked.contains("nicti:pick=\"1\""), "{picked}");
        assert!(picked.contains("crs:WhiteBalance=\"Custom\""), "{picked}");
        assert!(lrc_fields::read(&picked).unwrap().pick);

        // Re-picking doesn't duplicate the attribute or the namespace.
        let again = apply(
            &picked,
            &Patch {
                nicti_pick: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(again.matches("nicti:pick=").count(), 1, "{again}");
        assert_eq!(again.matches("xmlns:nicti=").count(), 1, "{again}");

        let cleared = apply(
            &picked,
            &Patch {
                nicti_pick: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!cleared.contains("nicti:pick"), "{cleared}");
        assert!(!lrc_fields::read(&cleared).unwrap().pick);
    }

    #[test]
    fn pick_with_edit_document_declares_namespace_once() {
        let out = apply(
            SAMPLE,
            &Patch {
                nicti_edit_document: Some(Some("e30=".into())),
                nicti_pick: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(out.matches("xmlns:nicti=").count(), 1, "{out}");
        assert!(lrc_fields::read(&out).unwrap().pick);
    }

    #[test]
    fn patching_never_touches_foreign_rating_or_label_properties() {
        let xmp = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:MicrosoftPhoto="http://ns.microsoft.com/photo/1.0/" xmp:Rating="1" MicrosoftPhoto:Rating="75"/></rdf:RDF></x:xmpmeta>"#;
        let out = apply(
            xmp,
            &Patch {
                rating: Some(Some(4)),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(out.contains("MicrosoftPhoto:Rating=\"75\""), "{out}");
        assert_eq!(lrc_fields::read(&out).unwrap().rating, Some(4));
    }

    #[test]
    fn special_characters_survive_a_write_then_read() {
        let out = apply(
            SAMPLE,
            &Patch {
                label: Some(Some("Tom & Jerry <\"x\">".into())),
                keywords: Some(vec!["a&b".into(), "it's".into()]),
                ..Default::default()
            },
        )
        .unwrap();
        let meta = lrc_fields::read(&out).unwrap();
        assert_eq!(meta.label.as_deref(), Some("Tom & Jerry <\"x\">"));
        assert_eq!(meta.keywords, vec!["a&b", "it's"]);
    }

    #[test]
    fn keyword_blocks_and_scalars_never_use_an_unbound_prefix() {
        // Declares neither xmp, dc nor lr anywhere.
        let bare = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about=""/></rdf:RDF></x:xmpmeta>"#;
        let out = apply(
            bare,
            &Patch {
                rating: Some(Some(3)),
                keywords: Some(vec!["fox".into()]),
                hierarchical_keywords: Some(vec![vec!["Events".into(), "Con".into()]]),
                ..Default::default()
            },
        )
        .unwrap();
        for decl in ["xmlns:xmp=", "xmlns:dc=", "xmlns:lr="] {
            assert_eq!(out.matches(decl).count(), 1, "{decl} in {out}");
        }

        // A packet that already declares them is not given a second copy.
        let again = apply(
            &out,
            &Patch {
                rating: Some(Some(4)),
                keywords: Some(vec!["fox".into(), "cat".into()]),
                ..Default::default()
            },
        )
        .unwrap();
        for decl in ["xmlns:xmp=", "xmlns:dc=", "xmlns:lr="] {
            assert_eq!(again.matches(decl).count(), 1, "{decl} in {again}");
        }
    }

    #[test]
    fn later_descriptions_lose_the_properties_the_patch_owns() {
        let xmp = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"><rdf:Description xmp:Rating="1"/><rdf:Description xmp:Rating="5" crs:Note="keep"><dc:subject><rdf:Bag><rdf:li>stale</rdf:li></rdf:Bag></dc:subject></rdf:Description></rdf:RDF></x:xmpmeta>"#;
        let patch = Patch {
            rating: Some(Some(3)),
            keywords: Some(vec!["fox".into()]),
            ..Default::default()
        };
        let out = apply(xmp, &patch).unwrap();
        let meta = crate::lrc_fields::read(&out).unwrap();
        assert_eq!(meta.rating, Some(3));
        assert_eq!(meta.keywords, vec!["fox".to_string()]);
        assert!(out.contains(r#"crs:Note="keep""#));
        assert!(!out.contains("stale"));
    }

    #[test]
    fn descriptions_about_another_resource_are_left_alone() {
        let xmp = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:xmp="http://ns.adobe.com/xap/1.0/"><rdf:Description rdf:about="" xmp:Rating="1"/><rdf:Description rdf:about="uuid:other" xmp:Rating="5"/></rdf:RDF></x:xmpmeta>"#;
        let patch = Patch {
            rating: Some(Some(3)),
            ..Default::default()
        };
        let out = apply(xmp, &patch).unwrap();
        assert!(out.contains(r#"rdf:about="uuid:other" xmp:Rating="5""#));
    }

    const EXIF: &str = "http://ns.adobe.com/exif/1.0/";
    const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

    #[test]
    fn gps_clear_removes_every_gps_property_under_any_prefix() {
        let xmp = format!(
            r#"<rdf:RDF xmlns:rdf="{RDF}"><rdf:Description xmlns:e="{EXIF}" e:GPSLatitude="10,0N" e:GPSAreaInformation="Home Street" e:GPSTimeStamp="x" e:DateTimeOriginal="2026"><e:GPSLongitude>1,0E</e:GPSLongitude></rdf:Description></rdf:RDF>"#
        );
        let p = apply(
            &xmp,
            &Patch {
                gps: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!p.contains("GPS"), "{p}");
        assert!(p.contains("e:DateTimeOriginal"), "{p}");
        // replace under an aliased prefix leaves no stale or duplicate GPS attribute
        let g = Gps {
            latitude: 1.0,
            longitude: 2.0,
            altitude: None,
        };
        let p = apply(
            &xmp,
            &Patch {
                gps: Some(Some(g)),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!p.contains("e:GPS") && !p.contains("Home Street"), "{p}");
        assert_eq!(p.matches("GPSLatitude").count(), 1, "{p}");
    }

    #[test]
    fn gps_refuses_a_foreign_exif_prefix_and_invalid_positions() {
        let xmp = format!(
            r#"<rdf:RDF xmlns:rdf="{RDF}"><rdf:Description xmlns:exif="urn:other"/></rdf:RDF>"#
        );
        let g = Gps {
            latitude: 1.0,
            longitude: 2.0,
            altitude: None,
        };
        assert!(matches!(
            apply(
                &xmp,
                &Patch {
                    gps: Some(Some(g)),
                    ..Default::default()
                }
            ),
            Err(PatchError::ExifPrefixConflict)
        ));
        for bad in [
            Gps {
                latitude: f64::NAN,
                longitude: 0.0,
                altitude: None,
            },
            Gps {
                latitude: 0.0,
                longitude: f64::INFINITY,
                altitude: None,
            },
            Gps {
                latitude: 200.0,
                longitude: 0.0,
                altitude: None,
            },
            Gps {
                latitude: 0.0,
                longitude: -400.0,
                altitude: None,
            },
            Gps {
                latitude: 0.0,
                longitude: 0.0,
                altitude: Some(1e300),
            },
        ] {
            assert!(matches!(
                apply(
                    SAMPLE,
                    &Patch {
                        gps: Some(Some(bad)),
                        ..Default::default()
                    }
                ),
                Err(PatchError::InvalidGps)
            ));
        }
        let tiny = Gps {
            latitude: 0.0,
            longitude: 0.0,
            altitude: Some(-0.001),
        };
        let p = apply(
            SAMPLE,
            &Patch {
                gps: Some(Some(tiny)),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            p.contains("GPSAltitudeRef=\"0\"") && p.contains("GPSAltitude=\"0/100\""),
            "{p}"
        );
    }

    #[test]
    fn gps_patch_leaves_nested_and_other_resource_gps_alone() {
        let xmp = format!(
            r#"<rdf:RDF xmlns:rdf="{RDF}" xmlns:exif="{EXIF}"><rdf:Description rdf:about="a"><exif:GPSLatitude>5,0N</exif:GPSLatitude><x:Ingredients xmlns:x="urn:x"><rdf:Description rdf:about="b"><exif:GPSLatitude>77,0N</exif:GPSLatitude></rdf:Description></x:Ingredients></rdf:Description></rdf:RDF>"#
        );
        let g = Gps {
            latitude: 1.0,
            longitude: 2.0,
            altitude: None,
        };
        let p = apply(
            &xmp,
            &Patch {
                gps: Some(Some(g)),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            p.contains("77,0N"),
            "nested resource's GPS must survive: {p}"
        );
        assert!(!p.contains("5,0N"), "{p}");
    }

    #[test]
    fn gps_clear_sees_ancestor_and_default_namespace_bindings() {
        let clear = Patch {
            gps: Some(None),
            ..Default::default()
        };
        let anc = format!(
            r#"<rdf:RDF xmlns:rdf="{RDF}" xmlns:e="{EXIF}"><rdf:Description rdf:about="" e:GPSLatitude="10,0N" e:Keep="1"/></rdf:RDF>"#
        );
        let p = apply(&anc, &clear).unwrap();
        assert!(!p.contains("GPS") && p.contains("e:Keep"), "{p}");
        let g = Gps {
            latitude: 1.0,
            longitude: 2.0,
            altitude: None,
        };
        let p = apply(
            &anc,
            &Patch {
                gps: Some(Some(g)),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(p.matches("GPSLatitude").count(), 1, "{p}");
        let dflt = format!(
            r#"<rdf:RDF xmlns:rdf="{RDF}"><rdf:Description xmlns="{EXIF}"><GPSLatitude>10,0N</GPSLatitude><Other>1</Other></rdf:Description></rdf:RDF>"#
        );
        let p = apply(&dflt, &clear).unwrap();
        assert!(!p.contains("GPS") && p.contains("<Other>"), "{p}");
    }

    #[test]
    fn gps_prefix_rebound_in_a_later_description_is_not_stripped() {
        let xmp = format!(
            r#"<rdf:RDF xmlns:rdf="{RDF}"><rdf:Description rdf:about="" xmlns:exif="{EXIF}"/><rdf:Description rdf:about="" xmlns:exif="urn:o" exif:GPSLatitude="9"/></rdf:RDF>"#
        );
        let p = apply(
            &xmp,
            &Patch {
                gps: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(p.contains("exif:GPSLatitude=\"9\""), "{p}");
    }
}
