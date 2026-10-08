use std::{
    fs::File,
    io::{Read, Write},
    os::fd::OwnedFd,
    thread,
    time::{Duration, Instant},
};

use super::{Client, string_arg, u32s};

const MIME: &str = "text/plain;charset=utf-8";

pub struct Clipboard {
    manager: u32,
    device: u32,
    control: bool,
}

impl Clipboard {
    pub fn bind(client: &mut Client, control: bool) -> Self {
        let manager = client.bind(
            if control {
                "zwlr_data_control_manager_v1"
            } else {
                "wl_data_device_manager"
            },
            if control { 2 } else { 3 },
        );
        let seat = client.bind("wl_seat", 5);
        let device = client.alloc();
        client.request(manager, 1, &u32s(&[device, seat]));
        client.sync();
        client.fds.clear();
        Self {
            manager,
            device,
            control,
        }
    }

    pub fn offer(&self, client: &mut Client) -> u32 {
        let source = client.alloc();
        client.request(self.manager, 0, &u32s(&[source]));
        client.request(source, 0, &string_arg(MIME));
        let args = if self.control {
            u32s(&[source])
        } else {
            u32s(&[source, 0])
        };
        client.request(self.device, u16::from(!self.control), &args);
        source
    }

    pub fn serve(&self, client: &mut Client, source: u32, text: &str) {
        let message = client.read_until(|message| {
            message.object == source && message.opcode == u16::from(!self.control)
        });
        let len = message.u32_at(0) as usize;
        assert_eq!(&message.body[4..4 + len - 1], MIME.as_bytes());
        assert!(!client.fds.is_empty(), "clipboard send omitted its fd");
        File::from(client.fds.remove(0))
            .write_all(text.as_bytes())
            .unwrap();
    }

    pub fn selection(&self, client: &mut Client) -> u32 {
        client
            .read_until(|message| {
                message.object == self.device
                    && message.opcode == if self.control { 1 } else { 5 }
                    && message.u32_at(0) != 0
            })
            .u32_at(0)
    }

    pub fn receive(&self, client: &Client, offer: u32) -> Vec<u8> {
        let (read, write) = rustix::pipe::pipe().unwrap();
        client.send_fd(offer, u16::from(!self.control), &string_arg(MIME), &write);
        drop(write);
        read_text(read)
    }
}

pub fn read_text(fd: OwnedFd) -> Vec<u8> {
    let flags = rustix::fs::fcntl_getfl(&fd).unwrap();
    rustix::fs::fcntl_setfl(&fd, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
    let mut file = File::from(fd);
    let mut text = Vec::new();
    let mut bytes = [0; 4096];
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline, "clipboard transfer stalled");
        match file.read(&mut bytes) {
            Ok(0) => return text,
            Ok(read) => text.extend_from_slice(&bytes[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("clipboard read failed: {error}"),
        }
    }
}
