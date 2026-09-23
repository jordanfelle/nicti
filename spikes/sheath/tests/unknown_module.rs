//! Registry-level analogue of ADR-0002's "a stage id this build doesn't recognize stays
//! read-only rather than being dropped or guessed at": looking up an id with no installed
//! module returns `None` cleanly, never a panic or a guessed fallback.

use sheath::registry::{find_descriptor, Descriptor};

#[test]
fn unknown_id_returns_none_not_a_guess() {
    let installed = [
        Descriptor {
            id: "nicti.decoder.libraw",
            version: 1,
        },
        Descriptor {
            id: "nicti.exporter.jpeg",
            version: 1,
        },
    ];

    assert!(find_descriptor(&installed, "nicti.decoder.libraw").is_some());
    assert!(find_descriptor(&installed, "someplugin.render_stage.dreamify").is_none());
}
