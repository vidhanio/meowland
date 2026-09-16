//! The render nodes that clients are offered for their GPU buffers.
//!
//! A client that renders on the GPU does not hand over pixels. It hands over a
//! file descriptor for memory that its driver allocated. The compositor chooses
//! the device that the memory is on, so the compositor has to name one. That
//! device has to be one that the client can render on and the compositor can
//! read. Otherwise the client is better off drawing into shared memory.
//!
//! # What keeps the offer honest
//!
//! The offer is what a client chooses on. Mesa's Wayland WSI takes GPU buffers
//! when a compositor advertises them. It has no window when the buffers that it
//! produces are then refused. An offer that cannot be honoured is therefore
//! worse than no offer. Three things keep this offer inside what the compositor
//! can read:
//!
//! - No renderer, no offer. The device is named only when a renderer on it can
//!   bring buffers back. See [`crate::gpu`]. A machine with no render node
//!   never advertises the global.
//! - What is advertised is what that renderer takes, not what the protocol
//!   allows.
//! - Every buffer is read once before the compositor reports it to the client
//!   as good. A layout that imports but cannot be copied out of is therefore
//!   refused while the client can still fall back to shared memory.
//!
//! A driver that keeps a buffer where the CPU cannot reach it is expected, not
//! exceptional. `mmap` of such a buffer fails with `EPERM`, whether or not CPU
//! access was begun first, and reading the descriptor fails with `EINVAL`. That
//! is why the renderer exists.
//!
//! [`Offer::Off`] is the way out, if a client turns out to be worse off with
//! the offer than without it.
//!
//! # Choosing a device
//!
//! On a machine with more than one GPU, every render node is offered and the
//! client picks the one that it renders on. The order is a hint, not a rule.

use std::{
    fs,
    path::{Path, PathBuf},
};

use rustix::fs::{FileType, Mode, OFlags};

/// How clients are offered GPU buffers.
///
/// The value comes from a command line flag that falls back to an environment
/// variable. See `Settings` in `cli.rs`. This module holds the values, not
/// their source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, usage::ValueEnum)]
pub enum Offer {
    /// Offer clients the render nodes this machine has.
    Auto,
    /// Offer nothing: clients draw into shared memory.
    Off,
}

const DEVICE_DIRECTORY: &str = "/dev/dri";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderNode {
    pub path: PathBuf,
    /// The device number. Clients match this against their own devices, not
    /// against the path.
    pub device: u64,
}

impl RenderNode {
    /// Open a render node, so that a named node is known to work before use.
    fn open(path: PathBuf) -> std::io::Result<Self> {
        let file = rustix::fs::open(&path, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())?;
        let stat = rustix::fs::fstat(&file)?;
        if !FileType::from_raw_mode(stat.st_mode).is_char_device() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "not a device",
            ));
        }
        Ok(Self {
            path,
            device: stat.st_rdev,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the render node {path} cannot be used")]
    Unusable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl Offer {
    /// How this offer is written on a command line (`--gpu-buffers`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Off => "off",
        }
    }

    /// The render nodes that clients may be offered, most preferred first.
    ///
    /// `pinned` names one node to use instead of looking for any. A machine
    /// with more than one GPU needs this. The order in which the kernel
    /// numbers render nodes says nothing about which of them is worth
    /// rendering on.
    ///
    /// A machine with no render node has none. That covers a machine without a
    /// GPU, a container without the device, and a session over the network. It
    /// is not an error: clients then draw into shared memory, which is what
    /// they do by default.
    pub fn nodes(self, pinned: Option<&Path>) -> Result<Vec<RenderNode>, Error> {
        match (self, pinned) {
            (Self::Off, _) => Ok(Vec::new()),
            (Self::Auto, None) => Ok(find()),
            (Self::Auto, Some(path)) => {
                let node =
                    RenderNode::open(path.to_path_buf()).map_err(|source| Error::Unusable {
                        path: path.to_path_buf(),
                        source,
                    })?;
                Ok(vec![node])
            }
        }
    }
}

/// Every render node on the machine, in the order that the kernel numbers them.
///
/// A `renderD*` node renders without owning a screen. That is the only kind of
/// node that can be offered to a client here.
fn find() -> Vec<RenderNode> {
    let Ok(entries) = fs::read_dir(DEVICE_DIRECTORY) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(number) = number_of(&path) else {
            continue;
        };
        match RenderNode::open(path) {
            Ok(node) => {
                tracing::debug!(?node, "found a render node");
                found.push((number, node));
            }
            Err(err) => {
                tracing::debug!(path = %entry.path().display(), ?err, "ignoring an unusable render node");
            }
        }
    }
    found.sort_by_key(|(number, _)| *number);
    found.into_iter().map(|(_, node)| node).collect()
}

fn number_of(path: &std::path::Path) -> Option<u32> {
    path.file_name()?
        .to_str()?
        .strip_prefix("renderD")?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offering_nothing_offers_no_nodes() {
        assert!(
            matches!(Offer::Off.nodes(None), Ok(nodes) if nodes.is_empty()),
            "a client that takes an offer we cannot serve has no window at all"
        );
        // Even a node that was asked for by name: `off` is `off`.
        assert!(matches!(
            Offer::Off.nodes(Some(std::path::Path::new("/dev/dri/renderD128"))),
            Ok(nodes) if nodes.is_empty()
        ));
    }

    #[test]
    fn only_render_nodes_are_numbered() {
        assert_eq!(
            number_of(std::path::Path::new("/dev/dri/renderD128")),
            Some(128)
        );
        assert_eq!(number_of(std::path::Path::new("/dev/dri/card0")), None);
        assert_eq!(number_of(std::path::Path::new("/dev/dri/renderD")), None);
        assert_eq!(number_of(std::path::Path::new("/dev/dri/by-path/x")), None);
    }
}
