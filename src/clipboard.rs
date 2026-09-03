//! The clipboard, owned in-process. One thread holds a window, takes the
//! CLIPBOARD selection when the agent writes, answers the paste requests
//! of other clients, and asks the current owner for text when the agent
//! reads. X has no clipboard store, only an owner who answers; with no xclip
//! the agent is that owner.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt, CreateWindowAux, EventMask, PropMode, SELECTION_NOTIFY_EVENT,
    SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

const IDLE_POLL: Duration = Duration::from_millis(5);
/// How long an owner gets to answer a read.
const OWNER_WAIT: Duration = Duration::from_secs(2);
const REPLY_WAIT: Duration = Duration::from_secs(5);

enum Command {
    Write(String, mpsc::Sender<Result<(), String>>),
    Read(mpsc::Sender<Result<String, String>>),
}

#[derive(Clone)]
pub struct Clipboard {
    commands: mpsc::Sender<Command>,
}

impl Clipboard {
    pub fn start(display: &str) -> Result<Self, String> {
        let (commands, incoming) = mpsc::channel();
        let (ready, started) = mpsc::channel();
        let display = display.to_owned();
        std::thread::Builder::new()
            .name("clipboard".to_owned())
            .spawn(move || match Owner::new(&display) {
                Ok(mut owner) => {
                    let _ = ready.send(Ok(()));
                    if let Err(error) = owner.serve(incoming) {
                        eprintln!("toad-computer: clipboard: {error}");
                    }
                }
                Err(error) => {
                    let _ = ready.send(Err(error));
                }
            })
            .map_err(|error| format!("spawn clipboard thread: {error}"))?;
        started
            .recv()
            .map_err(|_| "the clipboard thread died".to_owned())??;
        Ok(Self { commands })
    }

    pub fn write(&self, text: &str) -> Result<(), String> {
        let (reply, answer) = mpsc::channel();
        self.commands
            .send(Command::Write(text.to_owned(), reply))
            .map_err(|_| "the clipboard thread is gone".to_owned())?;
        answer
            .recv_timeout(REPLY_WAIT)
            .map_err(|_| "the clipboard did not answer".to_owned())?
    }

    pub fn read(&self) -> Result<String, String> {
        let (reply, answer) = mpsc::channel();
        self.commands
            .send(Command::Read(reply))
            .map_err(|_| "the clipboard thread is gone".to_owned())?;
        answer
            .recv_timeout(REPLY_WAIT)
            .map_err(|_| "the clipboard did not answer".to_owned())?
    }
}

struct Owner {
    connection: RustConnection,
    window: Window,
    clipboard: Atom,
    utf8: Atom,
    targets: Atom,
    landing: Atom,
    held: Option<String>,
}

impl Owner {
    fn new(display: &str) -> Result<Self, String> {
        let (connection, screen_number) =
            x11rb::connect(Some(display)).map_err(|error| format!("x11 connect: {error}"))?;
        let root = connection.setup().roots[screen_number].root;
        let window = connection
            .generate_id()
            .map_err(|error| error.to_string())?;
        connection
            .create_window(
                0,
                window,
                root,
                -1,
                -1,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new()
                    .override_redirect(1)
                    .event_mask(EventMask::PROPERTY_CHANGE),
            )
            .map_err(|error| error.to_string())?;
        let atom = |name: &[u8]| -> Result<Atom, String> {
            connection
                .intern_atom(false, name)
                .map_err(|error| error.to_string())?
                .reply()
                .map(|reply| reply.atom)
                .map_err(|error| error.to_string())
        };
        let owner = Self {
            clipboard: atom(b"CLIPBOARD")?,
            utf8: atom(b"UTF8_STRING")?,
            targets: atom(b"TARGETS")?,
            landing: atom(b"TOAD_CLIPBOARD")?,
            connection,
            window,
            held: None,
        };
        owner
            .connection
            .flush()
            .map_err(|error| error.to_string())?;
        Ok(owner)
    }

    fn serve(&mut self, commands: mpsc::Receiver<Command>) -> Result<(), String> {
        loop {
            let mut busy = false;
            while let Some(event) = self.poll()? {
                busy = true;
                self.handle(event)?;
            }
            match commands.try_recv() {
                Ok(Command::Write(text, reply)) => {
                    busy = true;
                    let _ = reply.send(self.take(text));
                }
                Ok(Command::Read(reply)) => {
                    busy = true;
                    let _ = reply.send(self.fetch());
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
            if !busy {
                std::thread::sleep(IDLE_POLL);
            }
        }
    }

    fn poll(&self) -> Result<Option<Event>, String> {
        self.connection
            .poll_for_event()
            .map_err(|error| format!("display connection lost: {error}"))
    }

    fn handle(&mut self, event: Event) -> Result<(), String> {
        match event {
            Event::SelectionRequest(request) => self.answer(request),
            Event::SelectionClear(_) => {
                self.held = None;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn take(&mut self, text: String) -> Result<(), String> {
        self.held = Some(text);
        self.connection
            .set_selection_owner(self.window, self.clipboard, CURRENT_TIME)
            .map_err(|error| error.to_string())?;
        self.connection.flush().map_err(|error| error.to_string())
    }

    /// Another client wants what we hold: TARGETS, or the text as UTF-8 or
    /// Latin-1. Anything else is refused with an empty property.
    fn answer(&self, request: SelectionRequestEvent) -> Result<(), String> {
        let property = if request.property == NONE {
            request.target
        } else {
            request.property
        };
        let mut granted = property;
        match &self.held {
            Some(_) if request.target == self.targets => {
                self.connection
                    .change_property32(
                        PropMode::REPLACE,
                        request.requestor,
                        property,
                        AtomEnum::ATOM,
                        &[self.targets, self.utf8, AtomEnum::STRING.into()],
                    )
                    .map_err(|error| error.to_string())?;
            }
            Some(text) if request.target == self.utf8 => {
                self.connection
                    .change_property8(
                        PropMode::REPLACE,
                        request.requestor,
                        property,
                        self.utf8,
                        text.as_bytes(),
                    )
                    .map_err(|error| error.to_string())?;
            }
            Some(text) if request.target == u32::from(AtomEnum::STRING) => {
                let latin1: Vec<u8> = text
                    .chars()
                    .map(|character| u8::try_from(character as u32).unwrap_or(b'?'))
                    .collect();
                self.connection
                    .change_property8(
                        PropMode::REPLACE,
                        request.requestor,
                        property,
                        AtomEnum::STRING,
                        &latin1,
                    )
                    .map_err(|error| error.to_string())?;
            }
            _ => granted = NONE,
        }
        let notify = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: request.time,
            requestor: request.requestor,
            selection: request.selection,
            target: request.target,
            property: granted,
        };
        self.connection
            .send_event(false, request.requestor, EventMask::NO_EVENT, notify)
            .map_err(|error| error.to_string())?;
        self.connection.flush().map_err(|error| error.to_string())
    }

    /// What the clipboard holds: ours if we own it, else what the owner
    /// answers with, else nothing.
    fn fetch(&mut self) -> Result<String, String> {
        if let Some(text) = &self.held {
            return Ok(text.clone());
        }
        let owner = self
            .connection
            .get_selection_owner(self.clipboard)
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| error.to_string())?
            .owner;
        if owner == NONE {
            return Ok(String::new());
        }
        self.connection
            .convert_selection(
                self.window,
                self.clipboard,
                self.utf8,
                self.landing,
                CURRENT_TIME,
            )
            .map_err(|error| error.to_string())?;
        self.connection.flush().map_err(|error| error.to_string())?;
        let deadline = Instant::now() + OWNER_WAIT;
        loop {
            while let Some(event) = self.poll()? {
                if let Event::SelectionNotify(notify) = &event
                    && notify.requestor == self.window
                {
                    if notify.property == NONE {
                        return Err("the clipboard owner offered no text".to_owned());
                    }
                    let reply = self
                        .connection
                        .get_property(true, self.window, self.landing, AtomEnum::ANY, 0, u32::MAX)
                        .map_err(|error| error.to_string())?
                        .reply()
                        .map_err(|error| error.to_string())?;
                    if reply.format != 8 {
                        return Err("the clipboard content is too large to read at once".to_owned());
                    }
                    return Ok(String::from_utf8_lossy(&reply.value).into_owned());
                }
                self.handle(event)?;
            }
            if Instant::now() > deadline {
                return Err("the clipboard owner did not answer".to_owned());
            }
            std::thread::sleep(IDLE_POLL);
        }
    }
}
