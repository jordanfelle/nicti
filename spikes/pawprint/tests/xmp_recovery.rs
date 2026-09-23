//! Proves the ADR's "rebuild the catalog from sidecars" recovery path: an
//! `EditDocument` survives a round-trip through the `nicti:` XMP packet
//! format losslessly, including virtual-copy-style multi-stage documents
//! and an unrecognized plugin stage.

use pawprint::{xmp, EditDocument, StageEntry};
use serde_json::json;

#[test]
fn document_roundtrips_through_xmp_packet_losslessly() {
    let mut doc = EditDocument::default();
    doc.stages.insert(
        "white_balance".to_string(),
        StageEntry {
            schema_version: 1,
            params: json!({"temp": 5500, "tint": 10}),
        },
    );
    doc.stages.insert(
        "mask.subject_0".to_string(),
        StageEntry {
            schema_version: 2,
            params: json!({"model_id": "sam2-nicti", "model_version": "0.4.1", "exposure": 0.4}),
        },
    );
    doc.stages.insert(
        "vendor.unrecognized_plugin".to_string(),
        StageEntry {
            schema_version: 1,
            params: json!({"opaque": [1, 2, 3]}),
        },
    );

    let packet = xmp::to_packet(&doc);
    assert!(packet.contains("nicti:editDocument"));

    let recovered = xmp::from_packet(&packet).expect("packet must parse");
    assert_eq!(
        doc, recovered,
        "recovering from the XMP packet must reproduce the document exactly"
    );
}

#[test]
fn malformed_packet_is_rejected_not_silently_defaulted() {
    let result = xmp::from_packet("<x:xmpmeta></x:xmpmeta>");
    assert!(
        result.is_err(),
        "a packet with no nicti:editDocument attribute must fail, not return an empty document"
    );
}
