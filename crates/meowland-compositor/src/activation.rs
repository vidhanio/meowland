//! One-shot launch tokens associate a process launch with a specific toplevel.
//! Activations can arrive before the pane connects or before the window maps.

use std::time::{Duration, Instant};

use smithay::{
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    wayland::xdg_activation::{
        XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
    },
};

use super::{Compositor, Selection, ServerToPane, WindowId};

const TOKEN_LIFETIME: Duration = Duration::from_secs(60);

pub(crate) struct Launch {
    pub window: Option<WindowId>,
    expires: Instant,
}

impl Compositor {
    /// Reserve an activation token for a client launched by the host.
    pub fn create_activation_token(&mut self) -> String {
        let (token, data) = self.protocols.activation.create_external_token(None);
        let token = token.as_str().to_owned();
        let expires = data.timestamp + TOKEN_LIFETIME;
        self.protocols.launches.insert(
            token.clone(),
            Launch {
                window: None,
                expires,
            },
        );
        self.refresh_activation_deadline();
        token
    }

    pub(crate) fn forget_activation(&mut self, token: &str) {
        self.protocols.launches.remove(token);
        self.protocols
            .activation
            .remove_token(&token.to_owned().into());
        self.refresh_activation_deadline();
    }

    /// Revoke a launch token and release panes waiting for it.
    pub fn cancel_activation(&mut self, token: &str, reason: &str) {
        self.forget_activation(token);
        let waiting: Vec<_> = self
            .attachments
            .iter()
            .filter_map(|(&pane, entry)| {
                matches!(entry.waiting(), Some(Selection::Activation(pending)) if pending == token)
                    .then_some(pane)
            })
            .collect();
        for pane in waiting {
            self.remove_pane(pane);
            self.emit(pane, ServerToPane::Release(reason.to_owned()));
        }
    }

    pub(crate) fn cancel_window_activations(&mut self, window: WindowId) {
        let tokens: Vec<_> = self
            .protocols
            .launches
            .iter()
            .filter(|(_, launch)| launch.window == Some(window))
            .map(|(token, _)| token.clone())
            .collect();
        for token in tokens {
            self.cancel_activation(&token, "window closed");
        }
    }

    /// Recompute the earliest launch or external-token expiry.
    pub(crate) fn refresh_activation_deadline(&mut self) {
        self.protocols.activation_deadline = self
            .protocols
            .launches
            .values()
            .map(|launch| launch.expires)
            .chain(
                self.protocols
                    .activation
                    .tokens()
                    .map(|(_, data)| data.timestamp + TOKEN_LIFETIME),
            )
            .min();
    }

    /// Expiry is scheduled by the readiness loop, not window-detection polling.
    pub(crate) fn expire_activation_tokens(&mut self) {
        let now = Instant::now();
        if self
            .protocols
            .activation_deadline
            .is_none_or(|deadline| now < deadline)
        {
            return;
        }
        let expired: Vec<_> = self
            .protocols
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
        self.protocols
            .activation
            .retain_tokens(|_, data| now < data.timestamp + TOKEN_LIFETIME);
        self.refresh_activation_deadline();
    }
}

impl XdgActivationHandler for Compositor {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.protocols.activation
    }

    /// Only the focused Wayland client may delegate activation to another app.
    fn token_created(&mut self, _token: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        self.windows
            .catalog
            .focused()
            .and_then(|window| self.windows.by_id.get(&window))
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
        let Some(window) = self.windows.id_by_surface.get(&surface).copied() else {
            return;
        };
        self.protocols.activation.remove_token(&token);
        if let Some(launch) = self.protocols.launches.get_mut(token.as_str()) {
            launch.window = Some(window);
        }
        self.refresh_activation_deadline();
        self.resolve_pending_panes();
        if self.windows.catalog.drawable(window) {
            self.focus_window(window);
        }
    }
}
