//! Headless UI test harness (#426): drive a real egui view without a window or a GPU adapter.
//!
//! Named for a cat swatting at something to see how it reacts. Built on `egui_kittest`'s
//! AccessKit queries and input injection; frames are drawn by [`softpaint`], a CPU rasteriser, so
//! pixel snapshots work where there is no wgpu device (WSL, Linux CI, a laptop with the GPU asleep).
//!
//! Writing a test:
//! 1. Put the view's state in a struct and build a harness with [`harness`] -- the closure draws
//!    the view exactly as `app.rs` does.
//! 2. Find widgets by their AccessKit label (`harness.get_by_label("Exposure")`); custom-painted
//!    widgets need a `response.widget_info(..)` to be findable (see `grid/view.rs`'s cells).
//! 3. Drive input with [`click_at`] / `Node::click` / `harness.key_press`, then [`wait_until`]
//!    for background work (Pounce jobs) or just `harness.run()` for plain egui animation.
//! 4. Snapshot with `harness.snapshot("name")` (PNG under `tests/snapshots/`, refresh with
//!    `UPDATE_SNAPSHOTS=1`).
//!
//! Paint callbacks (the wgpu image viewport) are not drawn by softpaint, so a snapshot shows the
//! chrome around the photo, never the photo itself; anything that needs the pixels of a render
//! stays a GPU test gated on `test_gpu::shared()`.

mod softpaint;
#[cfg(test)]
mod tests_develop;
#[cfg(test)]
mod tests_grid;

use std::time::{Duration, Instant};

use egui::{Pos2, TexturesDelta, Vec2};
use egui_kittest::{Harness, HarnessBuilder, TestRenderer};

/// A kittest renderer that draws with [`softpaint`] instead of wgpu.
#[derive(Default)]
pub struct SoftRenderer {
    textures: softpaint::TextureStore,
}

impl TestRenderer for SoftRenderer {
    fn handle_delta(&mut self, delta: &mut TexturesDelta) {
        self.textures.apply(std::mem::take(delta));
    }

    fn render(
        &mut self,
        ctx: &egui::Context,
        output: &egui::FullOutput,
    ) -> Result<image::RgbaImage, String> {
        let ppp = ctx.pixels_per_point();
        let size = ctx.content_rect().size() * ppp;
        let size = [size.x.round() as usize, size.y.round() as usize];
        let primitives = ctx.tessellate(output.shapes.clone(), ppp);
        let img = softpaint::paint(&primitives, &self.textures, size, ppp, egui::Color32::BLACK);
        // The framebuffer is opaque, so premultiplied and straight alpha agree.
        let bytes = img.pixels.iter().flat_map(|p| p.to_array()).collect();
        image::RgbaImage::from_raw(size[0] as u32, size[1] as u32, bytes)
            .ok_or_else(|| "softpaint produced a mis-sized image".to_string())
    }
}

/// Builds a harness for `app` with the app's real theme and fonts, a 60 Hz step and the CPU
/// renderer. `state` is whatever the view needs; it is reachable as `harness.state()`.
pub fn harness<'a, S>(
    size: impl Into<Vec2>,
    mut app: impl FnMut(&mut egui::Ui, &mut S) + 'a,
    state: S,
) -> Harness<'a, S> {
    // kittest runs one frame while building the harness and offers no way to set up the context
    // first, but fonts and styles only take effect on the *next* frame -- and the app's panels
    // panic on its named font families if they are missing. So the first frame only installs the
    // theme and draws nothing; the view starts on the second.
    let mut booted = false;
    let mut harness = HarnessBuilder::default()
        .with_size(size)
        .with_step_dt(1.0 / 60.0)
        .renderer(SoftRenderer::default())
        .build_ui_state(
            move |ui, state| {
                if !booted {
                    booted = true;
                    crate::fur::install_fonts(ui.ctx());
                    crate::fur::apply(ui.ctx());
                    return;
                }
                app(ui, state);
            },
            state,
        );
    harness.run_steps(2);
    harness
}

/// Steps `harness` until `done` holds, for work that completes on another thread (a Pounce job
/// filling the grid). Panics with `what` after `timeout` -- never silently passes.
pub fn wait_until<S>(
    harness: &mut Harness<'_, S>,
    what: &str,
    timeout: Duration,
    done: impl Fn(&S) -> bool,
) {
    let deadline = Instant::now() + timeout;
    while !done(harness.state()) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        harness.step();
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// A primary-button click at `pos`: press and release on separate frames, as a real mouse does
/// (egui only reports `clicked()` once it has seen the press settle on the widget).
pub fn click_at<S>(harness: &mut Harness<'_, S>, pos: Pos2) {
    click_at_with(harness, pos, egui::Modifiers::NONE);
}

/// [`click_at`] with modifier keys held (Ctrl/Shift multi-select).
pub fn click_at_with<S>(harness: &mut Harness<'_, S>, pos: Pos2, modifiers: egui::Modifiers) {
    use egui::{Event, PointerButton};
    let button = |pressed| Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed,
        modifiers,
    };
    harness.event(Event::ModifiersChanged(modifiers));
    harness.event(Event::PointerMoved(pos));
    harness.step();
    harness.event(button(true));
    harness.step();
    harness.event(button(false));
    harness.run_steps(2);
    harness.event(Event::ModifiersChanged(egui::Modifiers::NONE));
    harness.step();
}
