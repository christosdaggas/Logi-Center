//! Foreground application identification, never a source of hardware writes.
//!
//! GNOME Wayland deliberately does not fall back to XWayland. An optional,
//! user-installed GNOME Shell companion exports object
//! `/org/gnome/Shell/Extensions/LogiCenter` on the existing `org.gnome.Shell`
//! session bus connection, interface `org.gnome.Shell.Extensions.LogiCenter`,
//! and a read-only `GetForeground() -> (s)` method. The string contains JSON
//! `{"app_id":"org.gnome.Terminal.desktop","pid":123,"wm_class":"..."}`.
//! An empty app_id means no focused application. A companion can
//! obtain it using `Shell.WindowTracker.get_default().get_window_app(
//! global.display.focus_window)?.get_id()`. It should return an empty app_id
//! for the lock screen and export/unexport with extension enable/disable.
//! Introspection XML:
//! `<node><interface name="org.gnome.Shell.Extensions.LogiCenter"><method
//! name="GetForeground"><arg type="s" direction="out"/></method></interface></node>`.
//! Create/install/enable that extension explicitly through GNOME's normal
//! extension management; Logi Center neither installs it nor enables Shell Eval.
//!
//! wlroots uses the optional wlrctl foreign-toplevel helper and its
//! `toplevel list state:active` filter. Compositors without that protocol (or
//! helpers without filtering) report detection unavailable; manual selection
//! remains usable. No inference is made from arbitrary/XWayland windows.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Detection {
    pub app_id: Option<String>,
    pub backend: String,
    pub detail: String,
}

const LIMIT: usize = 8192;
const TIMEOUT: Duration = Duration::from_millis(800);

fn command(program: &str, args: &[&str]) -> Result<String, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("{program} unavailable: {error}"))?;
    let mut stdout = child.stdout.take().ok_or("No command output")?;
    // Nonblocking reads keep the deadline effective even if a helper's child
    // inherits its stdout pipe. Never join an unbounded pipe-reader thread.
    let fd = stdout.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        let _ = child.kill();
        let _ = child.wait();
        return Err("Could not configure bounded helper output".into());
    }
    let mut bytes = Vec::new();
    let started = Instant::now();
    let mut status: Option<std::process::ExitStatus> = None;
    loop {
        let mut block = [0u8; 1024];
        loop {
            match stdout.read(&mut block) {
                Ok(0) => break,
                Ok(size) => {
                    bytes.extend_from_slice(&block[..size]);
                    if bytes.len() > LIMIT {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(format!("{program} output exceeds limit"));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("{program} output read failed: {error}"));
                }
            }
        }
        if let Some(status) = status {
            if !status.success() {
                return Err(format!("{program} reported {status}"));
            }
            break;
        }
        match child.try_wait() {
            Ok(Some(exit)) => {
                status = Some(exit);
                continue;
            }
            Ok(None) if started.elapsed() < TIMEOUT => {
                std::thread::sleep(Duration::from_millis(10))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{program} timed out or could not be waited for"));
            }
        }
    }
    String::from_utf8(bytes).map_err(|_| format!("{program} returned non-UTF-8 output"))
}

fn app_id(text: &str) -> Option<String> {
    let lowered = text.trim().to_lowercase();
    let id = lowered.trim_end_matches(".desktop");
    (!id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)).then(|| id.to_owned())
}
fn unavailable(backend: &str, detail: impl Into<String>) -> Detection {
    Detection {
        app_id: None,
        backend: backend.into(),
        detail: format!(
            "{}; automatic app matching unavailable, use manual profile selection",
            detail.into()
        ),
    }
}
fn found(backend: &str, id: Option<String>) -> Detection {
    Detection {
        detail: if id.is_some() {
            "Foreground application detected".into()
        } else {
            "No focused application; selected profile remains active".into()
        },
        app_id: id,
        backend: backend.into(),
    }
}

fn x11() -> Result<Option<String>, String> {
    let active = command("xprop", &["-root", "_NET_ACTIVE_WINDOW"])
        .ok()
        .and_then(|output| output.split_whitespace().last().map(str::to_owned))
        .filter(|id| id.starts_with("0x") && id[2..].chars().all(|c| c.is_ascii_hexdigit()))
        .or_else(|| {
            command("xdotool", &["getactivewindow"])
                .ok()
                .map(|v| v.trim().into())
                .filter(|id: &String| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()))
        })
        .ok_or("Neither xprop nor xdotool returned an active X11 window")?;
    if active == "0x0" || active == "0" {
        return Ok(None);
    }
    let class = match command("xprop", &["-id", &active, "WM_CLASS"]) {
        Ok(class) => class,
        Err(error) => {
            return command("xdotool", &["getwindowclassname", &active])
                .map(|class| app_id(&class))
                .map_err(|fallback| format!("{error}; {fallback}"));
        }
    };
    parse_x11_class(&class)
}
fn parse_x11_class(class: &str) -> Result<Option<String>, String> {
    // WM_CLASS has instance then class; match the application class, not title.
    let quoted: Vec<_> = class.split('"').collect();
    quoted
        .get(3)
        .map(|class| app_id(class))
        .ok_or_else(|| "Active X11 window has no WM_CLASS".into())
}
fn kde() -> Result<Option<String>, String> {
    let window = command("kdotool", &["getactivewindow"])?;
    let window = window.trim();
    if window.is_empty() || window == "0" {
        return Ok(None);
    }
    if window.len() > 128
        || !window
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-{}".contains(c))
    {
        return Err("Invalid KWin window identifier".into());
    }
    command("kdotool", &["getwindowclassname", window]).map(|class| app_id(&class))
}
fn gnome() -> Result<Option<String>, String> {
    let output = command(
        "gdbus",
        &[
            "call",
            "--session",
            "--dest",
            "org.gnome.Shell",
            "--object-path",
            "/org/gnome/Shell/Extensions/LogiCenter",
            "--method",
            "org.gnome.Shell.Extensions.LogiCenter.GetForeground",
        ],
    )?;
    parse_gnome(&output)
}
fn parse_gnome(output: &str) -> Result<Option<String>, String> {
    if output.len() > LIMIT {
        return Err("Companion response exceeds limit".into());
    }
    let tuple_type = glib::VariantTy::new("(s)").map_err(|e| e.to_string())?;
    let value = glib::Variant::parse(Some(tuple_type), output.trim())
        .map_err(|e| format!("Invalid companion tuple: {e}"))?;
    let (json,) = value
        .get::<(String,)>()
        .ok_or("GNOME companion must return one string")?;
    let record: serde_json::Value =
        serde_json::from_str(&json).map_err(|e| format!("Invalid companion JSON: {e}"))?;
    let id = record
        .get("app_id")
        .and_then(|v| v.as_str())
        .ok_or("Companion JSON has no app_id string")?;
    let normalized = app_id(id);
    if !id.trim().is_empty() && normalized.is_none() {
        return Err("Companion app_id is invalid".into());
    }
    Ok(normalized)
}
fn wlroots() -> Result<Option<String>, String> {
    let output = command("wlrctl", &["toplevel", "list", "state:active"])?;
    parse_wlrctl(&output)
}
fn parse_wlrctl(output: &str) -> Result<Option<String>, String> {
    let lines: Vec<_> = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return Ok(None);
    }
    if lines.len() != 1 {
        return Err("wlrctl did not provide a unique active toplevel".into());
    }
    let (id, _) = lines[0]
        .split_once(':')
        .ok_or("Unrecognized wlrctl toplevel format")?;
    Ok(app_id(id))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContextBackend {
    X11,
    Kde,
    Gnome,
    Wlroots,
    Manual,
}
fn choose_backend(
    session: &str,
    desktop: &str,
    wayland_display: bool,
    x_display: bool,
) -> ContextBackend {
    if session == "wayland" || wayland_display {
        if desktop.contains("kde") {
            ContextBackend::Kde
        } else if desktop.contains("gnome") {
            ContextBackend::Gnome
        } else {
            ContextBackend::Wlroots
        }
    } else if session == "x11" || x_display {
        ContextBackend::X11
    } else {
        ContextBackend::Manual
    }
}

pub fn detect() -> Detection {
    let session = std::env::var("XDG_SESSION_TYPE")
        .unwrap_or_default()
        .to_lowercase();
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_lowercase();
    let selected = choose_backend(
        &session,
        &desktop,
        std::env::var_os("WAYLAND_DISPLAY").is_some(),
        std::env::var_os("DISPLAY").is_some(),
    );
    let (backend, result) = match selected {
        ContextBackend::Kde => ("kde-kdotool", kde()),
        ContextBackend::Gnome => ("gnome-companion", gnome()),
        ContextBackend::Wlroots => ("wlroots-wlrctl", wlroots()),
        ContextBackend::X11 => ("x11-xprop", x11()),
        ContextBackend::Manual => {
            return unavailable("manual", "No supported graphical session detected");
        }
    };
    match result {
        Ok(id) => found(backend, id),
        Err(error) => unavailable(backend, error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identifiers_and_honest_fallback() {
        assert_eq!(
            app_id(" Org.Example.App.desktop\n"),
            Some("org.example.app".into())
        );
        assert_eq!(app_id("\n"), None);
        assert_eq!(app_id("bad\0id"), None);
        let status = unavailable("gnome-companion", "Companion absent");
        assert!(status.app_id.is_none());
        assert!(status.detail.contains("manual"));
    }
    #[test]
    fn companion_variant_json_is_data_not_code() {
        assert_eq!(
            parse_gnome(
                r#"('{"app_id":"org.gnome.Terminal.desktop","pid":123,"wm_class":"Terminal"}',)"#
            )
            .unwrap(),
            Some("org.gnome.terminal".into())
        );
        assert_eq!(parse_gnome(r#"('{"app_id":""}',)"#).unwrap(), None);
        assert!(parse_gnome("(true,)").is_err());
        assert!(parse_gnome("('not json',)").is_err());
    }
    #[test]
    fn wayland_never_uses_x11_as_native_foreground_detection() {
        assert_eq!(
            choose_backend("wayland", "gnome", true, true),
            ContextBackend::Gnome
        );
        assert_eq!(
            choose_backend("wayland", "kde", true, true),
            ContextBackend::Kde
        );
        assert_eq!(
            choose_backend("wayland", "sway", true, true),
            ContextBackend::Wlroots
        );
        assert_eq!(choose_backend("", "", true, true), ContextBackend::Wlroots);
        assert_eq!(
            choose_backend("x11", "gnome", false, true),
            ContextBackend::X11
        );
        assert_eq!(choose_backend("", "", false, false), ContextBackend::Manual);
    }
    #[test]
    fn helper_outputs_require_an_actual_unique_identity() {
        assert_eq!(
            parse_x11_class("WM_CLASS(STRING) = \"instance\", \"Firefox\"\n").unwrap(),
            Some("firefox".into())
        );
        assert!(parse_x11_class("WM_CLASS: not found").is_err());
        assert_eq!(
            parse_wlrctl("org.example.app: A window\n").unwrap(),
            Some("org.example.app".into())
        );
        assert_eq!(parse_wlrctl("").unwrap(), None);
        assert!(parse_wlrctl("one: First\ntwo: Second\n").is_err());
        assert!(parse_wlrctl("unsupported helper response").is_err());
    }
}
