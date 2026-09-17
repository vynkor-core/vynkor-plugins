//! Session/DE detection (`XDG_SESSION_TYPE`/`XDG_CURRENT_DESKTOP`/
//! `WAYLAND_DISPLAY`/`DISPLAY`) and a pure `$PATH` scan used only for
//! `capture_status` reporting. Actual screenshot/record calls do NOT
//! consult this module — they try their argv chain directly and fall
//! through on `ErrorKind::NotFound` (see `spawner.rs`), so a stale or
//! wrong detection here can never break a real capture, only misreport
//! `capture_status`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionType {
    Wlroots,
    Gnome,
    Kde,
    X11,
    Unknown,
}

impl SessionType {
    pub const fn as_str(self) -> &'static str {
        match self {
            SessionType::Wlroots => "wlroots",
            SessionType::Gnome => "gnome",
            SessionType::Kde => "kde",
            SessionType::X11 => "x11",
            SessionType::Unknown => "unknown",
        }
    }
}

pub fn detect_session(env: &dyn Fn(&str) -> Option<String>) -> SessionType {
    let is_wayland = env("WAYLAND_DISPLAY").is_some()
        || env("XDG_SESSION_TYPE").as_deref() == Some("wayland");
    let desktop = env("XDG_CURRENT_DESKTOP").unwrap_or_default().to_lowercase();

    if is_wayland {
        if desktop.contains("gnome") {
            return SessionType::Gnome;
        }
        if desktop.contains("kde") {
            return SessionType::Kde;
        }
        // Hyprland/sway/river/labwc all identify as themselves in
        // XDG_CURRENT_DESKTOP, not as a shared "wlroots" string — treat
        // any non-GNOME/KDE Wayland session as the wlroots-protocol family,
        // which is what grim/slurp/wf-recorder actually depend on.
        return SessionType::Wlroots;
    }
    if env("DISPLAY").is_some() {
        return SessionType::X11;
    }
    SessionType::Unknown
}

/// Pure `$PATH` scan — does not spawn anything.
pub fn binary_on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join(name).is_file())
        })
        .unwrap_or(false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    pub session_type: &'static str,
    pub screenshot_backend: Option<&'static str>,
    pub record_backend: Option<&'static str>,
    pub ocr_available: bool,
    pub portal_available: bool,
}

pub fn build_status_report() -> StatusReport {
    let session = detect_session(&|k| std::env::var(k).ok());
    let screenshot_backend = match session {
        SessionType::Wlroots if binary_on_path("grim") => Some("grim"),
        SessionType::Gnome if binary_on_path("gnome-screenshot") => Some("gnome-screenshot"),
        SessionType::Kde if binary_on_path("spectacle") => Some("spectacle"),
        SessionType::X11 if binary_on_path("maim") => Some("maim"),
        SessionType::X11 if binary_on_path("scrot") => Some("scrot"),
        SessionType::X11 if binary_on_path("import") => Some("import"),
        _ => None,
    };
    let record_backend = match session {
        SessionType::Wlroots if binary_on_path("wf-recorder") => Some("wf-recorder"),
        SessionType::X11 if binary_on_path("ffmpeg") => Some("ffmpeg"),
        _ => None,
    };
    StatusReport {
        session_type: session.as_str(),
        screenshot_backend,
        record_backend,
        ocr_available: binary_on_path("tesseract"),
        portal_available: binary_on_path("xdg-desktop-portal") || screenshot_backend.is_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_map(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn detects_wlroots_on_hyprland() {
        let env = env_map(&[("WAYLAND_DISPLAY", "wayland-1"), ("XDG_CURRENT_DESKTOP", "Hyprland")]);
        assert_eq!(detect_session(&env), SessionType::Wlroots);
    }

    #[test]
    fn detects_gnome_wayland() {
        let env = env_map(&[("WAYLAND_DISPLAY", "wayland-0"), ("XDG_CURRENT_DESKTOP", "GNOME")]);
        assert_eq!(detect_session(&env), SessionType::Gnome);
    }

    #[test]
    fn detects_kde_wayland() {
        let env = env_map(&[("WAYLAND_DISPLAY", "wayland-0"), ("XDG_CURRENT_DESKTOP", "KDE")]);
        assert_eq!(detect_session(&env), SessionType::Kde);
    }

    #[test]
    fn detects_x11_when_no_wayland_display() {
        let env = env_map(&[("DISPLAY", ":0")]);
        assert_eq!(detect_session(&env), SessionType::X11);
    }

    #[test]
    fn unknown_when_nothing_set() {
        let env = env_map(&[]);
        assert_eq!(detect_session(&env), SessionType::Unknown);
    }

    #[test]
    fn binary_on_path_finds_real_binary_and_rejects_bogus_one() {
        // `sh` exists on every POSIX CI runner; a random UUID-ish name won't.
        assert!(binary_on_path("sh"));
        assert!(!binary_on_path("definitely-not-a-real-binary-xyz123"));
    }
}
