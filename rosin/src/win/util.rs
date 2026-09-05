//! Win32 input helpers.
//!
//! Mirrors `mac/util.rs`: turns low-level platform keyboard events into
//! `keyboard-types` [`KeyboardEvent`]s so the rest of Rosin stays platform agnostic.
//!
//! On Windows the raw event is a `WM_KEYDOWN`/`WM_KEYUP` whose `wParam` is a
//! virtual-key code (`VK_*`). Unlike AppKit we do not get a character string from the
//! system, so printable keys go through `MapVirtualKeyW(..., MAPVK_VK_TO_CHAR)` (which
//! returns the character the key generates on the current layout, unshifted and for
//! letters always uppercase); the shift/caps-lock state is then applied locally to
//! recover the shifted symbol. This is accurate for letters/digits and the common
//! ASCII symbols, and is documented as an approximation for non-US layouts. Full IME
//! composition is handled separately in `win/ime.rs` via the IMM32 window messages.

use crate::keyboard_types::{Code, Key, KeyState, KeyboardEvent, Location, Modifiers, NamedKey};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, MAPVK_VK_TO_CHAR, VIRTUAL_KEY, VK_0, VK_1, VK_2, VK_3, VK_4, VK_5, VK_6, VK_7, VK_8, VK_9, VK_A, VK_ADD,
    VK_APPS, VK_B, VK_BACK, VK_BROWSER_BACK, VK_BROWSER_FAVORITES, VK_BROWSER_FORWARD, VK_BROWSER_HOME, VK_BROWSER_REFRESH,
    VK_BROWSER_SEARCH, VK_BROWSER_STOP, VK_C, VK_CAPITAL, VK_CLEAR, VK_CONVERT, VK_D, VK_DECIMAL, VK_DELETE, VK_DIVIDE, VK_DOWN,
    VK_E, VK_END, VK_ESCAPE, VK_EXECUTE, VK_F, VK_F1, VK_F10, VK_F11, VK_F12, VK_F13, VK_F14, VK_F15, VK_F16, VK_F17, VK_F18,
    VK_F19, VK_F2, VK_F20, VK_F21, VK_F22, VK_F23, VK_F24, VK_F3, VK_F4, VK_F5, VK_F6, VK_F7, VK_F8, VK_F9, VK_G, VK_H, VK_HELP,
    VK_HOME, VK_I, VK_INSERT, VK_J, VK_K, VK_KANA, VK_L, VK_LAUNCH_APP1, VK_LAUNCH_APP2, VK_LAUNCH_MAIL, VK_LAUNCH_MEDIA_SELECT,
    VK_LCONTROL, VK_LEFT, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_M, VK_MEDIA_NEXT_TRACK, VK_MEDIA_PLAY_PAUSE, VK_MEDIA_PREV_TRACK,
    VK_MEDIA_STOP, VK_MULTIPLY, VK_N, VK_NEXT, VK_NONCONVERT, VK_NUMLOCK, VK_NUMPAD0, VK_NUMPAD1, VK_NUMPAD2, VK_NUMPAD3,
    VK_NUMPAD4, VK_NUMPAD5, VK_NUMPAD6, VK_NUMPAD7, VK_NUMPAD8, VK_NUMPAD9, VK_O, VK_OEM_1, VK_OEM_102, VK_OEM_2, VK_OEM_3,
    VK_OEM_4, VK_OEM_5, VK_OEM_6, VK_OEM_7, VK_OEM_COMMA, VK_OEM_MINUS, VK_OEM_PERIOD, VK_OEM_PLUS, VK_P, VK_PAUSE, VK_PRIOR,
    VK_Q, VK_R, VK_RCONTROL, VK_RETURN, VK_RIGHT, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_S, VK_SCROLL, VK_SELECT, VK_SEPARATOR, VK_SLEEP, VK_SNAPSHOT, VK_SPACE, VK_SUBTRACT, VK_T, VK_TAB, VK_U, VK_UP, VK_V, VK_VOLUME_DOWN, VK_VOLUME_MUTE,
    VK_VOLUME_UP, VK_W, VK_X, VK_Y, VK_Z,
};

pub(crate) fn convert_key_event(vk: u32, down: bool, repeat: bool, modifiers: Modifiers, is_composing: bool) -> KeyboardEvent {
    let code = convert_code(vk);
    let key = convert_key(code)
        .or_else(|| convert_character(vk, modifiers))
        .unwrap_or(Key::Named(NamedKey::Unidentified));

    KeyboardEvent {
        state: if down { KeyState::Down } else { KeyState::Up },
        key,
        code,
        location: convert_location(code),
        modifiers,
        repeat,
        is_composing,
    }
}

/// `VK_*` -> physical [`Code`]. Virtual key codes describe the physical key on a
/// standard Windows keyboard, so this is a direct translation.
fn convert_code(vk: u32) -> Code {
    let vk = VIRTUAL_KEY(vk as u16);
    match vk {
        VK_0 => Code::Digit0,
        VK_1 => Code::Digit1,
        VK_2 => Code::Digit2,
        VK_3 => Code::Digit3,
        VK_4 => Code::Digit4,
        VK_5 => Code::Digit5,
        VK_6 => Code::Digit6,
        VK_7 => Code::Digit7,
        VK_8 => Code::Digit8,
        VK_9 => Code::Digit9,
        VK_A => Code::KeyA,
        VK_B => Code::KeyB,
        VK_C => Code::KeyC,
        VK_D => Code::KeyD,
        VK_E => Code::KeyE,
        VK_F => Code::KeyF,
        VK_G => Code::KeyG,
        VK_H => Code::KeyH,
        VK_I => Code::KeyI,
        VK_J => Code::KeyJ,
        VK_K => Code::KeyK,
        VK_L => Code::KeyL,
        VK_M => Code::KeyM,
        VK_N => Code::KeyN,
        VK_O => Code::KeyO,
        VK_P => Code::KeyP,
        VK_Q => Code::KeyQ,
        VK_R => Code::KeyR,
        VK_S => Code::KeyS,
        VK_T => Code::KeyT,
        VK_U => Code::KeyU,
        VK_V => Code::KeyV,
        VK_W => Code::KeyW,
        VK_X => Code::KeyX,
        VK_Y => Code::KeyY,
        VK_Z => Code::KeyZ,
        VK_OEM_1 => Code::Semicolon,
        VK_OEM_PLUS => Code::Equal,
        VK_OEM_COMMA => Code::Comma,
        VK_OEM_MINUS => Code::Minus,
        VK_OEM_PERIOD => Code::Period,
        VK_OEM_2 => Code::Slash,
        VK_OEM_3 => Code::Backquote,
        VK_OEM_4 => Code::BracketLeft,
        VK_OEM_5 => Code::Backslash,
        VK_OEM_6 => Code::BracketRight,
        VK_OEM_7 => Code::Quote,
        VK_OEM_102 => Code::IntlBackslash,
        VK_LCONTROL => Code::ControlLeft,
        VK_RCONTROL => Code::ControlRight,
        VK_LSHIFT => Code::ShiftLeft,
        VK_RSHIFT => Code::ShiftRight,
        VK_LMENU => Code::AltLeft,
        VK_RMENU => Code::AltRight,
        VK_LWIN => Code::MetaLeft,
        VK_RWIN => Code::MetaRight,
        VK_APPS => Code::ContextMenu,
        VK_BACK => Code::Backspace,
        VK_TAB => Code::Tab,
        VK_RETURN => Code::Enter,
        VK_CAPITAL => Code::CapsLock,
        VK_ESCAPE => Code::Escape,
        VK_SPACE => Code::Space,
        VK_PRIOR => Code::PageUp,
        VK_NEXT => Code::PageDown,
        VK_END => Code::End,
        VK_HOME => Code::Home,
        VK_LEFT => Code::ArrowLeft,
        VK_UP => Code::ArrowUp,
        VK_RIGHT => Code::ArrowRight,
        VK_DOWN => Code::ArrowDown,
        VK_INSERT => Code::Insert,
        VK_DELETE => Code::Delete,
        VK_CLEAR => Code::NumLock,
        VK_NUMLOCK => Code::NumLock,
        VK_SCROLL => Code::ScrollLock,
        VK_PAUSE => Code::Pause,
        VK_SNAPSHOT => Code::PrintScreen,
        VK_EXECUTE => Code::Open,
        VK_HELP => Code::Help,
        VK_SELECT => Code::Select,
        VK_SLEEP => Code::Sleep,
        VK_CONVERT => Code::Convert,
        VK_NONCONVERT => Code::NonConvert,
        VK_KANA => Code::KanaMode,
        VK_NUMPAD0 => Code::Numpad0,
        VK_NUMPAD1 => Code::Numpad1,
        VK_NUMPAD2 => Code::Numpad2,
        VK_NUMPAD3 => Code::Numpad3,
        VK_NUMPAD4 => Code::Numpad4,
        VK_NUMPAD5 => Code::Numpad5,
        VK_NUMPAD6 => Code::Numpad6,
        VK_NUMPAD7 => Code::Numpad7,
        VK_NUMPAD8 => Code::Numpad8,
        VK_NUMPAD9 => Code::Numpad9,
        VK_ADD => Code::NumpadAdd,
        VK_SUBTRACT => Code::NumpadSubtract,
        VK_MULTIPLY => Code::NumpadMultiply,
        VK_DIVIDE => Code::NumpadDivide,
        VK_DECIMAL => Code::NumpadDecimal,
        VK_SEPARATOR => Code::NumpadComma,
        VK_F1 => Code::F1,
        VK_F2 => Code::F2,
        VK_F3 => Code::F3,
        VK_F4 => Code::F4,
        VK_F5 => Code::F5,
        VK_F6 => Code::F6,
        VK_F7 => Code::F7,
        VK_F8 => Code::F8,
        VK_F9 => Code::F9,
        VK_F10 => Code::F10,
        VK_F11 => Code::F11,
        VK_F12 => Code::F12,
        VK_F13 => Code::F13,
        VK_F14 => Code::F14,
        VK_F15 => Code::F15,
        VK_F16 => Code::F16,
        VK_F17 => Code::F17,
        VK_F18 => Code::F18,
        VK_F19 => Code::F19,
        VK_F20 => Code::F20,
        VK_F21 => Code::F21,
        VK_F22 => Code::F22,
        VK_F23 => Code::F23,
        VK_F24 => Code::F24,
        VK_VOLUME_MUTE => Code::AudioVolumeMute,
        VK_VOLUME_DOWN => Code::AudioVolumeDown,
        VK_VOLUME_UP => Code::AudioVolumeUp,
        VK_MEDIA_NEXT_TRACK => Code::MediaTrackNext,
        VK_MEDIA_PREV_TRACK => Code::MediaTrackPrevious,
        VK_MEDIA_STOP => Code::MediaStop,
        VK_MEDIA_PLAY_PAUSE => Code::MediaPlayPause,
        VK_LAUNCH_MAIL => Code::LaunchMail,
        VK_LAUNCH_MEDIA_SELECT => Code::MediaSelect,
        VK_LAUNCH_APP1 => Code::LaunchApp1,
        VK_LAUNCH_APP2 => Code::LaunchApp2,
        VK_BROWSER_BACK => Code::BrowserBack,
        VK_BROWSER_FORWARD => Code::BrowserForward,
        VK_BROWSER_REFRESH => Code::BrowserRefresh,
        VK_BROWSER_STOP => Code::BrowserStop,
        VK_BROWSER_SEARCH => Code::BrowserSearch,
        VK_BROWSER_FAVORITES => Code::BrowserFavorites,
        VK_BROWSER_HOME => Code::BrowserHome,
        _ => Code::Unidentified,
    }
}

/// [`Code`] -> `Key::Named` for keys without a character. Returns `None` for
/// printable keys, which are resolved via `convert_character`.
fn convert_key(code: Code) -> Option<Key> {
    Some(match code {
        Code::AltLeft | Code::AltRight => Key::Named(NamedKey::Alt),
        Code::ArrowDown => Key::Named(NamedKey::ArrowDown),
        Code::ArrowLeft => Key::Named(NamedKey::ArrowLeft),
        Code::ArrowRight => Key::Named(NamedKey::ArrowRight),
        Code::ArrowUp => Key::Named(NamedKey::ArrowUp),
        Code::Backspace => Key::Named(NamedKey::Backspace),
        Code::CapsLock => Key::Named(NamedKey::CapsLock),
        Code::ContextMenu => Key::Named(NamedKey::ContextMenu),
        Code::ControlLeft | Code::ControlRight => Key::Named(NamedKey::Control),
        Code::Delete => Key::Named(NamedKey::Delete),
        Code::End => Key::Named(NamedKey::End),
        Code::Enter => Key::Named(NamedKey::Enter),
        Code::Escape => Key::Named(NamedKey::Escape),
        Code::F1 => Key::Named(NamedKey::F1),
        Code::F2 => Key::Named(NamedKey::F2),
        Code::F3 => Key::Named(NamedKey::F3),
        Code::F4 => Key::Named(NamedKey::F4),
        Code::F5 => Key::Named(NamedKey::F5),
        Code::F6 => Key::Named(NamedKey::F6),
        Code::F7 => Key::Named(NamedKey::F7),
        Code::F8 => Key::Named(NamedKey::F8),
        Code::F9 => Key::Named(NamedKey::F9),
        Code::F10 => Key::Named(NamedKey::F10),
        Code::F11 => Key::Named(NamedKey::F11),
        Code::F12 => Key::Named(NamedKey::F12),
        Code::F13 => Key::Named(NamedKey::F13),
        Code::F14 => Key::Named(NamedKey::F14),
        Code::F15 => Key::Named(NamedKey::F15),
        Code::F16 => Key::Named(NamedKey::F16),
        Code::F17 => Key::Named(NamedKey::F17),
        Code::F18 => Key::Named(NamedKey::F18),
        Code::F19 => Key::Named(NamedKey::F19),
        Code::F20 => Key::Named(NamedKey::F20),
        Code::F21 => Key::Named(NamedKey::F21),
        Code::F22 => Key::Named(NamedKey::F22),
        Code::F23 => Key::Named(NamedKey::F23),
        Code::F24 => Key::Named(NamedKey::F24),
        Code::Help => Key::Named(NamedKey::Help),
        Code::Home => Key::Named(NamedKey::Home),
        Code::Insert => Key::Named(NamedKey::Insert),
        Code::MetaLeft | Code::MetaRight => Key::Named(NamedKey::Meta),
        Code::PageDown => Key::Named(NamedKey::PageDown),
        Code::PageUp => Key::Named(NamedKey::PageUp),
        Code::Pause => Key::Named(NamedKey::Pause),
        Code::PrintScreen => Key::Named(NamedKey::PrintScreen),
        Code::ScrollLock => Key::Named(NamedKey::ScrollLock),
        Code::ShiftLeft | Code::ShiftRight => Key::Named(NamedKey::Shift),
        Code::Tab => Key::Named(NamedKey::Tab),
        _ => return None,
    })
}

/// Printable `VK_*` -> `Key::Character`. Uses `MapVirtualKeyW(MAPVK_VK_TO_CHAR)`,
/// which yields the layout's character for the key (unshifted, letters uppercase);
/// shift/caps lock are applied locally to recover the shifted symbol.
pub(crate) fn convert_character(vk: u32, modifiers: Modifiers) -> Option<Key> {
    // Low word: the character (0 or a control/PUA value means "no printable char").
    let code = unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_CHAR) } & 0xFFFF;
    let c = char::from_u32(code)?;
    if (c as u32) < 0x20 || (0xE000..0xF900).contains(&(c as u32)) {
        return None;
    }
    if c.is_ascii_lowercase() {
        let uppercase = modifiers.shift() != modifiers.contains(Modifiers::CAPS_LOCK);
        return Some(Key::Character(if uppercase { c.to_ascii_uppercase() } else { c }.to_string()));
    }
    if modifiers.shift() {
        shifted_symbol(c).map(Key::Character)
    } else {
        Some(Key::Character(c.to_string()))
    }
}

/// US-layout shifted symbols for the OEM/digit rows, keyed by the base character
/// `MapVirtualKeyW` returned. Only the common ASCII cases are enumerated.
fn shifted_symbol(base: char) -> Option<String> {
    let shifted = match base {
        '`' => '~',
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '-' => '_',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        _ => return None,
    };
    Some(shifted.to_string())
}

fn convert_location(code: Code) -> Location {
    match code {
        Code::MetaLeft | Code::ShiftLeft | Code::AltLeft | Code::ControlLeft => Location::Left,
        Code::MetaRight | Code::ShiftRight | Code::AltRight | Code::ControlRight => Location::Right,
        Code::Numpad0
        | Code::Numpad1
        | Code::Numpad2
        | Code::Numpad3
        | Code::Numpad4
        | Code::Numpad5
        | Code::Numpad6
        | Code::Numpad7
        | Code::Numpad8
        | Code::Numpad9
        | Code::NumpadAdd
        | Code::NumpadComma
        | Code::NumpadDecimal
        | Code::NumpadDivide
        | Code::NumpadSubtract
        | Code::NumpadMultiply => Location::Numpad,
        _ => Location::Standard,
    }
}