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
    /// The download is a zip (or wheel) of which several members are installed together into
    /// `<store>/<dir>/` -- a directory several artifacts can share, which a native library set
    /// (ONNX Runtime + cuDNN + cuBLAS) needs, because it resolves its dependencies from one place.
    /// The artifact's `file_name`/`installed_*` describe the primary member only; the store checks
    /// every member.
    ZipMembers {
        dir: &'static str,
        members: &'static [Member],
    },
}

/// One file extracted from a [`Payload::ZipMembers`] archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Member {
    /// Path of the entry inside the archive.
    pub zip_path: &'static str,
    /// The installed file's name inside the artifact's directory.
    pub file_name: &'static str,
    pub size: u64,
    /// SHA-256 (lowercase hex) of the installed file.
    pub sha256: &'static str,
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

/// The members of the ONNX Runtime 1.28.0 Windows x64 **CUDA 13** build's zip that Nicti keeps: the
/// runtime itself, its provider-loader, and the CUDA execution provider (the TensorRT provider and
/// debug symbols are dropped).
const ORT_CUDA_MEMBERS: &[Member] = &[
    Member {
        zip_path: "onnxruntime-win-x64-gpu_cuda13-1.28.0/lib/onnxruntime.dll",
        file_name: "onnxruntime.dll",
        size: 16_277_856,
        sha256: "2462fe2d64ce063babefda3d9b1998380ffa74e99acf5d24d520ee67daa9e0f1",
    },
    Member {
        zip_path: "onnxruntime-win-x64-gpu_cuda13-1.28.0/lib/onnxruntime_providers_cuda.dll",
        file_name: "onnxruntime_providers_cuda.dll",
        size: 267_649_848,
        sha256: "ea191544c07fcb772d64737198004f259b7ac8a568150463c555cdb1b81ae91e",
    },
    Member {
        zip_path: "onnxruntime-win-x64-gpu_cuda13-1.28.0/lib/onnxruntime_providers_shared.dll",
        file_name: "onnxruntime_providers_shared.dll",
        size: 21_856,
        sha256: "c09e22fafb16675521c91db7a50a1f7e4f8922acb905ad50f8e5c66798eb153d",
    },
];

/// cuDNN 9.27.0.42 for CUDA 13 -- the DLLs from NVIDIA's own `nvidia-cudnn-cu13` PyPI wheel (the
/// full NVIDIA redistributable zip is 1.3 GB of headers and static libraries Nicti doesn't need).
const CUDNN_MEMBERS: &[Member] = &[
    Member {
        zip_path: "nvidia/cudnn/bin/cudnn64_9.dll",
        file_name: "cudnn64_9.dll",
        size: 270_448,
        sha256: "0016bd73ac9192537ae5af6a3f05814316726dad1d1da44f113f0a9ef00ae34c",
    },
    Member {
        zip_path: "nvidia/cudnn/bin/cudnn_adv64_9.dll",
        file_name: "cudnn_adv64_9.dll",
        size: 105_226_864,
        sha256: "041cab1d6439d2558b4a7073a42dd95058d0395463ffb0d37275af11ec0af8d7",
    },
    Member {
        zip_path: "nvidia/cudnn/bin/cudnn_cnn64_9.dll",
        file_name: "cudnn_cnn64_9.dll",
        size: 1_533_040,
        sha256: "7dd1bcc6d8f00ad635f6a8a2cd076faba21626f3412e13c61a6bece3f88eacb1",
    },
    Member {
        zip_path: "nvidia/cudnn/bin/cudnn_engines_precompiled64_9.dll",
        file_name: "cudnn_engines_precompiled64_9.dll",
        size: 231_660_656,
        sha256: "5d9b829228ae18594dafedf718d9612606c8ce6d199aa7151bea53e863baeefe",
    },
    Member {
        zip_path: "nvidia/cudnn/bin/cudnn_engines_runtime_compiled64_9.dll",
        file_name: "cudnn_engines_runtime_compiled64_9.dll",
        size: 39_266_928,
        sha256: "8e65eeb0a4a3b7afa48ea3ca71c4b88e355199f85485b6fc0badcc97ebc552ef",
    },
    Member {
        zip_path: "nvidia/cudnn/bin/cudnn_engines_tensor_ir64_9.dll",
        file_name: "cudnn_engines_tensor_ir64_9.dll",
        size: 156_272,
        sha256: "51afb44ce4b901ccdac1434f9fc9d8d707b690f39ae1e2a6ffca98c5fd9dfa96",
    },
    Member {
        zip_path: "nvidia/cudnn/bin/cudnn_ext64_9.dll",
        file_name: "cudnn_ext64_9.dll",
        size: 130_160,
        sha256: "a0e0e2e8e8e4678956bbef93fdccc4730bd5edf9b23d38bff49030ebba1940a6",
    },
    Member {
        zip_path: "nvidia/cudnn/bin/cudnn_graph64_9.dll",
        file_name: "cudnn_graph64_9.dll",
        size: 116_132_464,
        sha256: "ccc90435b2fd37b5ddd6a9fa0fd053ad8f46c2dc8b0a6256ef0ebea7f3efbace",
    },
    Member {
        zip_path: "nvidia/cudnn/bin/cudnn_heuristic64_9.dll",
        file_name: "cudnn_heuristic64_9.dll",
        size: 94_795_376,
        sha256: "c0439ee74a3dbec8ecf56fecbc6ac2506efcce702453e36119649545d02509c3",
    },
    Member {
        zip_path: "nvidia/cudnn/bin/cudnn_ops64_9.dll",
        file_name: "cudnn_ops64_9.dll",
        size: 37_462_128,
        sha256: "2bd7f4122eaeccb85538c2a6f7923d7254ad77d7b7149fed0a8139f1e3cf0b07",
    },
];

/// cuBLAS 13.8.0.4 from NVIDIA's CUDA redistributables (`redistrib_13.4.2.json`); `nvblas` is dropped.
const CUBLAS_MEMBERS: &[Member] = &[
    Member {
        zip_path: "libcublas-windows-x86_64-13.8.0.4-archive/bin/x64/cublas64_13.dll",
        file_name: "cublas64_13.dll",
        size: 54_873_200,
        sha256: "60bbba8868290311e9c1657b2193ddec667744eb555ff843f87acb7c039f9efa",
    },
    Member {
        zip_path: "libcublas-windows-x86_64-13.8.0.4-archive/bin/x64/cublasLt64_13.dll",
        file_name: "cublasLt64_13.dll",
        size: 493_474_416,
        sha256: "cad63434448e7141629e240ea093ad596a7ef6a0f67b468ba9bc1df6e1eeee33",
    },
];

/// Where the whole NVIDIA GPU pack installs: ONNX Runtime resolves the CUDA provider's own
/// dependencies from one directory, so the runtime, cuDNN and cuBLAS share it (#345).
const GPU_RUNTIME_DIR: &str = "ort-cuda";

/// ONNX Runtime 1.28.0, Windows x64 **CUDA 13** build (MIT), from the official Microsoft release --
/// a superset of [`ORT_RUNTIME`]'s CPU build. Part of the optional NVIDIA GPU pack
/// ([`gpu_pack_artifacts`]); needs cuDNN and cuBLAS beside it.
pub const ORT_CUDA_RUNTIME: Artifact = Artifact {
    id: "onnxruntime-cuda",
    label: "ONNX Runtime 1.28.0 (CUDA 13)",
    file_name: "onnxruntime.dll",
    url: "https://github.com/microsoft/onnxruntime/releases/download/v1.28.0/onnxruntime-win-x64-gpu_cuda13-1.28.0.zip",
    download_size: 365_825_268,
    download_sha256: "137f0822a4923b1d84d3e09496e0792ebbb221eb3a61a0657f71a12ab68ab1e2",
    installed_size: 16_277_856,
    installed_sha256: "2462fe2d64ce063babefda3d9b1998380ffa74e99acf5d24d520ee67daa9e0f1",
    license: "MIT",
    payload: Payload::ZipMembers {
        dir: GPU_RUNTIME_DIR,
        members: ORT_CUDA_MEMBERS,
    },
};

/// NVIDIA cuDNN 9.27.0.42 (CUDA 13), the CUDA execution provider's convolution library.
pub const CUDNN_RUNTIME: Artifact = Artifact {
    id: "cudnn",
    label: "NVIDIA cuDNN 9.27 (CUDA 13)",
    file_name: "cudnn64_9.dll",
    url: "https://files.pythonhosted.org/packages/87/6a/e55ff0ac26a5c6e2b21f41c9d04ad096b4ed6da593fba7e25845c61b0532/nvidia_cudnn_cu13-9.27.0.42-py3-none-win_amd64.whl",
    download_size: 436_469_905,
    download_sha256: "7d96f634adafd55c72231eb0500ca77ab109ec8ebff7b33000b76e081bc4558e",
    installed_size: 270_448,
    installed_sha256: "0016bd73ac9192537ae5af6a3f05814316726dad1d1da44f113f0a9ef00ae34c",
    license: "NVIDIA cuDNN license (redistributable with an application; docs/licensing.md)",
    payload: Payload::ZipMembers {
        dir: GPU_RUNTIME_DIR,
        members: CUDNN_MEMBERS,
    },
};

/// NVIDIA cuBLAS 13.8.0.4 (CUDA 13), the CUDA execution provider's matrix-multiply library.
pub const CUBLAS_RUNTIME: Artifact = Artifact {
    id: "cublas",
    label: "NVIDIA cuBLAS 13.8 (CUDA 13)",
    file_name: "cublas64_13.dll",
    url: "https://developer.download.nvidia.com/compute/cuda/redist/libcublas/windows-x86_64/libcublas-windows-x86_64-13.8.0.4-archive.zip",
    download_size: 422_415_931,
    download_sha256: "0974318e9861a61cb9091cf7de8d4c2880e9787fe311cb4bcbd08865663c5f43",
    installed_size: 54_873_200,
    installed_sha256: "60bbba8868290311e9c1657b2193ddec667744eb555ff843f87acb7c039f9efa",
    license: "NVIDIA CUDA Toolkit EULA (redistributable runtime; docs/licensing.md)",
    payload: Payload::ZipMembers {
        dir: GPU_RUNTIME_DIR,
        members: CUBLAS_MEMBERS,
    },
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

/// BiRefNet fp16 ONNX export from the same repo and revision as [`BIREFNET`] -- float32 inputs and
/// outputs, so a drop-in swap. Part of the optional NVIDIA GPU pack ([`gpu_pack_artifacts`]): on the
/// CUDA execution provider it is ~1.6x faster than fp32 and takes ~8 GB instead of ~13 GB of VRAM
/// (ADR-0049's measured results). Not used on the CPU provider, where fp16 is slower.
pub const BIREFNET_FP16: Artifact = Artifact {
    id: "birefnet-fp16",
    label: "BiRefNet fp16 masking model (GPU)",
    file_name: "birefnet_fp16.onnx",
    url: "https://huggingface.co/onnx-community/BiRefNet-ONNX/resolve/534d3c82d3bb8b2f0867db6dfbc3a525b8e42f67/onnx/model_fp16.onnx",
    download_size: 489_666_272,
    download_sha256: "3654c741eb80bd926ada8fed1713b506ccf8d30eb1f6487e87eb9f234f33df09",
    installed_size: 489_666_272,
    installed_sha256: "3654c741eb80bd926ada8fed1713b506ccf8d30eb1f6487e87eb9f234f33df09",
    license: "MIT (trained on DIS-TR per the upstream model card; a third-party ONNX conversion, see docs/licensing.md)",
    payload: Payload::File,
};

/// The optional **NVIDIA GPU pack** (#345), in install order: the CUDA build of ONNX Runtime, the
/// cuDNN and cuBLAS it loads, and the fp16 BiRefNet. Windows only; elsewhere `NICTI_ORT_DYLIB` must
/// point at a GPU runtime. Additive to [`mask_artifacts`] -- the CPU path stays the fallback.
pub fn gpu_pack_artifacts() -> Vec<&'static Artifact> {
    #[cfg(windows)]
    let v = vec![
        &ORT_CUDA_RUNTIME,
        &CUDNN_RUNTIME,
        &CUBLAS_RUNTIME,
        &BIREFNET_FP16,
    ];
    #[cfg(not(windows))]
    let v = vec![&BIREFNET_FP16];
    v
}

/// True when an NVIDIA display driver (which provides `nvcuda.dll`) is installed -- the precondition
/// for offering the GPU pack. Windows only; the CUDA runtime can't work without the driver
/// (`docs/adr/0018`), and the pack is never offered on other GPUs.
pub fn nvidia_driver_present() -> bool {
    #[cfg(windows)]
    {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        Path::new(&root)
            .join("System32")
            .join("nvcuda.dll")
            .is_file()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Total bytes a user agrees to download for the GPU pack (skips anything already installed).
pub fn gpu_pack_download_bytes(store: &ModelStore) -> u64 {
    gpu_pack_artifacts()
        .iter()
        .filter(|a| store.status(a) != Status::Installed)
        .map(|a| a.download_size)
        .sum()
}

/// Everything AI subject/background masking needs, in install order (the ONNX Runtime entry exists
/// only where Nicti ships a runtime download, as for removal).
pub fn mask_artifacts() -> Vec<&'static Artifact> {
    let mut v: Vec<&'static Artifact> = Vec::new();
    #[cfg(windows)]
    v.push(&ORT_RUNTIME);
    v.extend([&BIREFNET]);
    v
}

/// [`mask_artifacts`] for what `store` has: the CPU ONNX Runtime is left out when the GPU pack's
/// CUDA build is the runtime in use, so a user with the pack isn't asked to download one nothing
/// would load (the same rule as [`removal_artifacts_for`]).
pub fn mask_artifacts_for(store: &ModelStore) -> Vec<&'static Artifact> {
    let gpu = store.gpu_runtime_path().is_some();
    mask_artifacts()
        .into_iter()
        .filter(|a| !(gpu && a.id == ORT_RUNTIME.id))
        .collect()
}

/// What a Repair must re-check for AI masks: exactly the set the load path verifies
/// ([`MaskModels::artifacts`] -- including the GPU pack's files when that is the runtime in use),
/// else the default set when the masks aren't fully installed yet.
pub fn mask_repair_artifacts(store: &ModelStore) -> Vec<&'static Artifact> {
    MaskModels::locate(store)
        .map(|m| m.artifacts())
        .unwrap_or_else(|| mask_artifacts_for(store))
}

/// [`mask_repair_artifacts`] for AI removal ([`RemovalModels::artifacts`]).
pub fn removal_repair_artifacts(store: &ModelStore) -> Vec<&'static Artifact> {
    RemovalModels::locate(store)
        .map(|m| m.artifacts())
        .unwrap_or_else(|| removal_artifacts_for(store))
}

/// Total bytes a user agrees to download for AI masks, for the confirmation prompt (skips anything
/// already installed, e.g. a runtime AI removal already fetched).
pub fn mask_download_bytes(store: &ModelStore) -> u64 {
    mask_artifacts_for(store)
        .iter()
        .filter(|a| store.status(a) != Status::Installed)
        .map(|a| a.download_size)
        .sum()
}

/// Resolved on-disk locations of every file AI masks need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaskModels {
    pub ort_dylib: PathBuf,
    /// The fp32 BiRefNet: always present, and what the CPU provider runs.
    pub birefnet: PathBuf,
    /// True when `ort_dylib` is the NVIDIA GPU pack's CUDA build of the runtime.
    pub gpu_runtime: bool,
    /// The fp16 BiRefNet: only when the GPU pack's runtime is the one in use *and* the model is
    /// installed (an interrupted pack download can leave the runtime without it).
    pub birefnet_gpu: Option<PathBuf>,
}

impl MaskModels {
    /// `Some` only when BiRefNet is installed and an ONNX Runtime library is available (the store's
    /// own copy on Windows -- the GPU pack's CUDA build when that is installed -- or
    /// `NICTI_ORT_DYLIB`, which wins where set).
    pub fn locate(store: &ModelStore) -> Option<Self> {
        let ort_dylib = match std::env::var_os("NICTI_ORT_DYLIB") {
            Some(p) if !p.is_empty() => PathBuf::from(p),
            _ => store.ort_runtime_path()?,
        };
        let gpu_runtime = store.gpu_runtime_path().as_deref() == Some(ort_dylib.as_path());
        let birefnet_gpu = gpu_runtime
            .then(|| store.installed_path(&BIREFNET_FP16))
            .flatten();
        let models = Self {
            ort_dylib,
            birefnet: store.installed_path(&BIREFNET)?,
            gpu_runtime,
            birefnet_gpu,
        };
        models.ort_dylib.is_file().then_some(models)
    }

    /// The store artifacts these paths came from, for [`verify_artifacts`] on first load (the
    /// runtime entries are skipped there when `NICTI_ORT_DYLIB` supplied the library).
    pub fn artifacts(&self) -> Vec<&'static Artifact> {
        let mut v: Vec<&'static Artifact> = Vec::new();
        if self.gpu_runtime {
            v.extend([&ORT_CUDA_RUNTIME, &CUDNN_RUNTIME, &CUBLAS_RUNTIME]);
        } else {
            #[cfg(windows)]
            v.push(&ORT_RUNTIME);
        }
        v.push(&BIREFNET);
        if self.birefnet_gpu.is_some() {
            v.push(&BIREFNET_FP16);
        }
        v
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

/// [`removal_artifacts`] for what `store` has: the CPU ONNX Runtime is left out when the GPU pack's
/// CUDA build (which also runs the CPU provider) is the runtime in use, so a user with only the
/// pack isn't asked to download a runtime nothing would load.
pub fn removal_artifacts_for(store: &ModelStore) -> Vec<&'static Artifact> {
    let gpu = store.gpu_runtime_path().is_some();
    removal_artifacts()
        .into_iter()
        .filter(|a| !(gpu && a.id == ORT_RUNTIME.id))
        .collect()
}

/// Total bytes a user agrees to download for AI removal, for the confirmation prompt.
pub fn removal_download_bytes(store: &ModelStore) -> u64 {
    removal_artifacts_for(store)
        .iter()
        .filter(|a| store.status(a) != Status::Installed)
        .map(|a| a.download_size)
        .sum()
}

/// Resolved on-disk locations of every file AI removal needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovalModels {
    pub ort_dylib: PathBuf,
    /// True when `ort_dylib` is the NVIDIA GPU pack's CUDA build of the runtime.
    pub gpu_runtime: bool,
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
        let gpu_runtime = store.gpu_runtime_path().as_deref() == Some(ort_dylib.as_path());
        let models = Self {
            ort_dylib,
            gpu_runtime,
            sam_encoder: store.installed_path(&MOBILE_SAM_ENCODER)?,
            sam_decoder: store.installed_path(&MOBILE_SAM_DECODER)?,
            lama: store.installed_path(&LAMA)?,
        };
        models.ort_dylib.is_file().then_some(models)
    }
}

/// Artifacts that are the ONNX Runtime library set itself, which `NICTI_ORT_DYLIB` replaces.
const RUNTIME_IDS: [&str; 4] = ["onnxruntime", "onnxruntime-cuda", "cudnn", "cublas"];

impl RemovalModels {
    /// The store artifacts these paths came from, for [`verify_artifacts`] on first load -- the
    /// runtime set actually in use (the GPU pack's CUDA build serves removal too), not a fixed one.
    pub fn artifacts(&self) -> Vec<&'static Artifact> {
        let mut v: Vec<&'static Artifact> = Vec::new();
        if self.gpu_runtime {
            v.extend([&ORT_CUDA_RUNTIME, &CUDNN_RUNTIME, &CUBLAS_RUNTIME]);
        } else {
            #[cfg(windows)]
            v.push(&ORT_RUNTIME);
        }
        v.extend([&MOBILE_SAM_ENCODER, &MOBILE_SAM_DECODER, &LAMA]);
        v
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
        if !ort_from_store && RUNTIME_IDS.contains(&artifact.id) {
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

    /// The directory `artifact` installs into: its own id, unless it shares one
    /// ([`Payload::ZipMembers`]).
    pub fn dir(&self, artifact: &Artifact) -> PathBuf {
        match artifact.payload {
            Payload::ZipMembers { dir, .. } => self.root.join(dir),
            _ => self.root.join(artifact.id),
        }
    }

    /// Where `artifact`'s installed file lives (whether or not it exists yet); for a multi-member
    /// artifact, its primary member.
    pub fn path(&self, artifact: &Artifact) -> PathBuf {
        self.dir(artifact).join(artifact.file_name)
    }

    /// Every file `artifact` installs: path, exact size, SHA-256.
    fn files(&self, artifact: &Artifact) -> Vec<(PathBuf, u64, &'static str)> {
        match artifact.payload {
            Payload::ZipMembers { members, .. } => members
                .iter()
                .map(|m| (self.dir(artifact).join(m.file_name), m.size, m.sha256))
                .collect(),
            _ => vec![(
                self.path(artifact),
                artifact.installed_size,
                artifact.installed_sha256,
            )],
        }
    }

    /// Cheap check: every file exists at exactly the pinned size. Doesn't re-hash; call
    /// [`Self::verify`] for that. A partial or foreign file at the right path can't pass, because
    /// install only ever renames a fully verified temp file into place.
    pub fn status(&self, artifact: &Artifact) -> Status {
        let all_present = self.files(artifact).iter().all(|(path, size, _)| {
            fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() == *size)
        });
        if all_present {
            Status::Installed
        } else {
            Status::NotInstalled
        }
    }

    /// The installed file's path, or `None` if it isn't installed.
    pub fn installed_path(&self, artifact: &Artifact) -> Option<PathBuf> {
        (self.status(artifact) == Status::Installed).then(|| self.path(artifact))
    }

    /// The ONNX Runtime library this store installed, where Nicti ships one: the NVIDIA GPU pack's
    /// CUDA build when that is installed (it also runs the CPU provider, so AI removal and masks
    /// share the one process-wide runtime), else the CPU build.
    pub fn ort_runtime_path(&self) -> Option<PathBuf> {
        #[cfg(windows)]
        {
            self.gpu_runtime_path()
                .or_else(|| self.installed_path(&ORT_RUNTIME))
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    /// The CUDA build of ONNX Runtime, only if it and the cuDNN/cuBLAS it needs are all installed
    /// **and were already installed the first time this process asked**. ONNX Runtime's environment
    /// is process-wide and can't be re-pointed once committed, so a pack installed mid-session must
    /// not change which runtime masks and removal resolve (that would fail every later load with a
    /// path mismatch) -- it takes effect on the next start, as the panel says.
    pub fn gpu_runtime_path(&self) -> Option<PathBuf> {
        #[cfg(windows)]
        {
            static AVAILABLE_AT_START: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            let complete = [&ORT_CUDA_RUNTIME, &CUDNN_RUNTIME, &CUBLAS_RUNTIME]
                .into_iter()
                .all(|a| self.status(a) == Status::Installed);
            let at_start = *AVAILABLE_AT_START.get_or_init(|| complete);
            (complete && at_start).then(|| self.path(&ORT_CUDA_RUNTIME))
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
        for (path, _, _) in self.files(artifact) {
            match fs::remove_file(path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        Ok(())
    }

    /// Re-hashes every installed file against its pinned SHA-256.
    pub fn verify(&self, artifact: &Artifact) -> io::Result<bool> {
        for (path, _, sha256) in self.files(artifact) {
            let mut file = match fs::File::open(&path) {
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
            if hex(&hasher.finalize()) != sha256 {
                return Ok(false);
            }
        }
        Ok(true)
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
        let dir = self.dir(artifact);
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

        if let Payload::ZipMembers { members, .. } = artifact.payload {
            return self.install_members(artifact, members, download_tmp);
        }
        let staged = final_path.with_extension("partial");
        // Whatever happens below, don't leave the staged file behind.
        let _cleanup = RemoveOnDrop(&staged);
        match artifact.payload {
            Payload::File => fs::rename(download_tmp, &staged)?,
            Payload::ZipMember(member) => extract_member(
                artifact,
                download_tmp,
                member,
                artifact.installed_size,
                &staged,
            )?,
            Payload::ZipMembers { .. } => unreachable!("handled above"),
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

    /// Extracts every member of a verified multi-member archive: each is staged next to its final
    /// name and hash-checked, and only once all of them pass are any renamed into place, so a bad
    /// member never leaves a half-installed set that `status` could mistake for complete.
    fn install_members(
        &self,
        artifact: &Artifact,
        members: &'static [Member],
        archive: &Path,
    ) -> Result<PathBuf, InstallError> {
        let dir = self.dir(artifact);
        // A killed earlier install can have left hundreds of MB of `.partial` files behind.
        for member in members {
            let _ = fs::remove_file(dir.join(format!("{}.partial", member.file_name)));
        }
        let mut staged: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(members.len());
        let result = (|| {
            for member in members {
                let final_path = dir.join(member.file_name);
                let partial = dir.join(format!("{}.partial", member.file_name));
                staged.push((partial.clone(), final_path));
                extract_member(artifact, archive, member.zip_path, member.size, &partial)?;
                let (len, hash) = hash_file(&partial)?;
                if len != member.size || hash != member.sha256 {
                    return Err(InstallError::HashMismatch {
                        id: artifact.id,
                        got: hash,
                    });
                }
            }
            for (partial, final_path) in &staged {
                fs::rename(partial, final_path)?;
            }
            Ok(())
        })();
        for (partial, _) in &staged {
            let _ = fs::remove_file(partial);
        }
        result?;
        Ok(self.path(artifact))
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
    max_len: u64,
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
    let copied = io::copy(&mut (&mut entry).take(max_len + 1), &mut out)?;
    if copied > max_len {
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

    fn zip_with_files(files: &[(&str, &[u8])]) -> Vec<u8> {
        use zip::write::SimpleFileOptions;
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(io::Cursor::new(&mut buf));
            for (name, content) in files {
                w.start_file(*name, SimpleFileOptions::default()).unwrap();
                w.write_all(content).unwrap();
            }
            w.finish().unwrap();
        }
        buf
    }

    /// A multi-member artifact over `zip_bytes`, installing into `dir`; `members` are leaked so
    /// they are `'static` like the real manifest's.
    fn members_artifact(
        id: &'static str,
        dir: &'static str,
        zip_bytes: &[u8],
        members: Vec<Member>,
    ) -> Artifact {
        let members: &'static [Member] = Box::leak(members.into_boxed_slice());
        Artifact {
            id,
            label: "test members",
            file_name: members[0].file_name,
            url: "https://example.invalid/m.zip",
            download_size: zip_bytes.len() as u64,
            download_sha256: sha(zip_bytes),
            installed_size: members[0].size,
            installed_sha256: members[0].sha256,
            license: "test",
            payload: Payload::ZipMembers { dir, members },
        }
    }

    fn member(zip_path: &'static str, file_name: &'static str, content: &[u8]) -> Member {
        Member {
            zip_path,
            file_name,
            size: content.len() as u64,
            sha256: sha(content),
        }
    }

    #[test]
    fn a_multi_member_artifact_installs_every_member_and_needs_them_all() {
        let (a, b) = (vec![7u8; 3000], vec![9u8; 4000]);
        let zip_bytes = zip_with_files(&[
            ("pkg/bin/a.dll", &a),
            ("pkg/bin/b.dll", &b),
            ("pkg/x", b"no"),
        ]);
        let art = members_artifact(
            "multi",
            "shared-dir",
            &zip_bytes,
            vec![
                member("pkg/bin/a.dll", "a.dll", &a),
                member("pkg/bin/b.dll", "b.dll", &b),
            ],
        );
        let store = temp_store("multi");
        let path = store
            .install(&art, &Fake::new(zip_bytes), &mut |_| {}, &no_cancel())
            .unwrap();
        assert_eq!(path, store.dir(&art).join("a.dll"));
        assert_eq!(fs::read(store.dir(&art).join("a.dll")).unwrap(), a);
        assert_eq!(fs::read(store.dir(&art).join("b.dll")).unwrap(), b);
        assert_eq!(store.dir(&art), store.root().join("shared-dir"));
        // Only the two members remain: no archive, no partials, nothing from outside the list.
        assert_eq!(fs::read_dir(store.dir(&art)).unwrap().count(), 2);
        assert_eq!(store.status(&art), Status::Installed);
        assert!(store.verify(&art).unwrap());

        // Losing or corrupting any one member makes the whole artifact not-installed / unverified.
        let mut bad = b.clone();
        bad[0] ^= 1;
        fs::write(store.dir(&art).join("b.dll"), &bad).unwrap();
        assert_eq!(store.status(&art), Status::Installed, "same size");
        assert!(!store.verify(&art).unwrap());
        fs::remove_file(store.dir(&art).join("b.dll")).unwrap();
        assert_eq!(store.status(&art), Status::NotInstalled);
    }

    #[test]
    fn a_bad_member_installs_none_of_the_members() {
        let (a, b) = (vec![7u8; 3000], vec![9u8; 4000]);
        let zip_bytes = zip_with_files(&[("pkg/a.dll", &a), ("pkg/b.dll", &b)]);
        let mut wrong = member("pkg/b.dll", "b.dll", &b);
        wrong.sha256 = sha(b"not b");
        let art = members_artifact(
            "multi-bad",
            "multi-bad",
            &zip_bytes,
            vec![member("pkg/a.dll", "a.dll", &a), wrong],
        );
        let store = temp_store("multi-bad");
        let err = store
            .install(&art, &Fake::new(zip_bytes), &mut |_| {}, &no_cancel())
            .unwrap_err();
        assert!(matches!(err, InstallError::HashMismatch { .. }), "{err}");
        assert_eq!(store.status(&art), Status::NotInstalled);
        assert_eq!(
            fs::read_dir(store.dir(&art)).unwrap().count(),
            0,
            "the good member must not be left behind beside a rejected one"
        );
    }

    #[test]
    fn a_missing_archive_member_is_a_bad_archive_and_installs_nothing() {
        let a = vec![7u8; 100];
        let zip_bytes = zip_with_files(&[("pkg/a.dll", &a)]);
        let art = members_artifact(
            "multi-missing",
            "multi-missing",
            &zip_bytes,
            vec![
                member("pkg/a.dll", "a.dll", &a),
                member("pkg/gone.dll", "gone.dll", b"x"),
            ],
        );
        let store = temp_store("multi-missing");
        let err = store
            .install(&art, &Fake::new(zip_bytes), &mut |_| {}, &no_cancel())
            .unwrap_err();
        assert!(matches!(err, InstallError::BadArchive { .. }), "{err}");
        assert_eq!(fs::read_dir(store.dir(&art)).unwrap().count(), 0);
    }

    #[test]
    fn artifacts_can_share_one_directory() {
        let (a, b) = (vec![1u8; 500], vec![2u8; 600]);
        let (za, zb) = (
            zip_with_files(&[("a.dll", &a)]),
            zip_with_files(&[("b.dll", &b)]),
        );
        let first = members_artifact("first", "pack", &za, vec![member("a.dll", "a.dll", &a)]);
        let second = members_artifact("second", "pack", &zb, vec![member("b.dll", "b.dll", &b)]);
        let store = temp_store("shared");
        store
            .install(&first, &Fake::new(za), &mut |_| {}, &no_cancel())
            .unwrap();
        store
            .install(&second, &Fake::new(zb), &mut |_| {}, &no_cancel())
            .unwrap();
        assert_eq!(store.dir(&first), store.dir(&second));
        assert_eq!(fs::read_dir(store.dir(&first)).unwrap().count(), 2);
        // Removing one artifact leaves the other.
        store.remove(&first).unwrap();
        assert_eq!(store.status(&first), Status::NotInstalled);
        assert_eq!(store.status(&second), Status::Installed);
    }

    #[test]
    fn the_gpu_pack_manifest_is_well_formed() {
        let runtime = [&ORT_CUDA_RUNTIME, &CUDNN_RUNTIME, &CUBLAS_RUNTIME];
        let mut names = Vec::new();
        for a in runtime {
            assert_eq!(a.download_sha256.len(), 64, "{}", a.id);
            assert!(a.download_sha256.bytes().all(|b| b.is_ascii_hexdigit()));
            assert!(a.url.starts_with("https://"), "{}", a.id);
            let Payload::ZipMembers { dir, members } = a.payload else {
                panic!("{} must be a multi-member artifact", a.id);
            };
            assert_eq!(
                dir, GPU_RUNTIME_DIR,
                "{}: the pack shares one directory",
                a.id
            );
            assert!(!members.is_empty());
            for m in members {
                assert_eq!(m.sha256.len(), 64, "{}", m.file_name);
                assert!(m.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
                assert!(m.size > 0 && !m.zip_path.is_empty());
                assert!(m.zip_path.ends_with(m.file_name), "{}", m.file_name);
                names.push(m.file_name);
            }
            // The artifact's own file_name/installed_* describe one of its members exactly.
            let primary = members.iter().find(|m| m.file_name == a.file_name).unwrap();
            assert_eq!(
                (primary.size, primary.sha256),
                (a.installed_size, a.installed_sha256)
            );
            assert!(RUNTIME_IDS.contains(&a.id), "{} must be a runtime id", a.id);
        }
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            names.len(),
            total,
            "no two pack files may share a name: {names:?}"
        );
        // The loader the runtime is started from, and the provider it needs, are in the pack.
        assert!(names.contains(&"onnxruntime.dll"));
        assert!(names.contains(&"onnxruntime_providers_cuda.dll"));
        assert!(names.contains(&"cudnn64_9.dll"));
        assert!(names.contains(&"cublasLt64_13.dll"));
    }

    #[test]
    fn birefnet_fp16_is_the_same_revision_as_fp32_and_a_plain_file() {
        let revision = "/resolve/534d3c82d3bb8b2f0867db6dfbc3a525b8e42f67/onnx/";
        assert!(BIREFNET.url.contains(revision));
        assert!(BIREFNET_FP16.url.contains(revision));
        assert!(BIREFNET_FP16.url.ends_with("model_fp16.onnx"));
        assert_eq!(BIREFNET_FP16.payload, Payload::File);
        assert_eq!(
            BIREFNET_FP16.download_sha256,
            BIREFNET_FP16.installed_sha256
        );
        assert_ne!(BIREFNET_FP16.id, BIREFNET.id);
        assert_ne!(BIREFNET_FP16.file_name, BIREFNET.file_name);
    }

    #[test]
    fn removal_verifies_the_runtime_it_actually_loads() {
        let cpu = RemovalModels {
            ort_dylib: PathBuf::from("onnxruntime.dll"),
            gpu_runtime: false,
            sam_encoder: PathBuf::new(),
            sam_decoder: PathBuf::new(),
            lama: PathBuf::new(),
        };
        let ids: Vec<_> = cpu.artifacts().iter().map(|a| a.id).collect();
        assert!(ids.contains(&LAMA.id) && !ids.contains(&ORT_CUDA_RUNTIME.id));
        let gpu = RemovalModels {
            gpu_runtime: true,
            ..cpu
        };
        let ids: Vec<_> = gpu.artifacts().iter().map(|a| a.id).collect();
        for want in [
            ORT_CUDA_RUNTIME.id,
            CUDNN_RUNTIME.id,
            CUBLAS_RUNTIME.id,
            LAMA.id,
        ] {
            assert!(ids.contains(&want), "{want} must be verified: {ids:?}");
        }
        assert!(!ids.contains(&ORT_RUNTIME.id));
    }

    #[test]
    fn an_oversized_archive_member_is_rejected_by_its_pinned_size() {
        let real = vec![5u8; 2000];
        let zip_bytes = zip_with_files(&[("pkg/a.dll", &real)]);
        // The manifest pins 100 bytes; the archive's member is 2000.
        let mut pinned = member("pkg/a.dll", "a.dll", &real);
        pinned.size = 100;
        let art = members_artifact("multi-big", "multi-big", &zip_bytes, vec![pinned]);
        let store = temp_store("multi-big");
        let err = store
            .install(&art, &Fake::new(zip_bytes), &mut |_| {}, &no_cancel())
            .unwrap_err();
        assert!(matches!(err, InstallError::BadArchive { .. }), "{err}");
        assert_eq!(fs::read_dir(store.dir(&art)).unwrap().count(), 0);
    }

    #[test]
    fn a_stale_partial_from_a_killed_install_is_cleaned_up() {
        let a = vec![1u8; 300];
        let zip_bytes = zip_with_files(&[("a.dll", &a)]);
        let art = members_artifact(
            "multi-stale",
            "multi-stale",
            &zip_bytes,
            vec![member("a.dll", "a.dll", &a)],
        );
        let store = temp_store("multi-stale");
        fs::create_dir_all(store.dir(&art)).unwrap();
        fs::write(store.dir(&art).join("a.dll.partial"), vec![0u8; 999]).unwrap();
        store
            .install(&art, &Fake::new(zip_bytes), &mut |_| {}, &no_cancel())
            .unwrap();
        assert_eq!(fs::read_dir(store.dir(&art)).unwrap().count(), 1);
    }

    #[test]
    fn without_the_pack_the_for_store_lists_are_the_default_lists() {
        let store = temp_store("for-store");
        let ids = |v: Vec<&'static Artifact>| v.iter().map(|a| a.id).collect::<Vec<_>>();
        assert_eq!(ids(mask_artifacts_for(&store)), ids(mask_artifacts()));
        assert_eq!(ids(removal_artifacts_for(&store)), ids(removal_artifacts()));
        // Nothing installed: a repair re-checks the default sets (locate() is None).
        assert_eq!(ids(mask_repair_artifacts(&store)), ids(mask_artifacts()));
        assert_eq!(
            ids(removal_repair_artifacts(&store)),
            ids(removal_artifacts())
        );
    }

    #[test]
    fn the_gpu_pack_is_additive_and_never_part_of_the_default_downloads() {
        let default_ids: Vec<_> = mask_artifacts()
            .iter()
            .chain(removal_artifacts().iter())
            .map(|a| a.id)
            .collect();
        for a in gpu_pack_artifacts() {
            assert!(!default_ids.contains(&a.id), "{} must be opt-in", a.id);
        }
        let store = temp_store("gpu-size");
        assert!(gpu_pack_download_bytes(&store) >= BIREFNET_FP16.download_size);
    }

    #[test]
    fn the_gpu_runtime_needs_all_three_pieces_and_is_ignored_off_windows() {
        let store = temp_store("gpu-runtime");
        assert_eq!(store.gpu_runtime_path(), None);
        // Nothing installed: masks fall back to whatever the CPU path offers.
        assert_eq!(store.ort_runtime_path(), None);
    }

    #[test]
    fn mask_models_name_the_artifacts_they_were_built_from() {
        let cpu = MaskModels {
            ort_dylib: PathBuf::from("onnxruntime.dll"),
            birefnet: PathBuf::from("birefnet_fp32.onnx"),
            gpu_runtime: false,
            birefnet_gpu: None,
        };
        let ids: Vec<_> = cpu.artifacts().iter().map(|a| a.id).collect();
        assert!(ids.contains(&BIREFNET.id));
        assert!(!ids.contains(&BIREFNET_FP16.id));
        assert!(!ids.contains(&ORT_CUDA_RUNTIME.id));

        // The CUDA runtime in use but the fp16 model missing (an interrupted pack download): the
        // runtime that is loaded is still the one verified, never the CPU build's.
        let incomplete = MaskModels {
            gpu_runtime: true,
            ..cpu.clone()
        };
        let ids: Vec<_> = incomplete.artifacts().iter().map(|a| a.id).collect();
        assert!(ids.contains(&CUDNN_RUNTIME.id) && ids.contains(&ORT_CUDA_RUNTIME.id));
        assert!(!ids.contains(&ORT_RUNTIME.id) && !ids.contains(&BIREFNET_FP16.id));

        let gpu = MaskModels {
            gpu_runtime: true,
            birefnet_gpu: Some(PathBuf::from("birefnet_fp16.onnx")),
            ..cpu
        };
        let ids: Vec<_> = gpu.artifacts().iter().map(|a| a.id).collect();
        for want in [
            ORT_CUDA_RUNTIME.id,
            CUDNN_RUNTIME.id,
            CUBLAS_RUNTIME.id,
            BIREFNET.id,
            BIREFNET_FP16.id,
        ] {
            assert!(ids.contains(&want), "{want} missing from {ids:?}");
        }
        assert!(
            !ids.contains(&ORT_RUNTIME.id),
            "the CPU runtime isn't used with the GPU one"
        );
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
