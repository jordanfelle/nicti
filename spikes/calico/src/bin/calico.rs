use std::fs;
use std::path::PathBuf;

use calico::dcp::DcpProfile;
use calico::deltae::{ciede2000, srgb8_to_xyz, xyz_to_lab};
use calico::linear_input;
use calico::pipeline::{render, RenderOptions};
use calico::tonecurve::ToneCurve;
use calico::workspace::WorkingSpace;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "calico", about = "Spike for #38: color pipeline research")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Renders `retina dump-linear`'s output pair to an 8-bit sRGB PNG using a DCP camera
    /// profile.
    Render {
        /// `<stem>.linear.tiff` from `retina dump-linear`.
        tiff: PathBuf,
        /// `<stem>.meta.json` from `retina dump-linear`.
        meta: PathBuf,
        #[arg(long)]
        dcp: PathBuf,
        /// Adobe Raw "Look" profile (e.g. Adobe Vivid) -- see xmp_profile.rs's research-risk
        /// note; omit to measure against the DCP's own base look (Adobe Standard/Color).
        #[arg(long)]
        look: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "prophoto")]
        space: WorkingSpace,
        #[arg(long)]
        out: PathBuf,
    },
    /// Reports mean/p95/max CIEDE2000 between two sRGB images (calico's own render vs. an LRC
    /// export), plus a false-color ΔE heatmap PNG.
    Compare {
        ours: PathBuf,
        reference: PathBuf,
        #[arg(long)]
        heatmap: Option<PathBuf>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Render {
            tiff,
            meta,
            dcp,
            look,
            space,
            out,
        } => cmd_render(&tiff, &meta, &dcp, look.as_deref(), space, &out),
        Command::Compare {
            ours,
            reference,
            heatmap,
        } => cmd_compare(&ours, &reference, heatmap.as_deref()),
    }
}

fn cmd_render(
    tiff: &std::path::Path,
    meta: &std::path::Path,
    dcp_path: &std::path::Path,
    look_path: Option<&std::path::Path>,
    space: WorkingSpace,
    out: &std::path::Path,
) -> anyhow::Result<()> {
    let input = linear_input::load(tiff, meta)?;
    let dcp_bytes = fs::read(dcp_path)?;
    let profile = DcpProfile::parse(&dcp_bytes)
        .map_err(|e| anyhow::anyhow!("parsing DCP {}: {e}", dcp_path.display()))?;

    let look = match look_path {
        Some(p) => {
            let text = fs::read_to_string(p)?;
            let look = calico::xmp_profile::parse(&text)
                .map_err(|e| anyhow::anyhow!("parsing look profile {}: {e}", p.display()))?;
            eprintln!("using look profile '{}'", look.name);
            if !look.unsupported_settings.is_empty() {
                eprintln!(
                    "warning: look profile '{}' has settings calico doesn't apply: {}",
                    look.name,
                    look.unsupported_settings.join(", ")
                );
            }
            Some((look.look_table, look.encoding))
        }
        None => None,
    };

    let tone_curve = match &profile.tone_curve_points {
        Some(points) => ToneCurve::new(points),
        None => ToneCurve::acr_default(),
    };

    let opts = RenderOptions {
        profile: &profile,
        look: look.as_ref().map(|(table, encoding)| (table, *encoding)),
        working_space: space,
        tone_curve: &tone_curve,
    };
    let image = render(&input, &opts);
    image.save(out)?;
    eprintln!("wrote {}", out.display());
    Ok(())
}

fn cmd_compare(
    ours_path: &std::path::Path,
    reference_path: &std::path::Path,
    heatmap_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let ours = image::open(ours_path)?.into_rgb8();
    let reference = image::open(reference_path)?.into_rgb8();
    anyhow::ensure!(
        ours.dimensions() == reference.dimensions(),
        "dimension mismatch: ours {:?} vs reference {:?}",
        ours.dimensions(),
        reference.dimensions()
    );

    // 1/4 resolution: the demosaic stand-in's edge differences shouldn't dominate the
    // comparison, per ADR-0038's decision rule.
    let step = 4u32;
    let mut diffs = Vec::new();
    let mut heatmap =
        image::RgbImage::new(ours.width().div_ceil(step), ours.height().div_ceil(step));

    for y in (0..ours.height()).step_by(step as usize) {
        for x in (0..ours.width()).step_by(step as usize) {
            let a = ours.get_pixel(x, y).0;
            let b = reference.get_pixel(x, y).0;
            let lab_a = xyz_to_lab(srgb8_to_xyz(a));
            let lab_b = xyz_to_lab(srgb8_to_xyz(b));
            let de = ciede2000(lab_a, lab_b);
            diffs.push(de);
            if heatmap_path.is_some() {
                let color = heatmap_color(de);
                heatmap.put_pixel(x / step, y / step, image::Rgb(color));
            }
        }
    }

    diffs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean: f64 = diffs.iter().sum::<f64>() / diffs.len() as f64;
    let p95 = diffs[(diffs.len() as f64 * 0.95) as usize];
    let max = *diffs.last().unwrap();

    println!("mean dE00: {mean:.4}");
    println!("p95  dE00: {p95:.4}");
    println!("max  dE00: {max:.4}");

    if let Some(path) = heatmap_path {
        heatmap.save(path)?;
        eprintln!("wrote heatmap {}", path.display());
    }
    Ok(())
}

/// Blue (0) -> green (~2) -> yellow (~5) -> red (10+), for `--heatmap`.
fn heatmap_color(de: f64) -> [u8; 3] {
    let t = (de / 10.0).clamp(0.0, 1.0);
    let r = (t * 255.0) as u8;
    let g = ((1.0 - (t - 0.5).abs() * 2.0).clamp(0.0, 1.0) * 255.0) as u8;
    let b = ((1.0 - t) * 255.0) as u8;
    [r, g, b]
}
