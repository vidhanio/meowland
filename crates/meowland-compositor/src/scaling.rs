//! Preferred fractional scale and physical-to-logical pane geometry.

use smithay::{
    utils::{Logical, Size},
    wayland::{
        compositor::with_states,
        fractional_scale::{FractionalScaleHandler, with_fractional_scale},
    },
};

use super::{State, WlSurface};

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
