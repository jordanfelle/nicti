//! Batch planning (#57): turn a list of photos plus an [`ExportSpec`] into the exact output path
//! for every photo *before* anything is rendered.
//!
//! Sequence numbers are `sequence_start + position in the list`, fixed here, so no amount of
//! parallelism or failure can change a name; a photo that later fails or is skipped just leaves a
//! gap. Paths are de-duplicated within the batch on a **case-folded** full path (NTFS is
//! case-insensitive, so `IMG.jpg` and `img.jpg` are the same file) under every collision policy --
//! one photo in a batch must never overwrite another. The disk is consulted through a
//! [`PathProbe`] so this stays a pure function in tests. The write step
//! ([`crate::write::write_output`]) re-checks at write time, so a file that appears between
//! planning and writing still can't be clobbered under `UniqueSuffix`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::naming::{truncate_bytes, truncate_utf16, AssetFacts, Template, MAX_COMPONENT_UTF16};
use crate::spec::{CollisionPolicy, DestinationBase, ExportSpec};

/// Windows' classic `MAX_PATH` minus the terminating NUL.
pub const MAX_PATH_UTF16: usize = 259;
/// Room reserved for a `-NNNNN` collision suffix when truncating a stem.
const SUFFIX_RESERVE: usize = 6;
/// Never truncate a stem below this many characters, even if the folder alone is too long.
const MIN_STEM: usize = 8;

pub trait PathProbe {
    fn exists(&self, path: &Path) -> bool;
}

/// The real filesystem.
pub struct FsProbe;

impl PathProbe for FsProbe {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
}

/// One photo to export.
#[derive(Debug, Clone)]
pub struct PlanItem {
    pub facts: AssetFacts,
    /// The source file's directory: the destination when the spec says `SameAsSource`.
    pub source_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanAction {
    /// Write a new file.
    Write,
    /// Replace the existing file at `path` (`Overwrite` policy).
    Overwrite,
    /// Don't write; the string says why (`Skip` policy, file exists).
    Skip(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedOutput {
    /// Position in the input list.
    pub index: usize,
    pub asset_id: i64,
    pub sequence: u32,
    pub path: PathBuf,
    pub action: PlanAction,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub outputs: Vec<PlannedOutput>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PlanError {
    #[error(transparent)]
    Spec(#[from] crate::spec::SpecError),
    #[error("no unique name found for photo {asset_id} after {tries} tries")]
    Exhausted { asset_id: i64, tries: u32 },
}

const MAX_TRIES: u32 = 100_000;

fn utf16_len(p: &Path) -> usize {
    p.to_string_lossy().encode_utf16().count()
}

fn fold_key(p: &Path) -> String {
    p.to_string_lossy().to_lowercase()
}

/// Plans `items` (in the given order) for a file extension `ext` (no dot).
pub fn plan_batch(
    items: &[PlanItem],
    spec: &ExportSpec,
    ext: &str,
    probe: &dyn PathProbe,
) -> Result<Plan, PlanError> {
    spec.validate()?;
    let template = Template::parse(&spec.naming.template).map_err(crate::spec::SpecError::from)?;
    let subfolder = match spec.destination.subfolder.as_deref() {
        Some(s) if !s.is_empty() => {
            Some(Template::parse_subfolder(s).map_err(crate::spec::SpecError::Subfolder)?)
        }
        _ => None,
    };

    let mut plan = Plan::default();
    let mut taken: HashSet<String> = HashSet::new();

    for (index, item) in items.iter().enumerate() {
        let sequence = spec.naming.sequence_start.saturating_add(index as u32);
        let mut dir = match &spec.destination.base {
            DestinationBase::SameAsSource => item.source_dir.clone(),
            DestinationBase::Folder(p) => p.clone(),
        };
        if let Some(sub) = &subfolder {
            for component in sub.render_subfolder(&item.facts, sequence) {
                dir.push(component);
            }
        }

        let mut stem = template.render_filename(&item.facts, sequence);
        // Keep dir + stem + "-NNNNN" + "." + ext within MAX_PATH where we can.
        let fixed = utf16_len(&dir) + 1 + 1 + ext.len() + SUFFIX_RESERVE;
        let budget = MAX_PATH_UTF16.saturating_sub(fixed).max(MIN_STEM);
        if stem.encode_utf16().count() > budget {
            stem = truncate_utf16(&stem, budget)
                .trim_end_matches(['.', ' '])
                .to_string();
            if stem.is_empty() {
                stem = "untitled".to_string();
            }
        }
        // A single name component is limited to 255 UTF-16 units on NTFS but 255 *bytes* on
        // ext4/APFS/etc; leave room for "-NNNNN.ext" under both.
        let component_room = MAX_COMPONENT_UTF16.saturating_sub(SUFFIX_RESERVE + 1 + ext.len());
        stem = truncate_utf16(&stem, component_room);
        stem = truncate_bytes(&stem, component_room);
        if fixed + MIN_STEM > MAX_PATH_UTF16 {
            plan.warnings.push(format!(
                "destination folder for photo {} is very long ({} characters); the exported path \
                 exceeds {} and may fail on Windows",
                item.facts.asset_id,
                utf16_len(&dir),
                MAX_PATH_UTF16
            ));
        }

        let mut n = 1u32;
        let (path, action) = loop {
            if n > MAX_TRIES {
                return Err(PlanError::Exhausted {
                    asset_id: item.facts.asset_id,
                    tries: MAX_TRIES,
                });
            }
            let name = if n == 1 {
                format!("{stem}.{ext}")
            } else {
                format!("{stem}-{n}.{ext}")
            };
            let candidate = dir.join(name);
            if taken.contains(&fold_key(&candidate)) {
                n += 1;
                continue;
            }
            let exists = probe.exists(&candidate);
            match (spec.collision, exists) {
                (CollisionPolicy::UniqueSuffix, true) => {
                    n += 1;
                    continue;
                }
                (CollisionPolicy::Skip, true) => {
                    break (
                        candidate,
                        PlanAction::Skip("a file with this name already exists".to_string()),
                    );
                }
                (CollisionPolicy::Overwrite, true) => break (candidate, PlanAction::Overwrite),
                (_, false) => break (candidate, PlanAction::Write),
            }
        };
        taken.insert(fold_key(&path));
        plan.outputs.push(PlannedOutput {
            index,
            asset_id: item.facts.asset_id,
            sequence,
            path,
            action,
        });
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{DestinationSpec, NamingSpec};
    use std::collections::HashSet;

    struct FakeDisk(HashSet<PathBuf>);
    impl PathProbe for FakeDisk {
        fn exists(&self, p: &Path) -> bool {
            self.0.contains(p)
        }
    }

    fn item(id: i64, stem: &str, dir: &str) -> PlanItem {
        PlanItem {
            facts: AssetFacts {
                asset_id: id,
                stem: stem.into(),
                folder: "shoot".into(),
                mtime_unix: 1_700_000_000,
                ..AssetFacts::default()
            },
            source_dir: PathBuf::from(dir),
        }
    }

    fn spec_folder(template: &str) -> ExportSpec {
        ExportSpec {
            naming: NamingSpec {
                template: template.into(),
                sequence_start: 5,
            },
            destination: DestinationSpec {
                base: DestinationBase::Folder(PathBuf::from("out")),
                subfolder: None,
            },
            ..ExportSpec::default()
        }
    }

    fn paths(plan: &Plan) -> Vec<String> {
        plan.outputs
            .iter()
            .map(|o| o.path.to_string_lossy().replace('\\', "/"))
            .collect()
    }

    #[test]
    fn sequence_numbers_follow_position_and_start() {
        let items = [
            item(1, "a", "src"),
            item(2, "b", "src"),
            item(3, "c", "src"),
        ];
        let plan = plan_batch(
            &items,
            &spec_folder("{Sequence:3}_{Filename}"),
            "jpg",
            &FakeDisk(Default::default()),
        )
        .unwrap();
        assert_eq!(
            paths(&plan),
            ["out/005_a.jpg", "out/006_b.jpg", "out/007_c.jpg"]
        );
        assert_eq!(plan.outputs[2].sequence, 7);
    }

    #[test]
    fn same_as_source_uses_each_items_own_directory() {
        let mut spec = spec_folder("{Filename}");
        spec.destination.base = DestinationBase::SameAsSource;
        let items = [item(1, "a", "d1"), item(2, "a", "d2")];
        let plan = plan_batch(&items, &spec, "jpg", &FakeDisk(Default::default())).unwrap();
        assert_eq!(paths(&plan), ["d1/a.jpg", "d2/a.jpg"]);
    }

    #[test]
    fn duplicates_in_the_batch_get_suffixes_case_insensitively_under_every_policy() {
        let items = [
            item(1, "IMG", "s"),
            item(2, "img", "s"),
            item(3, "Img", "s"),
        ];
        for policy in [
            CollisionPolicy::UniqueSuffix,
            CollisionPolicy::Overwrite,
            CollisionPolicy::Skip,
        ] {
            let mut spec = spec_folder("{Filename}");
            spec.collision = policy;
            let plan = plan_batch(&items, &spec, "jpg", &FakeDisk(Default::default())).unwrap();
            assert_eq!(
                paths(&plan),
                ["out/IMG.jpg", "out/img-2.jpg", "out/Img-3.jpg"],
                "{policy:?}"
            );
        }
    }

    #[test]
    fn collision_policies_against_the_disk() {
        let disk = FakeDisk(
            [
                PathBuf::from("out").join("a.jpg"),
                PathBuf::from("out").join("a-2.jpg"),
            ]
            .into(),
        );
        let items = [item(1, "a", "s")];

        let mut spec = spec_folder("{Filename}");
        spec.collision = CollisionPolicy::UniqueSuffix;
        let plan = plan_batch(&items, &spec, "jpg", &disk).unwrap();
        assert_eq!(paths(&plan), ["out/a-3.jpg"]);
        assert_eq!(plan.outputs[0].action, PlanAction::Write);

        spec.collision = CollisionPolicy::Overwrite;
        let plan = plan_batch(&items, &spec, "jpg", &disk).unwrap();
        assert_eq!(paths(&plan), ["out/a.jpg"]);
        assert_eq!(plan.outputs[0].action, PlanAction::Overwrite);

        spec.collision = CollisionPolicy::Skip;
        let plan = plan_batch(&items, &spec, "jpg", &disk).unwrap();
        assert!(matches!(plan.outputs[0].action, PlanAction::Skip(_)));
    }

    #[test]
    fn subfolder_template_is_rendered_and_sanitized() {
        let mut spec = spec_folder("{Filename}");
        spec.destination.subfolder = Some("{Folder}/{Date:YYYY}".into());
        let plan = plan_batch(
            &[item(1, "a", "s")],
            &spec,
            "png",
            &FakeDisk(Default::default()),
        )
        .unwrap();
        assert_eq!(paths(&plan), ["out/shoot/2023/a.png"]);
    }

    #[test]
    fn long_stems_are_truncated_to_fit_max_path_with_room_for_a_suffix() {
        let long = "x".repeat(400);
        let plan = plan_batch(
            &[item(1, &long, "s")],
            &spec_folder("{Filename}"),
            "jpg",
            &FakeDisk(Default::default()),
        )
        .unwrap();
        let len = utf16_len(&plan.outputs[0].path);
        assert!(len <= MAX_PATH_UTF16 - SUFFIX_RESERVE, "{len}");
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn an_overlong_destination_folder_warns_but_still_plans() {
        let mut spec = spec_folder("{Filename}");
        spec.destination.base = DestinationBase::Folder(PathBuf::from("d".repeat(300)));
        let plan = plan_batch(
            &[item(1, "a", "s")],
            &spec,
            "jpg",
            &FakeDisk(Default::default()),
        )
        .unwrap();
        assert_eq!(plan.outputs.len(), 1);
        assert_eq!(plan.warnings.len(), 1);
    }

    #[test]
    fn an_invalid_spec_is_refused_before_planning() {
        let mut spec = spec_folder("{Nope}");
        spec.dpi = 300;
        assert!(matches!(
            plan_batch(
                &[item(1, "a", "s")],
                &spec,
                "jpg",
                &FakeDisk(Default::default())
            ),
            Err(PlanError::Spec(_))
        ));
    }
}
