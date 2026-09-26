//! Adobe Camera Raw "Look" `.xmp` profiles (e.g. the user's own installed "Adobe Vivid" preset
//! under `...\CameraRaw\Settings\Adobe\Profiles\`). **Research risk, per ADR-0021 and the plan
//! this spike implements**: the embedded look-table encoding inside these files is not publicly
//! documented, and this parser could only be timeboxed, not completed with confidence, in this
//! sandbox (no real `.xmp` sample was available to validate against -- ADR-0003/licensing forbid
//! bundling one, and reverse-engineering an undocumented binary encoding from zero real samples
//! risks silently producing plausible-looking but wrong colors, which is worse than refusing).
//!
//! What this *does* do: parse the RDF/XML container far enough to find a `crs:Look` /
//! `crs:LookTable` (or `crs:HasSettings`) property containing base64 data, and attempt to decode
//! that payload as a DCP-style TIFF IFD (the same format `dcp.rs` reads) -- plausible since Adobe
//! is known to reuse DNG/DCP tag semantics for these embedded tables. If the embedded blob
//! doesn't parse as such an IFD, this returns [`LookProfileError::UnrecognizedTableFormat`]
//! rather than guessing. See the follow-up issue this PR files for finishing this once real
//! sample files and a way to validate against them are available.

use thiserror::Error;

use crate::dcp::{DcpError, DcpProfile};
use crate::huesatmap::HueSatMap;

#[derive(Debug, Error)]
pub enum LookProfileError {
    #[error("xml parse error: {0}")]
    Xml(#[from] roxmltree::Error),
    #[error("no crs:Look/crs:LookTable (or equivalent) property found in this .xmp")]
    NoLookTableProperty,
    #[error("base64 decode failed: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error(
        "embedded look-table payload did not parse as a DCP-style IFD -- this encoding is not \
         yet understood, see ADR-0021's Deferred section and the filed follow-up issue"
    )]
    UnrecognizedTableFormat(#[source] DcpError),
}

#[derive(Debug)]
pub struct LookProfile {
    pub name: String,
    pub look_table: HueSatMap,
}

/// Attempts to parse a `.xmp` look-profile file's embedded look table. On
/// `UnrecognizedTableFormat`, callers should fall back to measuring against Adobe Standard/Color
/// (the DCP-only path) rather than guessing at a partially-decoded table.
pub fn parse(xmp_text: &str) -> Result<LookProfile, LookProfileError> {
    let doc = roxmltree::Document::parse(xmp_text)?;

    let mut name = String::from("(unnamed look)");
    let mut table_b64: Option<String> = None;

    for node in doc.descendants() {
        for attr in node.attributes() {
            match attr.name() {
                "Name" if table_b64.is_none() => name = attr.value().to_string(),
                "LookTable" | "Table" => table_b64 = Some(attr.value().to_string()),
                _ => {}
            }
        }
        if matches!(node.tag_name().name(), "LookTable" | "Table") {
            if let Some(text) = node.text() {
                table_b64 = Some(text.trim().to_string());
            }
        }
    }

    let b64 = table_b64.ok_or(LookProfileError::NoLookTableProperty)?;
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64.trim())?;

    let profile = DcpProfile::parse(&bytes).map_err(LookProfileError::UnrecognizedTableFormat)?;
    let look_table = profile.look_table.or(profile.hue_sat_map1).ok_or(
        LookProfileError::UnrecognizedTableFormat(DcpError::MissingTag(
            51959,
            "ProfileLookTableData",
        )),
    )?;

    Ok(LookProfile { name, look_table })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_look_table_property_is_reported_clearly() {
        let xmp = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description crs:Name="Adobe Vivid" xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"/></rdf:RDF></x:xmpmeta>"#;
        let err = parse(xmp).unwrap_err();
        assert!(matches!(err, LookProfileError::NoLookTableProperty));
    }

    #[test]
    fn malformed_xml_is_reported_as_xml_error() {
        let err = parse("<not valid xml").unwrap_err();
        assert!(matches!(err, LookProfileError::Xml(_)));
    }

    #[test]
    fn invalid_base64_is_reported_clearly() {
        let xmp = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"><crs:LookTable>not-valid-base64!!!</crs:LookTable></rdf:Description></rdf:RDF></x:xmpmeta>"#;
        let err = parse(xmp).unwrap_err();
        assert!(matches!(err, LookProfileError::Base64(_)));
    }
}
