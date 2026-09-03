//! The person's hands on the desktop: pointer and keyboard events injected
//! into the X server with XTEST.
//!
//! A browser key event names a key ("a", "Enter", "Shift"); the server wants
//! a keycode. Keysyms bridge the two: the name becomes a keysym, the keyboard
//! mapping says which keycode carries it, and a keysym no keycode carries is
//! given a spare keycode for the life of the session, the way xdotool does.
//! A person's modifiers arrive as their own key events, so pressing the
//! keycode that carries `a` while Shift is down is how they type `A`. The
//! agent types text instead, so for it the keyboard mapping also says whether
//! the keysym sits in a shifted column, and Shift is held around the press.

use std::collections::HashMap;
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ConnectionExt, KEY_PRESS_EVENT, KEY_RELEASE_EVENT,
    Keycode, Keysym, MOTION_NOTIFY_EVENT, Window,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

const SHIFT: Keysym = 0xffe1;
const CONTROL: Keysym = 0xffe3;
const ALT: Keysym = 0xffe9;
const SUPER: Keysym = 0xffeb;
/// Between a press and its release, and between typed characters, so a
/// client that reads its queue in order still sees two events.
const KEY_GAP: Duration = Duration::from_millis(2);
const CLICK_GAP: Duration = Duration::from_millis(60);

pub struct Hands {
    connection: RustConnection,
    root: Window,
    min_keycode: Keycode,
    per_keycode: usize,
    keysyms: Vec<Keysym>,
    spare: Vec<Keycode>,
    remapped: HashMap<Keysym, Keycode>,
}

impl Hands {
    pub fn new(display: &str) -> Result<Self, String> {
        let (connection, screen_number) =
            x11rb::connect(Some(display)).map_err(|error| format!("x11 connect: {error}"))?;
        let root = connection.setup().roots[screen_number].root;
        let min_keycode = connection.setup().min_keycode;
        let max_keycode = connection.setup().max_keycode;
        let mapping = connection
            .get_keyboard_mapping(min_keycode, max_keycode - min_keycode + 1)
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| format!("keyboard mapping: {error}"))?;
        let per_keycode = usize::from(mapping.keysyms_per_keycode);
        let spare = (min_keycode..=max_keycode)
            .filter(|keycode| {
                let start = usize::from(keycode - min_keycode) * per_keycode;
                mapping.keysyms[start..start + per_keycode]
                    .iter()
                    .all(|keysym| *keysym == 0)
            })
            .collect();
        Ok(Self {
            connection,
            root,
            min_keycode,
            per_keycode,
            keysyms: mapping.keysyms,
            spare,
            remapped: HashMap::new(),
        })
    }

    pub fn move_to(&self, x: i16, y: i16) -> Result<(), String> {
        self.fake(MOTION_NOTIFY_EVENT, 0, x, y)
    }

    pub fn button(&self, button: u8, down: bool) -> Result<(), String> {
        let kind = if down {
            BUTTON_PRESS_EVENT
        } else {
            BUTTON_RELEASE_EVENT
        };
        self.fake(kind, button, 0, 0)
    }

    pub fn key(&mut self, name: &str, down: bool) -> Result<(), String> {
        let Some(keysym) = keysym_for(name) else {
            return Ok(());
        };
        let keycode = self.keycode_for(keysym)?;
        let kind = if down {
            KEY_PRESS_EVENT
        } else {
            KEY_RELEASE_EVENT
        };
        self.fake(kind, keycode, 0, 0)
    }

    /// `count` clicks of `button` where the pointer is.
    pub fn click(&self, button: u8, count: u32) -> Result<(), String> {
        for index in 0..count {
            if index > 0 {
                std::thread::sleep(CLICK_GAP);
            }
            self.button(button, true)?;
            std::thread::sleep(KEY_GAP);
            self.button(button, false)?;
        }
        Ok(())
    }

    /// Text as a person would type it, one character at a time, Shift held
    /// for the characters that need it. A newline is Return and a tab is Tab.
    pub fn type_text(&mut self, text: &str) -> Result<(), String> {
        for character in text.chars() {
            let keysym = match character {
                '\n' => 0xff0d,
                '\t' => 0xff09,
                other => match keysym_for(&other.to_string()) {
                    Some(keysym) => keysym,
                    None => continue,
                },
            };
            let (keycode, shifted) = self.key_for(keysym)?;
            let shift = if shifted {
                Some(self.keycode_for(SHIFT)?)
            } else {
                None
            };
            if let Some(shift) = shift {
                self.fake(KEY_PRESS_EVENT, shift, 0, 0)?;
            }
            self.fake(KEY_PRESS_EVENT, keycode, 0, 0)?;
            std::thread::sleep(KEY_GAP);
            self.fake(KEY_RELEASE_EVENT, keycode, 0, 0)?;
            if let Some(shift) = shift {
                self.fake(KEY_RELEASE_EVENT, shift, 0, 0)?;
            }
            std::thread::sleep(KEY_GAP);
        }
        Ok(())
    }

    /// A chord such as `ctrl+shift+t` or `Return`: modifiers down, the key
    /// pressed and released, modifiers up in reverse.
    pub fn combo(&mut self, combo: &str) -> Result<(), String> {
        let mut parts: Vec<&str> = combo.split('+').map(str::trim).collect();
        let key = parts
            .pop()
            .filter(|key| !key.is_empty())
            .ok_or_else(|| "combo is required".to_owned())?;
        let mut modifiers = Vec::new();
        for part in parts {
            let keysym = match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => CONTROL,
                "shift" => SHIFT,
                "alt" | "option" => ALT,
                "super" | "meta" | "cmd" | "command" | "win" => SUPER,
                other => return Err(format!("unknown modifier {other:?}")),
            };
            modifiers.push(self.keycode_for(keysym)?);
        }
        let keysym = keysym_named(key).ok_or_else(|| format!("unknown key {key:?}"))?;
        let (keycode, shifted) = self.key_for(keysym)?;
        let shift = self.keycode_for(SHIFT)?;
        if shifted && !modifiers.contains(&shift) {
            modifiers.push(shift);
        }
        for modifier in &modifiers {
            self.fake(KEY_PRESS_EVENT, *modifier, 0, 0)?;
        }
        self.fake(KEY_PRESS_EVENT, keycode, 0, 0)?;
        std::thread::sleep(KEY_GAP);
        self.fake(KEY_RELEASE_EVENT, keycode, 0, 0)?;
        for modifier in modifiers.iter().rev() {
            self.fake(KEY_RELEASE_EVENT, *modifier, 0, 0)?;
        }
        Ok(())
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<(), String> {
        self.connection
            .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, x, y, 0)
            .map_err(|error| error.to_string())?;
        self.connection.flush().map_err(|error| error.to_string())
    }

    /// Any keycode that carries the keysym in some column.
    fn keycode_for(&mut self, keysym: Keysym) -> Result<Keycode, String> {
        self.key_for(keysym).map(|(keycode, _)| keycode)
    }

    /// The keycode that carries the keysym, and whether it sits in the
    /// shifted column. The first column wins; the second needs Shift.
    fn key_for(&mut self, keysym: Keysym) -> Result<(Keycode, bool), String> {
        if let Some(keycode) = self.remapped.get(&keysym) {
            return Ok((*keycode, false));
        }
        for column in [0, 1] {
            let found = self
                .keysyms
                .chunks(self.per_keycode)
                .position(|columns| columns.get(column) == Some(&keysym))
                .map(|index| self.min_keycode + index as u8);
            if let Some(keycode) = found {
                return Ok((keycode, column == 1));
            }
        }
        let found = self
            .keysyms
            .chunks(self.per_keycode)
            .position(|columns| columns.contains(&keysym))
            .map(|index| self.min_keycode + index as u8);
        if let Some(keycode) = found {
            return Ok((keycode, false));
        }
        let keycode = self
            .spare
            .pop()
            .ok_or_else(|| format!("no spare keycode for keysym {keysym:#x}"))?;
        let columns = vec![keysym; self.per_keycode];
        self.connection
            .change_keyboard_mapping(1, keycode, self.per_keycode as u8, &columns)
            .map_err(|error| error.to_string())?
            .check()
            .map_err(|error| format!("remap keycode: {error}"))?;
        let start = usize::from(keycode - self.min_keycode) * self.per_keycode;
        self.keysyms[start..start + self.per_keycode].copy_from_slice(&columns);
        self.remapped.insert(keysym, keycode);
        Ok((keycode, false))
    }
}

/// The keysym for a key named the X way (`Return`, `Page_Up`, `space`), the
/// browser way (`Enter`, `PageUp`), or as one character.
pub fn keysym_named(name: &str) -> Option<Keysym> {
    let x_name = match name {
        "Return" | "KP_Enter" => Some(0xff0d),
        "BackSpace" => Some(0xff08),
        "space" => Some(0x20),
        "Up" => Some(0xff52),
        "Down" => Some(0xff54),
        "Left" => Some(0xff51),
        "Right" => Some(0xff53),
        "Page_Up" | "Prior" => Some(0xff55),
        "Page_Down" | "Next" => Some(0xff56),
        "Menu" => Some(0xff67),
        "Print" => Some(0xff61),
        "Super_L" | "Super" => Some(SUPER),
        "Shift_L" => Some(SHIFT),
        "Control_L" => Some(CONTROL),
        "Alt_L" => Some(ALT),
        "minus" => Some(0x2d),
        "plus" => Some(0x2b),
        "equal" => Some(0x3d),
        "comma" => Some(0x2c),
        "period" => Some(0x2e),
        "slash" => Some(0x2f),
        "backslash" => Some(0x5c),
        "semicolon" => Some(0x3b),
        "apostrophe" => Some(0x27),
        "grave" => Some(0x60),
        "bracketleft" => Some(0x5b),
        "bracketright" => Some(0x5d),
        _ => None,
    };
    x_name.or_else(|| keysym_for(name))
}

/// The X keysym for a browser `KeyboardEvent.key`. A single character is
/// its own keysym below U+0100 and a Unicode keysym above; a named key is
/// looked up. Dead and unidentified keys are nothing to press.
pub fn keysym_for(name: &str) -> Option<Keysym> {
    let mut chars = name.chars();
    if let (Some(character), None) = (chars.next(), chars.next()) {
        let code = character as u32;
        if code < 0x20 {
            return None;
        }
        return Some(if code < 0x100 {
            code
        } else {
            0x0100_0000 | code
        });
    }
    if let Some(number) = name.strip_prefix('F')
        && let Ok(number) = number.parse::<u32>()
        && (1..=12).contains(&number)
    {
        return Some(0xffbe + number - 1);
    }
    Some(match name {
        "Enter" => 0xff0d,
        "Tab" => 0xff09,
        "Backspace" => 0xff08,
        "Escape" => 0xff1b,
        "Delete" => 0xffff,
        "Insert" => 0xff63,
        "Home" => 0xff50,
        "End" => 0xff57,
        "PageUp" => 0xff55,
        "PageDown" => 0xff56,
        "ArrowLeft" => 0xff51,
        "ArrowUp" => 0xff52,
        "ArrowRight" => 0xff53,
        "ArrowDown" => 0xff54,
        "Shift" => 0xffe1,
        "Control" => 0xffe3,
        "Alt" => 0xffe9,
        "AltGraph" => 0xfe03,
        "Meta" => 0xffeb,
        "CapsLock" => 0xffe5,
        "NumLock" => 0xff7f,
        "ScrollLock" => 0xff14,
        "ContextMenu" => 0xff67,
        "PrintScreen" => 0xff61,
        "Pause" => 0xff13,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn characters_are_their_own_keysyms() {
        assert_eq!(keysym_for("a"), Some(0x61));
        assert_eq!(keysym_for("A"), Some(0x41));
        assert_eq!(keysym_for(" "), Some(0x20));
        assert_eq!(keysym_for("é"), Some(0xe9));
        assert_eq!(keysym_for("€"), Some(0x0100_20ac));
    }

    #[test]
    fn x_names_and_browser_names_both_resolve() {
        assert_eq!(keysym_named("Return"), keysym_named("Enter"));
        assert_eq!(keysym_named("Page_Up"), keysym_named("PageUp"));
        assert_eq!(keysym_named("space"), Some(0x20));
        assert_eq!(keysym_named("l"), Some(0x6c));
        assert_eq!(keysym_named("Bogus"), None);
    }

    #[test]
    fn named_keys_are_looked_up() {
        assert_eq!(keysym_for("Enter"), Some(0xff0d));
        assert_eq!(keysym_for("F5"), Some(0xffc2));
        assert_eq!(keysym_for("F13"), None);
        assert_eq!(keysym_for("Dead"), None);
    }
}
