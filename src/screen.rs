//! The screen as a stream: what changed, when, as PNG rectangles.
//!
//! X DAMAGE reports the rectangles that changed on the root window, so an
//! idle desktop costs nothing and a busy one costs only the pixels that
//! moved. One thread owns the connections, unions the reports, and at most
//! twenty times a second reads the dirty rectangle, encodes it, and hands it
//! to every viewer. A viewer that just arrived, or fell behind, gets the
//! whole screen next. With nobody watching, damage is cleared and nothing is
//! read or encoded.
//!
//! Events and pixels travel on separate connections. Reading the screen with
//! GetImage on the connection that receives DamageNotify is not safe: the
//! server removes its software cursor from the framebuffer while it serves
//! GetImage, that is damage, and the event lands in the middle of the reply.
//! The client then loses the sequence numbers and waits forever. So damage
//! arrives on one connection and pixels are fetched on another, through
//! MIT-SHM into a memory file the server writes directly, or by GetImage
//! when the extension is missing. If a connection still fails, the stream is
//! reopened and viewers see the whole screen again.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::broadcast;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::damage::{ConnectionExt as _, ReportLevel};
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xproto::{ImageFormat, Rectangle, Screen as XScreen};
use x11rb::rust_connection::RustConnection;

use crate::x11::{self, Screenshot};

const FRAME_INTERVAL: Duration = Duration::from_millis(50);
const IDLE_POLL: Duration = Duration::from_millis(10);
/// Frames a slow viewer may fall behind by before it is sent the whole screen.
const BACKLOG: usize = 8;
/// How long the stream waits before reopening after its connection failed.
const REOPEN_DELAY: Duration = Duration::from_secs(1);

pub struct Frame {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub screen_width: u16,
    pub screen_height: u16,
    pub png: Vec<u8>,
}

impl Frame {
    /// The wire form: six little-endian u16 — x, y, width, height, screen
    /// width, screen height — then the PNG.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(12 + self.png.len());
        for value in [
            self.x,
            self.y,
            self.width,
            self.height,
            self.screen_width,
            self.screen_height,
        ] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&self.png);
        bytes
    }
}

#[derive(Clone)]
pub struct Screen {
    frames: broadcast::Sender<Arc<Frame>>,
    full: Arc<AtomicBool>,
}

impl Screen {
    pub fn start(display: &str) -> Result<Self, String> {
        let (frames, _) = broadcast::channel(BACKLOG);
        let screen = Self {
            frames,
            full: Arc::new(AtomicBool::new(true)),
        };
        let display = display.to_owned();
        let streamer = screen.clone();
        std::thread::Builder::new()
            .name("screen".to_owned())
            .spawn(move || {
                loop {
                    match stream(&display, &streamer) {
                        Ok(()) => return,
                        Err(error) => {
                            eprintln!("toad-computer: screen: {error}; reopening the stream");
                        }
                    }
                    std::thread::sleep(REOPEN_DELAY);
                    streamer.full.store(true, Ordering::SeqCst);
                }
            })
            .map_err(|error| format!("spawn screen thread: {error}"))?;
        Ok(screen)
    }

    /// A new viewer. The next frame is the whole screen.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Frame>> {
        self.full.store(true, Ordering::SeqCst);
        self.frames.subscribe()
    }

    /// A viewer fell behind and may hold a stale rectangle.
    pub fn request_full(&self) {
        self.full.store(true, Ordering::SeqCst);
    }
}

fn stream(display: &str, screen: &Screen) -> Result<(), String> {
    let (events, screen_number) =
        x11rb::connect(Some(display)).map_err(|error| format!("x11 connect: {error}"))?;
    let root_screen = events.setup().roots[screen_number].clone();
    let whole = Rectangle {
        x: 0,
        y: 0,
        width: root_screen.width_in_pixels,
        height: root_screen.height_in_pixels,
    };
    events
        .damage_query_version(1, 1)
        .map_err(|error| error.to_string())?
        .reply()
        .map_err(|error| format!("DAMAGE: {error}"))?;
    let damage = events.generate_id().map_err(|error| error.to_string())?;
    events
        .damage_create(damage, root_screen.root, ReportLevel::BOUNDING_BOX)
        .map_err(|error| error.to_string())?;
    events.flush().map_err(|error| error.to_string())?;
    let mut capture = Capture::open(display, &whole)?;

    let mut dirty: Option<Rectangle> = None;
    let mut last_frame = Instant::now() - FRAME_INTERVAL;
    loop {
        while let Some(event) = events
            .poll_for_event()
            .map_err(|error| format!("display connection lost: {error}"))?
        {
            if let Event::DamageNotify(notify) = event {
                dirty = Some(union(dirty, notify.area));
            }
        }
        let wanted = screen.full.load(Ordering::SeqCst) || dirty.is_some();
        if !wanted || last_frame.elapsed() < FRAME_INTERVAL {
            std::thread::sleep(IDLE_POLL);
            continue;
        }
        // Cleared before the read: anything drawn from here on is the next frame's.
        events
            .damage_subtract(damage, x11rb::NONE, x11rb::NONE)
            .map_err(|error| error.to_string())?;
        events.flush().map_err(|error| error.to_string())?;
        let full = screen.full.swap(false, Ordering::SeqCst);
        let region = if full {
            whole
        } else {
            clip(dirty.take().unwrap_or(whole), whole)
        };
        dirty = None;
        last_frame = Instant::now();
        if screen.frames.receiver_count() == 0 {
            continue;
        }
        let shot = capture.grab(region)?;
        let png = x11::encode_png(
            shot.width,
            shot.height,
            &shot.rgba,
            png::Compression::Fastest,
        )?;
        let _ = screen.frames.send(Arc::new(Frame {
            x: region.x as u16,
            y: region.y as u16,
            width: region.width,
            height: region.height,
            screen_width: whole.width,
            screen_height: whole.height,
            png,
        }));
    }
}

/// The connection pixels come over. It selects no events, so nothing can
/// land inside a reply.
struct Capture {
    connection: RustConnection,
    root_screen: XScreen,
    shared: Option<Shared>,
}

impl Capture {
    fn open(display: &str, whole: &Rectangle) -> Result<Self, String> {
        let (connection, screen_number) =
            x11rb::connect(Some(display)).map_err(|error| format!("x11 connect: {error}"))?;
        let root_screen = connection.setup().roots[screen_number].clone();
        let bytes = usize::from(whole.width) * usize::from(whole.height) * 4;
        let shared = match Shared::attach(&connection, bytes) {
            Ok(shared) => Some(shared),
            Err(error) => {
                eprintln!(
                    "toad-computer: screen: MIT-SHM unavailable ({error}); reading the screen over the socket"
                );
                None
            }
        };
        Ok(Self {
            connection,
            root_screen,
            shared,
        })
    }

    fn grab(&mut self, region: Rectangle) -> Result<Screenshot, String> {
        let Some(shared) = &self.shared else {
            return x11::grab(
                &self.connection,
                &self.root_screen,
                region.x,
                region.y,
                region.width,
                region.height,
            );
        };
        let reply = self
            .connection
            .shm_get_image(
                self.root_screen.root,
                region.x,
                region.y,
                region.width,
                region.height,
                u32::MAX,
                u8::from(ImageFormat::Z_PIXMAP),
                shared.segment,
                0,
            )
            .map_err(|error| format!("ShmGetImage: {error}"))?
            .reply()
            .map_err(|error| format!("ShmGetImage: {error}"))?;
        let size = reply.size as usize;
        if size > shared.len {
            return Err(format!(
                "ShmGetImage wrote {size} bytes into a {} byte segment",
                shared.len
            ));
        }
        // The server has finished writing when its reply arrives; the mapping
        // is ours alone until the next request.
        let data = unsafe { std::slice::from_raw_parts(shared.map, size) };
        x11::unpack(
            self.connection.setup(),
            reply.depth,
            region.width,
            region.height,
            data,
        )
    }
}

/// A memory file both sides have mapped: the server writes pixels into it
/// on ShmGetImage and only a 32-byte reply crosses the socket.
struct Shared {
    segment: u32,
    map: *mut u8,
    len: usize,
    _file: OwnedFd,
}

impl Shared {
    fn attach(connection: &RustConnection, len: usize) -> Result<Self, String> {
        connection
            .shm_query_version()
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| error.to_string())?;
        let raw = unsafe { libc::memfd_create(c"toad-screen".as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            return Err(format!("memfd_create: {}", std::io::Error::last_os_error()));
        }
        let file = unsafe { OwnedFd::from_raw_fd(raw) };
        if unsafe { libc::ftruncate(file.as_raw_fd(), len as libc::off_t) } < 0 {
            return Err(format!("ftruncate: {}", std::io::Error::last_os_error()));
        }
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if map == libc::MAP_FAILED {
            return Err(format!("mmap: {}", std::io::Error::last_os_error()));
        }
        let shared = Self {
            segment: connection
                .generate_id()
                .map_err(|error| error.to_string())?,
            map: map.cast::<u8>(),
            len,
            _file: file,
        };
        let for_server = shared
            ._file
            .try_clone()
            .map_err(|error| format!("dup: {error}"))?;
        connection
            .shm_attach_fd(shared.segment, for_server, false)
            .map_err(|error| error.to_string())?
            .check()
            .map_err(|error| format!("ShmAttachFd: {error}"))?;
        Ok(shared)
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.map.cast(), self.len);
        }
    }
}

// The mapping is only ever touched from the screen thread.
unsafe impl Send for Shared {}

fn union(current: Option<Rectangle>, added: Rectangle) -> Rectangle {
    let Some(current) = current else {
        return added;
    };
    let left = i32::from(current.x).min(i32::from(added.x));
    let top = i32::from(current.y).min(i32::from(added.y));
    let right = (i32::from(current.x) + i32::from(current.width))
        .max(i32::from(added.x) + i32::from(added.width));
    let bottom = (i32::from(current.y) + i32::from(current.height))
        .max(i32::from(added.y) + i32::from(added.height));
    Rectangle {
        x: left as i16,
        y: top as i16,
        width: (right - left) as u16,
        height: (bottom - top) as u16,
    }
}

fn clip(region: Rectangle, bounds: Rectangle) -> Rectangle {
    let x = region.x.max(0);
    let y = region.y.max(0);
    let right = (i32::from(region.x) + i32::from(region.width)).min(i32::from(bounds.width));
    let bottom = (i32::from(region.y) + i32::from(region.height)).min(i32::from(bounds.height));
    Rectangle {
        x,
        y,
        width: (right - i32::from(x)).max(1) as u16,
        height: (bottom - i32::from(y)).max(1) as u16,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_union_is_the_bounding_box() {
        let first = Rectangle {
            x: 10,
            y: 10,
            width: 10,
            height: 10,
        };
        let second = Rectangle {
            x: 15,
            y: 0,
            width: 30,
            height: 5,
        };
        let both = union(Some(first), second);
        assert_eq!((both.x, both.y, both.width, both.height), (10, 0, 35, 20));
    }

    #[test]
    fn a_frame_leads_with_its_geometry() {
        let frame = Frame {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
            screen_width: 5,
            screen_height: 6,
            png: vec![0x89],
        };
        assert_eq!(
            frame.encode(),
            vec![1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0, 0x89]
        );
    }
}
