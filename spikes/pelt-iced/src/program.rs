//! `iced::widget::shader::Program` implementation wiring [`crate::viewport::LiveChainPrimitive`]
//! into the app: pointer-drag inside the viewport is translated into `Message::ViewportDragged`
//! (vibrance/white-balance), matching `pelt-egui`'s pan interaction.

use iced::advanced::mouse;
use iced::widget::shader::{self, Action};
use iced::{Point, Rectangle};

use crate::viewport::LiveChainPrimitive;
use crate::Message;
use pelt::live_chain::LiveChainParams;

#[derive(Debug, Clone, Copy)]
pub struct ViewportProgram {
    pub params: LiveChainParams,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DragState {
    drag_origin: Option<Point>,
}

impl shader::Program<Message> for ViewportProgram {
    type State = DragState;
    type Primitive = LiveChainPrimitive;

    fn update(
        &self,
        state: &mut Self::State,
        event: &iced::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<Action<Message>> {
        let iced::Event::Mouse(mouse_event) = event else {
            return None;
        };
        match mouse_event {
            iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left) => {
                if let Some(position) = cursor.position_in(bounds) {
                    state.drag_origin = Some(position);
                }
                None
            }
            iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left) => {
                state.drag_origin = None;
                None
            }
            iced::mouse::Event::CursorMoved { .. } => {
                let origin = state.drag_origin?;
                let position = cursor.position_in(bounds)?;
                let delta = position - origin;
                state.drag_origin = Some(position);
                Some(Action::publish(Message::ViewportDragged {
                    dx: delta.x / bounds.width,
                    dy: delta.y / bounds.height,
                }))
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        _state: &Self::State,
        _cursor: mouse::Cursor,
        _bounds: Rectangle,
    ) -> Self::Primitive {
        LiveChainPrimitive {
            params: self.params,
        }
    }
}
