//! One-shot launch tokens associate a process launch with a specific toplevel.
//! Activations can arrive before the pane connects or before the window maps.

use std::time::{Duration, Instant};

use smithay::{
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    wayland::xdg_activation::{
        XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
    },
};

use super::{Event, State};
use crate::protocol::Show;

const TOKEN_LIFETIME: Duration = Duration::from_secs(60);

pub(super) struct Launch {
    pub(super) window: Option<u64>,
    expires: Instant,
}

impl State {
    pub(super) fn create_activation_token(&mut self) -> String {
        let (token, data) = self.activation.create_external_token(None);
        let token = token.as_str().to_owned();
        let expires = data.timestamp + TOKEN_LIFETIME;
        self.launches.insert(
            token.clone(),
            Launch {
                window: None,
                expires,
            },
        );
        token
    }

    pub(super) fn forget_activation(&mut self, token: &str) {
        self.launches.remove(token);
        self.activation.remove_token(&token.to_owned().into());
    }

    pub(super) fn cancel_activation(&mut self, token: &str, reason: &str) {
        self.forget_activation(token);
        let waiting: Vec<_> = self
            .pending_shows
            .iter()
            .filter_map(|(pane, show)| {
                matches!(show, Show::Activation(pending) if pending == token).then_some(*pane)
            })
            .collect();
        for pane in waiting {
            self.remove_pane(pane);
            let _ = self.events.send(Event::Release {
                pane,
                reason: reason.to_owned(),
            });
        }
    }

    pub(super) fn cancel_window_activations(&mut self, window: u64) {
        let tokens: Vec<_> = self
            .launches
            .iter()
            .filter(|(_, launch)| launch.window == Some(window))
            .map(|(token, _)| token.clone())
            .collect();
        for token in tokens {
            self.cancel_activation(&token, "window closed");
        }
    }

    pub(super) fn next_activation_deadline(&self) -> Option<Instant> {
        self.launches
            .values()
            .map(|launch| launch.expires)
            .chain(
                self.activation
                    .tokens()
                    .map(|(_, data)| data.timestamp + TOKEN_LIFETIME),
            )
            .min()
    }

    /// Expiry is scheduled by the readiness loop, not window-detection polling.
    pub(super) fn expire_activation_tokens(&mut self) {
        let now = Instant::now();
        let expired: Vec<_> = self
            .launches
            .iter()
            .filter(|(_, launch)| launch.expires <= now)
            .map(|(token, _)| token.clone())
            .collect();
        for token in expired {
            self.cancel_activation(
                &token,
                "client did not activate a window; use --app-id for clients without xdg-activation-v1 support",
            );
        }
        self.activation
            .retain_tokens(|_, data| now < data.timestamp + TOKEN_LIFETIME);
    }
}

impl XdgActivationHandler for State {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.activation
    }

    /// Only the focused Wayland client may delegate activation to another app.
    fn token_created(&mut self, _token: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        self.focused
            .and_then(|window| self.windows.get(&window))
            .and_then(|window| window.surface.wl_surface().client())
            .is_some_and(|client| data.client_id == Some(client.id()))
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        if data.timestamp.elapsed() >= TOKEN_LIFETIME {
            self.cancel_activation(token.as_str(), "activation token expired");
            return;
        }
        let Some(window) = self.ids.get(&surface).copied() else {
            return;
        };
        self.activation.remove_token(&token);
        if let Some(launch) = self.launches.get_mut(token.as_str()) {
            launch.window = Some(window);
        }
        self.resolve_pending_panes();
        if self
            .windows
            .get(&window)
            .is_some_and(|window| window.announced)
        {
            self.focus_window(window);
        }
    }
}
