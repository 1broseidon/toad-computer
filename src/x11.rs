use std::io::Cursor;

use serde::Serialize;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageData, ClientMessageEvent, ConfigureWindowAux, ConnectionExt, EventMask,
    ImageFormat, MapState, Screen, Window as XWindow,
};
use x11rb::rust_connection::RustConnection;

#[derive(Clone, Debug, Serialize)]
pub struct Window {
    pub id: String,
    pub title: String,
    pub class: String,
    pub bounds: [i32; 4],
    pub focused: bool,
}

pub struct Screenshot {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub fn screenshot(display: &str) -> Result<Screenshot, String> {
    let (connection, screen_number) =
        x11rb::connect(Some(display)).map_err(|error| format!("x11 connect: {error}"))?;
    let screen = &connection.setup().roots[screen_number];
    grab(
        &connection,
        screen,
        0,
        0,
        screen.width_in_pixels,
        screen.height_in_pixels,
    )
}

/// The pixels of one rectangle of the root window, as RGBA.
pub fn grab<C: Connection>(
    connection: &C,
    screen: &Screen,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
) -> Result<Screenshot, String> {
    let reply = connection
        .get_image(
            ImageFormat::Z_PIXMAP,
            screen.root,
            x,
            y,
            width,
            height,
            u32::MAX,
        )
        .map_err(|error| format!("XGetImage: {error}"))?
        .reply()
        .map_err(|error| format!("XGetImage: {error}"))?;
    let format = connection
        .setup()
        .pixmap_formats
        .iter()
        .find(|format| format.depth == reply.depth)
        .ok_or_else(|| format!("x11: no pixel format for depth {}", reply.depth))?;
    let bytes_per_pixel = usize::from(format.bits_per_pixel / 8);
    if bytes_per_pixel < 3 {
        return Err(format!(
            "x11: unsupported {} bits per pixel",
            format.bits_per_pixel
        ));
    }
    let width = usize::from(width);
    let height = usize::from(height);
    let stride = (width * usize::from(format.bits_per_pixel)).div_ceil(32) * 4;
    if reply.data.len() < stride * height {
        return Err("x11: image reply was shorter than the rectangle".to_owned());
    }
    let mut rgba = vec![0_u8; width * height * 4];
    for row in 0..height {
        for column in 0..width {
            let source = row * stride + column * bytes_per_pixel;
            let target = (row * width + column) * 4;
            rgba[target] = reply.data[source + 2];
            rgba[target + 1] = reply.data[source + 1];
            rgba[target + 2] = reply.data[source];
            rgba[target + 3] = 255;
        }
    }
    Ok(Screenshot {
        width: width as u32,
        height: height as u32,
        rgba,
    })
}

pub fn scaled_png(display: &str, max_edge: u32) -> Result<Vec<u8>, String> {
    let shot = screenshot(display)?;
    let longer = shot.width.max(shot.height);
    let (width, height, pixels) = if longer <= max_edge {
        (shot.width, shot.height, shot.rgba)
    } else {
        let width = (u64::from(shot.width) * u64::from(max_edge) / u64::from(longer)) as u32;
        let height = (u64::from(shot.height) * u64::from(max_edge) / u64::from(longer)) as u32;
        (
            width.max(1),
            height.max(1),
            scale(&shot, width.max(1), height.max(1)),
        )
    };
    encode_png(width, height, &pixels, png::Compression::default())
}

pub fn raw_png(display: &str) -> Result<Vec<u8>, String> {
    let shot = screenshot(display)?;
    encode_png(
        shot.width,
        shot.height,
        &shot.rgba,
        png::Compression::default(),
    )
}

fn scale(source: &Screenshot, width: u32, height: u32) -> Vec<u8> {
    let mut target = vec![0_u8; width as usize * height as usize * 4];
    for y in 0..height {
        let source_y = u64::from(y) * u64::from(source.height) / u64::from(height);
        for x in 0..width {
            let source_x = u64::from(x) * u64::from(source.width) / u64::from(width);
            let from = (source_y * u64::from(source.width) + source_x) as usize * 4;
            let to = (u64::from(y) * u64::from(width) + u64::from(x)) as usize * 4;
            target[to..to + 4].copy_from_slice(&source.rgba[from..from + 4]);
        }
    }
    target
}

pub fn encode_png(
    width: u32,
    height: u32,
    rgba: &[u8],
    compression: png::Compression,
) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(Cursor::new(&mut bytes), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(compression);
    let mut writer = encoder
        .write_header()
        .map_err(|error| format!("encode PNG: {error}"))?;
    writer
        .write_image_data(rgba)
        .map_err(|error| format!("encode PNG: {error}"))?;
    drop(writer);
    Ok(bytes)
}

pub fn windows(display: &str) -> Result<Vec<Window>, String> {
    let (connection, screen_number) =
        x11rb::connect(Some(display)).map_err(|error| format!("x11 connect: {error}"))?;
    let root = connection.setup().roots[screen_number].root;
    let clients = atom(&connection, b"_NET_CLIENT_LIST")?;
    let active_atom = atom(&connection, b"_NET_ACTIVE_WINDOW")?;
    let utf8 = atom(&connection, b"UTF8_STRING")?;
    let name_atom = atom(&connection, b"_NET_WM_NAME")?;
    let active = property_windows(&connection, root, active_atom)?
        .into_iter()
        .next();
    let ids = property_windows(&connection, root, clients)?;
    let mut result = Vec::new();
    for id in ids {
        let attributes = match connection
            .get_window_attributes(id)
            .map_err(|error| error.to_string())?
            .reply()
        {
            Ok(reply) if reply.map_state == MapState::VIEWABLE => reply,
            _ => continue,
        };
        let _ = attributes;
        let title = property_string(&connection, id, name_atom, utf8)
            .or_else(|_| {
                property_string(
                    &connection,
                    id,
                    AtomEnum::WM_NAME.into(),
                    AtomEnum::STRING.into(),
                )
            })
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let class = property_string(
            &connection,
            id,
            AtomEnum::WM_CLASS.into(),
            AtomEnum::STRING.into(),
        )
        .unwrap_or_default()
        .replace('\0', ".")
        .trim_matches('.')
        .to_owned();
        let geometry = connection
            .get_geometry(id)
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| error.to_string())?;
        let translated = connection
            .translate_coordinates(id, root, 0, 0)
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| error.to_string())?;
        result.push(Window {
            id: format!("0x{id:08x}"),
            title,
            class,
            bounds: [
                i32::from(translated.dst_x),
                i32::from(translated.dst_y),
                i32::from(geometry.width),
                i32::from(geometry.height),
            ],
            focused: active == Some(id),
        });
    }
    Ok(result)
}

/// Ask the window manager, which is this agent's desktop thread, to
/// maximize or restore a window.
pub fn maximize(display: &str, window: &str, enabled: bool) -> Result<(), String> {
    let (connection, root) = connect(display)?;
    let state = atom(&connection, b"_NET_WM_STATE")?;
    let vertical = atom(&connection, b"_NET_WM_STATE_MAXIMIZED_VERT")?;
    let horizontal = atom(&connection, b"_NET_WM_STATE_MAXIMIZED_HORZ")?;
    ask_manager(
        &connection,
        root,
        window_id(window)?,
        state,
        [u32::from(enabled), vertical, horizontal, 1, 0],
    )
}

/// Ask the window manager to focus and raise a window.
pub fn activate(display: &str, window: &str) -> Result<(), String> {
    let (connection, root) = connect(display)?;
    let active = atom(&connection, b"_NET_ACTIVE_WINDOW")?;
    ask_manager(
        &connection,
        root,
        window_id(window)?,
        active,
        [2, 0, 0, 0, 0],
    )
}

/// Ask the window manager to close a window the polite way first.
pub fn close(display: &str, window: &str) -> Result<(), String> {
    let (connection, root) = connect(display)?;
    let close = atom(&connection, b"_NET_CLOSE_WINDOW")?;
    ask_manager(
        &connection,
        root,
        window_id(window)?,
        close,
        [0, 2, 0, 0, 0],
    )
}

/// Move and size a window. The request is redirected to the window
/// manager, which honours it for a window that is not maximized.
pub fn place(
    display: &str,
    window: &str,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> Result<(), String> {
    let (connection, _) = connect(display)?;
    connection
        .configure_window(
            window_id(window)?,
            &ConfigureWindowAux::new()
                .x(x)
                .y(y)
                .width(width.max(1))
                .height(height.max(1)),
        )
        .map_err(|error| error.to_string())?;
    connection.flush().map_err(|error| error.to_string())
}

fn connect(display: &str) -> Result<(RustConnection, XWindow), String> {
    let (connection, screen_number) =
        x11rb::connect(Some(display)).map_err(|error| format!("x11 connect: {error}"))?;
    let root = connection.setup().roots[screen_number].root;
    Ok((connection, root))
}

fn window_id(window: &str) -> Result<XWindow, String> {
    u32::from_str_radix(window.trim_start_matches("0x"), 16)
        .map_err(|_| "window_id must be an X11 window id".to_owned())
}

/// An EWMH client message on the root, which the desktop thread receives
/// because it selected substructure redirection there.
fn ask_manager(
    connection: &RustConnection,
    root: XWindow,
    window: XWindow,
    kind: u32,
    data: [u32; 5],
) -> Result<(), String> {
    let event = ClientMessageEvent::new(32, window, kind, ClientMessageData::from(data));
    connection
        .send_event(
            false,
            root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )
        .map_err(|error| error.to_string())?
        .check()
        .map_err(|error| error.to_string())?;
    connection.flush().map_err(|error| error.to_string())
}

fn atom<C: Connection>(connection: &C, name: &[u8]) -> Result<u32, String> {
    connection
        .intern_atom(false, name)
        .map_err(|error| error.to_string())?
        .reply()
        .map(|reply| reply.atom)
        .map_err(|error| error.to_string())
}

fn property_windows<C: Connection>(
    connection: &C,
    window: XWindow,
    property: u32,
) -> Result<Vec<XWindow>, String> {
    let reply = connection
        .get_property(false, window, property, AtomEnum::WINDOW, 0, u32::MAX)
        .map_err(|error| error.to_string())?
        .reply()
        .map_err(|error| error.to_string())?;
    // A property the window manager has not set yet — `_NET_ACTIVE_WINDOW`
    // on a desktop nothing has focused — comes back with format 0, which is
    // an empty list, not a wrong type.
    if reply.format == 0 {
        return Ok(Vec::new());
    }
    reply
        .value32()
        .map(Iterator::collect)
        .ok_or_else(|| "x11: window property has the wrong type".to_owned())
}

fn property_string<C: Connection>(
    connection: &C,
    window: XWindow,
    property: u32,
    property_type: u32,
) -> Result<String, String> {
    let value = connection
        .get_property(false, window, property, property_type, 0, u32::MAX)
        .map_err(|error| error.to_string())?
        .reply()
        .map_err(|error| error.to_string())?
        .value;
    Ok(String::from_utf8_lossy(&value).into_owned())
}
