//! MobileSAM click/box segmentation over `ort`/`load-dynamic`, against the **real** tensor contract
//! of the ONNX export in `nicti_stalk::models` (read from the model files, not assumed):
//!
//! - encoder `input_image` `float32 [H, W, 3]`, raw 0..255 RGB, longest side already resized to
//!   1024 (the graph normalizes and pads it) -> `image_embeddings [1, 256, 64, 64]`;
//! - decoder `image_embeddings`, `point_coords [1, N, 2]` (in the resized frame), `point_labels
//!   [1, N]`, `mask_input [1, 1, 256, 256]`, `has_mask_input [1]`, `orig_im_size [2]` (H, W) ->
//!   `masks` (logits at `orig_im_size`), `iou_predictions`, `low_res_masks`.
//!
//! The encoder is the expensive half and depends only on the image, so [`MobileSam`] caches the
//! embedding by image key: a second click on the same photo re-runs just the cheap decoder
//! (ADR-0048's "embedding bakes once" point).

use std::path::Path;

use ort::session::Session;
use ort::value::Tensor;
use serde_json::{json, Value};

use crate::space::SpaceMap;
use crate::{ort_err, PixelSource, RemovalError};

/// SAM's working resolution: the longest side of the frame the encoder sees.
pub const MODEL_SIZE: usize = 1024;

/// A click or box prompt. In *source-image pixels* unless a function says it is scaled.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Prompt {
    Click { x: f32, y: f32 },
    Box { x0: f32, y0: f32, x1: f32, y1: f32 },
}

impl Prompt {
    /// The JSON stored in a `MaskRecipe::params` (the recipe, not the pixels -- ADR-0021).
    pub fn to_json(&self) -> Value {
        match *self {
            Prompt::Click { x, y } => json!({ "click": [x, y] }),
            Prompt::Box { x0, y0, x1, y1 } => json!({ "box": [x0, y0, x1, y1] }),
        }
    }

    /// Parses [`Self::to_json`]'s output; `None` for anything else (including non-finite numbers).
    pub fn from_json(v: &Value) -> Option<Self> {
        let nums = |key: &str, n: usize| -> Option<Vec<f32>> {
            let arr = v.get(key)?.as_array()?;
            if arr.len() != n {
                return None;
            }
            let out: Vec<f32> = arr
                .iter()
                .filter_map(|x| x.as_f64().map(|f| f as f32))
                .collect();
            (out.len() == n && out.iter().all(|f| f.is_finite())).then_some(out)
        };
        if let Some(c) = nums("click", 2) {
            return Some(Prompt::Click { x: c[0], y: c[1] });
        }
        nums("box", 4).map(|b| Prompt::Box {
            x0: b[0].min(b[2]),
            y0: b[1].min(b[3]),
            x1: b[0].max(b[2]),
            y1: b[1].max(b[3]),
        })
    }

    /// The prompt with every coordinate multiplied per-axis (source pixels -> model frame).
    pub fn scaled(&self, sx: f32, sy: f32) -> Prompt {
        match *self {
            Prompt::Click { x, y } => Prompt::Click {
                x: x * sx,
                y: y * sy,
            },
            Prompt::Box { x0, y0, x1, y1 } => Prompt::Box {
                x0: x0 * sx,
                y0: y0 * sy,
                x1: x1 * sx,
                y1: y1 * sy,
            },
        }
    }
}

/// The image as the encoder wants it: the whole photo shrunk so its longest side is
/// [`MODEL_SIZE`], display-referred, 0..255, interleaved `HWC`.
pub struct ModelFrame {
    pub width: usize,
    pub height: usize,
    /// Model-frame pixels per source pixel, per axis.
    pub scale_x: f32,
    pub scale_y: f32,
    pub rgb255: Vec<f32>,
}

impl ModelFrame {
    /// Box-filters `source` down (averaging in linear camera space, then mapping through `space`)
    /// so no detail aliases into the mask. Reads every source pixel once.
    pub fn build(source: &dyn PixelSource, space: &SpaceMap) -> Self {
        let (w, h) = (source.width() as usize, source.height() as usize);
        let s = MODEL_SIZE as f32 / w.max(h).max(1) as f32;
        let mw = ((w as f32 * s).round() as usize).clamp(1, MODEL_SIZE);
        let mh = ((h as f32 * s).round() as usize).clamp(1, MODEL_SIZE);
        let (scale_x, scale_y) = (mw as f32 / w as f32, mh as f32 / h as f32);
        let mut rgb255 = Vec::with_capacity(mw * mh * 3);
        for oy in 0..mh {
            let y0 = ((oy as f32 / scale_y).floor() as usize).min(h - 1);
            let y1 = (((oy + 1) as f32 / scale_y).ceil() as usize).clamp(y0 + 1, h);
            for ox in 0..mw {
                let x0 = ((ox as f32 / scale_x).floor() as usize).min(w - 1);
                let x1 = (((ox + 1) as f32 / scale_x).ceil() as usize).clamp(x0 + 1, w);
                let mut acc = [0.0f32; 3];
                for y in y0..y1 {
                    for x in x0..x1 {
                        let p = source.pixel(x as u32, y as u32);
                        for c in 0..3 {
                            acc[c] += p[c];
                        }
                    }
                }
                let n = ((y1 - y0) * (x1 - x0)) as f32;
                let m = space.to_model([acc[0] / n, acc[1] / n, acc[2] / n]);
                rgb255.extend(m.map(|c| c * 255.0));
            }
        }
        Self {
            width: mw,
            height: mh,
            scale_x,
            scale_y,
            rgb255,
        }
    }
}

/// A mask as raw logits at the model frame's size: `> 0` is foreground.
pub struct SegMask {
    pub width: usize,
    pub height: usize,
    pub logits: Vec<f32>,
}

/// Something that turns a prompt into an object mask. `key` identifies the image (and so the
/// cacheable embedding); `prompt` is already in model-frame coordinates.
pub trait Segmenter {
    fn segment(
        &mut self,
        key: u64,
        frame: &ModelFrame,
        prompt: Prompt,
    ) -> Result<SegMask, RemovalError>;
}

pub struct MobileSam {
    encoder: Session,
    decoder: Session,
    embedding: Option<(u64, Vec<f32>)>,
}

fn load_session(model: &Path, dylib: &Path) -> Result<Session, RemovalError> {
    if !model.is_file() {
        return Err(RemovalError::ModelNotFound(model.to_path_buf()));
    }
    nicti_haw::ensure_ort_environment(dylib).map_err(ort_err)?;
    Session::builder()
        .map_err(ort_err)?
        .commit_from_file(model)
        .map_err(ort_err)
}

impl MobileSam {
    pub fn load(encoder: &Path, decoder: &Path, ort_dylib: &Path) -> Result<Self, RemovalError> {
        Ok(Self {
            encoder: load_session(encoder, ort_dylib)?,
            decoder: load_session(decoder, ort_dylib)?,
            embedding: None,
        })
    }

    fn encode(&mut self, frame: &ModelFrame) -> Result<Vec<f32>, RemovalError> {
        let tensor =
            Tensor::from_array(([frame.height, frame.width, 3usize], frame.rgb255.clone()))
                .map_err(ort_err)?;
        let outputs = self
            .encoder
            .run(ort::inputs!["input_image" => tensor])
            .map_err(ort_err)?;
        let (_shape, data) = outputs["image_embeddings"]
            .try_extract_tensor::<f32>()
            .map_err(ort_err)?;
        if data.len() != 256 * 64 * 64 {
            return Err(RemovalError::Ort(format!(
                "encoder returned {} embedding values, expected {}",
                data.len(),
                256 * 64 * 64
            )));
        }
        Ok(data.to_vec())
    }
}

/// Coordinates and labels for the decoder. A click is padded with a `(0, 0)` label `-1` point (the
/// export's convention for "no box"); a box is its two corners with labels 2 and 3 and no padding.
fn decoder_points(prompt: Prompt) -> (Vec<f32>, Vec<f32>) {
    match prompt {
        Prompt::Click { x, y } => (vec![x, y, 0.0, 0.0], vec![1.0, -1.0]),
        Prompt::Box { x0, y0, x1, y1 } => (vec![x0, y0, x1, y1], vec![2.0, 3.0]),
    }
}

impl Segmenter for MobileSam {
    fn segment(
        &mut self,
        key: u64,
        frame: &ModelFrame,
        prompt: Prompt,
    ) -> Result<SegMask, RemovalError> {
        if self.embedding.as_ref().is_none_or(|(k, _)| *k != key) {
            let embedding = self.encode(frame)?;
            self.embedding = Some((key, embedding));
        }
        let embedding = &self.embedding.as_ref().expect("just ensured").1;

        let (coords, labels) = decoder_points(prompt);
        let n = labels.len();
        let outputs = self
            .decoder
            .run(ort::inputs![
                "image_embeddings" => Tensor::from_array(([1usize, 256, 64, 64], embedding.clone())).map_err(ort_err)?,
                "point_coords" => Tensor::from_array(([1usize, n, 2], coords)).map_err(ort_err)?,
                "point_labels" => Tensor::from_array(([1usize, n], labels)).map_err(ort_err)?,
                "mask_input" => Tensor::from_array(([1usize, 1, 256, 256], vec![0.0f32; 256 * 256])).map_err(ort_err)?,
                "has_mask_input" => Tensor::from_array(([1usize], vec![0.0f32])).map_err(ort_err)?,
                "orig_im_size" => Tensor::from_array(([2usize], vec![frame.height as f32, frame.width as f32])).map_err(ort_err)?,
            ])
            .map_err(ort_err)?;
        let (_shape, data) = outputs["masks"]
            .try_extract_tensor::<f32>()
            .map_err(ort_err)?;
        if data.len() != frame.width * frame.height {
            return Err(RemovalError::Ort(format!(
                "decoder returned {} mask values, expected {}x{}",
                data.len(),
                frame.width,
                frame.height
            )));
        }
        Ok(SegMask {
            width: frame.width,
            height: frame.height,
            logits: data.to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RgbBuffer;

    #[test]
    fn prompt_json_round_trips() {
        for p in [
            Prompt::Click { x: 12.5, y: 40.0 },
            Prompt::Box {
                x0: 1.0,
                y0: 2.0,
                x1: 30.0,
                y1: 40.0,
            },
        ] {
            assert_eq!(Prompt::from_json(&p.to_json()), Some(p));
        }
    }

    #[test]
    fn prompt_json_rejects_garbage_and_normalizes_box_corners() {
        for bad in [
            json!({}),
            json!({ "click": [1.0] }),
            json!({ "click": "x" }),
            json!({ "click": [1.0, null] }),
            json!({ "box": [1, 2, 3] }),
            json!(null),
        ] {
            assert_eq!(Prompt::from_json(&bad), None, "{bad}");
        }
        assert_eq!(
            Prompt::from_json(&json!({ "box": [30, 40, 1, 2] })),
            Some(Prompt::Box {
                x0: 1.0,
                y0: 2.0,
                x1: 30.0,
                y1: 40.0
            })
        );
    }

    #[test]
    fn decoder_points_follow_the_export_conventions() {
        let (c, l) = decoder_points(Prompt::Click { x: 5.0, y: 6.0 });
        assert_eq!((c, l), (vec![5.0, 6.0, 0.0, 0.0], vec![1.0, -1.0]));
        let (c, l) = decoder_points(Prompt::Box {
            x0: 1.0,
            y0: 2.0,
            x1: 3.0,
            y1: 4.0,
        });
        assert_eq!((c, l), (vec![1.0, 2.0, 3.0, 4.0], vec![2.0, 3.0]));
    }

    #[test]
    fn prompt_scaling_is_per_axis() {
        assert_eq!(
            Prompt::Click { x: 10.0, y: 20.0 }.scaled(0.5, 0.25),
            Prompt::Click { x: 5.0, y: 5.0 }
        );
    }

    fn gray(w: u32, h: u32, v: f32) -> RgbBuffer {
        RgbBuffer {
            width: w,
            height: h,
            data: vec![[v, v, v]; (w * h) as usize],
        }
    }

    #[test]
    fn model_frame_fits_the_longest_side_to_1024_and_keeps_aspect() {
        let img = gray(4000, 2000, 0.4);
        let space = SpaceMap::for_source([1.0; 4], &img);
        let f = ModelFrame::build(&img, &space);
        assert_eq!((f.width, f.height), (1024, 512));
        assert_eq!(f.rgb255.len(), 1024 * 512 * 3);
        assert!((f.scale_x - 0.256).abs() < 1e-6 && (f.scale_y - 0.256).abs() < 1e-6);
        assert!(f.rgb255.iter().all(|v| (0.0..=255.0).contains(v)));
    }

    #[test]
    fn model_frame_upscales_a_small_image_to_1024() {
        let img = gray(64, 32, 0.4);
        let space = SpaceMap::for_source([1.0; 4], &img);
        let f = ModelFrame::build(&img, &space);
        assert_eq!((f.width, f.height), (1024, 512));
    }

    #[test]
    fn model_frame_box_filters_instead_of_aliasing() {
        // Alternating 0/1 columns: a nearest-neighbour shrink would return pure 0 or pure 1; a box
        // filter averages to mid-gray in linear light.
        let mut img = gray(2048, 2, 0.0);
        for y in 0..2 {
            for x in (0..2048).step_by(2) {
                img.data[y * 2048 + x] = [1.0; 3];
            }
        }
        let space = SpaceMap {
            gain: [1.0; 3],
            scale: 1.0,
        };
        let f = ModelFrame::build(&img, &space);
        let mid = f.rgb255[(f.width / 2) * 3];
        // 0.5 linear encodes to ~0.735 in sRGB, i.e. ~187.5 of 255; a nearest-neighbour shrink
        // would land on 0 or 255.
        assert!((mid - 187.5).abs() < 2.0, "got {mid}");
    }

    #[test]
    fn missing_model_files_are_a_clean_error_not_a_panic() {
        let r = MobileSam::load(
            Path::new("/nonexistent/enc.onnx"),
            Path::new("/nonexistent/dec.onnx"),
            Path::new("/nonexistent/libonnxruntime.so"),
        );
        assert!(matches!(r, Err(RemovalError::ModelNotFound(_))));
    }
}
