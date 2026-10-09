//! Fur: pelt's visual layer (#425) -- the app-wide dark theme and Inter fonts (`tokens`), the
//! Lightroom-style slider row (`slider`), section/segmented/icon-button chrome (`widgets`) and the
//! code-drawn vector icons (`icons`). Ported from LightCraft; each file's header carries the
//! provenance. Not to be confused with `nicti_tapetum::coat`, the edit-parameter structs.

// The icon set and palette are ported whole; callers adopt them as more panels move over (#432).
#![allow(dead_code)]

mod icons;
mod iris_wheel;
mod slider;
mod tokens;
mod whisker_curve;
mod widgets;

pub use icons::{paint as paint_icon, Icon};
pub use iris_wheel::show as iris_wheel;
pub use slider::{slider, SliderOut, SliderSpec, Track};
pub use tokens::{apply, install_fonts};
pub use whisker_curve::show as whisker_curve;
pub use widgets::{
    band_dots, divider, icon_button, install_section_states, section, segmented,
    take_changed_section_states, SectionStates,
};
