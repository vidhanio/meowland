mod support;

use std::time::Duration;

use meowland::protocol::{PaneToServer, Show};
use support::{
    Client, Pane, Server,
    dmabuf::{DmabufProtocol, Producer, wait_release},
    hello,
};

const WIDTH: u32 = 4;
const HEIGHT: u32 = 3;
const BACKGROUND: [u8; 3] = [32, 64, 96];

fn pattern() -> Vec<[u8; 4]> {
    vec![
        [201, 17, 39, 255],
        [60, 31, 9, 128],
        [0, 0, 0, 0],
        [4, 20, 51, 64],
        [11, 203, 41, 255],
        [17, 35, 53, 128],
        [71, 19, 101, 255],
        [12, 24, 36, 64],
        [9, 27, 221, 255],
        [43, 5, 61, 128],
        [7, 83, 29, 255],
        [41, 13, 3, 64],
    ]
}

fn shm_pixels(rgba: &[[u8; 4]]) -> Vec<u8> {
    rgba.iter()
        .flat_map(|pixel| [pixel[2], pixel[1], pixel[0], pixel[3]])
        .collect()
}

fn composite(rgba: &[[u8; 4]], inverted: bool) -> Vec<u8> {
    let width = WIDTH + 2;
    let height = HEIGHT + 2;
    let mut rgb = BACKGROUND.repeat((width * height) as usize);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let source_y = if inverted { HEIGHT - 1 - y } else { y };
            let pixel = rgba[(source_y * WIDTH + x) as usize];
            let offset = (((y + 1) * width + x + 1) * 3) as usize;
            for channel in 0..3 {
                rgb[offset + channel] = (u32::from(pixel[channel])
                    + (u32::from(BACKGROUND[channel]) * (255 - u32::from(pixel[3])) + 127) / 255)
                    as u8;
            }
        }
    }
    rgb
}

fn assert_shm_frame(server: &Server, title: &str) {
    let mut client = Client::connect(server);
    let window = client.create_toplevel(title, "meowland.dmabuf-test");
    let rgba = pattern();
    let buffer = client.shm_buffer(WIDTH, HEIGHT, WIDTH * 4, &shm_pixels(&rgba));
    client.attach(&window, buffer, WIDTH, HEIGHT);
    assert!(server.wait_for_window(Duration::from_secs(5)));
    let mut pane = Pane::attach(server, hello(WIDTH, HEIGHT, Show::Newest));
    let rgb = rgba
        .iter()
        .flat_map(|pixel| pixel[..3].iter().copied())
        .collect::<Vec<_>>();
    assert_eq!(pane.frame(), (WIDTH, HEIGHT, rgb));
}

#[test]
fn gpu_off_keeps_real_shm_pixels_without_advertising_dmabuf() {
    let server = Server::start_with_env(&[("MEOWLAND_RENDER_NODE", "off")]);
    let client = Client::connect(&server);
    assert!(!client.has_global("zwp_linux_dmabuf_v1"));
    assert_shm_frame(&server, "GPU disabled");
}

#[test]
fn gpu_pixels_alpha_orientation_and_released_buffer_reuse_are_owned() {
    let Some(mut producer) = Producer::discover() else {
        return;
    };
    let server =
        Server::start_with_env(&[("MEOWLAND_RENDER_NODE", producer.node.to_str().unwrap())]);
    let mut client = Client::connect(&server);
    assert!(client.has_global("zwp_linux_dmabuf_v1"), "{}", server.log());
    let protocol = DmabufProtocol::bind(&mut client);
    let mut gpu = producer
        .allocate(WIDTH, HEIGHT, &protocol.formats)
        .expect("allocate advertised GPU buffer");
    let first = pattern();
    let buffer = protocol.create(&mut client, &gpu, 0);

    let width = WIDTH + 2;
    let height = HEIGHT + 2;
    let window = client.create_toplevel("real GPU pixels", "meowland.dmabuf-test");
    let background =
        [BACKGROUND[2], BACKGROUND[1], BACKGROUND[0], 255].repeat((width * height) as usize);
    let root = client.shm_buffer(width, height, width * 4, &background);
    client.attach(&window, root, width, height);
    let surface = client.create_surface();
    let subsurface = client.create_subsurface(surface, window.surface);
    client.subsurface_position(subsurface, 1, 1);
    client.subsurface_desync(subsurface);
    client.commit(window.surface);
    let acquire = producer.paint(&mut gpu, &first).expect("GPU rendering");
    client.attach_surface(surface, buffer, WIDTH, HEIGHT);
    wait_release(&mut client, buffer, &acquire);

    // Overwrite the released allocation before any pane asks for the old frame.
    let second = first.iter().rev().copied().collect::<Vec<_>>();
    let overwrite = producer
        .paint(&mut gpu, &second)
        .expect("reuse released GPU allocation");
    overwrite
        .wait()
        .expect("finish overwrite before inspecting the saved snapshot");
    assert!(server.wait_for_window(Duration::from_secs(5)));
    let mut pane = Pane::attach(&server, hello(width, height, Show::Newest));
    assert_eq!(
        pane.frame(),
        (width, height, composite(&first, false)),
        "released storage changed the saved snapshot"
    );

    pane.send(&PaneToServer::Ack { drawn: true });
    client.attach_surface(surface, buffer, WIDTH, HEIGHT);
    wait_release(&mut client, buffer, &overwrite);
    assert_eq!(
        pane.frame(),
        (width, height, composite(&second, false)),
        "recommitted allocation was not sampled again"
    );

    pane.send(&PaneToServer::Ack { drawn: true });
    let inverted = protocol.create(&mut client, &gpu, 1);
    client.attach_surface(surface, inverted, WIDTH, HEIGHT);
    wait_release(&mut client, inverted, &overwrite);
    assert_eq!(
        pane.frame(),
        (width, height, composite(&second, true)),
        "Y_INVERT did not reverse only the rows"
    );

    client.request(buffer, 0, &[]);
    client.request(inverted, 0, &[]);
    drop(gpu);
    drop(producer);
    pane.send(&PaneToServer::Ack { drawn: false });
    assert_eq!(
        pane.frame(),
        (width, height, composite(&second, true)),
        "destroying released GPU buffers lost compositor-owned pixels"
    );
}

#[test]
fn unsupported_gpu_import_only_disconnects_the_offending_client() {
    let Some(mut producer) = Producer::discover() else {
        return;
    };
    let server =
        Server::start_with_env(&[("MEOWLAND_RENDER_NODE", producer.node.to_str().unwrap())]);
    let mut client = Client::connect(&server);
    let protocol = DmabufProtocol::bind(&mut client);
    let gpu = producer
        .allocate(WIDTH, HEIGHT, &protocol.formats)
        .expect("allocate advertised GPU buffer");
    protocol.reject_unsupported_format(&mut client, &gpu);
    drop(client);
    assert_shm_frame(&server, "survives rejected DMA-BUF");
}
