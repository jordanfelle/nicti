//! How the loaded photo's EXIF orientation turns the on-screen image (#341).
//!
//! The decode, the edit document (crop, straighten, heal spots, masks) and every render stage are
//! in **sensor** orientation, and export rotates the finished, resized buffer at the end
//! (`nicti_preen::orient`). The screen does the same thing the cheap way: the frame texture stays
//! as rendered and the display shader samples it through a ±1 axis swap/flip, while every
//! point-mapping helper (fit, zoom, pan, and the crop/heal/mask overlays' screen<->image
//! conversions) goes through this one type. [`ViewOrient::normal`] is the identity, bit for bit, so
//! a photo with no rotation behaves exactly as before.
//!
//! "Display" space is the rotated image as the user sees it; "sensor" space is the texture. Both
//! are in `[0, 1]` normalised coordinates, y down. The mapping is pinned to
//! `nicti_preen::orient::apply` (what export does) by a test over all eight orientations, so the
//! screen and the exported file cannot disagree.

use nicti_preen::orient::Orientation;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ViewOrient(pub Orientation);

impl ViewOrient {
    pub fn normal() -> Self {
        Self(Orientation::Normal)
    }

    /// From a decoded frame's EXIF value (`0` = none, anything outside 1-8 = none).
    pub fn from_exif(value: u8) -> Self {
        Self(Orientation::from_exif(u32::from(value)))
    }

    #[cfg_attr(not(test), allow(dead_code))] // used by the Develop tools' follow-up
    pub fn is_normal(self) -> bool {
        self.0 == Orientation::Normal
    }

    /// The shader's orientation index: `Orientation`'s declaration order, 0 = Normal ... 7.
    pub fn index(self) -> u32 {
        match self.0 {
            Orientation::Normal => 0,
            Orientation::FlipH => 1,
            Orientation::Rotate180 => 2,
            Orientation::FlipV => 3,
            Orientation::Transpose => 4,
            Orientation::Rotate90Cw => 5,
            Orientation::Transverse => 6,
            Orientation::Rotate270Cw => 7,
        }
    }

    /// The size of a `sensor` -sized image once displayed (width and height trade places for the
    /// 90-degree family).
    pub fn display_size(self, sensor: (f32, f32)) -> (f32, f32) {
        if self.0.swaps_dimensions() {
            (sensor.1, sensor.0)
        } else {
            sensor
        }
    }

    /// A display-space point (normalised) -> the sensor-space point it shows.
    #[cfg_attr(not(test), allow(dead_code))] // used by the Develop tools' follow-up
    pub fn to_sensor(self, (u, v): (f32, f32)) -> (f32, f32) {
        match self.0 {
            Orientation::Normal => (u, v),
            Orientation::FlipH => (1.0 - u, v),
            Orientation::Rotate180 => (1.0 - u, 1.0 - v),
            Orientation::FlipV => (u, 1.0 - v),
            Orientation::Transpose => (v, u),
            Orientation::Rotate90Cw => (v, 1.0 - u),
            Orientation::Transverse => (1.0 - v, 1.0 - u),
            Orientation::Rotate270Cw => (1.0 - v, u),
        }
    }

    /// A sensor-space point (normalised) -> where it appears in the displayed image.
    #[cfg_attr(not(test), allow(dead_code))] // used by the Develop tools' follow-up
    pub fn to_display(self, (su, sv): (f32, f32)) -> (f32, f32) {
        match self.0 {
            Orientation::Normal => (su, sv),
            Orientation::FlipH => (1.0 - su, sv),
            Orientation::Rotate180 => (1.0 - su, 1.0 - sv),
            Orientation::FlipV => (su, 1.0 - sv),
            Orientation::Transpose => (sv, su),
            Orientation::Rotate90Cw => (1.0 - sv, su),
            Orientation::Transverse => (1.0 - sv, 1.0 - su),
            Orientation::Rotate270Cw => (sv, 1.0 - su),
        }
    }

    /// Screen point inside `rect` (the rect the *displayed* image fills) -> source-image pixels.
    /// `source` is the sensor-orientation frame size. For [`Self::normal`] this is exactly the
    /// plain stretch `screen_to_image` always did.
    #[cfg_attr(not(test), allow(dead_code))] // used by the Develop tools' follow-up
    pub fn screen_to_image(
        self,
        rect: egui::Rect,
        source: (f32, f32),
        p: egui::Pos2,
    ) -> (f32, f32) {
        let u = (p.x - rect.left()) / rect.width().max(1.0);
        let v = (p.y - rect.top()) / rect.height().max(1.0);
        let (su, sv) = self.to_sensor((u, v));
        (su * source.0, sv * source.1)
    }

    /// The inverse of [`Self::screen_to_image`].
    #[cfg_attr(not(test), allow(dead_code))] // used by the Develop tools' follow-up
    pub fn image_to_screen(
        self,
        rect: egui::Rect,
        source: (f32, f32),
        p: (f32, f32),
    ) -> egui::Pos2 {
        let (u, v) = self.to_display((p.0 / source.0.max(1.0), p.1 / source.1.max(1.0)));
        egui::pos2(
            rect.left() + u * rect.width(),
            rect.top() + v * rect.height(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Orientation; 8] = [
        Orientation::Normal,
        Orientation::FlipH,
        Orientation::Rotate180,
        Orientation::FlipV,
        Orientation::Transpose,
        Orientation::Rotate90Cw,
        Orientation::Transverse,
        Orientation::Rotate270Cw,
    ];

    #[test]
    fn display_and_sensor_mappings_are_inverses_for_every_orientation() {
        for o in ALL {
            let v = ViewOrient(o);
            for p in [(0.0, 0.0), (1.0, 0.0), (0.25, 0.8), (0.5, 0.5), (1.0, 1.0)] {
                let back = v.to_display(v.to_sensor(p));
                assert!(
                    (back.0 - p.0).abs() < 1e-6 && (back.1 - p.1).abs() < 1e-6,
                    "{o:?} {p:?}"
                );
            }
        }
    }

    #[test]
    fn the_screen_agrees_with_what_export_does_pixel_for_pixel() {
        // A 3x2 sensor image with a distinct value per pixel, oriented by export's own code.
        let (w, h) = (3u32, 2u32);
        let src: Vec<u16> = (0..w * h).map(|i| i as u16).collect();
        for o in ALL {
            let (out, ow, oh) = nicti_preen::orient::apply(&src, w, h, 1, o);
            let v = ViewOrient(o);
            assert_eq!(
                v.display_size((w as f32, h as f32)),
                (ow as f32, oh as f32),
                "{o:?}"
            );
            // Every displayed pixel centre must map back to the sensor pixel export put there.
            for dy in 0..oh {
                for dx in 0..ow {
                    let u = (dx as f32 + 0.5) / ow as f32;
                    let vv = (dy as f32 + 0.5) / oh as f32;
                    let (su, sv) = v.to_sensor((u, vv));
                    let (sx, sy) = (
                        (su * w as f32).floor() as u32,
                        (sv * h as f32).floor() as u32,
                    );
                    let expected = out[(dy * ow + dx) as usize];
                    assert_eq!(
                        src[(sy * w + sx) as usize],
                        expected,
                        "{o:?} at ({dx},{dy})"
                    );
                }
            }
        }
    }

    #[test]
    fn normal_is_the_exact_identity_for_the_screen_helpers() {
        let rect = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(300.0, 200.0));
        let src = (6000.0, 4000.0);
        let p = egui::pos2(123.0, 77.0);
        let v = ViewOrient::normal();
        let plain = (
            (p.x - rect.left()) / rect.width() * src.0,
            (p.y - rect.top()) / rect.height() * src.1,
        );
        assert_eq!(v.screen_to_image(rect, src, p), plain);
        assert_eq!(v.display_size(src), src);
        assert!(
            v.is_normal()
                && ViewOrient::from_exif(0).is_normal()
                && ViewOrient::from_exif(9).is_normal()
        );
    }

    #[test]
    fn screen_and_image_round_trip_for_every_orientation_in_a_letterboxed_rect() {
        let src = (6000.0f32, 4000.0f32);
        for o in ALL {
            let v = ViewOrient(o);
            let (dw, dh) = v.display_size(src);
            // A rect with the displayed aspect, as fit_rect would produce.
            let rect =
                egui::Rect::from_min_size(egui::pos2(40.0, 10.0), egui::vec2(dw / 10.0, dh / 10.0));
            for p in [(0.0, 0.0), (1.0, 1.0), (1234.5, 3999.0), (5999.0, 1.0)] {
                let s = v.image_to_screen(rect, src, p);
                let back = v.screen_to_image(rect, src, s);
                assert!(
                    (back.0 - p.0).abs() < 0.05 && (back.1 - p.1).abs() < 0.05,
                    "{o:?} {p:?} -> {back:?}"
                );
            }
        }
    }

    #[test]
    fn a_portrait_shot_displays_taller_than_wide_and_the_corners_land_where_export_puts_them() {
        // Orientation 6 (Rotate90Cw): the sensor's top-left corner is the display's top-right.
        let v = ViewOrient::from_exif(6);
        assert_eq!(v.display_size((6000.0, 4000.0)), (4000.0, 6000.0));
        assert_eq!(v.to_display((0.0, 0.0)), (1.0, 0.0));
        assert_eq!(v.to_display((1.0, 0.0)), (1.0, 1.0));
        // Orientation 8 (Rotate270Cw): the sensor's top-left corner is the display's bottom-left.
        assert_eq!(ViewOrient::from_exif(8).to_display((0.0, 0.0)), (0.0, 1.0));
    }

    #[test]
    fn a_portrait_nef_fits_a_landscape_pane_by_its_turned_size() {
        use crate::viewport::{fit_scale, one_to_one_scale};
        let sensor = (6000.0, 4000.0);
        let pane = (800.0, 600.0);
        // Unturned, the 3:2 landscape frame is wider than the 4:3 pane: letterboxed top/bottom.
        let normal = fit_scale(pane, ViewOrient::normal().display_size(sensor));
        assert!(normal[0] == 1.0 && normal[1] > 1.0, "{normal:?}");
        // Orientation 6 shows it as 4000 x 6000 portrait: letterboxed left/right instead.
        let turned = fit_scale(pane, ViewOrient::from_exif(6).display_size(sensor));
        assert!(turned[0] > 1.0 && turned[1] == 1.0, "{turned:?}");
        // 1:1 is per axis in *displayed* pixels: a 4000-wide displayed image in 800 px is 0.2.
        let one = one_to_one_scale(pane, ViewOrient::from_exif(6).display_size(sensor));
        assert!(
            (one[0] - 0.2).abs() < 1e-6 && (one[1] - 0.1).abs() < 1e-6,
            "{one:?}"
        );
    }
}
