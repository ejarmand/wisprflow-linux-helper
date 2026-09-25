//! Hyprland active-app provider: identity from Hyprland's own IPC socket.
//!
//! Hyprland has no portable "focused app" protocol either, but it answers
//! `j/activewindow` and `j/clients` on `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock`
//! (what `hyprctl` talks to). That is exact where the AT-SPI fallback comes up
//! empty for apps without an accessibility bridge (Electron apps, terminals).
//!
//! The window class is turned into the app's name from its desktop entry
//! (`StartupWMClass`, else the desktop file id), so `com.mitchellh.ghostty`
//! reports "Ghostty" and `vesktop` reports "Vesktop".
//!
//! `bundle_id` is left empty on purpose. The app derives its per-app key from
//! the bundle id whenever one is present, prettifying reverse-DNS ids
//! (`com.t3tools.T3Code` -> "T3tools T3code"), and matches that key against the
//! app names users attach to a style. With no bundle id it keys on the app
//! name, so the name reported here is the one to type in the app picker.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

use super::{ActiveApp, Result, RunningApp};

pub struct HyprlandTracker {
    socket: PathBuf,
    /// Window class -> app name, filled on first sight of each class.
    names: Mutex<HashMap<String, String>>,
}

impl HyprlandTracker {
    pub fn start() -> Result<Self> {
        let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
            .map_err(|_| "HYPRLAND_INSTANCE_SIGNATURE is not set".to_string())?;
        let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        let tracker = Self {
            socket: PathBuf::from(runtime)
                .join("hypr")
                .join(sig)
                .join(".socket.sock"),
            names: Mutex::new(HashMap::new()),
        };
        tracker.request("j/version")?;
        Ok(tracker)
    }

    fn request(&self, command: &str) -> Result<Value> {
        let mut stream = UnixStream::connect(&self.socket)
            .map_err(|e| format!("connect {}: {e}", self.socket.display()))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .map_err(|e| e.to_string())?;
        stream
            .write_all(command.as_bytes())
            .map_err(|e| format!("write {command}: {e}"))?;
        let mut reply = Vec::new();
        stream
            .read_to_end(&mut reply)
            .map_err(|e| format!("read {command}: {e}"))?;
        serde_json::from_slice(&reply).map_err(|e| format!("parse {command}: {e}"))
    }

    fn app_name(&self, class: &str) -> String {
        let mut names = self.names.lock().unwrap_or_else(|e| e.into_inner());
        names
            .entry(class.to_string())
            .or_insert_with(|| desktop_name(class).unwrap_or_else(|| class.to_string()))
            .clone()
    }

    pub fn current(&self) -> Option<ActiveApp> {
        let window = match self.request("j/activewindow") {
            Ok(w) => w,
            Err(e) => {
                log::warn!("hyprland activewindow failed: {e}");
                return None;
            }
        };
        let class = window.get("class").and_then(Value::as_str).unwrap_or("");
        if class.is_empty() {
            return None;
        }
        Some(ActiveApp {
            app_name: self.app_name(class),
            bundle_id: String::new(),
            window_title: window
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            url: String::new(),
        })
    }

    pub fn get_running_apps(&self) -> Vec<RunningApp> {
        let clients = match self.request("j/clients") {
            Ok(Value::Array(c)) => c,
            Ok(_) => return Vec::new(),
            Err(e) => {
                log::warn!("hyprland clients failed: {e}");
                return Vec::new();
            }
        };
        let mut apps: Vec<RunningApp> = Vec::new();
        for client in &clients {
            let class = client.get("class").and_then(Value::as_str).unwrap_or("");
            if class.is_empty() {
                continue;
            }
            let name = self.app_name(class);
            if !apps.iter().any(|a| a.name == name) {
                apps.push(RunningApp {
                    bundle_id: String::new(),
                    name,
                });
            }
        }
        apps
    }

    /// The app asks for the active app (`GetAppInfo`) as each dictation starts,
    /// which is what style matching reads, so focus events are not needed yet.
    pub fn set_focus_detection(&self, active: bool) {
        log::debug!("hyprland: focus events not implemented (requested {active})");
    }
}

/// The name of the app whose desktop entry claims this window class.
fn desktop_name(class: &str) -> Option<String> {
    let mut entries = Vec::new();
    for dir in application_dirs() {
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let (Some(stem), Ok(text)) = (
                path.file_stem().and_then(|s| s.to_str()).map(String::from),
                std::fs::read_to_string(&path),
            ) else {
                continue;
            };
            entries.push(DesktopEntry::parse(stem, &text));
        }
    }
    match_class(&entries, class)
}

/// `$XDG_DATA_HOME/applications` first, then each `$XDG_DATA_DIRS` entry.
fn application_dirs() -> Vec<PathBuf> {
    let home = std::env::var("XDG_DATA_HOME").ok().or_else(|| {
        std::env::var("HOME")
            .ok()
            .map(|h| format!("{h}/.local/share"))
    });
    let dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    home.into_iter()
        .chain(dirs.split(':').map(String::from))
        .map(|d| PathBuf::from(d).join("applications"))
        .collect()
}

#[derive(Debug)]
struct DesktopEntry {
    /// Desktop file id without `.desktop` (e.g. `google-chrome`).
    id: String,
    name: Option<String>,
    wm_class: Option<String>,
}

impl DesktopEntry {
    /// Reads the untranslated `Name=` and `StartupWMClass=` of the
    /// `[Desktop Entry]` group only (actions have their own `Name=`).
    fn parse(id: String, text: &str) -> Self {
        let mut entry = Self {
            id,
            name: None,
            wm_class: None,
        };
        let mut in_main = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                in_main = line == "[Desktop Entry]";
            } else if !in_main {
                continue;
            } else if let Some(v) = line.strip_prefix("Name=") {
                entry.name.get_or_insert_with(|| v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("StartupWMClass=") {
                entry.wm_class = Some(v.trim().to_string());
            }
        }
        entry
    }
}

/// `StartupWMClass` matches first, then the desktop file id; both ignore case.
fn match_class(entries: &[DesktopEntry], class: &str) -> Option<String> {
    let wm = entries.iter().find(|e| {
        e.wm_class
            .as_deref()
            .is_some_and(|w| w.eq_ignore_ascii_case(class))
    });
    let by_id = || entries.iter().find(|e| e.id.eq_ignore_ascii_case(class));
    wm.or_else(by_id)
        .and_then(|e| e.name.clone())
        .filter(|n| !n.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, text: &str) -> DesktopEntry {
        DesktopEntry::parse(id.to_string(), text)
    }

    #[test]
    fn parse_reads_main_group_untranslated_name() {
        let e = entry(
            "google-chrome",
            "[Desktop Entry]\nName[de]=Google Chrome DE\nName=Google Chrome\n\
             StartupWMClass=google-chrome\n\n[Desktop Action new-window]\nName=New Window\n",
        );
        assert_eq!(e.name.as_deref(), Some("Google Chrome"));
        assert_eq!(e.wm_class.as_deref(), Some("google-chrome"));
    }

    #[test]
    fn startup_wm_class_wins_over_file_id() {
        let entries = [
            entry("vesktop", "[Desktop Entry]\nName=By Id\n"),
            entry(
                "other",
                "[Desktop Entry]\nName=Vesktop\nStartupWMClass=Vesktop\n",
            ),
        ];
        assert_eq!(match_class(&entries, "vesktop").as_deref(), Some("Vesktop"));
    }

    #[test]
    fn falls_back_to_file_id_ignoring_case() {
        let entries = [entry(
            "com.t3tools.T3Code",
            "[Desktop Entry]\nName=T3 Code (Nightly)\n",
        )];
        assert_eq!(
            match_class(&entries, "com.t3tools.t3code").as_deref(),
            Some("T3 Code (Nightly)")
        );
    }

    #[test]
    fn unknown_class_has_no_name() {
        let entries = [entry("foo", "[Desktop Entry]\nName=Foo\n")];
        assert_eq!(match_class(&entries, "bar"), None);
    }
}
