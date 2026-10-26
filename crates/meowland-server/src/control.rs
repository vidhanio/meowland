//! Nonblocking control requests and actor-owned process launches.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;

use meowland_protocol::{ControlRequest, ControlResponse};
use tokio::sync::oneshot;

use super::{MAX_CONNECTIONS, Server, process};

type Reply = oneshot::Sender<ControlResponse>;

pub(crate) struct Launch {
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    cwd: PathBuf,
}

pub(crate) enum Completion {
    Response {
        reply: Reply,
        response: io::Result<ControlResponse>,
    },
    Launch {
        reply: Reply,
        launch: Launch,
        token: io::Result<String>,
    },
}

impl Server {
    pub(crate) fn start_control(&mut self, request: ControlRequest, reply: Reply) {
        if reply.is_closed() {
            return;
        }
        if !self.shutdown.running() {
            let _ = reply.send(ControlResponse::Error("server stopping".into()));
            return;
        }
        if self.controls.len() >= MAX_CONNECTIONS {
            let _ = reply.send(ControlResponse::Error(
                "too many pending control requests".into(),
            ));
            return;
        }
        match request {
            ControlRequest::Ping => {
                let _ = reply.send(ControlResponse::Ok);
            }
            ControlRequest::List => {
                let operation = self
                    .compositor
                    .call(|state| ControlResponse::Windows(state.window_list()));
                self.controls.spawn(async move {
                    Completion::Response {
                        reply,
                        response: operation.await,
                    }
                });
            }
            ControlRequest::Kill(show) => {
                let operation = self.compositor.call(move |state| {
                    state.close_window(&show).map_or_else(
                        |error| ControlResponse::Error(error.to_string()),
                        |()| ControlResponse::Ok,
                    )
                });
                self.controls.spawn(async move {
                    Completion::Response {
                        reply,
                        response: operation.await,
                    }
                });
            }
            ControlRequest::Run { args, env, cwd } => {
                let operation = self
                    .compositor
                    .call(meowland_compositor::Compositor::create_activation_token);
                self.controls.spawn(async move {
                    Completion::Launch {
                        reply,
                        launch: Launch { args, env, cwd },
                        token: operation.await,
                    }
                });
            }
        }
    }

    pub(crate) fn control_finished(&mut self, completion: Completion) {
        match completion {
            Completion::Response { reply, response } => {
                let response = if self.shutdown.running() {
                    response.unwrap_or_else(|error| ControlResponse::Error(error.to_string()))
                } else {
                    ControlResponse::Error("server stopping".into())
                };
                let _ = reply.send(response);
            }
            Completion::Launch {
                reply,
                launch,
                token,
            } => {
                let token = match token {
                    Ok(token) => token,
                    Err(error) => {
                        let _ = reply.send(ControlResponse::Error(error.to_string()));
                        return;
                    }
                };
                if reply.is_closed() || !self.shutdown.running() {
                    self.cancel_activation(token);
                    let _ = reply.send(ControlResponse::Error("server stopping".into()));
                    return;
                }
                let response = match process::launch(
                    &launch.args,
                    &launch.env,
                    &launch.cwd,
                    &self.compositor.display,
                    &token,
                ) {
                    Ok(child) => {
                        self.children.spawn(process::wait(child));
                        ControlResponse::Started(token)
                    }
                    Err(error) => {
                        self.cancel_activation(token);
                        ControlResponse::Error(error.to_string())
                    }
                };
                let _ = reply.send(response);
            }
        }
    }

    fn cancel_activation(&self, token: String) {
        let _ = self
            .compositor
            .execute(move |state| state.cancel_activation(&token, "launch cancelled"));
    }
}
