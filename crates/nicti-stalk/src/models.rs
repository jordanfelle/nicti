//! On-demand model store (#51, `docs/adr/0218-local-only-ai.md`): the compiled-in manifest of every
//! model artifact Nicti can download, and the explicit install path for them.
//!
//! ADR-0218's rules, as enforced here:
//! - **Nothing is fetched unless a caller asks.** [`ModelStore::install`] is the only thing that
//!   touches the network, and only through the [`Downloader`] a user-initiated action hands it.
//! - **Every artifact is pinned**: a full URL at an immutable revision, its exact size, and its
//!   SHA-256. A download that doesn't match is discarded, never installed.
//! - **Install is atomic**: bytes stream to a temp file inside the store, are verified, and only
//!   then renamed into place, so an interrupted or corrupt download can never look installed.
//! - **Bounded**: a download is cut off at the manifest's declared size, so a host that serves more
//!   than was pinned can't fill the disk.
//!
//! Licensing (`docs/licensing.md`): none of these is bundled into Nicti's installer. LaMa's weights
//! are an on-demand download only (ADR-0050's Places2 flag, signed off 2026-09-29 in ADR-0051).

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use sha2::{Digest, Sha256};

/// How a downloaded payload becomes the installed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Payload {
    /// The download *is* the installed file.
    File,
    /// The download is a zip; the installed file is the single member at this path.
    ZipMember(&'static str),
}

/// One pinned, downloadable model artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Artifact {
    /// Stable id, also the artifact's directory name inside the store.
    pub id: &'static str,
    /// Human-readable name for the download prompt.
    pub label: &'static str,
    /// The installed file's name inside `<store>/<id>/`.
    pub file_name: &'static str,
    /// Pinned download URL (an immutable revision, never a moving branch).
    pub url: &'static str,
    /// Exact download size in bytes.
    pub download_size: u64,
    /// SHA-256 (lowercase hex) of the downloaded bytes.
    pub download_sha256: &'static str,
    /// Exact installed-file size in bytes (differs from `download_size` for a zip member).
    pub installed_size: u64,
    /// SHA-256 (lowercase hex) of the installed file.
    pub installed_sha256: &'static str,
    /// SPDX-ish license of the artifact, shown before the user agrees to download it.
    pub license: &'static str,
    pub payload: Payload,
}

/// ONNX Runtime 1.28.0, Windows x64 CPU build (MIT), from the official Microsoft release. The zip
/// also carries debug symbols, hence the size; only `onnxruntime.dll` is kept.
pub const ORT_RUNTIME: Artifact = Artifact {
    id: "onnxruntime",
    label: "ONNX Runtime 1.28.0 (CPU)",
    file_name: "onnxruntime.dll",
    url: "https://github.com/microsoft/onnxruntime/releases/download/v1.28.0/onnxruntime-win-x64-1.28.0.zip",
    download_size: 78_796_801,
    download_sha256: "abef733dacbe2f571547a7150b479b5cb9cc0df22f96c24983a42cadb1b4f8bc",
    installed_size: 15_809_848,
    installed_sha256: "18370c375f07357fa5874344a9d9ac17e6b6fe1eb18b1dd209d79483b4470257",
    license: "MIT",
    payload: Payload::ZipMember("onnxruntime-win-x64-1.28.0/lib/onnxruntime.dll"),
};

/// MobileSAM image encoder, ONNX export by Acly (Apache-2.0 weights from ChaoningZhang/MobileSAM;
/// export scripts MIT). Input: `input_image` float32 `[H, W, 3]`, raw 0..255 RGB with the longest
/// side already resized to 1024 (the graph normalizes and pads itself); output: `image_embeddings`
/// `[1, 256, 64, 64]`.
pub const MOBILE_SAM_ENCODER: Artifact = Artifact {
    id: "mobile-sam-encoder",
    label: "MobileSAM image encoder",
    file_name: "mobile_sam_image_encoder.onnx",
    url: "https://huggingface.co/Acly/MobileSAM/resolve/0d3b403339b4674a82493d5e97964dd78089ddc8/mobile_sam_image_encoder.onnx",
    download_size: 28_157_093,
    download_sha256: "580f5fb648ea1062c0aabc26217aed56921985f03f0cbbd852bba81d760cc749",
    installed_size: 28_157_093,
    installed_sha256: "580f5fb648ea1062c0aabc26217aed56921985f03f0cbbd852bba81d760cc749",
    license: "Apache-2.0",
    payload: Payload::File,
};

/// MobileSAM prompt decoder (single-mask variant). Inputs: `image_embeddings [1,256,64,64]`,
/// `point_coords [1,N,2]` (in the 1024-longest-side frame), `point_labels [1,N]`,
/// `mask_input [1,1,256,256]`, `has_mask_input [1]`, `orig_im_size [2]` (H, W); outputs `masks`
/// (logits at the original size), `iou_predictions`, `low_res_masks`.
pub const MOBILE_SAM_DECODER: Artifact = Artifact {
    id: "mobile-sam-decoder",
    label: "MobileSAM prompt decoder",
    file_name: "sam_mask_decoder_single.onnx",
    url: "https://huggingface.co/Acly/MobileSAM/resolve/0d3b403339b4674a82493d5e97964dd78089ddc8/sam_mask_decoder_single.onnx",
    download_size: 16_501_323,
    download_sha256: "93915fc7c993ab9d59ab8c9ccd3bce37f7509c81ab4150a74abd4d2abbd8570d",
    installed_size: 16_501_323,
    installed_sha256: "93915fc7c993ab9d59ab8c9ccd3bce37f7509c81ab4150a74abd4d2abbd8570d",
    license: "Apache-2.0",
    payload: Payload::File,
};

/// LaMa big-lama, fp32 ONNX export by Carve. Fixed 512x512: `image [B,3,512,512]` (0..1 RGB) and
/// `mask [B,1,512,512]` (1 = fill) in, `output [B,3,512,512]` (0..255 RGB) out.
///
/// **On-demand download only, never bundled**: the checkpoint was trained on Places2, whose terms
/// restrict use to non-commercial research (`docs/licensing.md`, ADR-0050 flag). Shipping it as an
/// explicit user download was signed off in ADR-0051.
pub const LAMA: Artifact = Artifact {
    id: "lama",
    label: "LaMa inpainting model",
    file_name: "lama_fp32.onnx",
    url: "https://huggingface.co/Carve/LaMa-ONNX/resolve/c3c0c9e468934d62e79c329e35d82dd09ff8c444/lama_fp32.onnx",
    download_size: 208_044_816,
    download_sha256: "1faef5301d78db7dda502fe59966957ec4b79dd64e16f03ed96913c7a4eb68d6",
    installed_size: 208_044_816,
    installed_sha256: "1faef5301d78db7dda502fe59966957ec4b79dd64e16f03ed96913c7a4eb68d6",
    license: "Apache-2.0 (weights trained on Places2 -- non-commercial-research data terms)",
    payload: Payload::File,
};

/// BiRefNet (general checkpoint), fp32 ONNX export by onnx-community, for AI subject/background
/// masks (#49, ADR-0048's default). Input `input_image` float32 `[1, 3, 1024, 1024]`, an RGB image
/// resized to 1024x1024 and normalized with the ImageNet mean/std; output `output_image` float32
/// `[1, 1, 1024, 1024]` **logits** (apply a sigmoid for alpha). The provider reads the real
/// input/output names from the loaded graph rather than trusting these, and the tensor contract is
/// re-verified by `nicti-siamese`'s `#[ignore]`d real-weight test.
///
/// **On-demand download only, never bundled** (ADR-0218): ~970 MB. MIT; the upstream model card
/// (`ZhengPeng7/BiRefNet`, which this export names as its `base_model`) states it is "trained on
/// DIS-TR", a dataset with no stated use restriction. This is a third-party ONNX conversion
/// (onnx-community); #348 compared it to the upstream checkpoint (`bench/birefnet-verify`): alpha
/// masks closely agree (IoU >= 0.997 at 0.5; output-level, not a weight proof), see `docs/licensing.md` for the result and its limits.
/// The fp16 export in the same repo (490 MB) is deliberately not used: ONNX Runtime's CPU provider
/// has thin fp16 kernel coverage and falls back through casts, so it is *slower* there.
pub const BIREFNET: Artifact = Artifact {
    id: "birefnet",
    label: "BiRefNet subject/background masking model",
    file_name: "birefnet_fp32.onnx",
    url: "https://huggingface.co/onnx-community/BiRefNet-ONNX/resolve/534d3c82d3bb8b2f0867db6dfbc3a525b8e42f67/onnx/model.onnx",
    download_size: 972_666_916,
    download_sha256: "58f621f00f5d756097615970a88a791584600dcf7c45b18a0a6267535a1ebd3c",
    installed_size: 972_666_916,
    installed_sha256: "58f621f00f5d756097615970a88a791584600dcf7c45b18a0a6267535a1ebd3c",
    license: "MIT (trained on DIS-TR per the upstream model card; a third-party ONNX conversion, see docs/licensing.md)",
    payload: Payload::File,
};

/// Everything AI subject/background masking needs, in install order (the ONNX Runtime entry exists
/// only where Nicti ships a runtime download, as for removal).
pub fn mask_artifacts() -> Vec<&'static Artifact> {
    let mut v: Vec<&'static Artifact> = Vec::new();
    #[cfg(windows)]
    v.push(&ORT_RUNTIME);
    v.extend([&BIREFNET]);
    v
}

/// Total bytes a user agrees to download for AI masks, for the confirmation prompt (skips anything
/// already installed, e.g. a runtime AI removal already fetched).
pub fn mask_download_bytes(store: &ModelStore) -> u64 {
    mask_artifacts()
        .iter()
        .filter(|a| store.status(a) != Status::Installed)
        .map(|a| a.download_size)
        .sum()
}

/// Resolved on-disk locations of every file AI masks need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaskModels {
    pub ort_dylib: PathBuf,
    pub birefnet: PathBuf,
}

impl MaskModels {
    /// `Some` only when BiRefNet is installed and an ONNX Runtime library is available (the store's
    /// own copy on Windows, or `NICTI_ORT_DYLIB`, which wins where set).
    pub fn locate(store: &ModelStore) -> Option<Self> {
        let ort_dylib = match std::env::var_os("NICTI_ORT_DYLIB") {
            Some(p) if !p.is_empty() => PathBuf::from(p),
            _ => store.ort_runtime_path()?,
        };
        let models = Self {
            ort_dylib,
            birefnet: store.installed_path(&BIREFNET)?,
        };
        models.ort_dylib.is_file().then_some(models)
    }
}

/// Everything AI object removal needs, in install order. The ONNX Runtime entry exists only where
/// Nicti ships a runtime download (Windows); elsewhere `NICTI_ORT_DYLIB` must point at one.
pub fn removal_artifacts() -> Vec<&'static Artifact> {
    let mut v: Vec<&'static Artifact> = Vec::new();
    #[cfg(windows)]
    v.push(&ORT_RUNTIME);
    v.extend([&MOBILE_SAM_ENCODER, &MOBILE_SAM_DECODER, &LAMA]);
    v
}

/// Total bytes a user agrees to download for AI removal, for the confirmation prompt.
pub fn removal_download_bytes(store: &ModelStore) -> u64 {
    removal_artifacts()
        .iter()
        .filter(|a| store.status(a) != Status::Installed)
        .map(|a| a.download_size)
        .sum()
}

/// Resolved on-disk locations of every file AI removal needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovalModels {
    pub ort_dylib: PathBuf,
    pub sam_encoder: PathBuf,
    pub sam_decoder: PathBuf,
    pub lama: PathBuf,
}

impl RemovalModels {
    /// `Some` only when every model is installed and an ONNX Runtime library is available: the
    /// store's own copy on Windows, or the path in `NICTI_ORT_DYLIB` (the dev/CI/Linux override,
    /// which wins where set).
    pub fn locate(store: &ModelStore) -> Option<Self> {
        let ort_dylib = match std::env::var_os("NICTI_ORT_DYLIB") {
            Some(p) if !p.is_empty() => PathBuf::from(p),
            _ => store.ort_runtime_path()?,
        };
        let models = Self {
            ort_dylib,
            sam_encoder: store.installed_path(&MOBILE_SAM_ENCODER)?,
            sam_decoder: store.installed_path(&MOBILE_SAM_DECODER)?,
            lama: store.installed_path(&LAMA)?,
        };
        models.ort_dylib.is_file().then_some(models)
    }
}

/// Re-hashes every installed AI-removal artifact against its pinned SHA-256. [`ModelStore::status`]
/// only checks sizes (cheap enough to call every frame), but these files are loaded into the process
/// -- the ONNX Runtime library is native code, and the models are parsed by it -- so the first load
/// should confirm they are exactly what was pinned. Hashes ~250 MB, so call it off the UI thread.
///
/// `ort_from_store` is false when `NICTI_ORT_DYLIB` supplies the runtime, which is the caller's own
/// file and not something this store pinned.
pub fn verify_removal_install(store: &ModelStore, ort_from_store: bool) -> Result<(), String> {
    verify_artifacts(store, &removal_artifacts(), ort_from_store)
}

/// [`verify_removal_install`] for any set of artifacts (AI masks use the same first-load check for
/// BiRefNet). `ort_from_store` is false when `NICTI_ORT_DYLIB` supplies the runtime.
pub fn verify_artifacts(
    store: &ModelStore,
    artifacts: &[&'static Artifact],
    ort_from_store: bool,
) -> Result<(), String> {
    for artifact in artifacts.iter().copied() {
        if artifact.id == ORT_RUNTIME.id && !ort_from_store {
            continue;
        }
        match store.verify(artifact) {
            Ok(true) => {}
            Ok(false) => {
                return Err(format!(
                    "{} is missing or failed its SHA-256 check. Delete it and download again.",
                    artifact.label
                ))
            }
            Err(e) => return Err(format!("Couldn't read {}: {e}", artifact.label)),
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    NotInstalled,
    Installed,
}

/// Bytes moved so far for the artifact currently downloading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub downloaded: u64,
    pub total: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("network error: {0}")]
    Network(String),
    #[error("downloads are not available on this platform")]
    Unsupported,
}

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error(transparent)]
    Download(#[from] DownloadError),
    #[error("cancelled")]
    Cancelled,
    #[error("download of {id} was {got} bytes, expected {expected}")]
    WrongSize {
        id: &'static str,
        got: u64,
        expected: u64,
    },
    #[error("download of {id} failed its SHA-256 check (got {got})")]
    HashMismatch { id: &'static str, got: String },
    #[error("{id}: zip member {member} is missing or unreadable: {reason}")]
    BadArchive {
        id: &'static str,
        member: &'static str,
        reason: String,
    },
    #[error("filesystem error: {0}")]
    Io(#[from] io::Error),
}

/// A source of bytes. `fetch` streams the body to `on_chunk`, which returns `false` to abort
/// (cancellation, size cap); `max_bytes` is the most the caller will accept. Implementations must
/// stop reading once `on_chunk` returns `false`.
pub trait Downloader {
    fn fetch(
        &self,
        url: &str,
        max_bytes: u64,
        on_chunk: &mut dyn FnMut(&[u8]) -> bool,
    ) -> Result<(), DownloadError>;
}

/// The real network downloader. Windows only, like `nicti-shed`'s updater; elsewhere it reports
/// [`DownloadError::Unsupported`] so the rest of the store still builds and tests everywhere.
pub struct HttpDownloader;

#[cfg(windows)]
impl Downloader for HttpDownloader {
    fn fetch(
        &self,
        url: &str,
        max_bytes: u64,
        on_chunk: &mut dyn FnMut(&[u8]) -> bool,
    ) -> Result<(), DownloadError> {
        use std::time::Duration;
        use ureq::tls::{TlsConfig, TlsProvider};

        // Explicit provider: ureq defaults to Rustls even when only native-tls is compiled in.
        let tls = TlsConfig::builder()
            .provider(TlsProvider::NativeTls)
            .build();
        let agent = ureq::Agent::config_builder()
            // A 208MB model on a slow link: generous, but never unbounded.
            .timeout_global(Some(Duration::from_secs(3600)))
            .tls_config(tls)
            .build()
            .new_agent();
        let mut response = agent
            .get(url)
            .call()
            .map_err(|e| DownloadError::Network(e.to_string()))?;
        let mut reader = response.body_mut().as_reader();
        let mut buf = vec![0u8; 64 * 1024];
        let mut total = 0u64;
        loop {
            let n = reader
                .read(&mut buf)
                .map_err(|e| DownloadError::Network(e.to_string()))?;
            if n == 0 {
                return Ok(());
            }
            total += n as u64;
            if total > max_bytes || !on_chunk(&buf[..n]) {
                return Ok(());
            }
        }
    }
}

#[cfg(not(windows))]
impl Downloader for HttpDownloader {
    fn fetch(
        &self,
        _url: &str,
        _max_bytes: u64,
        _on_chunk: &mut dyn FnMut(&[u8]) -> bool,
    ) -> Result<(), DownloadError> {
        Err(DownloadError::Unsupported)
    }
}

/// The on-disk store: `<root>/<artifact id>/<file name>`.
#[derive(Debug, Clone)]
pub struct ModelStore {
    root: PathBuf,
}

impl ModelStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `%LOCALAPPDATA%\nicti\models` on Windows, `$XDG_DATA_HOME/nicti/models` (else
    /// `~/.local/share/nicti/models`) elsewhere. `None` when no home can be determined.
    pub fn default_root() -> Option<PathBuf> {
        #[cfg(windows)]
        let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
        #[cfg(not(windows))]
        let base = std::env::var_os("XDG_DATA_HOME")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| Path::new(&h).join(".local").join("share"))
            });
        base.map(|b| b.join("nicti").join("models"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where `artifact`'s installed file lives (whether or not it exists yet).
    pub fn path(&self, artifact: &Artifact) -> PathBuf {
        self.root.join(artifact.id).join(artifact.file_name)
    }

    /// Cheap check: the file exists at exactly the pinned size. Doesn't re-hash; call
    /// [`Self::verify`] for that. A partial or foreign file at the right path can't pass, because
    /// install only ever renames a fully verified temp file into place.
    pub fn status(&self, artifact: &Artifact) -> Status {
        match fs::metadata(self.path(artifact)) {
            Ok(m) if m.is_file() && m.len() == artifact.installed_size => Status::Installed,
            _ => Status::NotInstalled,
        }
    }

    /// The installed file's path, or `None` if it isn't installed.
    pub fn installed_path(&self, artifact: &Artifact) -> Option<PathBuf> {
        (self.status(artifact) == Status::Installed).then(|| self.path(artifact))
    }

    /// The ONNX Runtime library this store installed, where Nicti ships one.
    pub fn ort_runtime_path(&self) -> Option<PathBuf> {
        #[cfg(windows)]
        {
            self.installed_path(&ORT_RUNTIME)
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    /// Deletes `artifact`'s installed file (a no-op if it isn't there), so a later `install` fetches
    /// it fresh -- the way out for a same-size but corrupt file that `status` still reports as
    /// installed.
    pub fn remove(&self, artifact: &Artifact) -> io::Result<()> {
        match fs::remove_file(self.path(artifact)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// Re-hashes the installed file against the pinned SHA-256.
    pub fn verify(&self, artifact: &Artifact) -> io::Result<bool> {
        let mut file = match fs::File::open(self.path(artifact)) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hex(&hasher.finalize()) == artifact.installed_sha256)
    }

    /// Downloads, verifies and installs one artifact. Already-installed artifacts are a no-op.
    /// `progress` is called as bytes arrive; `cancel` is polled per chunk.
    pub fn install(
        &self,
        artifact: &Artifact,
        downloader: &dyn Downloader,
        progress: &mut dyn FnMut(Progress),
        cancel: &AtomicBool,
    ) -> Result<PathBuf, InstallError> {
        if self.status(artifact) == Status::Installed {
            return Ok(self.path(artifact));
        }
        let dir = self.root.join(artifact.id);
        fs::create_dir_all(&dir)?;
        let final_path = self.path(artifact);
        let download_tmp = dir.join(format!("{}.download", artifact.file_name));
        let result = self.install_inner(
            artifact,
            downloader,
            progress,
            cancel,
            &download_tmp,
            &final_path,
        );
        // The temp payload never outlives a call, success or failure.
        let _ = fs::remove_file(&download_tmp);
        result
    }

    fn install_inner(
        &self,
        artifact: &Artifact,
        downloader: &dyn Downloader,
        progress: &mut dyn FnMut(Progress),
        cancel: &AtomicBool,
        download_tmp: &Path,
        final_path: &Path,
    ) -> Result<PathBuf, InstallError> {
        let mut out = fs::File::create(download_tmp)?;
        let mut hasher = Sha256::new();
        let mut got = 0u64;
        let mut cancelled = false;
        let mut write_err: Option<io::Error> = None;
        progress(Progress {
            downloaded: 0,
            total: artifact.download_size,
        });
        downloader.fetch(
            artifact.url,
            artifact.download_size,
            &mut |chunk: &[u8]| {
                if cancel.load(Ordering::Relaxed) {
                    cancelled = true;
                    return false;
                }
                if got + chunk.len() as u64 > artifact.download_size {
                    // More than was pinned: keep counting so the size check below rejects it.
                    got += chunk.len() as u64;
                    return false;
                }
                if let Err(e) = out.write_all(chunk) {
                    write_err = Some(e);
                    return false;
                }
                hasher.update(chunk);
                got += chunk.len() as u64;
                progress(Progress {
                    downloaded: got,
                    total: artifact.download_size,
                });
                true
            },
        )?;
        if cancelled {
            return Err(InstallError::Cancelled);
        }
        if let Some(e) = write_err {
            return Err(e.into());
        }
        out.flush()?;
        drop(out);
        if got != artifact.download_size {
            return Err(InstallError::WrongSize {
                id: artifact.id,
                got,
                expected: artifact.download_size,
            });
        }
        let digest = hex(&hasher.finalize());
        if digest != artifact.download_sha256 {
            return Err(InstallError::HashMismatch {
                id: artifact.id,
                got: digest,
            });
        }

        let staged = final_path.with_extension("partial");
        // Whatever happens below, don't leave the staged file behind.
        let _cleanup = RemoveOnDrop(&staged);
        match artifact.payload {
            Payload::File => fs::rename(download_tmp, &staged)?,
            Payload::ZipMember(member) => extract_member(artifact, download_tmp, member, &staged)?,
        }
        // The extracted/renamed file must itself match the pinned installed hash and size.
        // Streamed: the installed file can be 200+ MB and must not be loaded into memory to hash.
        let (installed_len, installed_hash) = hash_file(&staged)?;
        if installed_len != artifact.installed_size || installed_hash != artifact.installed_sha256 {
            let _ = fs::remove_file(&staged);
            return Err(InstallError::HashMismatch {
                id: artifact.id,
                got: installed_hash,
            });
        }
        fs::rename(&staged, final_path)?;
        Ok(final_path.to_path_buf())
    }
}

/// Removes a file when dropped -- after a successful rename it is already gone, so that is a no-op.
struct RemoveOnDrop<'a>(&'a Path);

impl Drop for RemoveOnDrop<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0);
    }
}

fn extract_member(
    artifact: &Artifact,
    zip_path: &Path,
    member: &'static str,
    dest: &Path,
) -> Result<(), InstallError> {
    let bad = |reason: String| InstallError::BadArchive {
        id: artifact.id,
        member,
        reason,
    };
    let file = fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| bad(e.to_string()))?;
    let mut entry = archive.by_name(member).map_err(|e| bad(e.to_string()))?;
    // Bounded like the download: never write more than the pinned installed size.
    let mut out = fs::File::create(dest)?;
    let copied = io::copy(
        &mut (&mut entry).take(artifact.installed_size + 1),
        &mut out,
    )?;
    if copied > artifact.installed_size {
        drop(out);
        let _ = fs::remove_file(dest);
        return Err(bad("member is larger than the pinned size".to_owned()));
    }
    Ok(())
}

/// Length and SHA-256 (hex) of a file, read in chunks.
fn hash_file(path: &Path) -> io::Result<(u64, String)> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut len = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        len += n as u64;
    }
    Ok((len, hex(&hasher.finalize())))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A fake network: serves `body` in `chunk`-sized pieces and counts calls.
    struct Fake {
        body: Vec<u8>,
        chunk: usize,
        calls: Cell<u32>,
    }

    impl Fake {
        fn new(body: Vec<u8>) -> Self {
            Self {
                body,
                chunk: 7,
                calls: Cell::new(0),
            }
        }
    }

    impl Downloader for Fake {
        fn fetch(
            &self,
            _url: &str,
            _max: u64,
            on_chunk: &mut dyn FnMut(&[u8]) -> bool,
        ) -> Result<(), DownloadError> {
            self.calls.set(self.calls.get() + 1);
            for piece in self.body.chunks(self.chunk) {
                if !on_chunk(piece) {
                    break;
                }
            }
            Ok(())
        }
    }

    fn sha(bytes: &[u8]) -> &'static str {
        Box::leak(hex(&Sha256::digest(bytes)).into_boxed_str())
    }

    fn file_artifact(body: &[u8]) -> Artifact {
        Artifact {
            id: "test-model",
            label: "test",
            file_name: "m.onnx",
            url: "https://example.invalid/m.onnx",
            download_size: body.len() as u64,
            download_sha256: sha(body),
            installed_size: body.len() as u64,
            installed_sha256: sha(body),
            license: "test",
            payload: Payload::File,
        }
    }

    fn temp_store(name: &str) -> ModelStore {
        let dir = std::env::temp_dir().join(format!(
            "nicti-stalk-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        ModelStore::new(dir)
    }

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn installs_a_verified_file_and_reports_progress() {
        let body = b"the quick brown fox jumps over the lazy dog".to_vec();
        let a = file_artifact(&body);
        let store = temp_store("ok");
        assert_eq!(store.status(&a), Status::NotInstalled);
        let mut last = None;
        let path = store
            .install(
                &a,
                &Fake::new(body.clone()),
                &mut |p| last = Some(p),
                &no_cancel(),
            )
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), body);
        assert_eq!(store.status(&a), Status::Installed);
        assert!(store.verify(&a).unwrap());
        assert_eq!(
            last,
            Some(Progress {
                downloaded: body.len() as u64,
                total: body.len() as u64
            })
        );
        // No temp files left behind.
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("m.onnx")]);
    }

    #[test]
    fn installing_an_installed_artifact_never_touches_the_network() {
        let body = b"abc".to_vec();
        let a = file_artifact(&body);
        let store = temp_store("noop");
        let dl = Fake::new(body);
        store.install(&a, &dl, &mut |_| {}, &no_cancel()).unwrap();
        store.install(&a, &dl, &mut |_| {}, &no_cancel()).unwrap();
        assert_eq!(dl.calls.get(), 1);
    }

    #[test]
    fn a_tampered_download_is_rejected_and_nothing_is_installed() {
        let good = b"0123456789".to_vec();
        let a = file_artifact(&good);
        let store = temp_store("tamper");
        let mut bad = good.clone();
        bad[3] ^= 0xff; // same length, different content
        let err = store
            .install(&a, &Fake::new(bad), &mut |_| {}, &no_cancel())
            .unwrap_err();
        assert!(matches!(err, InstallError::HashMismatch { .. }), "{err}");
        assert_eq!(store.status(&a), Status::NotInstalled);
        assert!(!store.path(&a).exists());
    }

    #[test]
    fn a_short_download_is_rejected() {
        let good = b"0123456789".to_vec();
        let a = file_artifact(&good);
        let store = temp_store("short");
        let err = store
            .install(
                &a,
                &Fake::new(good[..4].to_vec()),
                &mut |_| {},
                &no_cancel(),
            )
            .unwrap_err();
        assert!(
            matches!(err, InstallError::WrongSize { got: 4, .. }),
            "{err}"
        );
        assert_eq!(store.status(&a), Status::NotInstalled);
    }

    #[test]
    fn a_download_longer_than_pinned_is_cut_off_and_rejected() {
        let good = b"0123456789".to_vec();
        let a = file_artifact(&good);
        let store = temp_store("long");
        let mut long = good.clone();
        long.extend_from_slice(&[0u8; 1000]);
        let err = store
            .install(&a, &Fake::new(long), &mut |_| {}, &no_cancel())
            .unwrap_err();
        assert!(matches!(err, InstallError::WrongSize { .. }), "{err}");
        assert_eq!(store.status(&a), Status::NotInstalled);
    }

    #[test]
    fn cancelling_stops_the_download_and_installs_nothing() {
        let body = vec![7u8; 1000];
        let a = file_artifact(&body);
        let store = temp_store("cancel");
        let cancel = AtomicBool::new(true);
        let err = store
            .install(&a, &Fake::new(body), &mut |_| {}, &cancel)
            .unwrap_err();
        assert!(matches!(err, InstallError::Cancelled), "{err}");
        assert_eq!(store.status(&a), Status::NotInstalled);
        // The temp download is cleaned up too.
        let dir = store.root().join(a.id);
        assert_eq!(fs::read_dir(dir).unwrap().count(), 0);
    }

    #[test]
    fn a_network_error_installs_nothing() {
        struct Down;
        impl Downloader for Down {
            fn fetch(
                &self,
                _: &str,
                _: u64,
                _: &mut dyn FnMut(&[u8]) -> bool,
            ) -> Result<(), DownloadError> {
                Err(DownloadError::Network("connection reset".into()))
            }
        }
        let a = file_artifact(b"xyz");
        let store = temp_store("net");
        let err = store
            .install(&a, &Down, &mut |_| {}, &no_cancel())
            .unwrap_err();
        assert!(matches!(err, InstallError::Download(_)));
        assert_eq!(store.status(&a), Status::NotInstalled);
    }

    #[test]
    fn remove_deletes_an_installed_file_and_tolerates_a_missing_one() {
        let body = b"0123456789".to_vec();
        let a = file_artifact(&body);
        let store = temp_store("remove");
        store.remove(&a).unwrap(); // nothing there: fine
        store
            .install(&a, &Fake::new(body), &mut |_| {}, &no_cancel())
            .unwrap();
        assert_eq!(store.status(&a), Status::Installed);
        store.remove(&a).unwrap();
        assert_eq!(store.status(&a), Status::NotInstalled);
    }

    #[test]
    fn a_failed_extraction_leaves_no_partial_file() {
        let content = vec![1u8; 100];
        let zip_bytes = zip_with("pkg/lib/lib.dll", &content);
        let mut a = zip_artifact(&zip_bytes, &content);
        a.installed_sha256 = sha(b"not the content"); // extraction succeeds, verification fails
        let store = temp_store("no-partial");
        assert!(store
            .install(&a, &Fake::new(zip_bytes), &mut |_| {}, &no_cancel())
            .is_err());
        assert_eq!(fs::read_dir(store.root().join(a.id)).unwrap().count(), 0);
    }

    #[test]
    fn a_wrong_size_file_at_the_install_path_is_not_installed() {
        let a = file_artifact(b"expected content");
        let store = temp_store("foreign");
        fs::create_dir_all(store.path(&a).parent().unwrap()).unwrap();
        fs::write(store.path(&a), b"short").unwrap();
        assert_eq!(store.status(&a), Status::NotInstalled);
        assert!(store.installed_path(&a).is_none());
    }

    #[test]
    fn verify_catches_a_corrupted_install_of_the_right_size() {
        let body = b"0123456789".to_vec();
        let a = file_artifact(&body);
        let store = temp_store("corrupt");
        store
            .install(&a, &Fake::new(body.clone()), &mut |_| {}, &no_cancel())
            .unwrap();
        let mut bad = body;
        bad[0] ^= 1;
        fs::write(store.path(&a), &bad).unwrap();
        assert_eq!(
            store.status(&a),
            Status::Installed,
            "size check alone can't tell"
        );
        assert!(!store.verify(&a).unwrap());
    }

    fn zip_with(member: &str, content: &[u8]) -> Vec<u8> {
        use zip::write::SimpleFileOptions;
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(io::Cursor::new(&mut buf));
            w.start_file("other/README.txt", SimpleFileOptions::default())
                .unwrap();
            w.write_all(b"not the droid").unwrap();
            w.start_file(member, SimpleFileOptions::default()).unwrap();
            w.write_all(content).unwrap();
            w.finish().unwrap();
        }
        buf
    }

    fn zip_artifact(zip_bytes: &[u8], content: &[u8]) -> Artifact {
        Artifact {
            id: "test-zip",
            label: "test zip",
            file_name: "lib.dll",
            url: "https://example.invalid/x.zip",
            download_size: zip_bytes.len() as u64,
            download_sha256: sha(zip_bytes),
            installed_size: content.len() as u64,
            installed_sha256: sha(content),
            license: "test",
            payload: Payload::ZipMember("pkg/lib/lib.dll"),
        }
    }

    #[test]
    fn extracts_only_the_pinned_zip_member() {
        let content = vec![0xABu8; 5000];
        let zip_bytes = zip_with("pkg/lib/lib.dll", &content);
        let a = zip_artifact(&zip_bytes, &content);
        let store = temp_store("zip");
        let path = store
            .install(&a, &Fake::new(zip_bytes), &mut |_| {}, &no_cancel())
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), content);
        // Only the extracted file remains: no zip, no partial.
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn a_zip_missing_the_member_is_rejected() {
        let content = vec![1u8; 100];
        let zip_bytes = zip_with("pkg/other.dll", &content);
        let a = zip_artifact(&zip_bytes, &content);
        let store = temp_store("zip-missing");
        let err = store
            .install(&a, &Fake::new(zip_bytes), &mut |_| {}, &no_cancel())
            .unwrap_err();
        assert!(matches!(err, InstallError::BadArchive { .. }), "{err}");
        assert_eq!(store.status(&a), Status::NotInstalled);
    }

    #[test]
    fn a_zip_member_that_hashes_wrong_is_rejected() {
        let content = vec![1u8; 100];
        let zip_bytes = zip_with("pkg/lib/lib.dll", &content);
        let mut a = zip_artifact(&zip_bytes, &content);
        a.installed_sha256 = sha(b"something else entirely");
        let store = temp_store("zip-hash");
        let err = store
            .install(&a, &Fake::new(zip_bytes), &mut |_| {}, &no_cancel())
            .unwrap_err();
        assert!(matches!(err, InstallError::HashMismatch { .. }), "{err}");
        assert_eq!(store.status(&a), Status::NotInstalled);
        assert!(!store.path(&a).exists());
    }

    #[test]
    fn manifest_entries_are_well_formed() {
        // ORT_RUNTIME is only *installed* on Windows, but its manifest entry is checked everywhere:
        // a typo'd hash there would otherwise surface only on a Windows machine at download time.
        for a in [
            &ORT_RUNTIME,
            &MOBILE_SAM_ENCODER,
            &MOBILE_SAM_DECODER,
            &LAMA,
            &BIREFNET,
        ] {
            assert_eq!(a.download_sha256.len(), 64, "{}", a.id);
            assert_eq!(a.installed_sha256.len(), 64, "{}", a.id);
            assert!(a.download_sha256.bytes().all(|b| b.is_ascii_hexdigit()));
            assert!(a.url.starts_with("https://"), "{}", a.id);
            assert!(
                !a.url.contains("/resolve/main/"),
                "{} must be pinned to an immutable revision, not a branch",
                a.id
            );
            assert!(a.download_size > 0 && a.installed_size > 0);
        }
        let ids: Vec<_> = removal_artifacts().iter().map(|a| a.id).collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(ids.len(), unique.len());
        let mask_ids: Vec<_> = mask_artifacts().iter().map(|a| a.id).collect();
        assert!(mask_ids.contains(&BIREFNET.id));
        // Ids double as directory names in one shared store, so no two artifacts may collide.
        let mut all: Vec<_> = ids.into_iter().chain(mask_ids).collect();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), 5 - usize::from(!cfg!(windows)), "{all:?}");
    }

    #[test]
    fn birefnet_is_pinned_to_the_reviewed_revision_and_the_fp32_file() {
        assert!(BIREFNET
            .url
            .contains("/resolve/534d3c82d3bb8b2f0867db6dfbc3a525b8e42f67/onnx/model.onnx"));
        assert_eq!(BIREFNET.download_size, BIREFNET.installed_size);
        assert_eq!(BIREFNET.download_sha256, BIREFNET.installed_sha256);
        assert!(BIREFNET.license.starts_with("MIT"));
        assert_eq!(BIREFNET.payload, Payload::File);
    }

    #[test]
    fn mask_verify_names_birefnet_when_it_is_missing() {
        let store = temp_store("verify-mask");
        let err = verify_artifacts(&store, &mask_artifacts(), false).unwrap_err();
        assert!(err.contains("BiRefNet"), "{err}");
    }

    #[test]
    fn mask_download_size_counts_only_what_is_not_installed_yet() {
        let store = temp_store("mask-size");
        let total = mask_download_bytes(&store);
        assert!(total >= BIREFNET.download_size);
        assert!(
            total <= BIREFNET.download_size + ORT_RUNTIME.download_size,
            "{total}"
        );
    }

    #[test]
    fn mask_locate_needs_birefnet_and_a_runtime() {
        let store = temp_store("mask-locate");
        assert!(MaskModels::locate(&store).is_none());
    }

    #[test]
    fn verify_removal_install_rejects_a_right_sized_file_with_the_wrong_content() {
        // Sparse zero files at exactly the pinned sizes pass `status` but must fail the hash --
        // the size check alone would happily hand a tampered library to dlopen.
        let store = temp_store("verify-removal");
        let a = &MOBILE_SAM_DECODER; // 16 MB, hashes quickly even in a debug build
        fs::create_dir_all(store.path(a).parent().unwrap()).unwrap();
        fs::File::create(store.path(a))
            .unwrap()
            .set_len(a.installed_size)
            .unwrap();
        assert_eq!(store.status(a), Status::Installed, "size alone is fooled");
        assert!(!store.verify(a).unwrap());
        // Skip the runtime (as when NICTI_ORT_DYLIB supplies it) and the first artifact checked
        // is the encoder, which isn't installed at all.
        let err = verify_removal_install(&store, false).unwrap_err();
        assert!(err.contains("MobileSAM image encoder"), "{err}");
        assert!(err.contains("SHA-256"), "{err}");
    }

    #[test]
    fn verify_removal_install_names_the_runtime_first_when_it_comes_from_the_store() {
        let store = temp_store("verify-runtime");
        let err = verify_removal_install(&store, true).unwrap_err();
        // The runtime is only a pinned artifact where Nicti ships one (Windows); elsewhere the
        // first thing checked is the first model.
        let first = if cfg!(windows) {
            "ONNX Runtime"
        } else {
            "MobileSAM image encoder"
        };
        assert!(err.contains(first), "{err}");
    }

    #[test]
    fn locate_needs_every_model_and_a_runtime() {
        let store = temp_store("locate");
        assert!(RemovalModels::locate(&store).is_none());
    }
}
