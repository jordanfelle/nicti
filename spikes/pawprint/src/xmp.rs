//! Recovery-path proof for the ADR's `nicti:` XMP layer: serialize a full
//! `EditDocument` into an XMP-shaped packet and parse it back losslessly, so
//! a lost/corrupt catalog can be rebuilt from sidecars. This is a throwaway
//! wire format — the real XMP/RDF structure and library choice belong to
//! #59, not this spike.

use base64::{engine::general_purpose::STANDARD, Engine as _};

use crate::EditDocument;

const NS_URI: &str = "https://nicti.dev/xmp/1.0/";

/// Serialize a document into a minimal XMP-like packet: canonical JSON,
/// base64'd, carried in a single `nicti:editDocument` attribute. Lossless by
/// construction (it's just JSON round-tripped through base64), which is the
/// property the recovery path actually needs — the packet's XML shape here
/// is a placeholder, not a claim about the eventual real format.
pub fn to_packet(doc: &EditDocument) -> String {
    let json = serde_json::to_vec(doc).expect("EditDocument always serializes");
    let encoded = STANDARD.encode(json);
    format!(
        "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description xmlns:nicti=\"{NS_URI}\" nicti:editDocument=\"{encoded}\"/></rdf:RDF></x:xmpmeta>"
    )
}

#[derive(Debug)]
pub struct PacketParseError;

pub fn from_packet(packet: &str) -> Result<EditDocument, PacketParseError> {
    let marker = "nicti:editDocument=\"";
    let start = packet.find(marker).ok_or(PacketParseError)? + marker.len();
    let end = packet[start..].find('"').ok_or(PacketParseError)? + start;
    let encoded = &packet[start..end];
    let json = STANDARD.decode(encoded).map_err(|_| PacketParseError)?;
    serde_json::from_slice(&json).map_err(|_| PacketParseError)
}

/// One side of a catalog-vs-sidecar comparison: a document plus the
/// modification time it was last written at.
pub struct Side<'a> {
    pub document: &'a EditDocument,
    pub mtime_ms: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Both sides already agree — no conflict.
    NoConflict,
    /// Contents differ; this side's mtime is later.
    PreferCatalog,
    PreferSidecar,
    /// Contents differ and the mtimes are within the same ambiguity window
    /// (e.g. filesystem mtime-resolution granularity) — don't guess.
    FlagForManualReview,
}

/// The ADR's "Recovery from sidecars" conflict rule: compare the sidecar's
/// embedded document hash against the catalog's; if they match, there's no
/// conflict. If they differ, prefer whichever side has the later mtime,
/// unless the mtimes are within `ambiguity_window_ms` of each other, in
/// which case flag it for manual review rather than silently picking a
/// side.
pub fn resolve_conflict(catalog: Side<'_>, sidecar: Side<'_>, ambiguity_window_ms: u128) -> Resolution {
    if catalog.document.content_hash() == sidecar.document.content_hash() {
        return Resolution::NoConflict;
    }
    let diff = catalog.mtime_ms.abs_diff(sidecar.mtime_ms);
    if diff <= ambiguity_window_ms {
        return Resolution::FlagForManualReview;
    }
    if catalog.mtime_ms > sidecar.mtime_ms {
        Resolution::PreferCatalog
    } else {
        Resolution::PreferSidecar
    }
}
