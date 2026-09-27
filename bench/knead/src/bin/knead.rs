//! CLI for `bench/knead`, #45's real-NEF golden/perf harness. See the crate's own `lib.rs` doc
//! comment for the pipeline this drives. Every subcommand resolves the ref-10k root the same way
//! `prowl` does (`--ref-root`, falling back to `NICTI_REF10K`) and refuses to run against an
//! unverified copy, per docs/benchmarks.md's rule -- `nicti_prowl::perf::Protocol::run_verified`
//! is the actual enforcement point for the `bench` subcommands; `golden` checks the same manifest
//! entry directly since it isn't itself a `Protocol::run` caller.

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use knead::RealRender;
use nicti_prowl::golden::{bless_requested, CompareOutcome, CompareRequest, GoldenStore};
use nicti_prowl::manifest::{Manifest, Scope};
use nicti_prowl::perf::Protocol;
use nicti_prowl::refset;
use nicti_render::frame::Extent;

#[derive(Parser)]
#[command(
    name = "knead",
    about = "Real-NEF golden/perf harness for #45's Tapetum pipeline"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Renders one ref-10k NEF end-to-end and compares it against (or blesses) its stored golden.
    Golden {
        /// A ref-10k manifest id (e.g. ref-00001.nef).
        #[arg(long)]
        ref_id: String,
        /// Root directory holding the ref-10k files. Falls back to NICTI_REF10K.
        #[arg(long)]
        ref_root: Option<PathBuf>,
        #[arg(long, default_value = "docs/ref-10k-manifest.csv")]
        manifest: PathBuf,
        #[arg(long, default_value = "bench/knead/goldens")]
        golden_dir: PathBuf,
        /// SSIM threshold below which a render is flagged as diverged.
        #[arg(long, default_value_t = 0.98)]
        threshold: f64,
    },
    /// Times the fused live-suffix dispatch alone (decode runs once, unmeasured, to prepare it).
    BenchLiveSuffix {
        #[arg(long)]
        ref_id: String,
        #[arg(long)]
        ref_root: Option<PathBuf>,
        #[arg(long, default_value = "docs/ref-10k-manifest.csv")]
        manifest: PathBuf,
    },
    /// Times the crop/present-sample dispatch alone.
    BenchPresent {
        #[arg(long)]
        ref_id: String,
        #[arg(long)]
        ref_root: Option<PathBuf>,
        #[arg(long, default_value = "docs/ref-10k-manifest.csv")]
        manifest: PathBuf,
    },
    /// Times one `TiledRender::step` (one padded tile's geometry pass + readback) at a given
    /// tile dimension, reporting p50/p95 across repeated calls.
    BenchTile {
        #[arg(long)]
        ref_id: String,
        #[arg(long)]
        ref_root: Option<PathBuf>,
        #[arg(long, default_value = "docs/ref-10k-manifest.csv")]
        manifest: PathBuf,
        #[arg(long, default_value_t = 2048)]
        tile_dim: u32,
    },
}

/// Resolves `ref_id`'s real file path and verifies it against the manifest before returning it --
/// every subcommand's shared "don't benchmark or golden-test an unverified copy" gate.
fn verified_source_path(
    manifest_path: &Path,
    ref_root: Option<&Path>,
    ref_id: &str,
) -> anyhow::Result<PathBuf> {
    let manifest = Manifest::load(manifest_path)?;
    let root = refset::resolve_root(ref_root)?;
    let report = manifest.verify(&root, Scope::Ids(vec![ref_id.to_string()]));
    report.into_result()?;
    Ok(root.join(ref_id))
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Golden {
            ref_id,
            ref_root,
            manifest,
            golden_dir,
            threshold,
        } => {
            let manifest_data = Manifest::load(&manifest)?;
            let entry = manifest_data
                .get(&ref_id)
                .ok_or_else(|| anyhow::anyhow!("{ref_id} not found in {}", manifest.display()))?;
            let source_path = verified_source_path(&manifest, ref_root.as_deref(), &ref_id)?;

            let store = GoldenStore::new(&golden_dir);
            let outcome = store.compare(
                &knead::KneadRenderer,
                &CompareRequest {
                    render_name: "knead-neutral",
                    ref_id: &ref_id,
                    source_path: &source_path,
                    source_sha256: &entry.sha256,
                    threshold,
                    bless: bless_requested(),
                },
            )?;
            match outcome {
                CompareOutcome::Blessed => println!("blessed golden for {ref_id}"),
                CompareOutcome::Matched { score } => println!("matched, score={score:.4}"),
                CompareOutcome::Diverged { score, threshold } => {
                    anyhow::bail!("diverged: score={score:.4} < threshold={threshold:.4}")
                }
            }
            Ok(())
        }
        Command::BenchLiveSuffix {
            ref_id,
            ref_root,
            manifest,
        } => {
            let source_path = verified_source_path(&manifest, ref_root.as_deref(), &ref_id)?;
            let render = RealRender::decode(&source_path)?;
            let decoded = render.render_baked()?; // the live suffix's real predecessor
            let output = nicti_render::frame::FrameTexture::new(&render.gpu, render.extent);
            let protocol = Protocol::default();
            let stats = protocol.run(|| {
                let mut encoder =
                    render
                        .gpu
                        .device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                            label: Some("bench live_suffix"),
                        });
                nicti_render::renderer::LiveExec::encode(
                    &render.live_kernel,
                    &render.gpu,
                    &mut encoder,
                    &decoded,
                    &output,
                );
                render.gpu.queue.submit(Some(encoder.finish()));
                render
                    .gpu
                    .device
                    .poll(wgpu::PollType::wait_indefinitely())
                    .unwrap();
            });
            println!("{}", serde_json::to_string_pretty(&stats)?);
            Ok(())
        }
        Command::BenchPresent {
            ref_id,
            ref_root,
            manifest,
        } => {
            let source_path = verified_source_path(&manifest, ref_root.as_deref(), &ref_id)?;
            let render = RealRender::decode(&source_path)?;
            let decoded = render.render_live()?; // the crop/present's real predecessor
            let output = nicti_render::frame::FrameTexture::new(&render.gpu, render.extent);
            let protocol = Protocol::default();
            let stats = protocol.run(|| {
                let mut encoder =
                    render
                        .gpu
                        .device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                            label: Some("bench present"),
                        });
                nicti_render::renderer::GeometryExec::encode(
                    &render.crop_kernel,
                    &render.gpu,
                    &mut encoder,
                    &decoded,
                    &output,
                );
                render.gpu.queue.submit(Some(encoder.finish()));
                render
                    .gpu
                    .device
                    .poll(wgpu::PollType::wait_indefinitely())
                    .unwrap();
            });
            println!("{}", serde_json::to_string_pretty(&stats)?);
            Ok(())
        }
        Command::BenchTile {
            ref_id,
            ref_root,
            manifest,
            tile_dim,
        } => {
            let source_path = verified_source_path(&manifest, ref_root.as_deref(), &ref_id)?;
            let render = RealRender::decode(&source_path)?;
            let decoded = render.render_live()?; // the crop/present's real predecessor

            let roi = nicti_render::tile::Rect {
                x: 0,
                y: 0,
                width: render.extent.width,
                height: render.extent.height,
            };
            let budget = nicti_render::tile::TileBudget {
                max_dim: tile_dim,
                max_staging_bytes: u64::MAX,
                target_chunk_ms: 8.0,
            };
            let tiles = nicti_render::tile::TilePlanner::plan(render.extent, roi, 1, budget);
            println!("planned {} tiles at dim={tile_dim}", tiles.len());
            // Times a single representative tile (the first one plan() produces, which always
            // starts at (0,0)) repeatedly, rather than cycling through every planned tile --
            // p50/p95 across many renders of the *same* tile is what ADR-0054's per-chunk target
            // is actually about; different tiles' cost only varies with edge-clamping, not in a
            // way worth conflating into one perf number here.
            let first_tile = tiles[0];
            let sink_extent = Extent {
                width: first_tile.core.width,
                height: first_tile.core.height,
            };

            let protocol = Protocol::default();
            let stats = protocol.run(|| {
                let mut tiled = nicti_render::tile::TiledRender::new(
                    std::sync::Arc::clone(&render.gpu),
                    &decoded,
                    &render.crop_kernel,
                    nicti_render::geometry::Affine2D::IDENTITY,
                    vec![first_tile],
                );
                let mut sink = nicti_render::tile::MemorySink::new(sink_extent);
                tiled.step(&mut sink);
            });
            println!("{}", serde_json::to_string_pretty(&stats)?);
            Ok(())
        }
    }
}
