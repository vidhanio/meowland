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
//! `MEOWLAND_GPU_BUFFERS=off` is the way out if a client turns out to be worse
//! off with the offer than without it.
//!
//! # Choosing a device
//!
//! On a machine with more than one GPU every render node is offered and the
//! client picks the one it renders on. The order is a hint, not a rule.

use std::{fs, path::PathBuf};

use rustix::fs::{FileType, Mode, OFlags};

/// How clients are offered GPU buffers.
///
/// Unset or [`AUTO`] offers the first render node that has a renderer to read
/// buffers back through, [`OFF`] offers nothing, and any other value is the
/// path of the one render node to offer.
pub const VARIABLE: &str = "MEOWLAND_GPU_BUFFERS";

/// The value of [`VARIABLE`] that offers GPU buffers on every render node.
pub const AUTO: &str = "auto";

/// The value of [`VARIABLE`] that stops clients being offered GPU buffers.
pub const OFF: &str = "off";

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

/// The render nodes clients may be told to allocate on.
///
/// A machine without any - no GPU, a container without the device, a session
/// over the network - simply has none, which is not an error: clients then draw
/// into shared memory, which is what they do by default anyway.
pub fn nodes() -> Result<Vec<RenderNode>, Error> {
    match std::env::var_os(VARIABLE) {
        None => Ok(find()),
        Some(setting) if setting == AUTO => Ok(find()),
        Some(setting) if setting == OFF => Ok(Vec::new()),
        Some(setting) => {
            let path = PathBuf::from(setting);
            let node = RenderNode::open(path.clone())
                .map_err(|source| Error::Unusable { path, source })?;
            Ok(vec![node])
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
