use crate::config::{is_allowed, Config, DesktopLauncher, TerminalKind};
use crate::desktop::{self, DesktopEntry};
use crate::runner::binary_in_path;
use crate::steam::{self, SteamEntry};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct AppEntry {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub exec: Option<String>,
    pub path: PathBuf,
    pub hidden: bool,
    pub terminal: bool,
    pub working_dir: Option<String>,
}

impl AppEntry {
    pub fn from_desktop(e: DesktopEntry) -> Self {
        let hidden = e.no_display || e.hidden;
        Self {
            id: e.id,
            name: e.name,
            provider: "desktop".to_string(),
            exec: e.exec,
            path: e.path,
            hidden,
            terminal: e.terminal,
            working_dir: e.working_dir,
        }
    }
    pub fn from_steam(e: SteamEntry) -> Self {
        Self {
            id: e.appid,
            name: e.name,
            provider: "steam".to_string(),
            exec: None,
            path: e.path,
            hidden: false,
            terminal: false,
            working_dir: None,
        }
    }
}

pub fn list_apps(
    cfg: &Config,
    provider: &str,
    query: Option<&str>,
    limit: usize,
    include_hidden: bool,
) -> Vec<AppEntry> {
    let mut apps = Vec::new();
    if provider == "auto" || provider == "desktop" {
        let desktop = desktop::scan_desktop_dirs(&cfg.desktop_dirs, include_hidden);
        for e in desktop {
            apps.push(AppEntry::from_desktop(e));
        }
    }
    if provider == "auto" || provider == "steam" {
        let steam = steam::scan_steam_roots(&cfg.steam_roots);
        for e in steam {
            apps.push(AppEntry::from_steam(e));
        }
    }
    if provider == "tmux" {
        apps.extend(scan_tmux_sessions());
    }
    if let Some(q) = query {
        let ql = q.to_lowercase();
        apps.retain(|a| a.id.to_lowercase().contains(&ql) || a.name.to_lowercase().contains(&ql));
    }
    apps.sort_by(|a, b| a.id.cmp(&b.id));
    apps.truncate(limit);
    apps
}

pub fn find_app(cfg: &Config, app_id: &str, provider: &str) -> Option<AppEntry> {
    let all = list_apps(cfg, provider, None, 500, true);
    // exact id match first
    if let Some(found) = all.iter().find(|a| a.id == app_id) {
        return Some(found.clone());
    }
    // case-insensitive id
    if let Some(found) = all
        .iter()
        .find(|a| a.id.to_lowercase() == app_id.to_lowercase())
    {
        return Some(found.clone());
    }
    // name match (unique)
    let name_matches: Vec<_> = all
        .iter()
        .filter(|a| a.name.to_lowercase() == app_id.to_lowercase())
        .collect();
    if name_matches.len() == 1 {
        return Some(name_matches[0].clone());
    }
    None
}

#[derive(Debug, Clone)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub available: bool,
    pub description: String,
    pub roots: Vec<String>,
}

pub fn scan_tmux_sessions() -> Vec<AppEntry> {
    if !binary_in_path("tmux") {
        return Vec::new();
    }
    let Ok(output) = std::process::Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|name| AppEntry {
            id: name.to_string(),
            name: name.to_string(),
            provider: "tmux".to_string(),
            exec: None,
            path: PathBuf::from(format!("tmux:{name}")),
            hidden: false,
            terminal: false,
            working_dir: None,
        })
        .collect()
}

pub fn list_providers(cfg: &Config) -> Vec<ProviderInfo> {
    let steam_available = cfg.steam_roots.iter().any(|p| p.join("steamapps").is_dir());
    vec![
        ProviderInfo {
            id: "desktop".to_string(),
            name: "desktop".to_string(),
            available: true,
            description: "XDG desktop files (.desktop) via gtk-launch/gio/xdg-open or direct Exec".to_string(),
            roots: cfg.desktop_dirs.iter().map(|p| p.display().to_string()).collect(),
        },
        ProviderInfo {
            id: "steam".to_string(),
            name: "steam".to_string(),
            available: steam_available || binary_in_path(&cfg.steam_binary),
            description: "Steam games via steam://rungameid/<appid> (reads libraryfolders.vdf + appmanifest_*.acf)".to_string(),
            roots: cfg.steam_roots.iter().map(|p| p.display().to_string()).collect(),
        },
        ProviderInfo {
            id: "tmux".to_string(),
            name: "tmux".to_string(),
            available: binary_in_path("tmux"),
            description: "tmux sessions (tmux ls) — launch creates new-window in LAUNCHER_TMUX_SESSION".to_string(),
            roots: vec![cfg.tmux_session.clone()],
        },
        ProviderInfo {
            id: "kitty".to_string(),
            name: "kitty".to_string(),
            available: binary_in_path("kitty"),
            description: "kitty terminal — launch wraps command as kitty -- <cmd>".to_string(),
            roots: vec![],
        },
        ProviderInfo {
            id: "alacritty".to_string(),
            name: "alacritty".to_string(),
            available: binary_in_path("alacritty"),
            description: "alacritty terminal — launch wraps command as alacritty -e <cmd>".to_string(),
            roots: vec![],
        },
        ProviderInfo {
            id: "ghostty".to_string(),
            name: "ghostty".to_string(),
            available: binary_in_path("ghostty"),
            description: "ghostty terminal — launch wraps command as ghostty -e <cmd>".to_string(),
            roots: vec![],
        },
    ]
}

fn tmux_has_session(session: &str) -> bool {
    std::process::Command::new("tmux")
        .args(["has-session", "-t", session])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn wrap_with_terminal(
    cfg: &Config,
    terminal: TerminalKind,
    mut cmd: Vec<String>,
    window_name: &str,
    working_dir: Option<&str>,
) -> Result<(String, Vec<String>), String> {
    match terminal {
        TerminalKind::None => Ok((cmd.remove(0), cmd)),
        TerminalKind::Auto => {
            let eff = TerminalKind::detect_auto();
            if eff == TerminalKind::None {
                return Ok((cmd.remove(0), cmd));
            }
            wrap_with_terminal(cfg, eff, cmd, window_name, working_dir)
        }
        TerminalKind::Tmux => {
            if !binary_in_path("tmux") {
                return Err(
                    "ERR_LAUNCH_PROVIDER_MISSING: binary 'tmux' not found on PATH".to_string(),
                );
            }
            let session = &cfg.tmux_session;
            let has = tmux_has_session(session);
            let mut argv = Vec::new();
            if has {
                argv.extend([
                    "new-window".to_string(),
                    "-d".to_string(),
                    "-t".to_string(),
                    session.clone(),
                    "-n".to_string(),
                    window_name.to_string(),
                ]);
            } else {
                argv.extend([
                    "new-session".to_string(),
                    "-d".to_string(),
                    "-s".to_string(),
                    session.clone(),
                    "-n".to_string(),
                    window_name.to_string(),
                ]);
            }
            if let Some(dir) = working_dir {
                argv.push("-c".to_string());
                argv.push(dir.to_string());
            }
            argv.push("--".to_string());
            argv.extend(cmd);
            Ok(("tmux".to_string(), argv))
        }
        TerminalKind::Kitty => {
            if !binary_in_path("kitty") {
                return Err(
                    "ERR_LAUNCH_PROVIDER_MISSING: binary 'kitty' not found on PATH".to_string(),
                );
            }
            let mut argv = Vec::new();
            if let Some(dir) = working_dir {
                argv.push("--directory".to_string());
                argv.push(dir.to_string());
            }
            argv.push("--".to_string());
            argv.extend(cmd);
            Ok(("kitty".to_string(), argv))
        }
        TerminalKind::Alacritty => {
            if !binary_in_path("alacritty") {
                return Err(
                    "ERR_LAUNCH_PROVIDER_MISSING: binary 'alacritty' not found on PATH".to_string(),
                );
            }
            let mut argv = Vec::new();
            if let Some(dir) = working_dir {
                argv.push("--working-directory".to_string());
                argv.push(dir.to_string());
            }
            argv.push("-e".to_string());
            argv.extend(cmd);
            Ok(("alacritty".to_string(), argv))
        }
        TerminalKind::Ghostty => {
            if !binary_in_path("ghostty") {
                return Err(
                    "ERR_LAUNCH_PROVIDER_MISSING: binary 'ghostty' not found on PATH".to_string(),
                );
            }
            let mut argv = Vec::new();
            if let Some(dir) = working_dir {
                argv.push("--working-directory".to_string());
                argv.push(dir.to_string());
            }
            argv.push("-e".to_string());
            argv.extend(cmd);
            Ok(("ghostty".to_string(), argv))
        }
    }
}

/// argv for gtk-launch: it wants the desktop-file BASENAME — dotted ids
/// ("org.telegram.desktop") resolve only WITH the ".desktop" suffix on this
/// system (plain "firefox" works either way)
fn gtk_launch_argv(app_id: &str, extra_args: &[String]) -> Vec<String> {
    let mut a = vec![format!("{app_id}.desktop")];
    a.extend_from_slice(extra_args);
    a
}

pub fn resolve_launch_command(
    cfg: &Config,
    app: &AppEntry,
    extra_args: &[String],
) -> Result<(String, Vec<String>), String> {
    if !is_allowed(&app.id, &cfg.allowed_ids) {
        return Err(format!(
            "ERR_LAUNCH_BLOCKED: app_id '{}' not in LAUNCHER_ALLOWED_IDS",
            app.id
        ));
    }
    match app.provider.as_str() {
        "steam" => {
            let bin = cfg.steam_binary.clone();
            let mut argv = vec![format!("steam://rungameid/{}", app.id)];
            argv.extend_from_slice(extra_args);
            Ok((bin, argv))
        }
        "desktop" => {
            let (bin, argv) = {
                let method = cfg.desktop_launcher.clone();
                match method {
                    DesktopLauncher::GtkLaunch => {
                        if binary_in_path("gtk-launch") && !app.terminal {
                            (("gtk-launch".to_string()), gtk_launch_argv(&app.id, extra_args))
                        } else {
                            fallback_exec(app, extra_args)?
                        }
                    }
                    DesktopLauncher::Gio => {
                        if binary_in_path("gio") && !app.terminal {
                            let mut a = vec!["launch".to_string(), app.path.display().to_string()];
                            a.extend_from_slice(extra_args);
                            (("gio".to_string()), a)
                        } else {
                            fallback_exec(app, extra_args)?
                        }
                    }
                    DesktopLauncher::XdgOpen => {
                        if binary_in_path("xdg-open") && !app.terminal {
                            let mut a = vec![app.path.display().to_string()];
                            a.extend_from_slice(extra_args);
                            (("xdg-open".to_string()), a)
                        } else {
                            fallback_exec(app, extra_args)?
                        }
                    }
                    DesktopLauncher::Exec => fallback_exec(app, extra_args)?,
                    DesktopLauncher::Auto => {
                        if binary_in_path("gtk-launch") && !app.terminal {
                            (("gtk-launch".to_string()), gtk_launch_argv(&app.id, extra_args))
                        } else if binary_in_path("gio") && !app.terminal {
                            let mut a = vec!["launch".to_string(), app.path.display().to_string()];
                            a.extend_from_slice(extra_args);
                            (("gio".to_string()), a)
                        } else {
                            fallback_exec(app, extra_args)?
                        }
                    }
                }
            };
            if app.terminal {
                let term = cfg.terminal.effective();
                if term != TerminalKind::None {
                    let mut cmd = vec![bin];
                    cmd.extend(argv);
                    return wrap_with_terminal(cfg, term, cmd, &app.id, app.working_dir.as_deref());
                }
            }
            Ok((bin, argv))
        }
        "tmux" | "kitty" | "alacritty" | "ghostty" => {
            let kind = match app.provider.as_str() {
                "tmux" => TerminalKind::Tmux,
                "kitty" => TerminalKind::Kitty,
                "alacritty" => TerminalKind::Alacritty,
                "ghostty" => TerminalKind::Ghostty,
                _ => unreachable!(),
            };
            let mut cmd = if let Some(exec) = &app.exec {
                crate::desktop::exec_argv(exec)
            } else {
                vec![app.id.clone()]
            };
            if cmd.is_empty() {
                cmd.push(app.id.clone());
            }
            cmd.extend_from_slice(extra_args);
            wrap_with_terminal(cfg, kind, cmd, &app.id, app.working_dir.as_deref())
        }
        _ => Err(format!(
            "ERR_LAUNCH_NOT_SUPPORTED: unknown provider '{}'",
            app.provider
        )),
    }
}

fn fallback_exec(app: &AppEntry, extra_args: &[String]) -> Result<(String, Vec<String>), String> {
    let exec = app.exec.as_ref().ok_or_else(|| {
        format!(
            "ERR_LAUNCH_NOT_SUPPORTED: desktop entry '{}' has no Exec",
            app.id
        )
    })?;
    let mut argv = crate::desktop::exec_argv(exec);
    if argv.is_empty() {
        return Err(format!(
            "ERR_LAUNCH_SPAWN_FAILED: Exec for '{}' produced empty argv",
            app.id
        ));
    }
    let bin = argv.remove(0);
    argv.extend_from_slice(extra_args);
    Ok((bin, argv))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use tempfile::TempDir;

    fn cfg_with_dirs(
        desktop: Option<TempDir>,
        steam: Option<TempDir>,
    ) -> (Config, Option<TempDir>, Option<TempDir>) {
        let desktop_path = desktop.as_ref().map(|d| d.path().to_path_buf());
        let steam_path = steam.as_ref().map(|d| d.path().to_path_buf());
        let cfg = Config {
            desktop_dirs: desktop_path.into_iter().collect(),
            steam_roots: steam_path.into_iter().collect(),
            allowed_ids: None,
            timeout_ms: 5000,
            steam_binary: "steam".to_string(),
            desktop_launcher: DesktopLauncher::Exec,
            cache_ttl_ms: 60_000,
            terminal: crate::config::TerminalKind::None,
            tmux_session: "vynkor".to_string(),
        };
        (cfg, desktop, steam)
    }

    #[test]
    fn list_desktop_entries() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(
            td.path().join("firefox.desktop"),
            "[Desktop Entry]\nName=Firefox\nExec=firefox %u\nType=Application\n",
        )
        .unwrap();
        std::fs::write(
            td.path().join("hidden.desktop"),
            "[Desktop Entry]\nName=Hidden\nExec=hidden\nType=Application\nNoDisplay=true\n",
        )
        .unwrap();
        let (cfg, _a, _b) = cfg_with_dirs(Some(td), None);
        let apps = list_apps(&cfg, "desktop", None, 100, false);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].id, "firefox");
        let all = list_apps(&cfg, "desktop", None, 100, true);
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn list_with_query_filter() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(
            td.path().join("a.desktop"),
            "[Desktop Entry]\nName=Alpha\nExec=a\nType=Application\n",
        )
        .unwrap();
        std::fs::write(
            td.path().join("b.desktop"),
            "[Desktop Entry]\nName=Beta\nExec=b\nType=Application\n",
        )
        .unwrap();
        let (cfg, _a, _b) = cfg_with_dirs(Some(td), None);
        let filtered = list_apps(&cfg, "desktop", Some("alpha"), 100, false);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "a");
    }

    #[test]
    fn gtk_launch_argv_appends_desktop_suffix() {
        let argv = gtk_launch_argv("org.telegram.desktop", &["--new-window".to_string()]);
        assert_eq!(argv[0], "org.telegram.desktop.desktop");
        assert_eq!(argv[1], "--new-window");
    }

    #[test]
    fn waydroid_launchers_are_never_offered() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(
            td.path().join("waydroid.org.telegram.messenger.web.desktop"),
            "[Desktop Entry]\nName=Telegram\nExec=waydroid app launch org.telegram.messenger.web\nType=Application\nCategories=X-WayDroid-App;\n",
        )
        .unwrap();
        std::fs::write(
            td.path().join("org.telegram.desktop.desktop"),
            "[Desktop Entry]\nName=Telegram\nExec=telegram\nType=Application\n",
        )
        .unwrap();
        let (cfg, _a, _b) = cfg_with_dirs(Some(td), None);
        // even include_hidden=true must not surface waydroid entries
        let apps = list_apps(&cfg, "desktop", None, 100, true);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].id, "org.telegram.desktop");
        // unique-name resolution now works again
        assert!(find_app(&cfg, "Telegram", "desktop").is_some());
    }

    #[test]
    fn find_app_exact_and_name() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(
            td.path().join("code.desktop"),
            "[Desktop Entry]\nName=Visual Studio Code\nExec=code\nType=Application\n",
        )
        .unwrap();
        let (cfg, _a, _b) = cfg_with_dirs(Some(td), None);
        assert!(find_app(&cfg, "code", "desktop").is_some());
        assert!(find_app(&cfg, "Visual Studio Code", "desktop").is_some());
        assert!(find_app(&cfg, "missing", "desktop").is_none());
    }

    #[test]
    fn resolve_steam_command() {
        let cfg = Config {
            desktop_dirs: vec![],
            steam_roots: vec![],
            allowed_ids: None,
            timeout_ms: 5000,
            steam_binary: "steam".to_string(),
            desktop_launcher: DesktopLauncher::Auto,
            cache_ttl_ms: 60_000,
            terminal: crate::config::TerminalKind::None,
            tmux_session: "vynkor".to_string(),
        };
        let app = AppEntry {
            id: "123".to_string(),
            name: "Game".to_string(),
            provider: "steam".to_string(),
            exec: None,
            path: PathBuf::from("/tmp/appmanifest_123.acf"),
            hidden: false,
            terminal: false,
            working_dir: None,
        };
        let (bin, argv) = resolve_launch_command(&cfg, &app, &[]).unwrap();
        assert_eq!(bin, "steam");
        assert_eq!(argv, vec!["steam://rungameid/123"]);
    }

    #[test]
    fn resolve_blocked_by_allowlist() {
        let cfg = Config {
            desktop_dirs: vec![],
            steam_roots: vec![],
            allowed_ids: Some(vec!["allowed".to_string()]),
            timeout_ms: 5000,
            steam_binary: "steam".to_string(),
            desktop_launcher: DesktopLauncher::Exec,
            cache_ttl_ms: 60_000,
            terminal: crate::config::TerminalKind::None,
            tmux_session: "vynkor".to_string(),
        };
        let app = AppEntry {
            id: "blocked".to_string(),
            name: "Blocked".to_string(),
            provider: "steam".to_string(),
            exec: None,
            path: PathBuf::from("/tmp/x"),
            hidden: false,
            terminal: false,
            working_dir: None,
        };
        let err = resolve_launch_command(&cfg, &app, &[]).unwrap_err();
        assert!(err.contains("ERR_LAUNCH_BLOCKED"), "{err}");
    }

    #[test]
    fn resolve_desktop_exec() {
        let cfg = Config {
            desktop_dirs: vec![],
            steam_roots: vec![],
            allowed_ids: None,
            timeout_ms: 5000,
            steam_binary: "steam".to_string(),
            desktop_launcher: DesktopLauncher::Exec,
            cache_ttl_ms: 60_000,
            terminal: crate::config::TerminalKind::None,
            tmux_session: "vynkor".to_string(),
        };
        let app = AppEntry {
            id: "myapp".to_string(),
            name: "MyApp".to_string(),
            provider: "desktop".to_string(),
            exec: Some("myapp --flag %U".to_string()),
            path: PathBuf::from("/tmp/myapp.desktop"),
            hidden: false,
            terminal: false,
            working_dir: None,
        };
        let (bin, argv) = resolve_launch_command(&cfg, &app, &["extra".to_string()]).unwrap();
        assert_eq!(bin, "myapp");
        assert_eq!(argv, vec!["--flag", "extra"]);
    }

    #[test]
    fn resolve_terminal_providers() {
        // Hermetic: resolve_launch_command refuses a terminal whose binary is
        // not on PATH, and CI runners ship none of kitty/alacritty/ghostty.
        // Prepend a dir of stub executables instead of relying on the host.
        use std::os::unix::fs::PermissionsExt;
        let stubs = tempfile::tempdir().unwrap();
        for name in ["tmux", "kitty", "alacritty", "ghostty"] {
            let p = stubs.path().join(name);
            std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let mut dirs = vec![stubs.path().to_path_buf()];
        dirs.extend(std::env::split_paths(&old_path));
        std::env::set_var("PATH", std::env::join_paths(dirs).unwrap());

        let base = Config {
            desktop_dirs: vec![],
            steam_roots: vec![],
            allowed_ids: None,
            timeout_ms: 5000,
            steam_binary: "steam".to_string(),
            desktop_launcher: DesktopLauncher::Exec,
            cache_ttl_ms: 60_000,
            terminal: crate::config::TerminalKind::None,
            tmux_session: "vynkor".to_string(),
        };
        for (prov, expected_bin) in [
            ("tmux", "tmux"),
            ("kitty", "kitty"),
            ("alacritty", "alacritty"),
            ("ghostty", "ghostty"),
        ] {
            let app = AppEntry {
                id: "htop".to_string(),
                name: "htop".to_string(),
                provider: prov.to_string(),
                exec: Some("htop".to_string()),
                path: PathBuf::from(""),
                hidden: false,
                terminal: false,
                working_dir: None,
            };
            let (bin, argv) = resolve_launch_command(&base, &app, &[]).unwrap();
            assert_eq!(bin, expected_bin, "provider {prov}");
            assert!(
                argv.iter().any(|a| a == "htop"),
                "argv for {prov}: {:?}",
                argv
            );
        }
        std::env::set_var("PATH", old_path);
    }

    #[test]
    fn resolve_desktop_terminal_wrapped_with_tmux() {
        let cfg = Config {
            desktop_dirs: vec![],
            steam_roots: vec![],
            allowed_ids: None,
            timeout_ms: 5000,
            steam_binary: "steam".to_string(),
            desktop_launcher: DesktopLauncher::Exec,
            cache_ttl_ms: 60_000,
            terminal: crate::config::TerminalKind::Tmux,
            tmux_session: "testsession".to_string(),
        };
        let app = AppEntry {
            id: "nvim".to_string(),
            name: "nvim".to_string(),
            provider: "desktop".to_string(),
            exec: Some("nvim %F".to_string()),
            path: PathBuf::from("/tmp/nvim.desktop"),
            hidden: false,
            terminal: true,
            working_dir: Some("/tmp".to_string()),
        };
        let (bin, argv) = resolve_launch_command(&cfg, &app, &[]).unwrap();
        assert_eq!(bin, "tmux");
        assert!(argv.contains(&"testsession".to_string()));
        assert!(argv.contains(&"nvim".to_string()));
    }
}
