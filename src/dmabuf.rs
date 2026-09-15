//! Where clients are told to put the memory of a GPU buffer.
//!
//! A client that renders on the GPU does not hand over pixels, it hands over a
//! file descriptor for memory its driver allocated. Which device that memory
//! lives on is the compositor's to choose, so it has to name one - and it has
//! to be a device the client can render on *and* the compositor can read, or
//! the client is better off drawing into shared memory.
//!
//! # What keeps the offer honest
//!
//! The offer is what a client chooses on: Mesa's Wayland WSI takes GPU buffers
//! when a compositor advertises them and has no window at all when the buffers
//! it produces are then refused. So an offer that cannot be honoured is worse
//! than no offer, and three things keep this one inside what the compositor can
//! actually read:
//!
//! - No renderer, no offer. The device is only named when there is a renderer
//!   on it to bring buffers back through ([`crate::gpu`]), and a machine with
//!   no render node at all simply never advertises the global.
//! - What is advertised is what that renderer takes, not what the protocol
//!   allows.
//! - Every buffer is read once before the client is told it is good, so a
//!   layout that imports but cannot be copied out of is refused while the
//!   client can still fall back to shared memory.
//!
//! A driver keeping a buffer where the CPU cannot reach it is expected, not
//! exceptional: `mmap` of such a buffer fails with `EPERM` whether or not CPU
//! access was begun first, and reading the descriptor fails with `EINVAL`. That
//! is the reason the renderer exists.
//!
//! [`Offer::Off`] is the way out if a client turns out to be worse off with the
//! offer than without it.
//!
//! # Choosing a device
//!
//! On a machine with more than one GPU every render node is offered and the
//! client picks the one it renders on. The order is a hint, not a rule.

use std::{
    fs,
    path::{Path, PathBuf},
};

use rustix::fs::{FileType, Mode, OFlags};

/// How clients are offered GPU buffers.
///
/// Read from a command line flag that falls back to an environment variable
/// (see `Cli` in `main.rs`); what this module knows is the values, not where
/// they came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, usage::ValueEnum)]
pub enum Offer {
    /// Offer clients the render nodes this machine has.
    Auto,
    /// Offer nothing: clients draw into shared memory.
    Off,
}

/// Where render nodes live.
const DEVICE_DIRECTORY: &str = "/dev/dri";

/// A device clients can render on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderNode {
    /// The device file, as `/dev/dri/renderD128`.
    pub path: PathBuf,
    /// The device number, which is what clients match against their own
    /// devices rather than the path.
    pub device: u64,
}

impl RenderNode {
    /// Take a render node, so that naming one is known to work before any
    /// client relies on it.
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

/// Why clients cannot be told where to put a GPU buffer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A render node was named that cannot be opened as one.
    #[error("the render node {path} cannot be used")]
    Unusable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl Offer {
    /// The render nodes clients may be told to allocate on, most preferred
    /// first.
    ///
    /// `pinned` names one node to use instead of looking for any, which is what
    /// a machine with more than one GPU needs: the order the kernel numbers
    /// render nodes in is not a statement about which of them is worth
    /// rendering on.
    ///
    /// A machine without a render node - no GPU, a container without the
    /// device, a session over the network - simply has none, which is not an
    /// error: clients then draw into shared memory, which is what they do by
    /// default anyway.
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

/// Every render node on the machine, in the order the kernel numbers them.
///
/// `renderD*` is the node that renders without owning a screen, which is the
/// only kind that can be offered to a client here.
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

/// The number in `renderD<number>`, for a file named like one.
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
