// Anti Social companion: holds the encrypted connection to the server, remembers this
// install's identity, and drives two windows (the main panel and the floating overlay icon).
//
// The windows build their own protocol messages and send them through `send_message`;
// this side owns sign-in, reconnecting, settings, the tray and the overlay.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{
    AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, PhysicalSize, State, UserAttentionType, WebviewWindow, WindowEvent, Wry,
};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_dialog::DialogExt;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

const PROTOCOL_VERSION: u32 = 1;
/// The one server every copy of the app uses. It is deliberately not a setting: everyone on the
/// same server means one shared queue. To move the server, change this and release an update.
const SERVER: &str = "wss://lfg.lfgforever.org/ws";
/// The address earlier builds used. It reached the same server, so installs saved with it are moved over.
const OLD_SERVER: &str = "wss://lfg.lastweeksproject.com/ws";
const APP_TAG: &str = concat!("companion/", env!("CARGO_PKG_VERSION"));

/// The address to connect to. Development builds can point at a test server through the
/// ANTISOCIAL_SERVER environment variable; released builds always use the built-in address.
fn server_address() -> String {
    #[cfg(debug_assertions)]
    if let Ok(url) = std::env::var("ANTISOCIAL_SERVER") {
        if !url.trim().is_empty() {
            return url.trim().to_string();
        }
    }
    SERVER.to_string()
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone)]
#[serde(default, rename_all = "camelCase")]
struct Settings {
    wow_dir: String,
    overlay_enabled: bool,
    overlay_locked: bool,
    overlay_x: Option<i32>,
    overlay_y: Option<i32>,
    /// Where the main window was last left, and how big (size in logical pixels).
    main_x: Option<i32>,
    main_y: Option<i32>,
    main_w: Option<f64>,
    main_h: Option<f64>,
    /// This install's secret token, kept under the server address it belongs to. Never sent to the windows.
    tokens: HashMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            wow_dir: detect_wow_dir().unwrap_or_default(),
            overlay_enabled: true,
            overlay_locked: false,
            overlay_x: None,
            overlay_y: None,
            main_x: None,
            main_y: None,
            main_w: None,
            main_h: None,
            tokens: HashMap::new(),
        }
    }
}

/// Looks in the usual install locations for a Forever client folder.
fn detect_wow_dir() -> Option<String> {
    let roots = [
        "World of Warcraft",
        "Program Files (x86)\\World of Warcraft",
        "Program Files\\World of Warcraft",
        "Games\\World of Warcraft",
    ];
    for drive in ['C', 'D', 'E', 'F', 'G'] {
        for root in roots {
            for flavor in ["_forever_", "_retail_"] {
                let dir = PathBuf::from(format!("{drive}:\\{root}\\{flavor}"));
                if dir.join("WTF").is_dir() {
                    return Some(dir.to_string_lossy().into_owned());
                }
            }
        }
    }
    None
}

fn settings_path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|dir| dir.join("settings.json"))
}

fn load_settings(app: &AppHandle) -> Settings {
    settings_path(app)
        .and_then(|path| fs::read(path).ok())
        // Editors such as Notepad can add a byte-order mark, which JSON parsing rejects.
        .and_then(|bytes| serde_json::from_slice(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes)).ok())
        .map(migrate_settings)
        .unwrap_or_default()
}

/// Carries this install's identity over from the address earlier builds used. A server address
/// saved by an earlier build is ignored: the field no longer exists, so it is dropped on the next save.
fn migrate_settings(mut settings: Settings) -> Settings {
    if let Some(token) = settings.tokens.remove(OLD_SERVER) {
        settings.tokens.entry(SERVER.to_string()).or_insert(token);
    }
    settings
}

fn save_settings(app: &AppHandle, settings: &Settings) {
    let Some(path) = settings_path(app) else { return };
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(settings) {
        let _ = fs::write(path, bytes);
    }
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

enum Outbound {
    Message(String),
}

struct Shared {
    settings: Settings,
    /// "connecting", "online" or "offline".
    connection: &'static str,
    /// Why we are offline, when the reason is worth showing.
    detail: Option<String>,
    welcome: Option<Value>,
    state: Option<Value>,
    /// Queue totals and average waits, as last sent by the server.
    stats: Option<Value>,
    /// A newer published version of this app, when there is one.
    update: Option<String>,
    outbound: Option<mpsc::UnboundedSender<Outbound>>,
    settings_dirty: bool,
}

type SharedState = Arc<Mutex<Shared>>;

fn snapshot(s: &Shared) -> Value {
    json!({
        "connection": s.connection,
        "detail": s.detail,
        "welcome": s.welcome,
        "state": s.state,
        "stats": s.stats,
        "update": s.update,
        "version": env!("CARGO_PKG_VERSION"),
        "settings": {
            "wowDir": s.settings.wow_dir,
            "overlayEnabled": s.settings.overlay_enabled,
            "overlayLocked": s.settings.overlay_locked,
        },
    })
}

/// Sends the current snapshot to both windows.
fn broadcast(app: &AppHandle, shared: &SharedState) {
    let snap = snapshot(&shared.lock().unwrap());
    let _ = app.emit("app-state", snap);
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

fn show_main(app: &AppHandle, attention: bool) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
        if attention {
            let _ = window.request_user_attention(Some(UserAttentionType::Critical));
        }
    }
}

fn toggle_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        if window.is_visible().unwrap_or(false) {
            let _ = window.hide();
        } else {
            show_main(app, false);
        }
    }
}

#[cfg(windows)]
mod win32 {
    #[repr(C)]
    pub struct Rect {
        pub left: i32,
        pub top: i32,
        pub right: i32,
        pub bottom: i32,
    }
    #[link(name = "gdi32")]
    extern "system" {
        pub fn CreateEllipticRgn(x1: i32, y1: i32, x2: i32, y2: i32) -> isize;
    }
    #[link(name = "user32")]
    extern "system" {
        pub fn SetWindowRgn(hwnd: isize, region: isize, redraw: i32) -> i32;
        pub fn GetWindowLongPtrW(hwnd: isize, index: i32) -> isize;
        pub fn SetWindowLongPtrW(hwnd: isize, index: i32, value: isize) -> isize;
        pub fn SetWindowPos(hwnd: isize, after: isize, x: i32, y: i32, w: i32, h: i32, flags: u32) -> i32;
        pub fn FindWindowW(class: *const u16, title: *const u16) -> isize;
        pub fn GetWindowRect(hwnd: isize, rect: *mut Rect) -> i32;
        pub fn IsIconic(hwnd: isize) -> i32;
    }
}

/// Diameter, in logical pixels, of the circle the overlay window is cut down to. The orb is
/// 40 wide with a 3 pixel opaque dark ring around it (46 in all); cutting at 43 lands inside
/// that ring, so the window has no transparent part for anything behind it to show through.
const OVERLAY_DIAMETER: f64 = 43.0;

/// Where the game's window is on screen (left, top, width, height in physical pixels), if it
/// is open and not minimised. This only asks Windows where a window is, the same thing any
/// window manager does. It reads nothing from the game.
fn game_window() -> Option<(i32, i32, i32, i32)> {
    #[cfg(windows)]
    {
        let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
        let class = wide("waApplication Window");
        let title = wide("World of Warcraft");
        // Safety: plain Win32 lookups with valid, null-terminated strings and a valid out pointer.
        unsafe {
            let mut hwnd = win32::FindWindowW(class.as_ptr(), title.as_ptr());
            if hwnd == 0 {
                hwnd = win32::FindWindowW(class.as_ptr(), std::ptr::null());
            }
            if hwnd == 0 || win32::IsIconic(hwnd) != 0 {
                return None;
            }
            let mut r = win32::Rect { left: 0, top: 0, right: 0, bottom: 0 };
            if win32::GetWindowRect(hwnd, &mut r) == 0 {
                return None;
            }
            let (w, h) = (r.right - r.left, r.bottom - r.top);
            if w < 300 || h < 300 {
                return None;
            }
            Some((r.left, r.top, w, h))
        }
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Puts the orb just inside the top-left corner of the game window, or of the main screen
/// when the game is not open. Returns the window position it chose.
fn snap_overlay(window: &WebviewWindow) -> Option<(i32, i32)> {
    let scale = window.scale_factor().ok()?;
    let width = window.outer_size().ok()?.width as i32;
    let height = window.outer_size().ok()?.height as i32;
    let (left, top) = match game_window() {
        Some((left, top, _, _)) => (left, top),
        None => {
            let monitor = window.primary_monitor().ok()??;
            (monitor.position().x, monitor.position().y)
        }
    };
    let margin = (14.0 * scale).round() as i32;
    let diameter = (OVERLAY_DIAMETER * scale).round() as i32;
    // The orb sits in the middle of a window that is wider than it; line up the orb, not the window.
    let x = left + margin - (width - diameter) / 2;
    let y = top + margin - (height - diameter) / 2;
    let _ = window.set_position(PhysicalPosition::new(x, y));
    Some((x, y))
}

/// Cuts the overlay window down to a circle around the orb. Windows will not make a window
/// narrower than about 136 pixels, so without this an invisible strip beside the orb would
/// swallow clicks meant for the game.
fn shape_overlay(window: &WebviewWindow) {
    #[cfg(windows)]
    {
        let (Ok(size), Ok(scale), Ok(hwnd)) = (window.outer_size(), window.scale_factor(), window.hwnd()) else { return };
        let diameter = (OVERLAY_DIAMETER * scale).round() as i32;
        let cx = size.width as i32 / 2;
        let cy = size.height as i32 / 2;

        // A borderless window still carries a hidden title-bar style, and Windows sometimes paints
        // a ghost of that bar behind the orb when focus changes. Turn it into a plain popup with no
        // title bar or frame at all, which also never takes focus away from the game when clicked.
        const GWL_STYLE: i32 = -16;
        const GWL_EXSTYLE: i32 = -20;
        const FRAME_STYLES: isize = 0x00C0_0000 | 0x0004_0000 | 0x0008_0000 | 0x0002_0000 | 0x0001_0000; // caption, sizing frame, system menu, min and max boxes
        const WS_POPUP: isize = 0x8000_0000;
        const WS_EX_TOOLWINDOW: isize = 0x0000_0080;
        const WS_EX_NOACTIVATE: isize = 0x0800_0000;
        const SWP_REFRAME: u32 = 0x0001 | 0x0002 | 0x0004 | 0x0010 | 0x0020; // keep size, place and order; do not activate; recalculate the frame
        // Safety: plain Win32 calls on a window we own.
        unsafe {
            let handle = hwnd.0 as isize;
            let style = win32::GetWindowLongPtrW(handle, GWL_STYLE);
            let ex = win32::GetWindowLongPtrW(handle, GWL_EXSTYLE);
            let (new_style, new_ex) = ((style & !FRAME_STYLES) | WS_POPUP, ex | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE);
            // Only touch the window when something needs changing, so this cannot loop on its own events.
            if new_style != style || new_ex != ex {
                win32::SetWindowLongPtrW(handle, GWL_STYLE, new_style);
                win32::SetWindowLongPtrW(handle, GWL_EXSTYLE, new_ex);
                win32::SetWindowPos(handle, 0, 0, 0, 0, 0, SWP_REFRAME);
            }
        }

        // Safety: plain Win32 calls on a window we own. The system takes ownership of the region.
        unsafe {
            let region = win32::CreateEllipticRgn(cx - diameter / 2, cy - diameter / 2, cx + diameter / 2 + 1, cy + diameter / 2 + 1);
            win32::SetWindowRgn(hwnd.0 as isize, region, 1);
        }
    }
    #[cfg(not(windows))]
    let _ = window;
}

/// Shows or hides the overlay icon to match the settings, restoring its saved position
/// when that position is still on a connected monitor.
fn apply_overlay(app: &AppHandle, settings: &Settings) {
    let Some(window) = app.get_webview_window("overlay") else { return };
    if !settings.overlay_enabled {
        let _ = window.hide();
        return;
    }
    // Use the saved spot while its centre is still on a connected screen; otherwise (first run,
    // or a monitor was unplugged) start at the game window's top-left corner.
    let saved = match (settings.overlay_x, settings.overlay_y) {
        (Some(x), Some(y)) => {
            let size = window.outer_size().map(|s| (s.width as i32, s.height as i32)).unwrap_or((0, 0));
            point_on_screen(&window, x + size.0 / 2, y + size.1 / 2).then_some((x, y))
        }
        _ => None,
    };
    match saved {
        Some((x, y)) => {
            let _ = window.set_position(PhysicalPosition::new(x, y));
        }
        None => {
            snap_overlay(&window);
        }
    }
    let _ = window.show();
    shape_overlay(&window);
}

fn point_on_screen(window: &WebviewWindow, x: i32, y: i32) -> bool {
    window
        .available_monitors()
        .map(|monitors| {
            monitors.iter().any(|m| {
                let pos = m.position();
                let size = m.size();
                x >= pos.x && y >= pos.y && x < pos.x + size.width as i32 && y < pos.y + size.height as i32
            })
        })
        .unwrap_or(false)
}

/// Restores the main window's size and place. The first time, it opens beside the orb inside
/// the game window and no taller than the game.
fn restore_main(app: &AppHandle, settings: &Settings) {
    let Some(window) = app.get_webview_window("main") else { return };
    let scale = window.scale_factor().unwrap_or(1.0);
    if let (Some(w), Some(h)) = (settings.main_w, settings.main_h) {
        let _ = window.set_size(LogicalSize::new(w, h));
    }
    let size = window.outer_size().map(|s| (s.width as i32, s.height as i32)).unwrap_or((0, 0));
    if let (Some(x), Some(y)) = (settings.main_x, settings.main_y) {
        if point_on_screen(&window, x + size.0 / 2, y + 20) {
            let _ = window.set_position(PhysicalPosition::new(x, y));
            return;
        }
    }
    if let Some((left, top, _, game_height)) = game_window() {
        let margin = (14.0 * scale).round() as i32;
        let room = game_height - 2 * margin;
        if settings.main_h.is_none() && size.1 > room && room > 300 {
            // Keep the width as designed; only the height is brought down to fit the game.
            let width = window.inner_size().map(|s| s.width).unwrap_or(size.0.max(1) as u32);
            let _ = window.set_size(PhysicalSize::new(width, room as u32));
        }
        let beside_orb = left + margin + ((OVERLAY_DIAMETER + 14.0) * scale).round() as i32;
        let _ = window.set_position(PhysicalPosition::new(beside_orb, top + margin));
    }
}

// ---------------------------------------------------------------------------
// Server connection
// ---------------------------------------------------------------------------

enum Next {
    Continue,
    /// Reconnect straight away, for example after dropping a token the server no longer knows.
    RetryNow,
    /// The server refused us for a reason retrying will not fix soon.
    Refused,
}

/// True while this player still has to answer a ready check.
fn needs_answer(state: &Value) -> bool {
    state.pointer("/group/you/accepted") == Some(&Value::Bool(false))
}

fn on_server_message(app: &AppHandle, shared: &SharedState, url: &str, text: &str) -> Next {
    let Ok(msg) = serde_json::from_str::<Value>(text) else { return Next::Continue };
    let mut next = Next::Continue;

    match msg.get("type").and_then(Value::as_str) {
        Some("welcome") => {
            let mut s = shared.lock().unwrap();
            if let Some(token) = msg.get("token").and_then(Value::as_str) {
                s.settings.tokens.insert(url.to_string(), token.to_string());
                save_settings(app, &s.settings);
            }
            let mut welcome = msg.clone();
            if let Some(fields) = welcome.as_object_mut() {
                fields.remove("token");
            }
            s.welcome = Some(welcome);
            s.connection = "online";
            s.detail = None;
        }
        Some("state") => {
            let alert = {
                let mut s = shared.lock().unwrap();
                let was_waiting = s.state.as_ref().map(needs_answer).unwrap_or(false);
                let is_waiting = needs_answer(&msg);
                s.state = Some(msg.clone());
                is_waiting && !was_waiting
            };
            // A new ready check: bring the panel in front of the game.
            if alert {
                show_main(app, true);
            }
        }
        Some("stats") => {
            shared.lock().unwrap().stats = Some(msg.clone());
        }
        Some("notice") => {
            let _ = app.emit("server-message", msg.clone());
        }
        Some("error") => {
            let code = msg.get("code").and_then(Value::as_str).unwrap_or("");
            let message = msg.get("message").and_then(Value::as_str).unwrap_or("").to_string();
            match code {
                "bad_token" => {
                    // The server has no record of this install (for example its database was reset).
                    let mut s = shared.lock().unwrap();
                    s.settings.tokens.remove(url);
                    save_settings(app, &s.settings);
                    next = Next::RetryNow;
                }
                "banned" | "bad_version" => {
                    shared.lock().unwrap().detail = Some(message);
                    next = Next::Refused;
                }
                _ => {
                    let _ = app.emit("server-message", msg.clone());
                }
            }
        }
        // Keep-alive replies and anything unrecognised change nothing, so the windows are not redrawn.
        _ => return next,
    }

    broadcast(app, shared);
    next
}

fn set_connection(app: &AppHandle, shared: &SharedState, status: &'static str, detail: Option<String>) {
    {
        let mut s = shared.lock().unwrap();
        s.connection = status;
        if status != "online" {
            s.state = None;
            s.stats = None;
        }
        if detail.is_some() || status == "online" {
            s.detail = detail;
        }
    }
    broadcast(app, shared);
}

async fn run_connection(app: AppHandle, shared: SharedState, mut rx: mpsc::UnboundedReceiver<Outbound>) {
    let mut backoff = 1u64;
    loop {
        let url = server_address();
        let token = shared.lock().unwrap().settings.tokens.get(&url).cloned();
        set_connection(&app, &shared, "connecting", None);

        let mut outcome = Next::Continue;
        let mut detail: Option<String> = None;

        match connect_async(url.as_str()).await {
            Ok((socket, _)) => {
                let (mut write, mut read) = socket.split();
                let mut hello = json!({ "type": "hello", "v": PROTOCOL_VERSION, "app": APP_TAG });
                if let Some(token) = token {
                    hello["token"] = Value::String(token);
                }
                if write.send(Message::text(hello.to_string())).await.is_ok() {
                    backoff = 1;
                    let mut keepalive = tokio::time::interval(Duration::from_secs(25));
                    keepalive.tick().await;
                    loop {
                        tokio::select! {
                            incoming = read.next() => match incoming {
                                Some(Ok(Message::Text(text))) => {
                                    match on_server_message(&app, &shared, &url, text.as_str()) {
                                        Next::Continue => {}
                                        other => { outcome = other; break; }
                                    }
                                }
                                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                                Some(Ok(_)) => {}
                            },
                            outgoing = rx.recv() => match outgoing {
                                Some(Outbound::Message(text)) => {
                                    if write.send(Message::text(text)).await.is_err() { break; }
                                }
                                None => return,
                            },
                            _ = keepalive.tick() => {
                                if write.send(Message::text(r#"{"type":"ping"}"#)).await.is_err() { break; }
                            }
                        }
                    }
                    let _ = write.close().await;
                }
            }
            Err(err) => detail = Some(format!("Could not reach the server: {err}")),
        }

        set_connection(&app, &shared, "offline", detail);

        let wait = match outcome {
            Next::RetryNow => 0,
            Next::Refused => 120,
            Next::Continue => backoff,
        };
        if matches!(outcome, Next::Continue) {
            backoff = (backoff * 2).min(30);
        }

        // Wait before trying again.
        let sleep = tokio::time::sleep(Duration::from_secs(wait));
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                outgoing = rx.recv() => match outgoing {
                    Some(Outbound::Message(_)) => {} // nothing to send it to while offline
                    None => return,
                },
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Reading the addon's saved data
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
struct AddonCharacter {
    name: String,
    faction: String,
    class: String,
    level: u32,
    spec: Option<String>,
    ilvl: Option<f64>,
    /// Unix seconds when the addon took the snapshot.
    snapshot_at: Option<u64>,
}

/// The addon writes one line per character into its saved variables:
///   ["Name"] = "v1|Name|horde|Warrior|42|Fury|31.5|1794559247",
/// Fields: version, name, faction, class, level, spec, item level, snapshot time.
fn parse_addon_export(text: &str) -> Vec<AddonCharacter> {
    let mut found = Vec::new();
    for line in text.lines() {
        let Some(start) = line.find("\"v1|") else { continue };
        let rest = &line[start + 1..];
        let Some(end) = rest.rfind('"') else { continue };
        let fields: Vec<&str> = rest[..end].split('|').collect();
        if fields.len() < 8 {
            continue;
        }
        let faction = fields[2].to_lowercase();
        let Ok(level) = fields[4].parse::<u32>() else { continue };
        if fields[1].is_empty() || (faction != "alliance" && faction != "horde") {
            continue;
        }
        found.push(AddonCharacter {
            name: fields[1].to_string(),
            faction,
            class: fields[3].to_string(),
            level,
            spec: Some(fields[5].to_string()).filter(|s| !s.is_empty()),
            ilvl: fields[6].parse::<f64>().ok().filter(|v| *v > 0.0),
            snapshot_at: fields[7].parse::<u64>().ok(),
        });
    }
    found
}

fn read_addon_characters(wow_dir: &str) -> Vec<AddonCharacter> {
    let mut all: Vec<AddonCharacter> = Vec::new();
    let accounts = Path::new(wow_dir).join("WTF").join("Account");
    let Ok(entries) = fs::read_dir(accounts) else { return all };
    for entry in entries.flatten() {
        let file = entry.path().join("SavedVariables").join("AntiSocial.lua");
        if let Ok(text) = fs::read_to_string(file) {
            for character in parse_addon_export(&text) {
                // The same character can appear under two accounts' folders; keep the newest snapshot.
                match all.iter_mut().find(|c| c.name == character.name) {
                    Some(existing) if existing.snapshot_at < character.snapshot_at => *existing = character,
                    Some(_) => {}
                    None => all.push(character),
                }
            }
        }
    }
    all.sort_by(|a, b| b.snapshot_at.cmp(&a.snapshot_at));
    all
}

// ---------------------------------------------------------------------------
// Installing the addon
// ---------------------------------------------------------------------------

/// Where the addon is downloaded from: the zip on the latest release, whatever it is named,
/// then the main branch as a fallback. The repository is fixed here on purpose, so the app can
/// never be pointed at some other download.
const ADDON_RELEASE_API: &str = "https://api.github.com/repos/kl3mta3/Anti-Social-Addon/releases/latest";
const ADDON_RELEASE_FILES: &str = "https://github.com/kl3mta3/Anti-Social-Addon/releases/download/";
const ADDON_BRANCH_ZIP: &str = "https://github.com/kl3mta3/Anti-Social-Addon/archive/refs/heads/main.zip";
const ADDON_FOLDER: &str = "AntiSocial";
const MAX_DOWNLOAD_BYTES: u64 = 10 * 1024 * 1024;
const MAX_UNPACKED_BYTES: u64 = 30 * 1024 * 1024;
const MAX_FILES: usize = 400;
/// The only kinds of file an addon needs. Anything else in the archive is skipped.
const ADDON_FILE_TYPES: [&str; 11] = ["lua", "toc", "xml", "tga", "blp", "png", "jpg", "ttf", "ogg", "mp3", "txt"];

fn addons_dir(wow_dir: &str) -> PathBuf {
    Path::new(wow_dir).join("Interface").join("AddOns")
}

fn toc_version(toc: &Path) -> Option<String> {
    let text = fs::read_to_string(toc).ok()?;
    text.lines().find_map(|line| line.strip_prefix("## Version:").map(|v| v.trim().to_string()))
}

fn installed_addon_version(wow_dir: &str) -> Option<String> {
    toc_version(&addons_dir(wow_dir).join(ADDON_FOLDER).join("AntiSocial.toc"))
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let tls = native_tls::TlsConnector::new().map_err(|e| e.to_string())?;
    let agent = ureq::AgentBuilder::new()
        .tls_connector(Arc::new(tls))
        .timeout(Duration::from_secs(45))
        .redirects(5)
        .user_agent(APP_TAG)
        .build();
    let response = agent.get(url).call().map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_DOWNLOAD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_DOWNLOAD_BYTES {
        return Err("the download is larger than an addon should be".into());
    }
    Ok(bytes)
}

/// The download address of the first zip attached to a release, taken from GitHub's description
/// of that release. Only a file on GitHub's own download address for the addon's repository counts.
fn release_zip(release: &Value) -> Option<String> {
    release.get("assets")?.as_array()?.iter().find_map(|asset| {
        let name = asset.get("name")?.as_str()?;
        let url = asset.get("browser_download_url")?.as_str()?;
        (name.to_ascii_lowercase().ends_with(".zip") && url.starts_with(ADDON_RELEASE_FILES)).then(|| url.to_string())
    })
}

/// Downloads the zip attached to the addon's latest release.
fn download_release_zip() -> Result<Vec<u8>, String> {
    let release: Value = serde_json::from_slice(&download(ADDON_RELEASE_API)?).map_err(|e| e.to_string())?;
    let url = release_zip(&release).ok_or_else(|| "the latest release has no zip file".to_string())?;
    download(&url)
}

/// Downloads the addon and puts it in the AddOns folder: from the latest release if that works,
/// otherwise from the main branch. Returns the version installed.
fn fetch_and_unpack_addon(addons: &Path) -> Result<String, String> {
    if let Ok(version) = download_release_zip().and_then(|bytes| unpack_addon(&bytes, addons)) {
        return Ok(version);
    }
    let bytes = download(ADDON_BRANCH_ZIP).map_err(|e| format!("Could not download the addon: {e}"))?;
    unpack_addon(&bytes, addons)
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Unpacks the addon from a zip into `<addons>/AntiSocial`, replacing any copy already there.
/// Returns the version written in the new .toc file.
fn unpack_addon(zip_bytes: &[u8], addons: &Path) -> Result<String, String> {
    use std::io::Read;
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(zip_bytes)).map_err(|_| "The download is not a zip file.".to_string())?;

    // The addon is wherever AntiSocial.toc sits: the archive's root, a folder, or a folder
    // inside the wrapper folder GitHub adds to branch downloads.
    let mut root: Option<PathBuf> = None;
    for i in 0..archive.len() {
        let entry = archive.by_index(i).map_err(|e| e.to_string())?;
        if let Some(path) = entry.enclosed_name() {
            if path.file_name().and_then(|n| n.to_str()) == Some("AntiSocial.toc") {
                root = Some(path.parent().map(Path::to_path_buf).unwrap_or_default());
                break;
            }
        }
    }
    let root = root.ok_or_else(|| "The download does not contain the addon.".to_string())?;

    let staging = addons.join("AntiSocial.installing");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|e| format!("Could not write to the AddOns folder: {e}"))?;

    let result = (|| -> Result<String, String> {
        let (mut files, mut total) = (0usize, 0u64);
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
            // enclosed_name refuses absolute paths and anything that climbs out with "..".
            let Some(path) = entry.enclosed_name() else { continue };
            let Ok(relative) = path.strip_prefix(&root) else { continue };
            if relative.as_os_str().is_empty() || entry.is_dir() {
                continue;
            }
            let kind = relative.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
            if !ADDON_FILE_TYPES.contains(&kind.as_str()) {
                continue;
            }
            files += 1;
            if files > MAX_FILES {
                return Err("The download has more files than an addon should.".into());
            }
            let target = staging.join(relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut out = fs::File::create(&target).map_err(|e| e.to_string())?;
            let copied = std::io::copy(&mut (&mut entry).take(MAX_UNPACKED_BYTES - total + 1), &mut out).map_err(|e| e.to_string())?;
            total += copied;
            if total > MAX_UNPACKED_BYTES {
                return Err("The download unpacks to more than an addon should.".into());
            }
        }
        let version =
            toc_version(&staging.join("AntiSocial.toc")).ok_or_else(|| "The downloaded addon has no version.".to_string())?;
        if !staging.join("AntiSocial.lua").is_file() {
            return Err("The downloaded addon is incomplete.".into());
        }

        // Swap the new copy in, keeping the old one until the new one is in place.
        let target = addons.join(ADDON_FOLDER);
        let previous = addons.join("AntiSocial.previous");
        let _ = fs::remove_dir_all(&previous);
        if !target.exists() {
            fs::rename(&staging, &target).map_err(|e| format!("Could not put the addon in place: {e}"))?;
        } else if fs::rename(&target, &previous).is_ok() {
            if let Err(e) = fs::rename(&staging, &target) {
                let _ = fs::rename(&previous, &target);
                return Err(format!("Could not put the new addon in place: {e}"));
            }
            let _ = fs::remove_dir_all(&previous);
        } else {
            // The old folder could not be moved (something has it open), so write over it instead.
            copy_dir(&staging, &target).map_err(|e| format!("Could not update the addon files: {e}"))?;
            let _ = fs::remove_dir_all(&staging);
        }
        Ok(version)
    })();

    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AddonStatus {
    /// Version of the installed addon, if it is installed.
    installed: Option<String>,
    /// Whether the game's AddOns folder was found under the configured game folder.
    folder_found: bool,
}

#[tauri::command]
fn addon_status(shared: State<'_, SharedState>) -> AddonStatus {
    let dir = shared.lock().unwrap().settings.wow_dir.clone();
    AddonStatus { installed: installed_addon_version(&dir), folder_found: !dir.is_empty() && addons_dir(&dir).is_dir() }
}

#[tauri::command]
async fn install_addon(shared: State<'_, SharedState>) -> Result<Value, String> {
    let dir = shared.lock().unwrap().settings.wow_dir.clone();
    let addons = addons_dir(&dir);
    if dir.is_empty() || !addons.is_dir() {
        return Err("Set your World of Warcraft Forever folder in Settings first.".into());
    }
    let previous = installed_addon_version(&dir);
    let version = tauri::async_runtime::spawn_blocking(move || fetch_and_unpack_addon(&addons))
        .await
        .map_err(|e| e.to_string())??;
    Ok(json!({ "version": version, "previous": previous }))
}

// ---------------------------------------------------------------------------
// Checking for a newer version of this app
// ---------------------------------------------------------------------------

const RELEASES_API: &str = "https://api.github.com/repos/kl3mta3/Anti-Social-App/releases/latest";
const RELEASES_PAGE: &str = "https://github.com/kl3mta3/Anti-Social-App/releases/latest";

fn version_parts(version: &str) -> Vec<u32> {
    version
        .trim()
        .trim_start_matches(['v', 'V'])
        .split('.')
        .map(|part| part.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0))
        .collect()
}

/// True when `candidate` is a later version than `current`, comparing number by number.
fn is_newer(candidate: &str, current: &str) -> bool {
    let (a, b) = (version_parts(candidate), version_parts(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

/// Asks GitHub for the newest published version. Ok(None) means this copy is up to date,
/// or nothing has been published yet.
fn fetch_update() -> Result<Option<String>, String> {
    let bytes = match download(RELEASES_API) {
        Ok(bytes) => bytes,
        Err(e) if e.contains("404") => return Ok(None),
        Err(e) => return Err(e),
    };
    let release: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let tag = release.get("tag_name").and_then(Value::as_str).unwrap_or("");
    Ok(is_newer(tag, env!("CARGO_PKG_VERSION")).then(|| tag.trim_start_matches(['v', 'V']).to_string()))
}

async fn refresh_update(app: &AppHandle, shared: &SharedState) -> Result<Option<String>, String> {
    let found = tauri::async_runtime::spawn_blocking(fetch_update).await.map_err(|e| e.to_string())??;
    shared.lock().unwrap().update = found.clone();
    broadcast(app, shared);
    Ok(found)
}

#[tauri::command]
async fn check_for_update(app: AppHandle, shared: State<'_, SharedState>) -> Result<Option<String>, String> {
    refresh_update(&app, shared.inner()).await
}

/// Opens the download page in the player's browser. The address is fixed; nothing a window
/// sends can change where this goes.
#[tauri::command]
fn open_download_page() {
    let _ = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", RELEASES_PAGE]).spawn();
}

// ---------------------------------------------------------------------------
// Commands the windows can call
// ---------------------------------------------------------------------------

#[tauri::command]
fn get_snapshot(shared: State<'_, SharedState>) -> Value {
    snapshot(&shared.lock().unwrap())
}

#[tauri::command]
fn send_message(message: Value, shared: State<'_, SharedState>) -> Result<(), String> {
    let s = shared.lock().unwrap();
    if s.connection != "online" {
        return Err("Not connected to the server.".into());
    }
    match &s.outbound {
        Some(tx) => tx.send(Outbound::Message(message.to_string())).map_err(|_| "Connection closed.".to_string()),
        None => Err("Not connected to the server.".into()),
    }
}

#[tauri::command]
fn update_settings(patch: Value, app: AppHandle, shared: State<'_, SharedState>) -> Result<(), String> {
    let settings = {
        let mut s = shared.lock().unwrap();
        if let Some(dir) = patch.get("wowDir").and_then(Value::as_str) {
            s.settings.wow_dir = dir.trim().to_string();
        }
        if let Some(enabled) = patch.get("overlayEnabled").and_then(Value::as_bool) {
            s.settings.overlay_enabled = enabled;
        }
        if let Some(locked) = patch.get("overlayLocked").and_then(Value::as_bool) {
            s.settings.overlay_locked = locked;
        }
        s.settings.clone()
    };
    save_settings(&app, &settings);
    apply_overlay(&app, &settings);
    sync_tray(&app, &settings);
    broadcast(&app, shared.inner());
    Ok(())
}

#[tauri::command]
fn read_characters(shared: State<'_, SharedState>) -> Vec<AddonCharacter> {
    let dir = shared.lock().unwrap().settings.wow_dir.clone();
    if dir.is_empty() {
        return Vec::new();
    }
    read_addon_characters(&dir)
}

#[tauri::command]
async fn pick_folder(app: AppHandle, window: WebviewWindow) -> Option<String> {
    app.dialog()
        .file()
        .set_parent(&window)
        .set_title("Choose your World of Warcraft Forever folder")
        .blocking_pick_folder()
        .and_then(|path| path.into_path().ok())
        .map(|path| path.to_string_lossy().into_owned())
}

#[tauri::command]
fn copy_text(text: String, app: AppHandle) -> Result<(), String> {
    app.clipboard().write_text(text).map_err(|e| e.to_string())
}

#[tauri::command]
fn toggle_main(app: AppHandle) {
    toggle_main_window(&app);
}

#[tauri::command]
fn hide_main(app: AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
}

/// Called by the overlay when the mouse starts moving with the button held.
#[tauri::command]
fn overlay_drag(window: WebviewWindow, shared: State<'_, SharedState>) {
    if !shared.lock().unwrap().settings.overlay_locked {
        let _ = window.start_dragging();
    }
}

/// Moves the overlay icon to the top-left corner of the game window.
#[tauri::command]
fn snap_overlay_to_game(app: AppHandle) {
    if let Some(window) = app.get_webview_window("overlay") {
        snap_overlay(&window);
    }
}

#[tauri::command]
fn quit(app: AppHandle) {
    app.exit(0);
}

// ---------------------------------------------------------------------------
// Start-up
// ---------------------------------------------------------------------------

/// The two tray menu entries that carry a check mark.
struct TrayChecks {
    overlay: CheckMenuItem<Wry>,
    lock: CheckMenuItem<Wry>,
}

/// Makes the tray menu's check marks match the settings, however the settings were changed.
fn sync_tray(app: &AppHandle, settings: &Settings) {
    if let Some(checks) = app.try_state::<TrayChecks>() {
        let _ = checks.overlay.set_checked(settings.overlay_enabled);
        let _ = checks.lock.set_checked(settings.overlay_locked);
    }
}

fn build_tray(app: &AppHandle, settings: &Settings) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Anti Social", true, None::<&str>)?;
    let overlay = CheckMenuItem::with_id(app, "overlay", "Show overlay icon", true, settings.overlay_enabled, None::<&str>)?;
    let lock = CheckMenuItem::with_id(app, "lock", "Lock overlay icon in place", true, settings.overlay_locked, None::<&str>)?;
    let snap = MenuItem::with_id(app, "snap", "Move overlay icon to the game window", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let line1 = PredefinedMenuItem::separator(app)?;
    let line2 = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&open, &line1, &overlay, &lock, &snap, &line2, &quit])?;
    app.manage(TrayChecks { overlay, lock });

    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("Anti Social")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
            let shared = app.state::<SharedState>();
            match event.id.as_ref() {
                "open" => show_main(app, false),
                "overlay" | "lock" => {
                    let settings = {
                        let mut s = shared.lock().unwrap();
                        if event.id.as_ref() == "overlay" {
                            s.settings.overlay_enabled = !s.settings.overlay_enabled;
                        } else {
                            s.settings.overlay_locked = !s.settings.overlay_locked;
                        }
                        s.settings.clone()
                    };
                    save_settings(app, &settings);
                    apply_overlay(app, &settings);
                    sync_tray(app, &settings);
                    broadcast(app, shared.inner());
                }
                "snap" => {
                    // Works even when the icon is locked: this is a deliberate reset, not a drag.
                    if let Some(window) = app.get_webview_window("overlay") {
                        snap_overlay(&window);
                    }
                }
                "quit" => app.exit(0),
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                toggle_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_main(app, false)))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .setup(|app| {
            let handle = app.handle().clone();
            let settings = load_settings(&handle);
            let first_run = settings.tokens.is_empty();
            let (tx, rx) = mpsc::unbounded_channel();
            let shared: SharedState = Arc::new(Mutex::new(Shared {
                settings: settings.clone(),
                connection: "connecting",
                detail: None,
                welcome: None,
                state: None,
                stats: None,
                update: None,
                outbound: Some(tx),
                settings_dirty: false,
            }));
            app.manage(shared.clone());

            build_tray(&handle, &settings)?;
            apply_overlay(&handle, &settings);
            restore_main(&handle, &settings);
            if first_run || !settings.overlay_enabled {
                show_main(&handle, false);
            }

            tauri::async_runtime::spawn(run_connection(handle.clone(), shared.clone(), rx));

            // Look for a newer version shortly after starting, then every six hours.
            {
                let (app, shared) = (handle.clone(), shared.clone());
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(8)).await;
                    loop {
                        let _ = refresh_update(&app, &shared).await;
                        tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
                    }
                });
            }

            // Dragging the overlay produces a stream of move events; write the position at most every two seconds.
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let pending = {
                        let mut s = shared.lock().unwrap();
                        if s.settings_dirty {
                            s.settings_dirty = false;
                            Some(s.settings.clone())
                        } else {
                            None
                        }
                    };
                    if let Some(settings) = pending {
                        save_settings(&handle, &settings);
                    }
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| match (window.label(), event) {
            // Closing the panel only hides it; the app keeps running in the tray.
            ("main", WindowEvent::CloseRequested { api, .. }) => {
                api.prevent_close();
                let _ = window.hide();
            }
            ("overlay", WindowEvent::Moved(position)) => {
                let shared = window.state::<SharedState>();
                let mut s = shared.lock().unwrap();
                s.settings.overlay_x = Some(position.x);
                s.settings.overlay_y = Some(position.y);
                s.settings_dirty = true;
            }
            // Remember where the panel was left and how big it was made.
            ("main", WindowEvent::Moved(position)) => {
                // A minimised window reports a far off-screen position; ignore that.
                if position.x > -30000 && position.y > -30000 {
                    let shared = window.state::<SharedState>();
                    let mut s = shared.lock().unwrap();
                    s.settings.main_x = Some(position.x);
                    s.settings.main_y = Some(position.y);
                    s.settings_dirty = true;
                }
            }
            ("main", WindowEvent::Resized(size)) => {
                let scale = window.scale_factor().unwrap_or(1.0);
                let (w, h) = (size.width as f64 / scale, size.height as f64 / scale);
                if w >= 300.0 && h >= 300.0 {
                    let shared = window.state::<SharedState>();
                    let mut s = shared.lock().unwrap();
                    s.settings.main_w = Some(w.round());
                    s.settings.main_h = Some(h.round());
                    s.settings_dirty = true;
                }
            }
            // Moving to a monitor with different scaling changes the window's pixel size.
            ("overlay", WindowEvent::Resized(_)) | ("overlay", WindowEvent::ScaleFactorChanged { .. }) => {
                if let Some(overlay) = window.app_handle().get_webview_window("overlay") {
                    shape_overlay(&overlay);
                }
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            send_message,
            update_settings,
            read_characters,
            pick_folder,
            copy_text,
            toggle_main,
            hide_main,
            overlay_drag,
            snap_overlay_to_game,
            addon_status,
            install_addon,
            check_for_update,
            open_download_page,
            quit
        ])
        .run(tauri::generate_context!())
        .expect("failed to start Anti Social");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_addon_export_lines() {
        let text = r#"
AntiSocialExport = {
	["Brakka Ironhide"] = "v1|Brakka Ironhide|Horde|Warrior|42|Fury|31.5|1794559247",
	["Sella"] = "v1|Sella|alliance|Priest|60||0|1794559000",
	["Broken"] = "v1|Broken|horde|Mage",
	["Neutral"] = "v1|Neutral|neutral|Monk|10|||1",
}
"#;
        let found = parse_addon_export(text);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, "Brakka Ironhide");
        assert_eq!(found[0].faction, "horde");
        assert_eq!(found[0].level, 42);
        assert_eq!(found[0].spec.as_deref(), Some("Fury"));
        assert_eq!(found[0].ilvl, Some(31.5));
        assert_eq!(found[0].snapshot_at, Some(1794559247));
        assert_eq!(found[1].spec, None);
        assert_eq!(found[1].ilvl, None);
    }

    /// Builds a zip in memory from (path, contents) pairs.
    fn zip_of(entries: &[(&str, &str)]) -> Vec<u8> {
        use std::io::Write;
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        for (name, body) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn temp_addons(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("antisocial-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn unpacks_the_addon_and_replaces_the_old_copy() {
        let addons = temp_addons("unpack");
        fs::create_dir_all(addons.join("AntiSocial")).unwrap();
        fs::write(addons.join("AntiSocial").join("stale.lua"), "old").unwrap();
        fs::write(addons.join("AntiSocial").join("AntiSocial.toc"), "## Version: 0.0.1\n").unwrap();

        // Laid out the way GitHub's branch download is: a wrapper folder around the repository.
        let bytes = zip_of(&[
            ("Repo-main/README.md", "not part of the addon"),
            ("Repo-main/AntiSocial/AntiSocial.toc", "## Interface: 16001\n## Version: 9.9.9\n"),
            ("Repo-main/AntiSocial/AntiSocial.lua", "print('hi')"),
            ("Repo-main/AntiSocial/Libs/LibStub/LibStub.lua", "-- lib"),
            ("Repo-main/AntiSocial/Media/icon.tga", "tga"),
            ("Repo-main/AntiSocial/sneaky.exe", "MZ"),
            ("Repo-main/AntiSocial/../../escaped.lua", "should never be written"),
            ("../outside.lua", "should never be written"),
        ]);
        assert_eq!(unpack_addon(&bytes, &addons).unwrap(), "9.9.9");

        let addon = addons.join("AntiSocial");
        assert!(addon.join("AntiSocial.lua").is_file());
        assert!(addon.join("Libs").join("LibStub").join("LibStub.lua").is_file());
        assert!(addon.join("Media").join("icon.tga").is_file());
        assert!(!addon.join("stale.lua").exists(), "files from the old copy are gone");
        assert!(!addon.join("sneaky.exe").exists(), "file types an addon does not need are skipped");
        assert!(!addon.join("README.md").exists(), "files outside the addon folder are skipped");
        assert!(!addons.join("escaped.lua").exists() && !addons.parent().unwrap().join("outside.lua").exists());
        assert!(!addons.join("AntiSocial.installing").exists() && !addons.join("AntiSocial.previous").exists());
        assert_eq!(toc_version(&addon.join("AntiSocial.toc")).as_deref(), Some("9.9.9"));
        let _ = fs::remove_dir_all(&addons);
    }

    #[test]
    fn refuses_downloads_that_are_not_the_addon() {
        let addons = temp_addons("refuse");
        fs::create_dir_all(addons.join("AntiSocial")).unwrap();
        fs::write(addons.join("AntiSocial").join("AntiSocial.toc"), "## Version: 0.0.1\n").unwrap();

        assert!(unpack_addon(b"this is not a zip", &addons).is_err());
        assert!(unpack_addon(&zip_of(&[("Other/Other.toc", "## Version: 1\n")]), &addons).is_err());
        assert!(unpack_addon(&zip_of(&[("AntiSocial/AntiSocial.toc", "## Version: 2\n")]), &addons).is_err(), "no .lua file");

        // Every failure leaves the installed copy exactly as it was.
        assert_eq!(toc_version(&addons.join("AntiSocial").join("AntiSocial.toc")).as_deref(), Some("0.0.1"));
        assert!(!addons.join("AntiSocial.installing").exists());
        let _ = fs::remove_dir_all(&addons);
    }

    /// Hits GitHub for real. Run with: cargo test -- --ignored
    #[test]
    #[ignore]
    fn downloads_and_unpacks_the_published_addon() {
        let addons = temp_addons("download");
        let sources: [(&str, fn() -> Result<Vec<u8>, String>); 2] =
            [("latest release", download_release_zip), ("main branch", || download(ADDON_BRANCH_ZIP))];
        for (name, fetch) in sources {
            let bytes = fetch().unwrap();
            let version = unpack_addon(&bytes, &addons).unwrap();
            println!("{name} -> version {version}, {} bytes", bytes.len());
            assert!(addons.join("AntiSocial").join("AntiSocial.lua").is_file());
            assert!(addons.join("AntiSocial").join("Libs").join("LibDBIcon-1.0").join("LibDBIcon-1.0.lua").is_file());
        }
        let version = fetch_and_unpack_addon(&addons).unwrap();
        println!("install as the app does it -> version {version}");
        let _ = fs::remove_dir_all(&addons);
    }

    #[test]
    fn finds_the_release_zip_by_type_not_by_name() {
        let files = "https://github.com/kl3mta3/Anti-Social-Addon/releases/download/1.0";
        let release = |assets: Value| json!({ "tag_name": "1.0", "assets": assets });

        // Any name will do, in any letter case, and other kinds of file are passed over.
        let found = release_zip(&release(json!([
            { "name": "notes.txt", "browser_download_url": format!("{files}/notes.txt") },
            { "name": "Whatever-Name.ZIP", "browser_download_url": format!("{files}/Whatever-Name.ZIP") },
            { "name": "second.zip", "browser_download_url": format!("{files}/second.zip") },
        ])));
        assert_eq!(found, Some(format!("{files}/Whatever-Name.ZIP")));

        // No zip, no files at all, or a zip that is not on the addon repository's download address.
        assert_eq!(release_zip(&release(json!([{ "name": "a.exe", "browser_download_url": format!("{files}/a.exe") }]))), None);
        assert_eq!(release_zip(&release(json!([]))), None);
        assert_eq!(release_zip(&json!({ "message": "Not Found" })), None);
        assert_eq!(release_zip(&release(json!([{ "name": "a.zip", "browser_download_url": "https://example.org/a.zip" }]))), None);
    }

    #[test]
    fn moves_old_installs_to_the_new_server_address() {
        let mut old = Settings::default();
        old.tokens.insert(OLD_SERVER.to_string(), "secret-token".to_string());
        let moved = migrate_settings(old);
        assert_eq!(moved.tokens.get(SERVER).map(String::as_str), Some("secret-token"));
        assert!(!moved.tokens.contains_key(OLD_SERVER));
    }

    #[test]
    fn a_saved_server_address_is_ignored_and_dropped() {
        let saved = r#"{"serverUrl":"wss://example.org/ws","wowDir":"D:/Game","tokens":{}}"#;
        let settings: Settings = serde_json::from_str(saved).unwrap();
        assert_eq!(settings.wow_dir, "D:/Game");
        assert!(!serde_json::to_string(&settings).unwrap().contains("serverUrl"));
    }

    #[test]
    fn compares_versions_number_by_number() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("v0.1.1", "0.1.0"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.1", "0.1.0"));
        assert!(!is_newer("", "0.1.0"));
        assert!(!is_newer("v0.1", "0.1.0"));
    }

    #[test]
    fn knows_when_a_ready_check_needs_answering() {
        assert!(needs_answer(&json!({ "group": { "you": { "accepted": false } } })));
        assert!(!needs_answer(&json!({ "group": { "you": { "accepted": true } } })));
        assert!(!needs_answer(&json!({ "group": null })));
    }
}
