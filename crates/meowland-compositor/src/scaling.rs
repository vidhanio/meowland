//! Preferred fractional scale and physical-to-logical pane geometry.

use meowland_core::scale::UiScale;
use smithay::{
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::wayland_server::DisplayHandle,
    utils::{Logical, Size},
    wayland::{
        compositor::with_states,
        fractional_scale::{FractionalScaleHandler, with_fractional_scale},
        output::OutputManagerState,
    },
};

use super::{State, WlSurface};

pub fn output(display: &DisplayHandle, scale: UiScale) -> (OutputManagerState, Output, Mode) {
    let mode = Mode {
        size: (1920, 1080).into(),
        refresh: 60_000,
    };
    let manager = OutputManagerState::new_with_xdg_output::<State>(display);
    let output = Output::new(
        "meowland".into(),
        PhysicalProperties {
            size: (300, 200).into(),
            subpixel: Subpixel::Unknown,
            make: "meowland".into(),
            model: "terminal".into(),
            serial_number: String::new(),
        },
    );
    output.create_global::<State>(display);
    output.change_current_state(
        Some(mode),
        None,
        Some(Scale::Fractional(scale.factor())),
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    (manager, output, mode)
}

impl FractionalScaleHandler for State {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        with_states(&surface, |states| {
            with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(self.scale.factor());
            });
        });
    }
}

impl State {
    pub fn logical_size(&self, width: u32, height: u32) -> Size<i32, Logical> {
        Size::from((self.scale.logical(width), self.scale.logical(height)))
    }
}
