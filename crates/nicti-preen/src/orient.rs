//! EXIF orientation (#57).
//!
//! LibRaw's decode is unrotated -- nothing in the render pipeline applies the file's IFD0
//! Orientation -- so without this a portrait shot would export sideways. Export rotates the
//! *finished, resized* buffer (cheap: it's the small one) and writes Orientation = 1. Crop
//! coordinates in the edit document are in sensor orientation (the same as the Develop view), so
//! this happens strictly after the render.

/// EXIF Orientation values 1..=8 (anything else is treated as 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Orientation {
    #[default]
    Normal,
    FlipH,
    Rotate180,
    FlipV,
    Transpose,
    Rotate90Cw,
    Transverse,
    Rotate270Cw,
}

impl Orientation {
    pub fn from_exif(value: u32) -> Self {
        match value {
            2 => Self::FlipH,
            3 => Self::Rotate180,
            4 => Self::FlipV,
            5 => Self::Transpose,
            6 => Self::Rotate90Cw,
            7 => Self::Transverse,
            8 => Self::Rotate270Cw,
            _ => Self::Normal,
        }
    }

    /// Whether width and height trade places.
    pub fn swaps_dimensions(self) -> bool {
        matches!(
            self,
            Self::Transpose | Self::Rotate90Cw | Self::Transverse | Self::Rotate270Cw
        )
    }

    /// The size after orienting a `w` x `h` image.
    pub fn oriented_size(self, w: u32, h: u32) -> (u32, u32) {
        if self.swaps_dimensions() {
            (h, w)
        } else {
            (w, h)
        }
    }
}

/// Reorients `src` (`w` x `h`, `channels` interleaved values per pixel) so it displays upright.
/// Returns the pixels and the new `(width, height)`.
pub fn apply<T: Copy>(
    src: &[T],
    w: u32,
    h: u32,
    channels: usize,
    orientation: Orientation,
) -> (Vec<T>, u32, u32) {
    debug_assert_eq!(src.len(), w as usize * h as usize * channels);
    if orientation == Orientation::Normal {
        return (src.to_vec(), w, h);
    }
    let (ow, oh) = orientation.oriented_size(w, h);
    let (wu, hu) = (w as usize, h as usize);
    let (owu, ohu) = (ow as usize, oh as usize);
    let mut out = Vec::with_capacity(src.len());
    for y in 0..ohu {
        for x in 0..owu {
            // Source coordinates feeding destination (x, y).
            let (sx, sy) = match orientation {
                Orientation::Normal => (x, y),
                Orientation::FlipH => (wu - 1 - x, y),
                Orientation::Rotate180 => (wu - 1 - x, hu - 1 - y),
                Orientation::FlipV => (x, hu - 1 - y),
                Orientation::Transpose => (y, x),
                Orientation::Rotate90Cw => (y, hu - 1 - x),
                Orientation::Transverse => (wu - 1 - y, hu - 1 - x),
                Orientation::Rotate270Cw => (wu - 1 - y, x),
            };
            let i = (sy * wu + sx) * channels;
            out.extend_from_slice(&src[i..i + channels]);
        }
    }
    (out, ow, oh)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 3 wide x 2 tall, one channel:  1 2 3
    //                                4 5 6
    const SRC: [u8; 6] = [1, 2, 3, 4, 5, 6];

    fn run(o: u32) -> (Vec<u8>, u32, u32) {
        apply(&SRC, 3, 2, 1, Orientation::from_exif(o))
    }

    #[test]
    fn all_eight_orientations() {
        assert_eq!(run(1), (vec![1, 2, 3, 4, 5, 6], 3, 2));
        assert_eq!(run(2), (vec![3, 2, 1, 6, 5, 4], 3, 2));
        assert_eq!(run(3), (vec![6, 5, 4, 3, 2, 1], 3, 2));
        assert_eq!(run(4), (vec![4, 5, 6, 1, 2, 3], 3, 2));
        // Transpose: rows become columns.
        assert_eq!(run(5), (vec![1, 4, 2, 5, 3, 6], 2, 3));
        // 90 CW: the left column (1,4) becomes the top row, read bottom-to-top.
        assert_eq!(run(6), (vec![4, 1, 5, 2, 6, 3], 2, 3));
        assert_eq!(run(7), (vec![6, 3, 5, 2, 4, 1], 2, 3));
        // 270 CW (90 CCW): the top row (1,2,3) becomes the left column, bottom-to-top.
        assert_eq!(run(8), (vec![3, 6, 2, 5, 1, 4], 2, 3));
    }

    #[test]
    fn rotating_cw_then_ccw_is_the_identity() {
        let (rot, w, h) = run(6);
        let (back, bw, bh) = apply(&rot, w, h, 1, Orientation::Rotate270Cw);
        assert_eq!((back, bw, bh), (SRC.to_vec(), 3, 2));
    }

    #[test]
    fn multi_channel_pixels_move_whole() {
        let src = [1u8, 10, 2, 20, 3, 30, 4, 40]; // 2x2, 2 channels
        let (out, w, h) = apply(&src, 2, 2, 2, Orientation::FlipH);
        assert_eq!((out, w, h), (vec![2, 20, 1, 10, 4, 40, 3, 30], 2, 2));
    }

    #[test]
    fn unknown_values_are_treated_as_normal_and_sizes_swap_correctly() {
        assert_eq!(Orientation::from_exif(0), Orientation::Normal);
        assert_eq!(Orientation::from_exif(9), Orientation::Normal);
        assert_eq!(
            Orientation::Rotate90Cw.oriented_size(6000, 4000),
            (4000, 6000)
        );
        assert_eq!(
            Orientation::Rotate180.oriented_size(6000, 4000),
            (6000, 4000)
        );
    }
}
