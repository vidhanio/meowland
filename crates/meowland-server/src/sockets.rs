//! Standalone and systemd-activated Unix sockets with identity-checked cleanup.

use std::{
    fs, io,
    os::{
        fd::{AsFd, BorrowedFd},
        unix::{
            fs::{FileTypeExt, MetadataExt, PermissionsExt},
            net::{UnixListener as StdListener, UnixStream as StdStream},
        },
    },
    path::{Path, PathBuf},
};

use meowland_core::{Error, Result};
use meowland_protocol::Paths;

#[derive(Clone, Copy, PartialEq, Eq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

impl SocketIdentity {
    fn at(path: &Path) -> io::Result<Option<Self>> {
        let metadata = fs::symlink_metadata(path)?;
        Ok(metadata.file_type().is_socket().then_some(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }))
    }
}

struct SocketPath {
    path: PathBuf,
    identity: SocketIdentity,
}

impl Drop for SocketPath {
    fn drop(&mut self) {
        if SocketIdentity::at(&self.path).ok() == Some(Some(self.identity)) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(crate) struct BoundSocket {
    listener: StdListener,
    _path: Option<SocketPath>,
}

pub(crate) struct Listeners {
    pub control: BoundSocket,
    pub pane: BoundSocket,
}

pub(crate) fn listeners(paths: &Paths) -> Result<Listeners> {
    let mut inherited = listenfd::ListenFd::from_env();
    if inherited.len() == 0 {
        return Ok(Listeners {
            control: bind(&paths.control)?,
            pane: bind(&paths.pane)?,
        });
    }
    if inherited.len() != 2 {
        return Err(invalid("socket activation requires exactly two listeners").into());
    }
    let mut control = None;
    let mut pane = None;
    for index in 0..2 {
        let listener = inherited
            .take_unix_listener(index)?
            .ok_or_else(|| invalid("missing activation listener"))?;
        let address = listener.local_addr()?;
        let slot = match address.as_pathname() {
            Some(path) if path == paths.control => &mut control,
            Some(path) if path == paths.pane => &mut pane,
            _ => return Err(invalid("unexpected activation socket path").into()),
        };
        if slot.is_some()
            || !rustix::net::sockopt::socket_acceptconn(&listener).map_err(io::Error::from)?
        {
            return Err(invalid("duplicate or non-listening activation socket").into());
        }
        listener.set_nonblocking(true)?;
        *slot = Some(BoundSocket {
            listener,
            _path: None,
        });
    }
    match (control, pane) {
        (Some(control), Some(pane)) => Ok(Listeners { control, pane }),
        _ => Err(invalid("missing activation socket").into()),
    }
}

impl BoundSocket {
    pub(crate) fn accept(&self) -> io::Result<StdStream> {
        let (stream, _) = self.listener.accept()?;
        stream.set_nonblocking(true)?;
        Ok(stream)
    }
}

impl AsFd for BoundSocket {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.listener.as_fd()
    }
}

fn bind(path: &Path) -> Result<BoundSocket> {
    let listener = match StdListener::bind(path) {
        Ok(listener) => listener,
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            let identity = match SocketIdentity::at(path) {
                Ok(Some(identity)) => identity,
                Ok(None) => return Err(error.into()),
                Err(missing) if missing.kind() == io::ErrorKind::NotFound => {
                    return own_socket(path, StdListener::bind(path)?);
                }
                Err(error) => return Err(error.into()),
            };
            match StdStream::connect(path) {
                Ok(_) => return Err(Error::ServerAlreadyListening(path.to_path_buf())),
                Err(refused) if refused.kind() == io::ErrorKind::ConnectionRefused => {
                    match SocketIdentity::at(path) {
                        Ok(Some(current)) if current == identity => match fs::remove_file(path) {
                            Ok(()) => {}
                            Err(missing) if missing.kind() == io::ErrorKind::NotFound => {}
                            Err(error) => return Err(error.into()),
                        },
                        Err(missing) if missing.kind() == io::ErrorKind::NotFound => {}
                        _ => return Err(error.into()),
                    }
                }
                Err(missing) if missing.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            StdListener::bind(path)?
        }
        Err(error) => return Err(error.into()),
    };
    own_socket(path, listener)
}

fn own_socket(path: &Path, listener: StdListener) -> Result<BoundSocket> {
    let identity =
        SocketIdentity::at(path)?.ok_or_else(|| invalid("bound socket path was replaced"))?;
    let owned_path = SocketPath {
        path: path.to_path_buf(),
        identity,
    };
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok(BoundSocket {
        listener,
        _path: Some(owned_path),
    })
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
