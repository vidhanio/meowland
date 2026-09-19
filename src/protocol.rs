use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
pub const MAX_MESSAGE: usize = 64 * 1024 * 1024;

/// The modifier bits a pane packs into [`Input::Key`], one per modifier.
///
/// Both sides of the pane socket read these, so the layout lives here rather
/// than in either of them.
pub mod modifiers {
    pub const SHIFT: u8 = 1;
    pub const CONTROL: u8 = 2;
    pub const ALT: u8 = 4;
    pub const SUPER: u8 = 8;
}

/// The keys a pane treats as compositor bindings, in Linux input codes.
pub const KEY_Q: u16 = 16;
pub const KEY_W: u16 = 17;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Show {
    Id(u64),
    Newest,
    Focused,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WindowInfo {
    pub id: u64,
    pub app_id: String,
    pub title: String,
    pub active: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct Hello {
    pub version: u32,
    pub width: u32,
    pub height: u32,
    pub cell_width: Option<u16>,
    pub cell_height: Option<u16>,
    pub show: Show,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Input {
    Key {
        /// Linux input code (`KEY_*`), which is 8 below what XKB uses for an
        /// `evdev` keymap; the conversion happens in the compositor alone.
        code: u16,
        pressed: bool,
        /// [`modifiers`] bits.
        modifiers: u8,
    },
    Text(String),
    Pointer {
        x: f64,
        y: f64,
        button: Option<u8>,
        pressed: bool,
        scroll: i16,
    },
}

#[derive(Debug, Deserialize, Serialize)]
pub enum PaneToServer {
    Hello(Hello),
    Input(Input),
    Resize {
        width: u32,
        height: u32,
        cell_width: Option<u16>,
        cell_height: Option<u16>,
    },
    Ack,
}

#[derive(Debug, Deserialize, Serialize)]
pub enum ServerToPane {
    HelloOk,
    Reject(String),
    Frame {
        width: u32,
        height: u32,
        #[serde(with = "serde_bytes")]
        rgb: Vec<u8>,
    },
    Release(String),
    Title(String),
    /// The pointer shape the terminal should show, or `None` for its own.
    Cursor(Option<String>),
}

#[derive(Debug, Deserialize, Serialize)]
pub enum ControlRequest {
    Ping,
    Run(Vec<String>),
    List,
    Stop,
}

#[derive(Debug, Deserialize, Serialize)]
pub enum ControlResponse {
    Ok,
    Windows(Vec<WindowInfo>),
    Error(String),
}

/// Write one length-prefixed message.
///
/// # Errors
/// Returns an error when the message does not encode, when it is larger than
/// [`MAX_MESSAGE`], or when the writer fails.
pub fn send<T: Serialize>(writer: &mut impl Write, value: &T) -> io::Result<()> {
    let encoded = bincode::serde::encode_to_vec(
        value,
        bincode::config::standard().with_limit::<MAX_MESSAGE>(),
    )
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if encoded.len() > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message too large",
        ));
    }
    writer.write_all(&(encoded.len() as u32).to_le_bytes())?;
    writer.write_all(&encoded)
}

/// Read one length-prefixed message.
///
/// # Errors
/// Returns an error when the length prefix is missing, when it claims more
/// than [`MAX_MESSAGE`] bytes, when the body is truncated, or when it does not
/// decode as exactly one value of the requested type.
pub fn recv<T: for<'de> Deserialize<'de>>(reader: &mut impl Read) -> io::Result<T> {
    let mut header = [0; 4];
    reader.read_exact(&mut header)?;
    let size = u32::from_le_bytes(header) as usize;
    if size > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message too large",
        ));
    }
    let mut encoded = vec![0; size];
    reader.read_exact(&mut encoded)?;
    let (value, used) = bincode::serde::decode_from_slice(
        &encoded,
        bincode::config::standard().with_limit::<MAX_MESSAGE>(),
    )
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if used != size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing message bytes",
        ));
    }
    Ok(value)
}

#[must_use]
pub fn sanitize(input: &str) -> String {
    input
        .chars()
        .filter(|c| !c.is_control())
        .take(256)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_reject_bad_frames() {
        let value = PaneToServer::Hello(Hello {
            version: VERSION,
            width: 800,
            height: 600,
            cell_width: Some(10),
            cell_height: Some(20),
            show: Show::Id(42),
        });
        let mut data = Vec::new();
        send(&mut data, &value).unwrap();
        assert!(matches!(
            recv::<PaneToServer>(&mut data.as_slice()).unwrap(),
            PaneToServer::Hello(Hello {
                show: Show::Id(42),
                ..
            })
        ));
        assert!(recv::<PaneToServer>(&mut data[..data.len() - 1].as_ref()).is_err());
        let mut oversized = ((MAX_MESSAGE + 1) as u32).to_le_bytes().to_vec();
        assert!(recv::<PaneToServer>(&mut oversized.as_slice()).is_err());
        oversized.clear();
    }

    #[test]
    fn strips_untrusted_terminal_controls() {
        assert_eq!(sanitize("abc\x1b[31m\n"), "abc[31m");
    }
}
