//! IMM32 text input bridge for the Windows backend.
//!
//! Mirrors `mac::window`'s `NSTextInputClient` handling (insert_text / set_marked_text /
//! unmark_text) using the legacy IME window messages. Windows forwards pre-edit and
//! committed text through `WM_IME_COMPOSITION`, which we decode with
//! `ImmGetCompositionStringW` and apply to the focused `rosin::InputHandler` the same way
//! macOS does with `insertText:` and `setMarkedText:`.
//!
//! The OS-drawn composition window is suppressed (see `WM_IME_SETCONTEXT` stripping
//! `IS_SHOWUICOMPOSITIONWINDOW` in `window.rs`) so that widgets render their own marked
//! text via `InputHandler::composition_range`, while the candidate window is repositioned
//! to the caret via `ImmSetCandidateWindow(CFS_EXCLUDE)`.

use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::Ime::{
    CANDIDATEFORM, CFS_EXCLUDE, CFS_RECT, COMPOSITIONFORM, GCS_COMPSTR, GCS_CURSORPOS,
    GCS_RESULTSTR, HIMC, IME_COMPOSITION_STRING, ImmGetCompositionStringW, ImmGetContext,
    ImmReleaseContext, ImmSetCandidateWindow, ImmSetCompositionWindow,
};

use crate::prelude::*;
use crate::win::{util, window};

/// Applies a plain character (`WM_CHAR` / `WM_UNICHAR`) to the focused input handler.
///
/// Control characters (`\r`, `\b`, `\t`, and the `Ctrl+letter` range) are ignored here --
/// they are routed through `InputHandler::handle_action` by `handle_editing_key`, exactly
/// like macOS's responder chain. Text is also dropped while an IME composition is active so
/// that pre-edit text comes only from `WM_IME_COMPOSITION`.
pub(crate) fn insert_character(hwnd: HWND, c: char) {
    if c.is_control() {
        return;
    }
    window::with_extra(hwnd, |extra| {
        let mut handler = extra.input_handler.borrow_mut();
        if let Some(ih) = handler.as_deref_mut() {
            if ih.composition_range().is_some() {
                return;
            }
            insert_text(ih, &c.to_string());
        }
    });
    sync_text_changed(hwnd);
}

/// Translates a key-down into a text-editing action on the focused input handler, mirroring
/// the macOS responder chain (`keyDown` -> `interpretKeyEvents` -> `doCommandBySelector`).
///
/// Returns `true` if the key was consumed as an editing action (or swallowed while composing)
/// and should not produce a `KeyboardEvent`.
pub(crate) fn handle_editing_key(hwnd: HWND, vk: u32, composing: bool) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    use crate::ime::{
        Action, HorizontalDirection as H, Movement as M, VerticalDirection as V,
    };

    let mods = window::current_modifiers();
    let ctrl = mods.ctrl();
    let shift = mods.shift();

    let action = match vk {
        x if x == VK_RETURN.0 as u32 => {
            if composing {
                return true; // IME commits via WM_IME_COMPOSITION; don't insert a newline.
            }
            Some(Action::InsertNewLine)
        }
        x if x == VK_TAB.0 as u32 => {
            if composing {
                return true;
            }
            Some(if shift { Action::InsertBacktab } else { Action::InsertTab })
        }
        x if x == VK_BACK.0 as u32 => {
            if composing {
                return true;
            }
            Some(Action::Delete(M::Grapheme(H::Left)))
        }
        x if x == VK_DELETE.0 as u32 => {
            if composing {
                return true;
            }
            Some(Action::Delete(M::Grapheme(H::Right)))
        }
        x if x == VK_LEFT.0 as u32 => {
            if composing {
                return true;
            }
            horizontal_move(ctrl, shift, H::Left)
        }
        x if x == VK_RIGHT.0 as u32 => {
            if composing {
                return true;
            }
            horizontal_move(ctrl, shift, H::Right)
        }
        x if x == VK_UP.0 as u32 => vertical_move(composing, shift, V::Up),
        x if x == VK_DOWN.0 as u32 => vertical_move(composing, shift, V::Down),
        x if x == VK_PRIOR.0 as u32 => vertical_move(composing, shift, V::PageUp),
        x if x == VK_NEXT.0 as u32 => vertical_move(composing, shift, V::PageDown),
        x if x == VK_HOME.0 as u32 => {
            if composing {
                return true;
            }
            move_or_select(
                shift,
                if ctrl { M::Document(H::Left) } else { M::Line(H::Left) },
            )
        }
        x if x == VK_END.0 as u32 => {
            if composing {
                return true;
            }
            move_or_select(
                shift,
                if ctrl { M::Document(H::Right) } else { M::Line(H::Right) },
            )
        }
        x if x == VK_ESCAPE.0 as u32 => {
            if composing {
                return true; // IME manages cancelling its own composition.
            }
            Some(Action::Cancel)
        }
        x if x == VK_A.0 as u32 && ctrl && !composing => Some(Action::Select(crate::ime::SelectionUnit::All)),
        x if x == VK_C.0 as u32 && ctrl && !composing => Some(Action::Copy),
        x if x == VK_X.0 as u32 && ctrl && !composing => Some(Action::Cut),
        x if x == VK_V.0 as u32 && ctrl && !composing => Some(Action::Paste),
        _ => None,
    };

    let Some(action) = action else {
        return false;
    };

    window::with_extra(hwnd, |extra| {
        let mut handler = extra.input_handler.borrow_mut();
        if let Some(ih) = handler.as_deref_mut() {
            if !ih.handle_action(action) {
                return false;
            }
            true
        } else {
            false
        }
    })
    .unwrap_or(false)
}

/// Decorates the composition/candidate window position after the caret moved or the
/// selection changed. Runs on `WM_IME_STARTCOMPOSITION` and after each composition update.
pub(crate) fn position_ime_windows(hwnd: HWND) {
    let himc = unsafe { ImmGetContext(hwnd) };
    if himc.is_invalid() {
        return;
    }
    if let Some(rect) = caret_rect(hwnd) {
        let composition = COMPOSITIONFORM {
            dwStyle: CFS_RECT,
            ptCurrentPos: POINT::default(),
            rcArea: rect,
        };
        let candidate = CANDIDATEFORM {
            dwIndex: 0,
            dwStyle: CFS_EXCLUDE,
            ptCurrentPos: POINT::default(),
            rcArea: rect,
        };
        let _ = unsafe { ImmSetCompositionWindow(himc, &composition) };
        let _ = unsafe { ImmSetCandidateWindow(himc, &candidate) };
    }
    let _ = unsafe { ImmReleaseContext(hwnd, himc) };
}

/// Applies a `WM_IME_COMPOSITION` payload (`GCS_RESULTSTR` = committed text,
/// `GCS_COMPSTR` = pre-edit update) to the focused input handler.
pub(crate) fn handle_ime_composition(hwnd: HWND, lparam: u32) {
    let himc = unsafe { ImmGetContext(hwnd) };
    if himc.is_invalid() {
        return;
    }

    let commit_flag = GCS_RESULTSTR.0;
    let preedit_flag = GCS_COMPSTR.0;
    let mut did_edit = false;

    window::with_extra(hwnd, |extra| {
        let mut handler = extra.input_handler.borrow_mut();
        let Some(ih) = handler.as_deref_mut() else {
            return;
        };

        // The same message can carry a commit AND the start of the next pre-edit (e.g. a
        // space after a word of pinyin). Commit first, then apply the new pre-edit.
        if lparam & commit_flag != 0 && let Some(result) = ime_string(himc, GCS_RESULTSTR) {
            let range = ih.composition_range().unwrap_or_else(|| ih.selection());
            let start = range.start;
            ih.replace_range(range, &result);
            let new_cursor = start + result.len();
            ih.set_selection(new_cursor..new_cursor);
            ih.set_composition_range(None);
            did_edit = true;
        }

        if lparam & preedit_flag != 0 && let Some(preedit) = ime_string(himc, GCS_COMPSTR) {
            let range = ih.composition_range().unwrap_or_else(|| ih.selection());
            let start = range.start;
            if preedit.is_empty() {
                // Vacuous pre-edit: drop any existing composition contents and end it.
                if ih.composition_range().is_some() {
                    ih.replace_range(range, "");
                }
                ih.set_composition_range(None);
                ih.set_selection(start..start);
            } else {
                ih.replace_range(range, &preedit);
                let comp_range = start..(start + preedit.len());
                ih.set_composition_range(Some(comp_range.clone()));
                // GCS_CURSORPOS is the caret within the pre-edit string (UTF-16 units).
                let cursor = match ime_cursor_pos(himc) {
                    Some(u16_cursor) => utf16_offset_to_utf8(&preedit, u16_cursor)
                        .map(|rel| (start + rel)..(start + rel)),
                    None => None,
                };
                ih.set_selection(cursor.unwrap_or(comp_range.end..comp_range.end));
            }
            did_edit = true;
        }
    });

    let _ = unsafe { ImmReleaseContext(hwnd, himc) };

    if did_edit {
        position_ime_windows(hwnd);
        sync_text_changed(hwnd);
    }
}

/// Ends an active composition (`WM_IME_ENDCOMPOSITION`).
pub(crate) fn handle_ime_end(hwnd: HWND) {
    let mut had_composition = false;
    window::with_extra(hwnd, |extra| {
        let mut handler = extra.input_handler.borrow_mut();
        if let Some(ih) = handler.as_deref_mut()
            && ih.composition_range().is_some()
        {
            ih.set_composition_range(None);
            had_composition = true;
        }
    });
    if had_composition {
        sync_text_changed(hwnd);
    }
}

/// The bounded caret rectangle (client coordinates, physical pixels) the IME should park its
/// candidate window next to, from the focused input handler's geometry.
fn caret_rect(hwnd: HWND) -> Option<RECT> {
    let rect = window::with_extra(hwnd, |extra| {
        let handler = extra.input_handler.borrow();
        let ih = handler.as_deref()?;
        let range = ih.composition_range().unwrap_or_else(|| ih.selection());
        ih.bounding_box_for_range(range)
    })
    .flatten()?;
    let scale = unsafe { GetDpiForWindow(hwnd) } as f64 / 96.0;
    Some(RECT {
        left: (rect.x0 * scale).round() as i32,
        top: (rect.y0 * scale).round() as i32,
        right: (rect.x1 * scale).round() as i32,
        bottom: (rect.y1 * scale).round() as i32,
    })
}

/// Fetches a composition string (`GCS_RESULTSTR` / `GCS_COMPSTR` / ...) from the IME context.
fn ime_string(himc: HIMC, index: IME_COMPOSITION_STRING) -> Option<String> {
    // Size query (lpBuff = None, dwBufLen = 0) returns the required byte count.
    let size = unsafe { ImmGetCompositionStringW(himc, index, None, 0) };
    if size <= 0 {
        return None;
    }
    // Some IMEs need a spare slot for the NUL terminator; allocate size + 2 bytes.
    let mut buf = vec![0u16; (size as usize / 2) + 1];
    let copied = unsafe {
        ImmGetCompositionStringW(
            himc,
            index,
            Some(buf.as_mut_ptr() as *mut _),
            ((buf.len()) * size_of::<u16>()) as u32,
        )
    };
    if copied <= 0 {
        return None;
    }
    let mut units = (copied as usize).min(buf.len() * size_of::<u16>()) / 2;
    while units > 0 && buf[units - 1] == 0 {
        units -= 1;
    }
    Some(String::from_utf16_lossy(&buf[..units]))
}

/// The caret position within the composition string, in UTF-16 code units
/// (`GCS_CURSORPOS`). Returns `None` if the IME didn't report one.
fn ime_cursor_pos(himc: HIMC) -> Option<usize> {
    let mut pos: u32 = 0;
    let copied = unsafe {
        ImmGetCompositionStringW(
            himc,
            GCS_CURSORPOS,
            Some((&mut pos) as *mut u32 as *mut _),
            size_of::<u32>() as u32,
        )
    };
    (copied == size_of::<u32>() as i32).then_some(pos as usize)
}

/// Maps a UTF-16 code-unit offset inside `text` to a UTF-8 byte offset.
/// Positions past the end clamp to `text.len()`.
fn utf16_offset_to_utf8(text: &str, utf16: usize) -> Option<usize> {
    let mut current = 0usize;
    for (byte_idx, ch) in text.char_indices() {
        if current == utf16 {
            return Some(byte_idx);
        }
        current += ch.len_utf16();
        if current > utf16 {
            return None; // fell inside a surrogate pair; drop the caret.
        }
    }
    (current <= utf16).then_some(text.len())
}

/// Horizontal caret/selection movement for the arrow keys.
fn horizontal_move(ctrl: bool, shift: bool, direction: crate::ime::HorizontalDirection) -> Option<crate::ime::Action> {
    let movement = if ctrl {
        crate::ime::Movement::Word(direction)
    } else {
        crate::ime::Movement::Grapheme(direction)
    };
    move_or_select(shift, movement)
}

/// Vertical caret/selection movement for Up/Down/PageUp/PageDown.
fn vertical_move(composing: bool, shift: bool, direction: crate::ime::VerticalDirection) -> Option<crate::ime::Action> {
    if composing {
        return None;
    }
    move_or_select(shift, crate::ime::Movement::Vertical(direction))
}

fn move_or_select(shift: bool, movement: crate::ime::Movement) -> Option<crate::ime::Action> {
    if shift {
        Some(crate::ime::Action::MoveSelecting(movement))
    } else {
        Some(crate::ime::Action::Move(movement))
    }
}

/// Shared primitive for inserting text, mirroring mac's `insertText(_:replacementRange:)`.
fn insert_text(ih: &mut dyn InputHandler, text: &str) {
    let range = ih.composition_range().unwrap_or_else(|| ih.selection());
    let start = range.start;
    ih.replace_range(range, text);
    ih.set_selection((start + text.len())..(start + text.len()));
    ih.set_composition_range(None);
}

/// Notifies the text widget's node of an edit and requests a redraw, mirroring
/// mac's `queue_change_event` + `dispatch_and_redraw`.
fn sync_text_changed(hwnd: HWND) {
    window::with_extra(hwnd, |extra| {
        if let Some(node) = extra.input_handler_node.get() {
            extra.viewport.borrow_mut().change_event(node);
        }
        extra.viewport.borrow_mut().dispatch_and_redraw(hwnd);
    });
}

/// Returns `true` if the key would produce a character via `TranslateMessage` ->
/// `WM_CHAR`, in which case text is delivered through `insert_character` rather than a
/// `KeyboardEvent` (matching macOS, where character keys go through `insertText:`).
pub(crate) fn is_text_key(vk: u32) -> bool {
    if is_editing_key(vk) {
        return false;
    }
    util::convert_character(vk, window::current_modifiers()).is_some()
}

fn is_editing_key(vk: u32) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        VK_A, VK_BACK, VK_C, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_NEXT, VK_PRIOR,
        VK_RETURN, VK_RIGHT, VK_TAB, VK_UP, VK_V, VK_X,
    };
    matches!(
        vk,
        x if x == VK_RETURN.0 as u32
            || x == VK_TAB.0 as u32
            || x == VK_BACK.0 as u32
            || x == VK_DELETE.0 as u32
            || x == VK_LEFT.0 as u32
            || x == VK_RIGHT.0 as u32
            || x == VK_UP.0 as u32
            || x == VK_DOWN.0 as u32
            || x == VK_HOME.0 as u32
            || x == VK_END.0 as u32
            || x == VK_PRIOR.0 as u32
            || x == VK_NEXT.0 as u32
            || x == VK_ESCAPE.0 as u32
            || x == VK_A.0 as u32
            || x == VK_C.0 as u32
            || x == VK_X.0 as u32
            || x == VK_V.0 as u32
    )
}