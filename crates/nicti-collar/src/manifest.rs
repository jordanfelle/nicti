//! The `collar.toml` manifest (ADR-0214 section 1): strict by construction. Every unknown field,
//! every missing scope or justification, a `RenderStage` extension point, a wildcard / IP / non-
//! canonical host, and a budget looser than the host's own default is an error at parse time, so
//! nothing downstream ever sees a half-valid manifest.

use serde::Deserialize;

/// The Claw host contract this build implements; a manifest built against another is refused.
pub const HOST_API_VERSION: u32 = 1;
/// The host's own resource ceilings. A manifest may only tighten them (ADR-0214 section 1).
pub const MAX_FUEL: u64 = 10_000_000_000;
pub const MAX_EPOCH_BUDGET_MS: u64 = 30_000;
pub const MAX_MEMORY_MB: u32 = 1024;

const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const MAX_ID_LEN: usize = 64;
const MAX_CAPABILITIES: usize = 32;
const MAX_HOSTS: usize = 16;
const MAX_JUSTIFICATION_CHARS: usize = 500;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ManifestError {
    #[error("manifest is not valid TOML for this schema: {0}")]
    Syntax(String),
    #[error("manifest is larger than {MAX_MANIFEST_BYTES} bytes")]
    TooLarge,
    #[error("invalid module id {0:?}: use lowercase `vendor.module_name` (2+ dot-separated segments of a-z, 0-9, _), at most {MAX_ID_LEN} characters, and not the first-party `nicti.` namespace")]
    BadId(String),
    #[error("invalid version {0:?}: {1}")]
    BadVersion(String, String),
    #[error("built against host API {0}, this host implements {HOST_API_VERSION}")]
    UnsupportedHostApi(u32),
    #[error("no extension points declared")]
    NoExtensionPoints,
    #[error("unknown extension point {0:?}")]
    UnknownExtensionPoint(String),
    #[error(
        "a third-party module cannot declare the RenderStage extension point (ADR-0019 section 6)"
    )]
    RenderStageNotAllowed,
    #[error("extension point {0:?} is declared twice")]
    DuplicateExtensionPoint(String),
    #[error("capability {index}: {reason}")]
    BadCapability { index: usize, reason: String },
    #[error("more than {MAX_CAPABILITIES} capabilities")]
    TooManyCapabilities,
    #[error("budget {field} = {value} must be between 1 and the host's {max}")]
    BudgetOutOfRange {
        field: &'static str,
        value: u64,
        max: u64,
    },
}

/// The extension points a third-party module may declare (ADR-0019 section 7 minus `RenderStage`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExtensionPoint {
    RawDecoder,
    CameraColorProfile,
    LensCorrectionData,
    ModelProvider,
    Exporter,
    CatalogStore,
}

impl ExtensionPoint {
    fn parse(name: &str) -> Result<Self, ManifestError> {
        Ok(match name {
            "RawDecoder" => Self::RawDecoder,
            "CameraColorProfile" => Self::CameraColorProfile,
            "LensCorrectionData" => Self::LensCorrectionData,
            "ModelProvider" => Self::ModelProvider,
            "Exporter" => Self::Exporter,
            "CatalogStore" => Self::CatalogStore,
            "RenderStage" => return Err(ManifestError::RenderStageNotAllowed),
            other => return Err(ManifestError::UnknownExtensionPoint(other.to_owned())),
        })
    }
}

/// ADR-0218's two network actions; there is no general "internet access".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NetworkScope {
    WeightDownload,
    CloudFeature,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FsScope {
    CatalogManaged,
    UserSelected,
}

/// A canonical DNS host: lowercase ASCII, no wildcard, no scheme/port/path, not an IP literal.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Host(String);

impl Host {
    pub fn parse(s: &str) -> Result<Host, String> {
        if s.is_empty() || s.len() > 253 {
            return Err(format!("host {s:?} has an invalid length"));
        }
        if s != s.to_ascii_lowercase() {
            return Err(format!("host {s:?} must be lowercase"));
        }
        let labels: Vec<&str> = s.split('.').collect();
        if labels.len() < 2 {
            return Err(format!("host {s:?} must be a fully qualified name (a.b)"));
        }
        for label in &labels {
            let ok = !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !ok {
                return Err(format!(
                    "host {s:?} has an invalid label {label:?} (wildcards, ports, schemes, \
                     non-ASCII (use punycode) and trailing dots are refused)"
                ));
            }
        }
        // An all-numeric final label is an IPv4 literal (or an attempt at one): hosts are names.
        if labels
            .last()
            .is_some_and(|l| l.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err(format!(
                "host {s:?} looks like an IP address; declare a DNS name"
            ));
        }
        Ok(Host(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Capability {
    Network {
        scope: NetworkScope,
        /// Sorted and de-duplicated, so two manifests compare by content.
        allowed_hosts: Vec<Host>,
        justification: String,
    },
    Filesystem {
        scope: FsScope,
        justification: String,
    },
    Gpu {
        justification: String,
    },
}

impl Capability {
    pub fn justification(&self) -> &str {
        match self {
            Capability::Network { justification, .. }
            | Capability::Filesystem { justification, .. }
            | Capability::Gpu { justification } => justification,
        }
    }
}

/// Optional budgets that tighten the host's ceilings; `None` means the host default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Budget {
    pub fuel_limit: Option<u64>,
    pub epoch_budget_ms: Option<u64>,
    pub memory_mb_max: Option<u32>,
}

impl Budget {
    /// The values a sandbox actually runs with.
    pub fn effective_fuel(&self) -> u64 {
        self.fuel_limit.unwrap_or(MAX_FUEL)
    }
    pub fn effective_epoch_budget_ms(&self) -> u64 {
        self.epoch_budget_ms.unwrap_or(MAX_EPOCH_BUDGET_MS)
    }
    pub fn effective_memory_mb(&self) -> u32 {
        self.memory_mb_max.unwrap_or(MAX_MEMORY_MB)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub id: String,
    pub version: semver::Version,
    pub host_api_version: u32,
    /// Sorted, no duplicates.
    pub extension_points: Vec<ExtensionPoint>,
    pub capabilities: Vec<Capability>,
    pub budget: Budget,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    id: String,
    version: String,
    host_api_version: u32,
    extension_points: Vec<String>,
    #[serde(default)]
    capabilities: Vec<RawCapability>,
    #[serde(default)]
    budget: Option<RawBudget>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCapability {
    kind: String,
    scope: Option<String>,
    allowed_hosts: Option<Vec<String>>,
    justification: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBudget {
    fuel_limit: Option<u64>,
    epoch_budget_ms: Option<u64>,
    memory_mb_max: Option<u32>,
}

fn valid_id(id: &str) -> bool {
    id.len() <= MAX_ID_LEN
        && !id.starts_with("nicti.")
        && id.split('.').count() >= 2
        && id.split('.').all(|seg| {
            !seg.is_empty()
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}

fn bad(index: usize, reason: impl Into<String>) -> ManifestError {
    ManifestError::BadCapability {
        index,
        reason: reason.into(),
    }
}

fn parse_capability(index: usize, raw: RawCapability) -> Result<Capability, ManifestError> {
    let justification = raw
        .justification
        .map(|j| j.trim().to_owned())
        .filter(|j| !j.is_empty())
        .ok_or_else(|| bad(index, "a plain-language justification is required"))?;
    if justification.chars().count() > MAX_JUSTIFICATION_CHARS
        || justification.chars().any(|c| c.is_control())
    {
        return Err(bad(
            index,
            format!(
                "justification must be at most {MAX_JUSTIFICATION_CHARS} characters of plain text"
            ),
        ));
    }
    match raw.kind.as_str() {
        "network" => {
            let scope = match raw.scope.as_deref() {
                Some("weight-download") => NetworkScope::WeightDownload,
                Some("cloud-feature") => NetworkScope::CloudFeature,
                Some(other) => {
                    return Err(bad(
                        index,
                        format!("network scope {other:?} must be weight-download or cloud-feature"),
                    ))
                }
                None => return Err(bad(index, "a network capability needs a scope")),
            };
            let hosts = raw
                .allowed_hosts
                .filter(|h| !h.is_empty())
                .ok_or_else(|| bad(index, "a network capability needs allowed_hosts"))?;
            if hosts.len() > MAX_HOSTS {
                return Err(bad(index, format!("more than {MAX_HOSTS} allowed_hosts")));
            }
            let mut allowed_hosts = hosts
                .iter()
                .map(|h| Host::parse(h).map_err(|e| bad(index, e)))
                .collect::<Result<Vec<_>, _>>()?;
            allowed_hosts.sort();
            allowed_hosts.dedup();
            Ok(Capability::Network {
                scope,
                allowed_hosts,
                justification,
            })
        }
        "filesystem" => {
            if raw.allowed_hosts.is_some() {
                return Err(bad(index, "allowed_hosts belongs to a network capability"));
            }
            let scope = match raw.scope.as_deref() {
                Some("catalog-managed") => FsScope::CatalogManaged,
                Some("user-selected") => FsScope::UserSelected,
                Some(other) => {
                    return Err(bad(
                        index,
                        format!(
                            "filesystem scope {other:?} must be catalog-managed or user-selected"
                        ),
                    ))
                }
                None => return Err(bad(index, "a filesystem capability needs a scope")),
            };
            Ok(Capability::Filesystem {
                scope,
                justification,
            })
        }
        "gpu" => {
            if raw.scope.is_some() || raw.allowed_hosts.is_some() {
                return Err(bad(index, "a gpu capability takes no scope or hosts"));
            }
            Ok(Capability::Gpu { justification })
        }
        other => Err(bad(index, format!("unknown capability kind {other:?}"))),
    }
}

fn check_budget(field: &'static str, value: Option<u64>, max: u64) -> Result<(), ManifestError> {
    match value {
        Some(v) if v == 0 || v > max => Err(ManifestError::BudgetOutOfRange {
            field,
            value: v,
            max,
        }),
        _ => Ok(()),
    }
}

/// Parses and validates a `collar.toml`.
pub fn parse(text: &str) -> Result<Manifest, ManifestError> {
    if text.len() > MAX_MANIFEST_BYTES {
        return Err(ManifestError::TooLarge);
    }
    let raw: Raw = toml::from_str(text).map_err(|e| ManifestError::Syntax(e.to_string()))?;
    if !valid_id(&raw.id) {
        return Err(ManifestError::BadId(raw.id));
    }
    let version = semver::Version::parse(&raw.version)
        .map_err(|e| ManifestError::BadVersion(raw.version.clone(), e.to_string()))?;
    if raw.host_api_version != HOST_API_VERSION {
        return Err(ManifestError::UnsupportedHostApi(raw.host_api_version));
    }
    if raw.extension_points.is_empty() {
        return Err(ManifestError::NoExtensionPoints);
    }
    let mut extension_points = Vec::new();
    for name in &raw.extension_points {
        let point = ExtensionPoint::parse(name)?;
        if extension_points.contains(&point) {
            return Err(ManifestError::DuplicateExtensionPoint(name.clone()));
        }
        extension_points.push(point);
    }
    extension_points.sort();
    if raw.capabilities.len() > MAX_CAPABILITIES {
        return Err(ManifestError::TooManyCapabilities);
    }
    let mut capabilities = Vec::new();
    for (i, cap) in raw.capabilities.into_iter().enumerate() {
        let cap = parse_capability(i, cap)?;
        // One entry per (kind, scope): two network entries for one scope would hide a widening.
        let dup = capabilities.iter().any(|c: &Capability| {
            std::mem::discriminant(c) == std::mem::discriminant(&cap)
                && match (c, &cap) {
                    (
                        Capability::Network { scope: a, .. },
                        Capability::Network { scope: b, .. },
                    ) => a == b,
                    (
                        Capability::Filesystem { scope: a, .. },
                        Capability::Filesystem { scope: b, .. },
                    ) => a == b,
                    _ => true,
                }
        });
        if dup {
            return Err(bad(i, "duplicate capability (same kind and scope)"));
        }
        capabilities.push(cap);
    }
    let budget = match raw.budget {
        None => Budget::default(),
        Some(b) => {
            check_budget("fuel_limit", b.fuel_limit, MAX_FUEL)?;
            check_budget("epoch_budget_ms", b.epoch_budget_ms, MAX_EPOCH_BUDGET_MS)?;
            check_budget(
                "memory_mb_max",
                b.memory_mb_max.map(u64::from),
                u64::from(MAX_MEMORY_MB),
            )?;
            Budget {
                fuel_limit: b.fuel_limit,
                epoch_budget_ms: b.epoch_budget_ms,
                memory_mb_max: b.memory_mb_max,
            }
        }
    };
    Ok(Manifest {
        id: raw.id,
        version,
        host_api_version: raw.host_api_version,
        extension_points,
        capabilities,
        budget,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const GOOD: &str = r#"
id = "vendor.module_name"
version = "1.2.0"
host_api_version = 1
extension_points = ["ModelProvider"]

[[capabilities]]
kind = "network"
scope = "weight-download"
allowed_hosts = ["cdn.vendor.example.com"]
justification = "Downloads the segmentation weights on first enable."

[[capabilities]]
kind = "filesystem"
scope = "user-selected"
justification = "Lets the user pick a folder of reference images."
"#;

    fn err(text: &str) -> ManifestError {
        parse(text).expect_err("must be refused")
    }

    fn with(replace: &str, by: &str) -> String {
        assert!(GOOD.contains(replace), "{replace}");
        GOOD.replace(replace, by)
    }

    #[test]
    fn the_adr_example_parses_to_exactly_what_it_declares() {
        let m = parse(GOOD).unwrap();
        assert_eq!(m.id, "vendor.module_name");
        assert_eq!(m.version.to_string(), "1.2.0");
        assert_eq!(m.extension_points, vec![ExtensionPoint::ModelProvider]);
        assert_eq!(m.capabilities.len(), 2);
        assert!(matches!(
            &m.capabilities[0],
            Capability::Network { scope: NetworkScope::WeightDownload, allowed_hosts, .. }
                if allowed_hosts[0].as_str() == "cdn.vendor.example.com"
        ));
        assert_eq!(m.budget, Budget::default());
        assert_eq!(m.budget.effective_fuel(), MAX_FUEL);
    }

    #[test]
    fn a_module_with_no_capabilities_is_valid_and_gets_none() {
        let m = parse(
            "id = \"a.b\"\nversion = \"0.1.0\"\nhost_api_version = 1\nextension_points = [\"Exporter\"]\n",
        )
        .unwrap();
        assert!(m.capabilities.is_empty());
    }

    #[test]
    fn unknown_fields_anywhere_are_refused() {
        for text in [
            with("version = \"1.2.0\"", "version = \"1.2.0\"\nsurprise = 1"),
            with("kind = \"network\"", "kind = \"network\"\nextra = true"),
            format!("{GOOD}\n[budget]\nfuel_limit = 5\nunlimited = true\n"),
        ] {
            assert!(matches!(err(&text), ManifestError::Syntax(_)), "{text}");
        }
    }

    #[test]
    fn every_capability_needs_a_scope_and_a_justification() {
        let no_just = with(
            "justification = \"Lets the user pick a folder of reference images.\"",
            "",
        );
        assert!(matches!(
            err(&no_just),
            ManifestError::BadCapability { index: 1, .. }
        ));
        let blank = with(
            "justification = \"Lets the user pick a folder of reference images.\"",
            "justification = \"   \"",
        );
        assert!(matches!(
            err(&blank),
            ManifestError::BadCapability { index: 1, .. }
        ));
        let no_scope = with("scope = \"user-selected\"\n", "");
        assert!(matches!(
            err(&no_scope),
            ManifestError::BadCapability { index: 1, .. }
        ));
        let long = with(
            "Lets the user pick a folder of reference images.",
            &"x".repeat(501),
        );
        assert!(matches!(
            err(&long),
            ManifestError::BadCapability { index: 1, .. }
        ));
        let control = with("Lets the user pick", "Lets the\\u0007 user pick");
        assert!(matches!(
            err(&control),
            ManifestError::BadCapability { index: 1, .. }
        ));
    }

    #[test]
    fn render_stage_and_unknown_extension_points_are_refused() {
        assert_eq!(
            err(&with(
                "[\"ModelProvider\"]",
                "[\"ModelProvider\", \"RenderStage\"]"
            )),
            ManifestError::RenderStageNotAllowed
        );
        assert_eq!(
            err(&with("[\"ModelProvider\"]", "[\"Plugin\"]")),
            ManifestError::UnknownExtensionPoint("Plugin".into())
        );
        assert_eq!(
            err(&with("[\"ModelProvider\"]", "[]")),
            ManifestError::NoExtensionPoints
        );
        assert_eq!(
            err(&with("[\"ModelProvider\"]", "[\"Exporter\", \"Exporter\"]")),
            ManifestError::DuplicateExtensionPoint("Exporter".into())
        );
    }

    #[test]
    fn network_needs_an_action_scope_and_canonical_dns_hosts() {
        for scope in ["internet", "general", ""] {
            let t = with(
                "scope = \"weight-download\"",
                &format!("scope = \"{scope}\""),
            );
            assert!(
                matches!(err(&t), ManifestError::BadCapability { index: 0, .. }),
                "{scope}"
            );
        }
        for host in [
            "*.vendor.example.com",
            "*",
            "https://cdn.vendor.example.com",
            "cdn.vendor.example.com:443",
            "cdn.vendor.example.com/path",
            "CDN.vendor.example.com",
            "cdn.vendor.example.com.",
            "localhost",
            "10.0.0.1",
            "127.0.0.1",
            "1.2.3.4",
            "xn--",
            "caf\u{e9}.example.com",
            "-bad.example.com",
            "bad-.example.com",
            "a..b.com",
            "",
        ] {
            let t = with("\"cdn.vendor.example.com\"", &format!("{host:?}"));
            assert!(
                matches!(err(&t), ManifestError::BadCapability { index: 0, .. }),
                "{host:?}"
            );
        }
        let no_hosts = with("allowed_hosts = [\"cdn.vendor.example.com\"]\n", "");
        assert!(matches!(
            err(&no_hosts),
            ManifestError::BadCapability { index: 0, .. }
        ));
        let none = with("[\"cdn.vendor.example.com\"]", "[]");
        assert!(matches!(
            err(&none),
            ManifestError::BadCapability { index: 0, .. }
        ));
    }

    #[test]
    fn hosts_are_sorted_and_deduplicated_so_manifests_compare_by_content() {
        let t = with(
            "[\"cdn.vendor.example.com\"]",
            "[\"b.example.com\", \"a.example.com\", \"b.example.com\"]",
        );
        let m = parse(&t).unwrap();
        let Capability::Network { allowed_hosts, .. } = &m.capabilities[0] else {
            panic!()
        };
        let names: Vec<_> = allowed_hosts.iter().map(Host::as_str).collect();
        assert_eq!(names, ["a.example.com", "b.example.com"]);
    }

    #[test]
    fn filesystem_scopes_are_never_ambient_and_gpu_takes_none() {
        for scope in ["whole-disk", "ambient", "home"] {
            let t = with("scope = \"user-selected\"", &format!("scope = \"{scope}\""));
            assert!(
                matches!(err(&t), ManifestError::BadCapability { index: 1, .. }),
                "{scope}"
            );
        }
        let hosts_on_fs = with(
            "scope = \"user-selected\"",
            "scope = \"user-selected\"\nallowed_hosts = [\"a.example.com\"]",
        );
        assert!(matches!(
            err(&hosts_on_fs),
            ManifestError::BadCapability { index: 1, .. }
        ));
        let gpu = format!(
            "{GOOD}\n[[capabilities]]\nkind = \"gpu\"\njustification = \"Runs the model on the GPU.\"\n"
        );
        assert!(parse(&gpu).is_ok());
        let gpu_scoped = format!(
            "{GOOD}\n[[capabilities]]\nkind = \"gpu\"\nscope = \"all\"\njustification = \"x\"\n"
        );
        assert!(matches!(
            err(&gpu_scoped),
            ManifestError::BadCapability { index: 2, .. }
        ));
        let unknown =
            format!("{GOOD}\n[[capabilities]]\nkind = \"process\"\njustification = \"x\"\n");
        assert!(matches!(
            err(&unknown),
            ManifestError::BadCapability { index: 2, .. }
        ));
    }

    #[test]
    fn two_entries_for_one_kind_and_scope_are_refused() {
        let dup = format!(
            "{GOOD}\n[[capabilities]]\nkind = \"network\"\nscope = \"weight-download\"\nallowed_hosts = [\"evil.example.com\"]\njustification = \"More.\"\n"
        );
        assert!(matches!(
            err(&dup),
            ManifestError::BadCapability { index: 2, .. }
        ));
        // A different scope is a different capability and is allowed.
        let other = format!(
            "{GOOD}\n[[capabilities]]\nkind = \"network\"\nscope = \"cloud-feature\"\nallowed_hosts = [\"api.example.com\"]\njustification = \"Opt-in cloud feature.\"\n"
        );
        assert!(parse(&other).is_ok());
    }

    #[test]
    fn ids_versions_and_the_host_api_are_checked() {
        for id in [
            "nicti.stage",
            "single",
            "Vendor.mod",
            "vendor..mod",
            "vendor.mod-x",
            "",
            &"a.".repeat(40),
        ] {
            let t = with("\"vendor.module_name\"", &format!("{id:?}"));
            assert!(matches!(err(&t), ManifestError::BadId(_)), "{id:?}");
        }
        for v in ["1", "1.2", "latest", "", "1.2.x"] {
            let t = with("\"1.2.0\"", &format!("{v:?}"));
            assert!(matches!(err(&t), ManifestError::BadVersion(..)), "{v:?}");
        }
        assert_eq!(
            err(&with("host_api_version = 1", "host_api_version = 2")),
            ManifestError::UnsupportedHostApi(2)
        );
    }

    #[test]
    fn budgets_may_only_tighten_the_host_defaults() {
        let ok = format!(
            "{GOOD}\n[budget]\nfuel_limit = 1000\nepoch_budget_ms = 250\nmemory_mb_max = 64\n"
        );
        let m = parse(&ok).unwrap();
        assert_eq!(m.budget.effective_fuel(), 1000);
        assert_eq!(m.budget.effective_epoch_budget_ms(), 250);
        assert_eq!(m.budget.effective_memory_mb(), 64);
        for (field, line) in [
            ("fuel_limit", format!("fuel_limit = {}", MAX_FUEL + 1)),
            (
                "epoch_budget_ms",
                format!("epoch_budget_ms = {}", MAX_EPOCH_BUDGET_MS + 1),
            ),
            (
                "memory_mb_max",
                format!("memory_mb_max = {}", MAX_MEMORY_MB + 1),
            ),
            ("fuel_limit", "fuel_limit = 0".to_string()),
        ] {
            let e = err(&format!("{GOOD}\n[budget]\n{line}\n"));
            assert!(
                matches!(e, ManifestError::BudgetOutOfRange { field: f, .. } if f == field),
                "{line}: {e:?}"
            );
        }
    }

    #[test]
    fn an_oversized_or_garbled_manifest_is_refused_not_panicked_on() {
        assert_eq!(
            err(&" ".repeat(MAX_MANIFEST_BYTES + 1)),
            ManifestError::TooLarge
        );
        assert!(matches!(err("id = "), ManifestError::Syntax(_)));
        assert!(matches!(err("\u{0}\u{1}garbage"), ManifestError::Syntax(_)));
        let many: String = (0..MAX_CAPABILITIES + 1)
            .map(|_| "[[capabilities]]\nkind = \"gpu\"\njustification = \"x\"\n".to_string())
            .collect();
        let t = format!("id = \"a.b\"\nversion = \"1.0.0\"\nhost_api_version = 1\nextension_points = [\"Exporter\"]\n{many}");
        assert_eq!(err(&t), ManifestError::TooManyCapabilities);
    }
}
