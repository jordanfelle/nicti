//! Classic (non-AI) sky-mask heuristic -- the no-model fallback `docs/adr/0024-masking.md` records
//! for sky segmentation if no license-clear model checks out (see `docs/research/siamese-masking.md`
//! for the model survey). Luminance/blue-ratio threshold, seeded from the top row and grown by
//! flood fill so a bright non-sky region (e.g. a white wall) lower in the frame isn't picked up
//! just because it passes the color test in isolation -- sky is defined by "connected to the top
//! of the frame", not "looks blue-ish" alone.

use crate::image::{Field, Image};

/// Per-pixel "looks like sky" test: bright and blue-dominant relative to red. Deliberately simple
/// (RapidRAW's own sky variant is a trained U-2-Net model, see the research doc) -- this is the
/// baseline to fall back to, not a claim of matching that quality.
fn looks_like_sky(px: [f32; 4]) -> bool {
    let [r, g, b, _] = px;
    let luminance = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let blue_dominant = b > r * 1.02 && b >= g * 0.95;
    luminance > 0.35 && blue_dominant
}

/// Flood-fills from every top-row pixel that passes `looks_like_sky`, 4-connected, only ever
/// visiting other pixels that also pass the test -- this is what keeps an unrelated bright/blue
/// region elsewhere in the frame (that isn't reachable from the top edge through other sky-like
/// pixels) out of the mask.
pub fn sky_mask(image: &Image) -> Field {
    let (width, height) = (image.width, image.height);
    let mut visited = vec![false; width * height];
    let mut out = Field::new(width, height, 0.0);
    let mut stack = Vec::new();

    for x in 0..width {
        if looks_like_sky(image.get(x as i32, 0)) {
            stack.push((x, 0usize));
        }
    }

    while let Some((x, y)) = stack.pop() {
        let i = y * width + x;
        if visited[i] {
            continue;
        }
        visited[i] = true;
        out.data[i] = 1.0;

        let neighbors = [
            (x.wrapping_sub(1), y),
            (x + 1, y),
            (x, y.wrapping_sub(1)),
            (x, y + 1),
        ];
        for (nx, ny) in neighbors {
            if nx < width && ny < height && !visited[ny * width + nx] {
                let npx = image.get(nx as i32, ny as i32);
                if looks_like_sky(npx) {
                    stack.push((nx, ny));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: usize, height: usize, rgb: [f32; 3]) -> Image {
        Image::new(width, height, [rgb[0], rgb[1], rgb[2], 1.0])
    }

    #[test]
    fn a_uniformly_blue_image_is_entirely_sky() {
        let image = solid(8, 8, [0.4, 0.5, 0.6]);
        let mask = sky_mask(&image);
        assert!(mask.data.iter().all(|&v| v == 1.0));
    }

    #[test]
    fn a_uniformly_brown_image_has_no_sky() {
        let image = solid(8, 8, [0.5, 0.3, 0.1]);
        let mask = sky_mask(&image);
        assert!(mask.data.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn sky_only_at_the_top_does_not_leak_into_a_disconnected_bright_patch_below() {
        // Ground (brown) fills the whole frame except a sky strip at the top and an unconnected
        // bright-blue patch near the bottom that a naive per-pixel color test would also match.
        let width = 10;
        let height = 10;
        let mut image = Image::new(width, height, [0.5, 0.3, 0.1, 1.0]);
        for x in 0..width {
            image.set(x as i32, 0, [0.4, 0.5, 0.6, 1.0]);
            image.set(x as i32, 1, [0.4, 0.5, 0.6, 1.0]);
        }
        // Disconnected patch, separated from the sky strip by ground rows.
        image.set(3, 8, [0.4, 0.5, 0.6, 1.0]);
        image.set(4, 8, [0.4, 0.5, 0.6, 1.0]);

        let mask = sky_mask(&image);
        assert_eq!(mask.get(3, 0), 1.0);
        assert_eq!(
            mask.get(3, 8),
            0.0,
            "disconnected patch must not be picked up"
        );
    }
}
