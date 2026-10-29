//! Direct control operations on the server and compositor owner.

use futures::channel::oneshot;
use meowland_protocol::{ControlRequest, ControlResponse};

use super::{Server, process};

impl Server {
    pub(crate) fn control(
        &mut self,
        request: ControlRequest,
        reply: oneshot::Sender<ControlResponse>,
    ) {
        if reply.is_canceled() {
            return;
        }
        if !self.shutdown.running() {
            let _ = reply.send(ControlResponse::Error("server stopping".into()));
            return;
        }
        let response = match request {
            ControlRequest::List => ControlResponse::Windows(self.compositor.window_list()),
            ControlRequest::Kill(show) => self.compositor.close_window(&show).map_or_else(
                |error| ControlResponse::Error(error.to_string()),
                |()| ControlResponse::Ok,
            ),
            ControlRequest::Run { args, env, cwd } => {
                let token = self.compositor.create_activation_token();
                match process::launch(&args, &env, &cwd, &self.display, &token) {
                    Ok(child) => {
                        self.children.push(child);
                        ControlResponse::Started(token)
                    }
                    Err(error) => {
                        self.compositor
                            .cancel_activation(&token, "launch cancelled");
                        ControlResponse::Error(error.report())
                    }
                }
            }
        };
        let _ = reply.send(response);
    }
}
