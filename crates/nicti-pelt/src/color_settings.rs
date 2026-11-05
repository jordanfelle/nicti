//! The Color menu's settings, remembered per catalog (#725): black point compensation, soft
//! proofing (on/off and the proof space) and the gamut warning. Persisted beside the catalog
//! (`<catalog>.color.json`, temp file + rename) like `preview_settings`, so they survive a restart.
//! The display profile itself is never stored: it is read from the OS every launch.

use std::path::{Path, PathBuf};

use nicti_calico::space::OutputSpace;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ColorSettings {
    /// Black point compensation for a LUT monitor profile (#320); on by default, as in LRC.
    pub black_point: bool,
    pub soft_proof: bool,
    /// The proof space by its `OutputSpace::name`; an unknown name falls back to sRGB.
    pub proof_space: String,
    pub gamut_warn: bool,
}

impl Default for ColorSettings {
    fn default() -> Self {
        ColorSettings {
            black_point: true,
            soft_proof: false,
            proof_space: OutputSpace::Srgb.name().to_owned(),
            gamut_warn: false,
        }
    }
}

impl ColorSettings {
    /// The proof space, or sRGB for a name this build does not know.
    pub fn proof(&self) -> OutputSpace {
        OutputSpace::ALL
            .into_iter()
            .find(|s| s.name() == self.proof_space)
            .unwrap_or(OutputSpace::Srgb)
    }
}

/// Where the settings live: `<catalog file name>.color.json`, next to the catalog.
pub fn settings_file_for(catalog_path: &Path) -> PathBuf {
    let mut name = catalog_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "nicti".into());
    name.push(".color.json");
    catalog_path.with_file_name(name)
}

/// The saved settings, or the defaults when the file is absent, unreadable or corrupt (a bad file
/// never breaks colour management -- it just resets to the defaults). Fields missing from an older
/// file take their defaults.
pub fn load(catalog_path: &Path) -> ColorSettings {
    std::fs::read_to_string(settings_file_for(catalog_path))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Writes via a temp file + rename so a crash never leaves a half-written document.
pub fn save(catalog_path: &Path, settings: &ColorSettings) -> std::io::Result<()> {
    let path = settings_file_for(catalog_path);
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string(settings).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

/// Remembers what was last saved so the per-frame check writes only when something changed (and
/// retries on a failed write, which leaves `last` stale).
pub struct Saver {
    last: ColorSettings,
}

impl Saver {
    /// `loaded` is what is on disk (or the defaults): nothing to write until it changes.
    pub fn new(loaded: ColorSettings) -> Self {
        Saver { last: loaded }
    }

    /// Saves `now` when it differs from the last saved state. Best effort: the colour setting is
    /// not worth a notice if the disk write fails (the next frame tries again). Returns whether a
    /// write happened.
    pub fn save_if_changed(&mut self, catalog_path: &Path, now: &ColorSettings) -> bool {
        if *now == self.last {
            return false;
        }
        if save(catalog_path, now).is_ok() {
            self.last = now.clone();
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nicti-color-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("lib.nicti")
    }

    #[test]
    fn settings_round_trip_beside_the_catalog_and_default_when_absent() {
        let cat = catalog("rt");
        assert_eq!(load(&cat), ColorSettings::default());
        assert!(
            ColorSettings::default().black_point,
            "on by default, as in LRC"
        );
        let s = ColorSettings {
            black_point: false,
            soft_proof: true,
            proof_space: OutputSpace::AdobeRgb.name().to_owned(),
            gamut_warn: true,
        };
        save(&cat, &s).unwrap();
        assert!(settings_file_for(&cat).ends_with("lib.nicti.color.json"));
        assert_eq!(load(&cat), s);
        assert_eq!(load(&cat).proof(), OutputSpace::AdobeRgb);
        assert!(!settings_file_for(&cat).with_extension("json.tmp").exists());
    }

    #[test]
    fn a_corrupt_partial_or_unknown_file_never_breaks_colour_management() {
        let cat = catalog("bad");
        let path = settings_file_for(&cat);
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(load(&cat), ColorSettings::default());
        // An older file with only some fields: the rest keep their defaults.
        std::fs::write(&path, r#"{"soft_proof":true}"#).unwrap();
        let s = load(&cat);
        assert!(s.soft_proof && s.black_point && !s.gamut_warn);
        // A proof space this build does not know falls back to sRGB.
        std::fs::write(&path, r#"{"proof_space":"ProPhoto 9000"}"#).unwrap();
        assert_eq!(load(&cat).proof(), OutputSpace::Srgb);
        // Wrong types are a corrupt file, not a panic.
        std::fs::write(&path, r#"{"black_point":"yes"}"#).unwrap();
        assert_eq!(load(&cat), ColorSettings::default());
    }

    #[test]
    fn the_saver_writes_only_when_something_changed_and_retries_a_failed_write() {
        let cat = catalog("saver");
        let mut saver = Saver::new(load(&cat));
        let mut s = ColorSettings::default();
        assert!(
            !saver.save_if_changed(&cat, &s),
            "nothing changed: no write"
        );
        assert!(!settings_file_for(&cat).exists());
        s.gamut_warn = true;
        assert!(saver.save_if_changed(&cat, &s));
        assert!(!saver.save_if_changed(&cat, &s), "saved: no second write");
        assert!(load(&cat).gamut_warn);
        // A failing write (the catalog's directory is gone) leaves the state stale so it retries.
        let gone = std::env::temp_dir().join("nicti-color-no-such-dir/x/lib.nicti");
        s.soft_proof = true;
        assert!(!saver.save_if_changed(&gone, &s));
        assert!(
            !saver.save_if_changed(&gone, &s),
            "still failing, still trying"
        );
        assert!(
            saver.save_if_changed(&cat, &s),
            "the next good write goes through"
        );
        assert!(load(&cat).soft_proof);
    }
}
