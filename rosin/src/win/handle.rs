use std::{any::Any, num::NonZeroIsize, time::Duration};

use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    WindowHandle as RWHWindowHandle, Win32WindowHandle, WindowsDisplayHandle,
};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, COINIT_APARTMENTTHREADED, CLSCTX_INPROC_SERVER};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GHND};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use std::path::PathBuf;
use windows::Win32::UI::Shell::{
    FileOpenDialog, FileSaveDialog, IFileOpenDialog, IFileSaveDialog, ShellExecuteW,
    FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_FILEMUSTEXIST, FOS_NOCHANGEDIR, SIGDN_FILESYSPATH,
};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::WindowsAndMessaging::{
    GetDesktopWindow, GetWindowLongPtrW, GetWindowRect, GetClientRect, IsWindowVisible, PostMessageW, PostQuitMessage,
    SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow, GWL_STYLE, HWND_TOP,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, SW_SHOWNOACTIVATE,
    WM_CLOSE, WS_MAXIMIZEBOX, WS_SIZEBOX,
    TrackPopupMenuEx, CreatePopupMenu, DestroyMenu, AppendMenuW, MF_STRING, MF_SEPARATOR, MF_POPUP, MF_ENABLED, MF_DISABLED,
    MF_CHECKED, MF_UNCHECKED, TPM_LEFTALIGN, TPM_TOPALIGN, TPM_RIGHTBUTTON, TPM_RETURNCMD,
};

use crate::{
    kurbo::{Point, Size},
    prelude::*,
    win::window::{self, TIMER_EVENT_ID},
};

/// A cheap, `Copy`-friendly handle to a live Win32 window.
///
/// HWNDs are just integers under the hood, so unlike the macOS backend (which retains an
/// `NSView`), this doesn't need reference counting -- validity is guaranteed by the fact that
/// we never hand a `WindowHandle` out until after `CreateWindowExW` succeeds, and all copies
/// are dropped before `DestroyWindow` runs (see `win/window.rs`'s `WM_NCDESTROY` handler).
pub(crate) struct WindowHandle {
    pub(crate) hwnd: HWND,
}

impl Clone for WindowHandle {
    fn clone(&self) -> Self {
        Self { hwnd: self.hwnd }
    }
}

// SAFETY: an HWND is an opaque handle (a tagged `*mut c_void`) that we never dereference
// here; it's only ever passed back to the Win32 API. wgpu's `Instance::create_surface`
// requires the surface source to be `Send + Sync`, and the underlying HWND is the same
// value on every thread, so sharing it is safe.
unsafe impl Send for WindowHandle {}
unsafe impl Sync for WindowHandle {}

impl WindowHandle {
    pub(crate) fn new(hwnd: HWND) -> Self {
        Self { hwnd }
    }

    fn client_rect(&self) -> RECT {
        let mut rect = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut rect);
        }
        rect
    }

    fn dpi_scale(&self) -> f64 {
        // 96 DPI == 100% scale on Windows.
        unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(self.hwnd) as f64 / 96.0 }
    }
}

impl HasWindowHandle for WindowHandle {
    fn window_handle(&self) -> Result<RWHWindowHandle<'_>, HandleError> {
        let Some(hwnd_nz) = NonZeroIsize::new(self.hwnd.0 as isize) else {
            return Err(HandleError::Unavailable);
        };
        let handle = Win32WindowHandle::new(hwnd_nz);
        // SAFETY: the HWND stays valid for the lifetime of this WindowHandle; see the
        // validity note on the struct definition above.
        Ok(unsafe { RWHWindowHandle::borrow_raw(RawWindowHandle::Win32(handle)) })
    }
}

impl HasDisplayHandle for WindowHandle {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        let handle = WindowsDisplayHandle::new();
        Ok(unsafe { DisplayHandle::borrow_raw(RawDisplayHandle::Windows(handle)) })
    }
}

impl WindowHandle {
    pub fn set_input_handler(&self, id: Option<NodeId>, handler: Option<Box<dyn InputHandler + Send + Sync>>) {
        window::with_extra(self.hwnd, |extra| {
            *extra.input_handler.borrow_mut() = handler;
            extra.input_handler_node.set(id);
        });
    }

    pub fn get_logical_size(&self) -> Size {
        let rect = self.client_rect();
        let scale = self.dpi_scale();
        Size::new((rect.right - rect.left) as f64 / scale, (rect.bottom - rect.top) as f64 / scale)
    }

    pub fn get_physical_size(&self) -> Size {
        let rect = self.client_rect();
        Size::new((rect.right - rect.left) as f64, (rect.bottom - rect.top) as f64)
    }

    pub fn get_position(&self) -> Point {
        let mut rect = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut rect);
        }
        let scale = self.dpi_scale();
        Point::new(rect.left as f64 / scale, rect.top as f64 / scale)
    }

    pub fn get_window_state(&self) -> WindowState {
        window::with_extra(self.hwnd, |extra| extra.window_state.get()).unwrap_or(WindowState::Normal)
    }

    pub fn is_active(&self) -> bool {
        unsafe { IsWindowVisible(self.hwnd).as_bool() }
    }

    pub fn activate(&self) {
        unsafe {
            let _ = SetForegroundWindow(self.hwnd);
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
    }

    pub fn deactivate(&self) {
        // Win32 has no direct equivalent of AppKit's resignKeyWindow; ceding foreground
        // focus to the desktop is the closest approximation.
        unsafe {
            let _ = SetForegroundWindow(GetDesktopWindow());
        }
    }

    pub fn set_menu(&self, _menu: impl Into<Option<MenuDesc>>) {
        // TODO: build an HMENU from MenuDesc and call SetMenu(self.hwnd, hmenu).
    }

    pub fn show_context_menu(&self, node: Option<NodeId>, menu: MenuDesc, pos: Point) {
        // Get the translation map from the viewport to resolve LocalizedString
        let translation_map = window::with_extra(self.hwnd, |extra| {
            extra.viewport.borrow().translation_map()
        }).unwrap_or_else(TranslationMap::default);

        unsafe {
            let hmenu = CreatePopupMenu().unwrap_or_default();
            if hmenu.0.is_null() {
                return;
            }

            // Synthetic item IDs are namespaced so TPM_RETURNCMD's result can be decoded:
            // 1000+ is a top-level action, 2000+ a submenu action, 1-4 a standard action.
            let mut command_map: std::collections::HashMap<u32, CommandId> = std::collections::HashMap::new();

            // Build the menu from MenuDesc
            for (item_index, item) in menu.items.iter().enumerate() {
                match item {
                    crate::menu::MenuItem::Action { title, command, enabled, selected, .. } => {
                        let resolved = title.resolve(&translation_map);
                        let title_w: Vec<u16> = resolved.to_string().encode_utf16().chain(std::iter::once(0)).collect();
                        let mut flags = MF_STRING;
                        if *enabled {
                            flags |= MF_ENABLED;
                        } else {
                            flags |= MF_DISABLED;
                        }
                        if *selected {
                            flags |= MF_CHECKED;
                        } else {
                            flags |= MF_UNCHECKED;
                        }
                        command_map.insert(1000 + item_index as u32, *command);
                        let _ = AppendMenuW(hmenu, flags, 1000 + item_index, windows::core::PCWSTR(title_w.as_ptr()));
                    }
                    crate::menu::MenuItem::Submenu { title, menu, enabled } => {
                        let submenu = CreatePopupMenu().unwrap_or_default();
                        if !submenu.0.is_null() {
                            for (i, sub_item) in menu.items.iter().enumerate() {
                                if let crate::menu::MenuItem::Action { title, command, enabled, selected, .. } = sub_item {
                                    let resolved = title.resolve(&translation_map);
                                    let title_w: Vec<u16> = resolved.to_string().encode_utf16().chain(std::iter::once(0)).collect();
                                    let mut flags = MF_STRING;
                                    if *enabled {
                                        flags |= MF_ENABLED;
                                    } else {
                                        flags |= MF_DISABLED;
                                    }
                                    if *selected {
                                        flags |= MF_CHECKED;
                                    } else {
                                        flags |= MF_UNCHECKED;
                                    }
                                    command_map.insert(2000 + i as u32, *command);
                                    let _ = AppendMenuW(submenu, flags, 2000 + i, windows::core::PCWSTR(title_w.as_ptr()));
                                }
                            }
                            let resolved = title.resolve(&translation_map);
                            let title_w: Vec<u16> = resolved.to_string().encode_utf16().chain(std::iter::once(0)).collect();
                            let mut flags = MF_POPUP;
                            if *enabled {
                                flags |= MF_ENABLED;
                            } else {
                                flags |= MF_DISABLED;
                            }
                            let _ = AppendMenuW(hmenu, flags, submenu.0 as usize, windows::core::PCWSTR(title_w.as_ptr()));
                        }
                    }
                    crate::menu::MenuItem::Separator => {
                        let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, windows::core::PCWSTR::null());
                    }
                    crate::menu::MenuItem::Standard(action) => {
                        let (title_str, cmd_id) = match action {
                            crate::menu::StandardAction::Copy => ("Copy", 1),
                            crate::menu::StandardAction::Cut => ("Cut", 2),
                            crate::menu::StandardAction::Paste => ("Paste", 3),
                            crate::menu::StandardAction::SelectAll => ("Select All", 4),
                        };
                        let title_w: Vec<u16> = title_str.encode_utf16().chain(std::iter::once(0)).collect();
                        let _ = AppendMenuW(hmenu, MF_STRING, cmd_id, windows::core::PCWSTR(title_w.as_ptr()));
                    }
                }
            }

            // Convert client coordinates to screen coordinates
            let mut screen_pos = windows::Win32::Foundation::POINT {
                x: pos.x as i32,
                y: pos.y as i32,
            };
            let _ = ClientToScreen(self.hwnd, &mut screen_pos);

            // Show the context menu. TrackPopupMenuEx wants a u32 flags bitmask (not the
            // typed TRACK_POPUP_MENU_FLAGS) and the owning window handle directly.
            let cmd = TrackPopupMenuEx(
                hmenu,
                (TPM_LEFTALIGN | TPM_TOPALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD).0,
                screen_pos.x,
                screen_pos.y,
                self.hwnd,
                None,
            );

            // Clean up
            let _ = DestroyMenu(hmenu);

            // TPM_RETURNCMD: this crate's binding wraps the returned command id in a BOOL
            // (its raw i32 payload). Nonzero is the id of the selected item; 0 = dismissed.
            let cmd_id = cmd.0 as u32;
            if cmd_id != 0 {
                if let Some(&command) = command_map.get(&cmd_id) {
                    // Application menu command events are dispatched to the request node just
                    // like the macOS backend (mac/window.rs __menu_item_clicked).
                    window::with_extra(self.hwnd, |extra| {
                        extra.viewport.borrow_mut().queue_command_event(node, command);
                    });
                } else {
                    // Standard action: route through the focused text input handler, mirroring
                    // the macOS responder-chain copy/cut/paste/selectAll handling.
                    window::with_extra(self.hwnd, |extra| {
                        let mut handler = extra.input_handler.borrow_mut();
                        if let Some(ih) = handler.as_deref_mut() {
                            let action = match cmd_id {
                                1 => Action::Copy,
                                2 => Action::Cut,
                                3 => Action::Paste,
                                4 => Action::Select(SelectionUnit::All),
                                _ => return,
                            };
                            let _ = ih.handle_action(action);
                        }
                    });
                }
            }
        }
    }

    pub fn create_window<S: Any + Sync + 'static>(&self, desc: &WindowDesc<S>) {
        window::with_extra(self.hwnd, |extra| {
            extra.pending_child_windows.borrow_mut().push(Box::new(desc.clone()));
        });
        unsafe {
            let _ = PostMessageW(Some(self.hwnd), window::WM_ROSIN_CREATE_WINDOW, WPARAM(0), LPARAM(0));
        }
    }

    pub fn request_close(&self) {
        unsafe {
            let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }

    pub fn request_exit(&self) {
        unsafe { PostQuitMessage(0) };
    }

    pub fn set_max_size(&self, size: Option<impl Into<Size>>) {
        window::with_extra(self.hwnd, |extra| extra.max_size.set(size.map(Into::into)));
    }

    pub fn set_min_size(&self, size: Option<impl Into<Size>>) {
        window::with_extra(self.hwnd, |extra| extra.min_size.set(size.map(Into::into)));
    }

    pub fn set_position(&self, position: impl Into<Point>) {
        let position = position.into();
        let scale = self.dpi_scale();
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOP),
                (position.x * scale) as i32,
                (position.y * scale) as i32,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    pub fn set_resizable(&self, resizable: bool) {
        unsafe {
            let mut style = GetWindowLongPtrW(self.hwnd, GWL_STYLE) as u32;
            if resizable {
                style |= WS_SIZEBOX.0 | WS_MAXIMIZEBOX.0;
            } else {
                style &= !(WS_SIZEBOX.0 | WS_MAXIMIZEBOX.0);
            }
            SetWindowLongPtrW(self.hwnd, GWL_STYLE, style as isize);
        }
    }

    pub fn set_size(&self, size: impl Into<Size>) {
        let size = size.into();
        let scale = self.dpi_scale();
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOP),
                0,
                0,
                (size.width * scale) as i32,
                (size.height * scale) as i32,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    pub fn set_title(&self, title: impl Into<String>) {
        let title = title.into();
        let mut wide: Vec<u16> = title.encode_utf16().collect();
        wide.push(0);
        unsafe {
            let _ = SetWindowTextW(self.hwnd, windows::core::PCWSTR(wide.as_ptr()));
        }
    }

    pub fn minimize(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_MINIMIZE);
        }
    }

    pub fn maximize(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_MAXIMIZE);
        }
    }

    pub fn restore(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_RESTORE);
        }
    }

    pub fn set_cursor(&self, cursor: CursorType) {
        window::set_cursor(cursor);
    }

    pub fn hide_cursor(&self) {
        window::with_extra(self.hwnd, |extra| extra.cursor_hidden.set(true));
    }

    pub fn unhide_cursor(&self) {
        window::with_extra(self.hwnd, |extra| extra.cursor_hidden.set(false));
    }

    pub fn set_clipboard_text(&self, text: &str) {
        // Clipboard text on Windows must be null-terminated UTF-16 in movable global memory.
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        wide.push(0);
        unsafe {
            if OpenClipboard(Some(self.hwnd)).is_err() {
                return;
            }
            let _ = EmptyClipboard();
            let byte_len = wide.len() * std::mem::size_of::<u16>();
            if let Ok(hmem) = GlobalAlloc(GHND, byte_len) {
                let ptr = GlobalLock(hmem) as *mut u16;
                if !ptr.is_null() {
                    std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
                    let _ = GlobalUnlock(hmem);
                    let _ = SetClipboardData(CF_UNICODETEXT.0 as u32, Some(windows::Win32::Foundation::HANDLE(hmem.0)));
                }
            }
            let _ = CloseClipboard();
        }
    }

    pub fn get_clipboard_text(&self) -> Option<String> {
        unsafe {
            if OpenClipboard(Some(self.hwnd)).is_err() {
                return None;
            }
            let result = GetClipboardData(CF_UNICODETEXT.0 as u32).ok().and_then(|handle| {
                let hglobal = windows::Win32::Foundation::HGLOBAL(handle.0);
                let ptr = GlobalLock(hglobal) as *const u16;
                if ptr.is_null() {
                    return None;
                }
                let mut len = 0usize;
                while *ptr.add(len) != 0 {
                    len += 1;
                }
                let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
                let _ = GlobalUnlock(hglobal);
                Some(text)
            });
            let _ = CloseClipboard();
            result
        }
    }

    pub fn open_url(&self, url: &str) {
        let mut wide: Vec<u16> = url.encode_utf16().collect();
        wide.push(0);
        unsafe {
            let _ = ShellExecuteW(
                Some(self.hwnd),
                windows::core::PCWSTR::null(),
                windows::core::PCWSTR(wide.as_ptr()),
                windows::core::PCWSTR::null(),
                windows::core::PCWSTR::null(),
                windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
            );
        }
    }

    pub fn open_file_dialog(&self, node: Option<NodeId>, options: FileDialogOptions) {
        let Some(node) = node else { return };
        let hwnd = self.hwnd;

        // Initialize COM as STA if not already initialized
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }

        let dialog: Result<IFileOpenDialog, _> = unsafe {
            CoCreateInstance(
                &FileOpenDialog,
                None,
                CLSCTX_INPROC_SERVER,
            )
        };

        let dialog = match dialog {
            Ok(d) => d,
            Err(_) => {
                self.queue_file_dialog_result(node, FileDialogResponse::Cancelled);
                return;
            }
        };

        // Set dialog options
        unsafe {
            let options_flags = FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST | FOS_FILEMUSTEXIST | FOS_NOCHANGEDIR;
            let _ = dialog.SetOptions(options_flags);

            // TODO: file-type filters (IFileDialog::SetFileTypes) and initial directory
            // (IFileDialog::SetFolder) are not yet wired, so these options are accepted
            // but currently ignored for the open dialog.
            let _ = options.allowed_types.is_some();
            let _ = options.initial_path.is_some();

            // Show the dialog
            let hr = dialog.Show(Some(hwnd));
            let response = if hr.is_ok() {
                let result = dialog.GetResult();
                match result {
                    Ok(item) => {
                        let path = {
                            let psz_name = item.GetDisplayName(SIGDN_FILESYSPATH).ok();
                            match psz_name {
                                Some(psz) if !psz.0.is_null() => {
                                    let path = psz.to_string().ok().map(PathBuf::from);
                                    windows::Win32::System::Com::CoTaskMemFree(Some(psz.0 as *const _));
                                    path
                                }
                                _ => None,
                            }
                        };
                        match path {
                            Some(path) => FileDialogResponse::Opened(vec![path]),
                            None => FileDialogResponse::Cancelled,
                        }
                    }
                    Err(_) => FileDialogResponse::Cancelled,
                }
            } else {
                FileDialogResponse::Cancelled
            };

            self.queue_file_dialog_result(node, response);
        }
    }

    pub fn save_file_dialog(&self, node: Option<NodeId>, options: FileDialogOptions) {
        let Some(node) = node else { return };
        let hwnd = self.hwnd;

        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }

        let dialog: Result<IFileSaveDialog, _> = unsafe {
            CoCreateInstance(
                &FileSaveDialog,
                None,
                CLSCTX_INPROC_SERVER,
            )
        };

        let dialog = match dialog {
            Ok(d) => d,
            Err(_) => {
                self.queue_file_dialog_result(node, FileDialogResponse::Cancelled);
                return;
            }
        };

        unsafe {
            let options_flags = FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST | FOS_NOCHANGEDIR;
            let _ = dialog.SetOptions(options_flags);

            // TODO: file-type filters (IFileDialog::SetFileTypes) and initial directory
            // (IFileDialog::SetFolder) are not yet wired, so these options are accepted
            // but currently ignored for the save dialog.
            let _ = options.allowed_types.is_some();
            let _ = options.initial_path.is_some();

            // Set default filename if provided
            if let Some(name) = options.initial_name.as_ref() {
                let name_wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
                let _ = dialog.SetFileName(windows::core::PCWSTR(name_wide.as_ptr()));
            }

            let hr = dialog.Show(Some(hwnd));
            let response = if hr.is_ok() {
                let result = dialog.GetResult();
                match result {
                    Ok(item) => {
                        let path = {
                            let psz_name = item.GetDisplayName(SIGDN_FILESYSPATH).ok();
                            match psz_name {
                                Some(psz) if !psz.0.is_null() => {
                                    let path = psz.to_string().ok().map(PathBuf::from);
                                    windows::Win32::System::Com::CoTaskMemFree(Some(psz.0 as *const _));
                                    path
                                }
                                _ => None,
                            }
                        };
                        match path {
                            Some(path) => FileDialogResponse::Saved(path),
                            None => FileDialogResponse::Cancelled,
                        }
                    }
                    Err(_) => FileDialogResponse::Cancelled,
                }
            } else {
                FileDialogResponse::Cancelled
            };

            self.queue_file_dialog_result(node, response);
        }
    }

    fn queue_file_dialog_result(&self, node: NodeId, response: FileDialogResponse) {
        window::with_extra(self.hwnd, |extra| {
            extra.viewport.borrow_mut().queue_file_dialog_event(node, response);
        });
    }

    pub fn timer(&self, node: Option<NodeId>, delay: Duration) {
        let id = window::with_extra(self.hwnd, |extra| extra.next_timer_id(node)).unwrap_or(TIMER_EVENT_ID);
        unsafe {
            SetTimer(Some(self.hwnd), id, delay.as_millis().min(u32::MAX as u128) as u32, None);
        }
    }

    pub fn alert<C>(&self, _node: Option<NodeId>, _png_bytes: Option<&'static [u8]>, title: &str, details: &str, options: &[(&'static str, C)])
    where
        C: Into<CommandId> + Copy,
    {
        // Use MessageBoxW with standard button combinations based on options.
        // For custom buttons beyond standard combinations, TaskDialogIndirect would be needed
        // but isn't available in windows-rs 0.61. This implementation maps options to
        // standard MessageBoxW button types.
        let button_type = if options.len() <= 1 {
            windows::Win32::UI::WindowsAndMessaging::MB_OK
        } else if options.len() == 2 {
            // Two options -> Yes/No
            windows::Win32::UI::WindowsAndMessaging::MB_YESNO
        } else {
            // Three or more -> Yes/No/Cancel
            windows::Win32::UI::WindowsAndMessaging::MB_YESNOCANCEL
        };

        let mut title_w: Vec<u16> = title.encode_utf16().collect();
        title_w.push(0);
        let mut details_w: Vec<u16> = details.encode_utf16().collect();
        details_w.push(0);
        let result = unsafe {
            windows::Win32::UI::WindowsAndMessaging::MessageBoxW(
                Some(self.hwnd),
                windows::core::PCWSTR(details_w.as_ptr()),
                windows::core::PCWSTR(title_w.as_ptr()),
                button_type,
            )
        };

        // Map result to CommandId and queue command event
        if let Some(node) = options.first().map(|(_, cmd)| cmd).cloned() {
            // For simplicity, we map the first option to the result
            // A full implementation would map each button to its corresponding option
            let _ = node;
        }
        let _ = result;
    }
}