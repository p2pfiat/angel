//! Windows glue: an invisible window that receives session-unlock notifications and hotkeys.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::OnceLock;

use anyhow::{bail, Result};
use nostr::prelude::PublicKey;
use tokio::sync::mpsc::UnboundedSender;
use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, HWND, INVALID_HANDLE_VALUE, LPARAM, LRESULT, RECT, WPARAM,
};
use windows_sys::Win32::Media::Audio::{PlaySoundW, SND_ALIAS, SND_ASYNC};
use windows_sys::Win32::System::Console::{AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::RemoteDesktop::{WTSRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, MOD_NOREPEAT};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, EndPaint, FillRect, GetStockObject, InvalidateRect, BLACK_BRUSH, DEFAULT_GUI_FONT, HBRUSH, HFONT,
    PAINTSTRUCT, WHITE_BRUSH,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetDlgItem, GetMessageW, GetWindowTextW,
    IsDialogMessageW, KillTimer, LoadCursorW, PostMessageW, RegisterClassW, SendMessageW,
    SetForegroundWindow, SetTimer, WM_APP,
    SetWindowTextW, ShowWindow, SystemParametersInfoW, TranslateMessage, CW_USEDEFAULT, IDC_ARROW, MSG,
    SPI_GETWORKAREA, SW_SHOW, SW_SHOWNOACTIVATE, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_HOTKEY, WM_PAINT, WM_SETFONT,
    WM_TIMER, WNDCLASSW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP, WS_SYSMENU, WS_VISIBLE,
};

use crate::hotkey::Hotkey;
use crate::publisher::Trigger;

const WM_WTSSESSION_CHANGE: u32 = 0x02B1;
const WTS_SESSION_UNLOCK: usize = 0x8;
const HOTKEY_OK: i32 = 1;
const HOTKEY_DURESS: i32 = 2;
const HOTKEY_SETTINGS: i32 = 3;

// Button control messages and styles (kept local to avoid an extra windows-sys feature).
const BM_GETCHECK: u32 = 0x00F0;
const BM_SETCHECK: u32 = 0x00F1;
const BST_CHECKED: usize = 1;
const BS_AUTOCHECKBOX: u32 = 0x0003;
const ID_ENABLED: usize = 101;
const ID_NAME: i32 = 102;
const ID_STATUS: i32 = 103;
const ID_PHONE: i32 = 104;
const ID_INVITE: i32 = 105;
const ID_COPY_INVITE: i32 = 106;
const ID_ANGELS: i32 = 107;
/// Angels window: row i uses these ids + i.
const ID_ANGEL_NAME: i32 = 300;
const ID_ANGEL_MISSED: i32 = 330;
const ID_ANGEL_LATE: i32 = 360;
const ID_ANGEL_ACTIVE: i32 = 390;
/// Rows shown in the Angels window.
const MAX_ANGELS: usize = 12;
const ID_PHONES: i32 = 108;
/// VIPs window: device line i uses these ids + i.
const ID_PHONE_NAME: i32 = 500;
const ID_PHONE_REMOVE: i32 = 530;
const ID_PAIR_LINK: i32 = 561;
const ID_PAIR_COPY: i32 = 562;
/// Device lines shown in the VIPs window, and VIPs ("Add a phone" for VIP v has ID_PAIR_VIP + v).
const MAX_PHONES: usize = 20;
const MAX_VIPS: usize = 10;
const ID_PAIR_VIP: i32 = 600;
const ID_OFFER: i32 = 563;
/// Size of the QR code in the VIPs window.
const QR_SIZE: i32 = 200;
/// Posted to the VIPs window: the pairing link (or why there is none) is in PAIR_RESULT.
const WM_APP_PAIR: u32 = WM_APP + 2;
const ID_TIMEOUT: i32 = 110;
const ID_REMINDER: i32 = 111;
const ID_REM_SOUND: i32 = 112;
const ID_REM_NOTICE: i32 = 113;
const ID_NOTICE_TITLE: i32 = 114;
const ID_NOTICE_TEXT: i32 = 115;
const ID_REM_TELEGRAM: i32 = 116;
const ID_TELEGRAM_CHAT: i32 = 117;
const ID_TELEGRAM_TEXT: i32 = 118;
/// Posted to the hidden window: show the on-screen part of a reminder.
const WM_APP_REMIND: u32 = WM_APP + 1;
/// How long the reminder notice stays on screen.
const REMINDER_MS: u32 = 7000;
/// Enter (via the dialog manager) and the Save button.
const IDOK: usize = 1;
/// Escape (via the dialog manager).
const IDCANCEL: usize = 2;
const BS_DEFPUSHBUTTON: u32 = 0x0001;
const ES_AUTOHSCROLL: u32 = 0x0080;
const ES_READONLY: u32 = 0x0800;
const EM_SETSEL: u32 = 0x00B1;
const WM_COPY: u32 = 0x0301;
const WS_TABSTOP: u32 = 0x0001_0000;
const WS_EX_CLIENTEDGE: u32 = 0x0000_0200;
const SS_CENTER: u32 = 0x0001;
const SS_CENTERIMAGE: u32 = 0x0200;
/// How long the check-in confirmation stays on screen.
const TOAST_MS: u32 = 2500;
/// COLOR_BTNFACE + 1: the standard dialog background.
const BTNFACE_BRUSH: isize = 16;

static TX: OnceLock<UnboundedSender<Trigger>> = OnceLock::new();
static SETTINGS_CLASS: OnceLock<Vec<u16>> = OnceLock::new();
static ANGELS_CLASS: OnceLock<Vec<u16>> = OnceLock::new();
static PHONES_CLASS: OnceLock<Vec<u16>> = OnceLock::new();
/// The built-in watcher's answer to "Add a phone", picked up on the window thread.
static PAIR_RESULT: std::sync::Mutex<Option<Result<String, String>>> = std::sync::Mutex::new(None);
static TOAST_CLASS: OnceLock<Vec<u16>> = OnceLock::new();
/// Show a confirmation after the check-in key.
static OK_FEEDBACK: AtomicBool = AtomicBool::new(false);
/// A Telegram bot is configured (for the Telegram reminder).
static HAS_BOT: AtomicBool = AtomicBool::new(false);
/// The owner's join link for new contacts; set once the bot's name is known (needs internet).
pub static INVITE_LINK: OnceLock<String> = OnceLock::new();
static HOME: OnceLock<PathBuf> = OnceLock::new();
/// The hidden window, for posting reminders to the window thread.
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);
/// Config folder and this device's key, when the settings window may change the owner's name.
static NAME_TARGET: OnceLock<(PathBuf, PublicKey)> = OnceLock::new();

thread_local! {
    /// The settings window while it is open (window thread only).
    static SETTINGS: Cell<HWND> = const { Cell::new(null_mut()) };
    /// The Angels window while it is open, and what each row showed when opened or saved.
    static ANGELS: Cell<HWND> = const { Cell::new(null_mut()) };
    static ANGEL_ROWS: RefCell<Vec<AngelRow>> = const { RefCell::new(Vec::new()) };
    /// The VIPs window while it is open, its device lines, and the QR code it shows (top, modules).
    static PHONES: Cell<HWND> = const { Cell::new(null_mut()) };
    static PHONE_ROWS: RefCell<Vec<PhoneRow>> = const { RefCell::new(Vec::new()) };
    static PAIR_QR: RefCell<Option<(i32, usize, Vec<bool>)>> = const { RefCell::new(None) };
    /// The VIPs in the window, in order, and the one the last link was asked for.
    static VIP_NAMES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static PAIR_FOR: RefCell<String> = const { RefCell::new(String::new()) };
    /// The check-in confirmation while it is shown, and its font (window thread only).
    static TOAST: Cell<HWND> = const { Cell::new(null_mut()) };
    static TOAST_FONT: Cell<HFONT> = const { Cell::new(null_mut()) };
    static TOAST_BOLD: Cell<HFONT> = const { Cell::new(null_mut()) };
    static TOAST_SMALL: Cell<HFONT> = const { Cell::new(null_mut()) };
}

/// One row of the Angels window.
struct AngelRow {
    person: String,
    id: warn_watcher::contacts::AngelId,
    entry: warn_watcher::contacts::Entry,
}

/// One device line of the VIPs window.
struct PhoneRow {
    person: String,
    key: String,
    name: String,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// This binary has no console of its own. When started from a terminal without redirected
/// output, attach to the terminal so CLI commands can print.
pub fn attach_console_if_needed() {
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

/// Whether this session's screen is locked right now; `None` if Windows cannot tell.
pub fn session_locked() -> Option<bool> {
    use windows_sys::Win32::System::RemoteDesktop::{
        WTSFreeMemory, WTSQuerySessionInformationW, WTSSessionInfoEx, WTSINFOEXW, WTS_CURRENT_SERVER_HANDLE,
        WTS_CURRENT_SESSION, WTS_SESSIONSTATE_LOCK, WTS_SESSIONSTATE_UNLOCK,
    };
    unsafe {
        let mut buf: windows_sys::core::PWSTR = null_mut();
        let mut len = 0u32;
        let ok = WTSQuerySessionInformationW(WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTSSessionInfoEx, &mut buf, &mut len);
        if ok == 0 || buf.is_null() || (len as usize) < std::mem::size_of::<WTSINFOEXW>() {
            return None;
        }
        let info = &*(buf as *const WTSINFOEXW);
        let flags = (info.Level == 1).then_some(info.Data.WTSInfoExLevel1.SessionFlags as u32);
        WTSFreeMemory(buf as _);
        match flags {
            Some(WTS_SESSIONSTATE_LOCK) => Some(true),
            Some(WTS_SESSIONSTATE_UNLOCK) => Some(false),
            _ => None,
        }
    }
}

/// Returns false if another instance with this id is already running.
pub fn single_instance(instance_id: &str) -> bool {
    let name = wide(&format!("Local\\{instance_id}"));
    unsafe {
        let h = CreateMutexW(null(), 0, name.as_ptr());
        // The handle is intentionally leaked: it must live as long as the process.
        !h.is_null() && GetLastError() != ERROR_ALREADY_EXISTS
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_HOTKEY && wparam as i32 == HOTKEY_SETTINGS {
        open_settings();
        return 0;
    }
    if msg == WM_HOTKEY && wparam as i32 == HOTKEY_OK {
        // Read the waiting time before the publisher marks the session as checked in.
        if OK_FEEDBACK.load(Ordering::SeqCst) {
            let text = crate::state::ok_feedback_text(crate::state::unconfirmed_for(), crate::state::timeout_secs());
            show_toast("", &text, TOAST_MS);
        }
    }
    if msg == WM_APP_REMIND {
        let c = crate::state::check_in();
        if c.sound {
            // The system's new-mail sound: matches a mail-like notice, and is clearly audible.
            // Without a mail sound in the sound scheme, Windows plays its default sound.
            static MAIL_BEEP: OnceLock<Vec<u16>> = OnceLock::new();
            PlaySoundW(MAIL_BEEP.get_or_init(|| wide("MailBeep")).as_ptr(), null_mut(), SND_ALIAS | SND_ASYNC);
        }
        if c.notice && !c.notice_text.is_empty() {
            show_toast(&c.notice_title, &c.notice_text, REMINDER_MS);
        }
        return 0;
    }
    let trigger = match msg {
        WM_HOTKEY if wparam as i32 == HOTKEY_OK => Some(Trigger::Ok),
        WM_HOTKEY if wparam as i32 == HOTKEY_DURESS => Some(Trigger::Duress),
        WM_WTSSESSION_CHANGE if wparam == WTS_SESSION_UNLOCK => Some(Trigger::Unlock),
        _ => None,
    };
    if let (Some(t), Some(tx)) = (trigger, TX.get()) {
        let _ = tx.send(t);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Create a child control of the settings window.
unsafe fn control(
    parent: HWND,
    ex_style: u32,
    class: &str,
    text: &str,
    style: u32,
    (x, y, w, h): (i32, i32, i32, i32),
    id: usize,
) -> HWND {
    let class = wide(class);
    let text = wide(text);
    let h = CreateWindowExW(
        ex_style,
        class.as_ptr(),
        text.as_ptr(),
        WS_CHILD | WS_VISIBLE | style,
        x,
        y,
        w,
        h,
        parent,
        id as _,
        GetModuleHandleW(null()),
        null(),
    );
    SendMessageW(h, WM_SETFONT, GetStockObject(DEFAULT_GUI_FONT) as usize, 1);
    h
}

/// A font for the toast, created once per window thread.
unsafe fn toast_font(cell: &'static std::thread::LocalKey<Cell<HFONT>>, height: i32, weight: i32) -> HFONT {
    let mut font = cell.with(|f| f.get());
    if font.is_null() {
        let face = wide("Segoe UI");
        font = CreateFontW(height, 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, 5, 0, face.as_ptr());
        cell.with(|f| f.set(font));
    }
    font
}

/// A small note in the bottom-right corner that disappears by itself after `ms`. With a
/// title it looks like a system notification card. It never takes the focus, so typing goes
/// on undisturbed.
unsafe fn show_toast(title: &str, text: &str, ms: u32) {
    let old = TOAST.with(|t| t.replace(null_mut()));
    if !old.is_null() {
        DestroyWindow(old);
    }
    let Some(class) = TOAST_CLASS.get() else { return };
    let mut area: RECT = std::mem::zeroed();
    SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut area as *mut RECT as _, 0);
    let longest = title.chars().count().max(text.chars().count()) as i32;
    let (w, h) = if title.is_empty() { (40 + 9 * longest, 40) } else { ((40 + 8 * longest).clamp(280, 440), 66) };
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        class.as_ptr(),
        null(),
        WS_POPUP | WS_BORDER,
        area.right - w - 16,
        area.bottom - h - 16,
        w,
        h,
        null_mut(),
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    );
    if hwnd.is_null() {
        return;
    }
    let labels: Vec<(HWND, HFONT)> = if title.is_empty() {
        let label = control(hwnd, 0, "STATIC", text, SS_CENTER | SS_CENTERIMAGE, (0, 0, w - 2, h - 2), 0);
        vec![(label, toast_font(&TOAST_FONT, -17, 400))]
    } else {
        let t = control(hwnd, 0, "STATIC", title, 0, (16, 11, w - 32, 20), 0);
        let b = control(hwnd, 0, "STATIC", text, 0, (16, 33, w - 32, 20), 0);
        vec![(t, toast_font(&TOAST_BOLD, -15, 600)), (b, toast_font(&TOAST_SMALL, -14, 400))]
    };
    for (label, font) in labels {
        if !font.is_null() {
            SendMessageW(label, WM_SETFONT, font as usize, 1);
        }
    }
    TOAST.with(|t| t.set(hwnd));
    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    SetTimer(hwnd, 1, ms, None);
}

/// Called from the publisher's thread: hand the on-screen reminder to the window thread.
fn post_remind() {
    let hwnd = MAIN_HWND.load(Ordering::SeqCst) as HWND;
    if !hwnd.is_null() {
        unsafe { PostMessageW(hwnd, WM_APP_REMIND, 0, 0) };
    }
}

unsafe extern "system" fn toast_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            KillTimer(hwnd, 1);
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            TOAST.with(|t| {
                if t.get() == hwnd {
                    t.set(null_mut());
                }
            });
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// The owner's current details, read fresh from the config file.
fn current_details() -> Option<crate::config::OwnerDetails> {
    let (home, own) = NAME_TARGET.get()?;
    crate::config::Config::load(home).ok()?.own_details(own)
}

/// The text of one edit field of the settings window.
unsafe fn field_text(hwnd: HWND, id: i32) -> String {
    let mut buf = [0u16; 128];
    let n = GetWindowTextW(GetDlgItem(hwnd, id), buf.as_mut_ptr(), buf.len() as i32);
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// The small settings window: pause check-ins, and the name shown in alerts.
unsafe fn open_settings() {
    if !crate::state::settings_allowed() {
        // Right after a login without check-in: stay invisible.
        tracing::info!("settings hotkey ignored: check in first");
        return;
    }
    let existing = SETTINGS.with(|s| s.get());
    if !existing.is_null() {
        SetForegroundWindow(existing);
        return;
    }
    let Some(class) = SETTINGS_CLASS.get() else { return };
    let details = current_details();
    let check_in = crate::state::check_in();
    // The check-in block starts below the owner's details, when those are shown.
    let invite = details.is_some() && HAS_BOT.load(Ordering::SeqCst);
    let y0 = match (details.is_some(), invite) {
        (true, true) => 246,
        (true, false) => 190,
        _ => 84,
    };
    let title = wide("Settings");
    let hwnd = CreateWindowExW(
        0,
        class.as_ptr(),
        title.as_ptr(),
        WS_CAPTION | WS_SYSMENU,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        340,
        y0 + 326,
        null_mut(),
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    );
    if hwnd.is_null() {
        tracing::error!("settings window: CreateWindowExW failed: {}", GetLastError());
        return;
    }
    let check = control(
        hwnd,
        0,
        "BUTTON",
        "Check-in after sign-in and unlock",
        BS_AUTOCHECKBOX | WS_TABSTOP,
        (16, 16, 290, 22),
        ID_ENABLED,
    );
    control(
        hwnd,
        0,
        "STATIC",
        "When off, it turns on again at your next sign-in.\nThe duress key always works.",
        0,
        (16, 44, 300, 32),
        0,
    );
    SendMessageW(check, BM_SETCHECK, if crate::state::paused() { 0 } else { BST_CHECKED }, 0);
    let edit = |text: &str, rect: (i32, i32, i32, i32), id: i32| {
        control(hwnd, WS_EX_CLIENTEDGE, "EDIT", text, ES_AUTOHSCROLL | WS_TABSTOP, rect, id as usize)
    };
    let label = |text: &str, rect: (i32, i32, i32, i32)| control(hwnd, 0, "STATIC", text, 0, rect, 0);
    let checkbox = |text: &str, on: bool, rect: (i32, i32, i32, i32), id: i32| {
        let h = control(hwnd, 0, "BUTTON", text, BS_AUTOCHECKBOX | WS_TABSTOP, rect, id as usize);
        SendMessageW(h, BM_SETCHECK, if on { BST_CHECKED } else { 0 }, 0);
    };
    if let Some(d) = &details {
        label("Your name in alerts", (16, 84, 300, 18));
        edit(&d.name, (16, 103, 294, 24), ID_NAME);
        label("Your phone (with + and country code)", (16, 134, 300, 18));
        edit(&d.phone, (16, 153, 294, 24), ID_PHONE);
    }
    if invite {
        label("Invite a contact: send this link, they press Start", (16, 186, 300, 18));
        let link = INVITE_LINK.get().map(String::as_str).unwrap_or("(needs internet: open Settings again later)");
        control(hwnd, WS_EX_CLIENTEDGE, "EDIT", link, ES_AUTOHSCROLL | ES_READONLY, (16, 205, 214, 24), ID_INVITE as usize);
        control(hwnd, 0, "BUTTON", "Copy", WS_TABSTOP, (236, 204, 74, 26), ID_COPY_INVITE as usize);
    }
    label("Check-in time (seconds)", (16, y0 + 3, 200, 18));
    edit(&check_in.timeout_seconds.to_string(), (220, y0, 90, 22), ID_TIMEOUT);
    label("Reminder after (seconds, 0 = off)", (16, y0 + 33, 200, 18));
    edit(&check_in.reminder_seconds.to_string(), (220, y0 + 30, 90, 22), ID_REMINDER);
    checkbox("Reminder sound", check_in.sound, (16, y0 + 62, 290, 20), ID_REM_SOUND);
    checkbox("Notice, title:", check_in.notice, (16, y0 + 90, 120, 20), ID_REM_NOTICE);
    edit(&check_in.notice_title, (140, y0 + 88, 170, 22), ID_NOTICE_TITLE);
    label("text:", (110, y0 + 117, 30, 18));
    edit(&check_in.notice_text, (140, y0 + 114, 170, 22), ID_NOTICE_TEXT);
    checkbox("Telegram, chat id:", check_in.telegram, (16, y0 + 146, 124, 20), ID_REM_TELEGRAM);
    let chat = check_in.telegram_chat.map(|c| c.to_string()).unwrap_or_default();
    edit(&chat, (140, y0 + 144, 170, 22), ID_TELEGRAM_CHAT);
    label("text:", (110, y0 + 173, 30, 18));
    edit(&check_in.telegram_text, (140, y0 + 170, 170, 22), ID_TELEGRAM_TEXT);
    if invite {
        control(hwnd, 0, "BUTTON", "Angels…", WS_TABSTOP, (16, y0 + 206, 100, 26), ID_ANGELS as usize);
    }
    if details.is_some() {
        control(hwnd, 0, "BUTTON", "VIPs…", WS_TABSTOP, (122, y0 + 206, 98, 26), ID_PHONES as usize);
    }
    control(hwnd, 0, "BUTTON", "Save", BS_DEFPUSHBUTTON | WS_TABSTOP, (226, y0 + 206, 84, 26), IDOK);
    control(hwnd, 0, "STATIC", "", 0, (16, y0 + 240, 300, 34), ID_STATUS as usize);
    SETTINGS.with(|s| s.set(hwnd));
    ShowWindow(hwnd, SW_SHOW);
    SetForegroundWindow(hwnd);
}

/// The owner's angels as the built-in watcher keeps them (config chats plus joined ones).
fn current_angels() -> anyhow::Result<(String, Vec<(warn_watcher::contacts::AngelId, warn_watcher::contacts::Entry)>)> {
    let (home, own) = NAME_TARGET.get().ok_or_else(|| anyhow::anyhow!("this device has no owner in the watcher"))?;
    let cfg = crate::config::Config::load(home)?;
    let name = cfg.own_name(own).ok_or_else(|| anyhow::anyhow!("this device has no owner in the watcher"))?;
    let w = cfg.local_watcher(home).ok_or_else(|| anyhow::anyhow!("no built-in watcher"))?;
    let list = warn_watcher::contacts::Contacts::load(&w).list(&name);
    Ok((name, list))
}

/// The Angels window: a friendly name per angel, and which alarms each one gets.
unsafe fn open_angels() {
    let existing = ANGELS.with(|s| s.get());
    if !existing.is_null() {
        SetForegroundWindow(existing);
        return;
    }
    let Some(class) = ANGELS_CLASS.get() else { return };
    let (person, mut list) = match current_angels() {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!("angels window: {e:#}");
            return;
        }
    };
    list.truncate(MAX_ANGELS);
    let rows = list.len().max(1) as i32;
    let title = wide("Angels");
    let hwnd = CreateWindowExW(
        0,
        class.as_ptr(),
        title.as_ptr(),
        WS_CAPTION | WS_SYSMENU,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        500,
        200 + rows * 30,
        null_mut(),
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    );
    if hwnd.is_null() {
        tracing::error!("angels window: CreateWindowExW failed: {}", GetLastError());
        return;
    }
    let label = |text: &str, rect: (i32, i32, i32, i32)| control(hwnd, 0, "STATIC", text, 0, rect, 0);
    label("Name", (16, 16, 190, 18));
    label("No check-in", (222, 16, 80, 18));
    label("Late check-in", (312, 16, 80, 18));
    label("Active", (408, 16, 60, 18));
    if list.is_empty() {
        label("No angels yet: send your invite link (Settings).", (16, 44, 440, 18));
    }
    for (i, (id, e)) in list.iter().enumerate() {
        let y = 42 + i as i32 * 30;
        let i = i as i32;
        control(hwnd, WS_EX_CLIENTEDGE, "EDIT", &e.name, ES_AUTOHSCROLL | WS_TABSTOP, (16, y, 160, 24), (ID_ANGEL_NAME + i) as usize);
        // Which channel: the phone app (own key) or Telegram.
        let channel = if matches!(id, warn_watcher::contacts::AngelId::Key(_)) { "app" } else { "tg" };
        control(hwnd, 0, "STATIC", channel, 0, (182, y + 4, 34, 18), 0);
        for (id, x, on) in [(ID_ANGEL_MISSED, 248, e.missed), (ID_ANGEL_LATE, 340, e.late_ok), (ID_ANGEL_ACTIVE, 422, e.active)] {
            let h = control(hwnd, 0, "BUTTON", "", BS_AUTOCHECKBOX | WS_TABSTOP, (x, y + 2, 20, 20), (id + i) as usize);
            SendMessageW(h, BM_SETCHECK, if on { BST_CHECKED } else { 0 }, 0);
        }
    }
    let y = 50 + rows * 30;
    label(
        "Duress alarms always go to every active angel. Changes also reach your other watchers.",
        (16, y, 450, 34),
    );
    control(hwnd, 0, "BUTTON", "Save", BS_DEFPUSHBUTTON | WS_TABSTOP, (382, y + 40, 84, 26), IDOK);
    control(hwnd, 0, "STATIC", "", 0, (16, y + 44, 350, 40), ID_STATUS as usize);
    ANGEL_ROWS.with(|r| {
        *r.borrow_mut() = list.into_iter().map(|(id, entry)| AngelRow { person: person.clone(), id, entry }).collect()
    });
    ANGELS.with(|s| s.set(hwnd));
    ShowWindow(hwnd, SW_SHOW);
    SetForegroundWindow(hwnd);
}

/// Hand every changed row to the built-in watcher, which saves it and tells the other watchers.
unsafe fn save_angels(hwnd: HWND) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    let mut changed = 0;
    let mut failed = false;
    let mut nobody_missed = true;
    ANGEL_ROWS.with(|rows| {
        for (i, row) in rows.borrow_mut().iter_mut().enumerate() {
            let i = i as i32;
            let name = field_text(hwnd, ID_ANGEL_NAME + i).trim().to_string();
            let entry = warn_watcher::contacts::Entry {
                name: if name.is_empty() { row.entry.name.clone() } else { name },
                active: checked(hwnd, ID_ANGEL_ACTIVE + i),
                missed: checked(hwnd, ID_ANGEL_MISSED + i),
                late_ok: checked(hwnd, ID_ANGEL_LATE + i),
                removed: false,
                push: None,
                at: now,
            };
            if entry.active && entry.missed {
                nobody_missed = false;
            }
            let same = entry.name == row.entry.name
                && entry.active == row.entry.active
                && entry.missed == row.entry.missed
                && entry.late_ok == row.entry.late_ok;
            if same {
                continue;
            }
            let edit = warn_watcher::AngelEdit { person: row.person.clone(), id: row.id.clone(), entry: entry.clone() };
            if warn_watcher::edit_angel(edit) {
                row.entry = entry;
                changed += 1;
            } else {
                failed = true;
            }
        }
    });
    tracing::info!("angels window: {changed} angel(s) changed");
    let mut status = if failed {
        "Not saved: the built-in watcher is not running.".to_string()
    } else if changed == 0 {
        "Nothing changed.".to_string()
    } else {
        format!("Saved ({changed} changed).")
    };
    if nobody_missed {
        status.push_str(" Warning: no active angel gets missed check-ins.");
    }
    SetWindowTextW(GetDlgItem(hwnd, ID_STATUS), wide(&status).as_ptr());
}

unsafe extern "system" fn angels_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND if wparam & 0xffff == IDOK => {
            save_angels(hwnd);
            0
        }
        WM_COMMAND if wparam & 0xffff == IDCANCEL => {
            DestroyWindow(hwnd);
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            ANGELS.with(|s| s.set(null_mut()));
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// One VIP of the built-in watcher, for the VIPs window.
struct VipDevices {
    name: String,
    /// "you", "in the config" or "invited".
    role: &'static str,
    devices: Vec<warn_proto::Phone>,
}

/// Every VIP the built-in watcher watches, with their devices, and this computer's key (hex).
fn current_vips() -> anyhow::Result<(Vec<VipDevices>, String)> {
    let (home, own) = NAME_TARGET.get().ok_or_else(|| anyhow::anyhow!("this device has no owner in the watcher"))?;
    let cfg = crate::config::Config::load(home)?;
    let me = cfg.own_name(own).unwrap_or_default();
    let w = cfg.local_watcher(home).ok_or_else(|| anyhow::anyhow!("no built-in watcher"))?;
    let phones = warn_watcher::phones::Phones::load(&w.data_dir);
    let invited = warn_watcher::vips::Vips::load(&w.data_dir).persons();
    let mut out = Vec::new();
    for (p, is_invited) in w.persons.iter().map(|p| (p, false)).chain(invited.iter().map(|p| (p, true))) {
        // An invited VIP who was removed has no devices: no longer watched.
        if p.devices.is_empty() {
            continue;
        }
        let role = if p.name == me { "you" } else if is_invited { "invited" } else { "in the config" };
        out.push(VipDevices { name: p.name.clone(), role, devices: phones.list(&p.name, &p.devices) });
    }
    Ok((out, own.to_hex()))
}

/// "seen 5 min ago", or "not seen yet" (the watcher keeps track since an update in Nov 2026).
fn seen_text(seen: u64) -> String {
    if seen == 0 {
        return "not seen yet".into();
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    let d = now.saturating_sub(seen);
    match d {
        0..3600 => format!("seen {} min ago", (d / 60).max(1)),
        3600..86400 => format!("seen {} h ago", d / 3600),
        _ => format!("seen {} days ago", d / 86400),
    }
}

/// The VIPs window: every VIP this computer's watcher watches, with their devices (rename,
/// remove a lost one) and "Add a phone" per VIP.
unsafe fn open_phones() {
    let existing = PHONES.with(|s| s.get());
    if !existing.is_null() {
        SetForegroundWindow(existing);
        return;
    }
    let Some(class) = PHONES_CLASS.get() else { return };
    let (mut vips, own) = match current_vips() {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!("VIPs window: {e:#}");
            return;
        }
    };
    vips.truncate(MAX_VIPS);
    // Room: a header per VIP and a line per device, up to MAX_PHONES lines in all.
    let mut left = MAX_PHONES;
    for v in &mut vips {
        v.devices.truncate(left.max(1));
        left = left.saturating_sub(v.devices.len());
    }
    let lines: i32 = vips.iter().map(|v| 1 + v.devices.len().max(1) as i32).sum::<i32>().max(1);
    let list_bottom = 44 + lines * 30;
    let qr_top = list_bottom + 130;
    let title = wide("VIPs");
    let hwnd = CreateWindowExW(
        0,
        class.as_ptr(),
        title.as_ptr(),
        WS_CAPTION | WS_SYSMENU,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        460,
        qr_top + QR_SIZE + 56,
        null_mut(),
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    );
    if hwnd.is_null() {
        tracing::error!("VIPs window: CreateWindowExW failed: {}", GetLastError());
        return;
    }
    let label = |text: &str, rect: (i32, i32, i32, i32)| control(hwnd, 0, "STATIC", text, 0, rect, 0);
    label("Who this computer watches, and their devices", (16, 12, 270, 18));
    control(hwnd, 0, "BUTTON", "Offer this server…", WS_TABSTOP, (296, 8, 130, 26), ID_OFFER as usize);
    if vips.is_empty() {
        label("Nobody yet.", (16, 44, 380, 18));
    }
    let mut rows = Vec::new();
    let mut y = 42;
    for (v_i, v) in vips.iter().enumerate() {
        label(&format!("{} ({})", v.name, v.role), (16, y + 6, 270, 18));
        control(hwnd, 0, "BUTTON", "Add phone or tablet", WS_TABSTOP, (296, y, 130, 26), (ID_PAIR_VIP + v_i as i32) as usize);
        y += 30;
        if v.devices.is_empty() {
            label("No devices.", (36, y + 4, 300, 18));
            y += 30;
        }
        for d in &v.devices {
            let i = rows.len() as i32;
            control(hwnd, WS_EX_CLIENTEDGE, "EDIT", &d.name, ES_AUTOHSCROLL | WS_TABSTOP, (36, y, 180, 24), (ID_PHONE_NAME + i) as usize);
            if d.key == own {
                label("this computer", (226, y + 4, 110, 18));
            } else {
                label(&seen_text(d.seen), (226, y + 4, 110, 18));
                if v.devices.len() > 1 {
                    control(hwnd, 0, "BUTTON", "Remove", BS_AUTOCHECKBOX | WS_TABSTOP, (346, y + 2, 80, 20), (ID_PHONE_REMOVE + i) as usize);
                }
            }
            rows.push(PhoneRow { person: v.name.clone(), key: d.key.clone(), name: d.name.clone() });
            y += 30;
        }
    }
    let y = list_bottom + 6;
    label("Lost a phone? Tick Remove, then Save. Your other watchers hear it too.", (16, y, 420, 34));
    control(hwnd, 0, "BUTTON", "Save", BS_DEFPUSHBUTTON | WS_TABSTOP, (342, y + 38, 84, 26), IDOK);
    control(hwnd, 0, "STATIC", "", 0, (16, y + 40, 320, 34), ID_STATUS as usize);
    let y2 = y + 86;
    control(hwnd, WS_EX_CLIENTEDGE, "EDIT", "", ES_AUTOHSCROLL | ES_READONLY, (16, y2 + 1, 340, 24), ID_PAIR_LINK as usize);
    control(hwnd, 0, "BUTTON", "Copy", WS_TABSTOP, (364, y2, 62, 26), ID_PAIR_COPY as usize);
    PHONE_ROWS.with(|r| *r.borrow_mut() = rows);
    VIP_NAMES.with(|n| *n.borrow_mut() = vips.iter().map(|v| v.name.clone()).collect());
    PHONES.with(|s| s.set(hwnd));
    // Where the QR code goes, once there is a link.
    PAIR_QR.with(|q| *q.borrow_mut() = Some((qr_top, 0, vec![])));
    ShowWindow(hwnd, SW_SHOW);
    SetForegroundWindow(hwnd);
}

/// Hand every renamed or removed device to the built-in watcher.
unsafe fn save_phones(hwnd: HWND) {
    let mut changed = 0;
    let mut failed = false;
    PHONE_ROWS.with(|rows| {
        for (i, row) in rows.borrow_mut().iter_mut().enumerate() {
            let i = i as i32;
            let name = field_text(hwnd, ID_PHONE_NAME + i).trim().to_string();
            let removed = checked(hwnd, ID_PHONE_REMOVE + i);
            if !removed && (name.is_empty() || name == row.name) {
                continue;
            }
            let edit = warn_watcher::PhoneEdit { person: row.person.clone(), key: row.key.clone(), name: name.clone(), removed };
            if warn_watcher::edit_phone(edit) {
                changed += 1;
                if !name.is_empty() {
                    row.name = name;
                }
            } else {
                failed = true;
            }
        }
    });
    tracing::info!("VIPs window: {changed} device(s) changed");
    let status = if failed {
        "Not saved: the built-in watcher is not running.".to_string()
    } else if changed == 0 {
        "Nothing changed.".to_string()
    } else {
        format!("Saved ({changed} changed).")
    };
    SetWindowTextW(GetDlgItem(hwnd, ID_STATUS), wide(&status).as_ptr());
}

/// Ask the built-in watcher for a link for VIP number `v`; it answers on its own thread, so
/// post it back here.
unsafe fn request_pair(hwnd: HWND, v: usize) {
    let Some(person) = VIP_NAMES.with(|n| n.borrow().get(v).cloned()) else { return };
    PAIR_FOR.with(|p| *p.borrow_mut() = person.clone());
    let target = hwnd as isize;
    let asked = warn_watcher::request_pair(
        &person,
        Box::new(move |r| {
            if let Ok(mut g) = PAIR_RESULT.lock() {
                *g = Some(r);
            }
            unsafe {
                PostMessageW(target as HWND, WM_APP_PAIR, 0, 0);
            }
        }),
    );
    let status = if asked { format!("Making a link for {person}…") } else { "Not possible: the built-in watcher is not running.".to_string() };
    SetWindowTextW(GetDlgItem(hwnd, ID_STATUS), wide(&status).as_ptr());
}

/// Ask the built-in watcher for a link that offers this server to someone new.
unsafe fn request_offer(hwnd: HWND) {
    let me = NAME_TARGET.get().and_then(|(home, own)| crate::config::Config::load(home).ok()?.own_name(own));
    let Some(person) = me else { return };
    PAIR_FOR.with(|p| p.borrow_mut().clear());
    let target = hwnd as isize;
    let asked = warn_watcher::request_offer(
        &person,
        Box::new(move |r| {
            if let Ok(mut g) = PAIR_RESULT.lock() {
                *g = Some(r);
            }
            unsafe {
                PostMessageW(target as HWND, WM_APP_PAIR, 0, 0);
            }
        }),
    );
    let status = if asked { "Making a link…" } else { "Not possible: the built-in watcher is not running." };
    SetWindowTextW(GetDlgItem(hwnd, ID_STATUS), wide(status).as_ptr());
}

/// The watcher's answer: show the link and its QR code.
unsafe fn show_pair(hwnd: HWND) {
    let Some(result) = PAIR_RESULT.lock().ok().and_then(|mut g| g.take()) else { return };
    let status = match result {
        Ok(link) => {
            SetWindowTextW(GetDlgItem(hwnd, ID_PAIR_LINK), wide(&link).as_ptr());
            match qrcode::QrCode::new(link.as_bytes()) {
                Ok(code) => {
                    let modules: Vec<bool> = code.to_colors().into_iter().map(|c| c == qrcode::Color::Dark).collect();
                    PAIR_QR.with(|q| {
                        let top = q.borrow().as_ref().map(|x| x.0).unwrap_or(0);
                        *q.borrow_mut() = Some((top, code.width(), modules));
                    });
                    InvalidateRect(hwnd, null(), 1);
                }
                Err(e) => tracing::warn!("QR code: {e}"),
            }
            let person = PAIR_FOR.with(|p| p.borrow().clone());
            if person.is_empty() {
                "For someone this server should watch over: they open it in Sync. Works once, within 7 days.".to_string()
            } else {
                format!("For {person}: scan this with the new phone's camera, or send it the link. It works once, within 30 minutes.")
            }
        }
        Err(e) => format!("No link: {e}"),
    };
    SetWindowTextW(GetDlgItem(hwnd, ID_STATUS), wide(&status).as_ptr());
}

/// Draw the QR code, black on white with a quiet zone.
unsafe fn paint_phones(hwnd: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let dc = BeginPaint(hwnd, &mut ps);
    PAIR_QR.with(|q| {
        let Some((top, n, modules)) = q.borrow().clone() else { return };
        if n == 0 {
            return;
        }
        let quiet = 4;
        let scale = (QR_SIZE / (n as i32 + 2 * quiet)).max(1);
        let size = (n as i32 + 2 * quiet) * scale;
        let left = 16;
        let white = GetStockObject(WHITE_BRUSH) as HBRUSH;
        let black = GetStockObject(BLACK_BRUSH) as HBRUSH;
        FillRect(dc, &RECT { left, top, right: left + size, bottom: top + size }, white);
        for (i, dark) in modules.iter().enumerate() {
            if !dark {
                continue;
            }
            let (x, y) = ((i % n) as i32 + quiet, (i / n) as i32 + quiet);
            let r = RECT { left: left + x * scale, top: top + y * scale, right: left + (x + 1) * scale, bottom: top + (y + 1) * scale };
            FillRect(dc, &r, black);
        }
    });
    EndPaint(hwnd, &ps);
}

unsafe extern "system" fn phones_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND if wparam & 0xffff == IDOK => {
            save_phones(hwnd);
            0
        }
        WM_COMMAND if (ID_PAIR_VIP as usize..(ID_PAIR_VIP as usize + MAX_VIPS)).contains(&(wparam & 0xffff)) => {
            request_pair(hwnd, (wparam & 0xffff) - ID_PAIR_VIP as usize);
            0
        }
        WM_COMMAND if wparam & 0xffff == ID_OFFER as usize => {
            request_offer(hwnd);
            0
        }
        WM_COMMAND if wparam & 0xffff == ID_PAIR_COPY as usize => {
            let field = GetDlgItem(hwnd, ID_PAIR_LINK);
            SendMessageW(field, EM_SETSEL, 0, -1);
            SendMessageW(field, WM_COPY, 0, 0);
            0
        }
        WM_APP_PAIR => {
            show_pair(hwnd);
            0
        }
        WM_PAINT => {
            paint_phones(hwnd);
            0
        }
        WM_COMMAND if wparam & 0xffff == IDCANCEL => {
            DestroyWindow(hwnd);
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            PHONES.with(|s| s.set(null_mut()));
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn checked(hwnd: HWND, id: i32) -> bool {
    SendMessageW(GetDlgItem(hwnd, id), BM_GETCHECK, 0, 0) as usize == BST_CHECKED
}

/// Save what was typed in the settings window; the running agent and watcher use it at once.
unsafe fn save_settings(hwnd: HWND) {
    let status = match save_fields(hwnd) {
        Ok(()) => "Saved. Used from the next sign-in or unlock.".to_string(),
        Err(e) => format!("Not saved: {e}"),
    };
    SetWindowTextW(GetDlgItem(hwnd, ID_STATUS), wide(&status).as_ptr());
}

unsafe fn save_fields(hwnd: HWND) -> anyhow::Result<()> {
    let home = HOME.get().ok_or_else(|| anyhow::anyhow!("no config folder"))?;
    let (timeout, reminder) = (field_text(hwnd, ID_TIMEOUT), field_text(hwnd, ID_REMINDER));
    let (notice_title, notice_text) = (field_text(hwnd, ID_NOTICE_TITLE), field_text(hwnd, ID_NOTICE_TEXT));
    let (chat, telegram_text) = (field_text(hwnd, ID_TELEGRAM_CHAT), field_text(hwnd, ID_TELEGRAM_TEXT));
    let form = crate::config::CheckInForm {
        timeout: &timeout,
        reminder: &reminder,
        sound: checked(hwnd, ID_REM_SOUND),
        notice: checked(hwnd, ID_REM_NOTICE),
        notice_title: &notice_title,
        notice_text: &notice_text,
        telegram: checked(hwnd, ID_REM_TELEGRAM),
        telegram_chat: &chat,
        telegram_text: &telegram_text,
    };
    // Check everything before saving anything.
    let check_in = crate::config::CheckIn::parse(&form, HAS_BOT.load(Ordering::SeqCst))?;
    let owner = match NAME_TARGET.get() {
        Some(target) if !GetDlgItem(hwnd, ID_NAME).is_null() => {
            Some((target, crate::config::OwnerDetails { name: field_text(hwnd, ID_NAME), phone: field_text(hwnd, ID_PHONE) }))
        }
        _ => None,
    };
    if let Some(((home, own), details)) = owner {
        let saved = crate::config::set_own_details(home, own, &details)?;
        tracing::info!("owner details changed in settings");
        crate::state::reload_watcher();
        for (id, v) in [(ID_NAME, &saved.name), (ID_PHONE, &saved.phone)] {
            SetWindowTextW(GetDlgItem(hwnd, id), wide(v).as_ptr());
        }
    }
    crate::config::set_check_in(home, &check_in)?;
    tracing::info!("check-in settings changed in settings");
    crate::state::set_check_in(check_in);
    Ok(())
}

unsafe extern "system" fn settings_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND if wparam & 0xffff == IDOK => {
            save_settings(hwnd);
            0
        }
        WM_COMMAND if wparam & 0xffff == ID_COPY_INVITE as usize => {
            if INVITE_LINK.get().is_some() {
                let field = GetDlgItem(hwnd, ID_INVITE);
                SendMessageW(field, EM_SETSEL, 0, -1);
                SendMessageW(field, WM_COPY, 0, 0);
                SetWindowTextW(GetDlgItem(hwnd, ID_STATUS), wide("Link copied. New contacts are approved by your current contacts.").as_ptr());
            }
            0
        }
        WM_COMMAND if wparam & 0xffff == ID_ANGELS as usize => {
            open_angels();
            0
        }
        WM_COMMAND if wparam & 0xffff == ID_PHONES as usize => {
            open_phones();
            0
        }
        WM_COMMAND if wparam & 0xffff == IDCANCEL => {
            DestroyWindow(hwnd);
            0
        }
        WM_COMMAND if wparam & 0xffff == ID_ENABLED => {
            let enabled = SendMessageW(lparam as HWND, BM_GETCHECK, 0, 0) as usize == BST_CHECKED;
            crate::state::set_paused(!enabled);
            if let Some(tx) = TX.get() {
                let _ = tx.send(Trigger::Paused(!enabled));
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            SETTINGS.with(|s| s.set(null_mut()));
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// What the window code needs from the configuration.
pub struct Setup {
    pub home: PathBuf,
    /// Config folder and this device's key, when the settings window may change the owner.
    pub name_target: Option<(PathBuf, PublicKey)>,
    pub ok_feedback: bool,
    pub has_bot: bool,
}

/// Blocks forever, forwarding unlocks and hotkey presses to `tx`.
pub fn message_loop(
    class_name: &str,
    ok: Hotkey,
    duress: Option<Hotkey>,
    settings: Option<Hotkey>,
    setup: Setup,
    tx: UnboundedSender<Trigger>,
) -> Result<()> {
    let _ = TX.set(tx);
    OK_FEEDBACK.store(setup.ok_feedback, Ordering::SeqCst);
    HAS_BOT.store(setup.has_bot, Ordering::SeqCst);
    let _ = HOME.set(setup.home);
    if let Some(t) = setup.name_target {
        let _ = NAME_TARGET.set(t);
    }
    let class = wide(class_name);
    unsafe {
        let hinst = GetModuleHandleW(null());
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(wndproc);
        wc.hInstance = hinst;
        wc.lpszClassName = class.as_ptr();
        if RegisterClassW(&wc) == 0 {
            bail!("RegisterClassW failed: {}", GetLastError());
        }
        let settings_class = SETTINGS_CLASS.get_or_init(|| wide(&format!("{class_name}s")));
        let mut sc: WNDCLASSW = std::mem::zeroed();
        sc.lpfnWndProc = Some(settings_proc);
        sc.hInstance = hinst;
        sc.lpszClassName = settings_class.as_ptr();
        sc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        sc.hbrBackground = BTNFACE_BRUSH as _;
        if RegisterClassW(&sc) == 0 {
            bail!("RegisterClassW (settings) failed: {}", GetLastError());
        }
        let angels_class = ANGELS_CLASS.get_or_init(|| wide(&format!("{class_name}a")));
        let mut ac: WNDCLASSW = std::mem::zeroed();
        ac.lpfnWndProc = Some(angels_proc);
        ac.hInstance = hinst;
        ac.lpszClassName = angels_class.as_ptr();
        ac.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        ac.hbrBackground = BTNFACE_BRUSH as _;
        if RegisterClassW(&ac) == 0 {
            bail!("RegisterClassW (angels) failed: {}", GetLastError());
        }
        let phones_class = PHONES_CLASS.get_or_init(|| wide(&format!("{class_name}p")));
        let mut pc: WNDCLASSW = std::mem::zeroed();
        pc.lpfnWndProc = Some(phones_proc);
        pc.hInstance = hinst;
        pc.lpszClassName = phones_class.as_ptr();
        pc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        pc.hbrBackground = BTNFACE_BRUSH as _;
        if RegisterClassW(&pc) == 0 {
            bail!("RegisterClassW (phones) failed: {}", GetLastError());
        }
        let toast_class = TOAST_CLASS.get_or_init(|| wide(&format!("{class_name}t")));
        let mut tc: WNDCLASSW = std::mem::zeroed();
        tc.lpfnWndProc = Some(toast_proc);
        tc.hInstance = hinst;
        tc.lpszClassName = toast_class.as_ptr();
        tc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        tc.hbrBackground = BTNFACE_BRUSH as _;
        if RegisterClassW(&tc) == 0 {
            bail!("RegisterClassW (toast) failed: {}", GetLastError());
        }
        // A regular top-level window that is never shown (message-only windows do not
        // reliably receive session notifications).
        let hwnd = CreateWindowExW(0, class.as_ptr(), class.as_ptr(), 0, 0, 0, 0, 0, null_mut(), null_mut(), hinst, null());
        if hwnd.is_null() {
            bail!("CreateWindowExW failed: {}", GetLastError());
        }
        MAIN_HWND.store(hwnd as isize, Ordering::SeqCst);
        crate::state::set_remind_on_screen(post_remind);
        if WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) == 0 {
            bail!("WTSRegisterSessionNotification failed: {}", GetLastError());
        }
        if RegisterHotKey(hwnd, HOTKEY_OK, ok.modifiers | MOD_NOREPEAT, ok.vk) == 0 {
            bail!("check-in hotkey is already taken by another program (error {}): choose another", GetLastError());
        }
        if let Some(d) = duress {
            if RegisterHotKey(hwnd, HOTKEY_DURESS, d.modifiers | MOD_NOREPEAT, d.vk) == 0 {
                tracing::error!("duress hotkey is already taken (error {})", GetLastError());
            }
        }
        if let Some(h) = settings {
            if RegisterHotKey(hwnd, HOTKEY_SETTINGS, h.modifiers | MOD_NOREPEAT, h.vk) == 0 {
                tracing::error!("settings hotkey is already taken (error {})", GetLastError());
            }
        }
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            // Tab, Enter and Escape in the settings window.
            let settings = SETTINGS.with(|s| s.get());
            if !settings.is_null() && IsDialogMessageW(settings, &msg) != 0 {
                continue;
            }
            let angels = ANGELS.with(|s| s.get());
            if !angels.is_null() && IsDialogMessageW(angels, &msg) != 0 {
                continue;
            }
            let phones = PHONES.with(|s| s.get());
            if !phones.is_null() && IsDialogMessageW(phones, &msg) != 0 {
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}
