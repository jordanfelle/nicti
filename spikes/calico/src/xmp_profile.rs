//! Thin shim over `nicti_calico::xmp_profile` (promoted from this spike in #321). The decoder and
//! its tests live in the crate; this only converts into the spike's own `HueSatMap` /
//! `TableEncoding` types so `pipeline.rs` and the `calico` CLI keep working until the spike is
//! deleted (after #149's DeltaE run).

use crate::dcp::TableEncoding;
use crate::huesatmap::HueSatMap;

pub use nicti_calico::xmp_profile::LookProfileError;

#[derive(Debug)]
pub struct LookProfile {
    pub name: String,
    pub look_table: HueSatMap,
    pub encoding: TableEncoding,
    pub unsupported_settings: Vec<String>,
}

pub fn parse(xmp_text: &str) -> Result<LookProfile, LookProfileError> {
    let p = nicti_calico::xmp_profile::parse(xmp_text)?;
    Ok(LookProfile {
        name: p.name,
        look_table: HueSatMap {
            hue_divisions: p.look_table.hue_divisions,
            sat_divisions: p.look_table.sat_divisions,
            val_divisions: p.look_table.val_divisions,
            data: p.look_table.data,
        },
        encoding: match p.encoding {
            nicti_calico::dcp::TableEncoding::Linear => TableEncoding::Linear,
            nicti_calico::dcp::TableEncoding::Srgb => TableEncoding::Srgb,
        },
        unsupported_settings: p.unsupported_settings,
    })
}
