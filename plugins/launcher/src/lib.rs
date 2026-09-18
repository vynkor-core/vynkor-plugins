pub mod config;
pub mod desktop;
pub mod error;
pub mod manifest;
pub mod providers;
pub mod request;
pub mod runner;
pub mod steam;

use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use vynkor_sdk::proto::{envelope, ActionResponse, ActionStatus, Envelope, PluginManifest};
use vynkor_sdk::{Plugin, VynkorError};

use crate::config::Config;
use crate::error::LauncherError;
use crate::providers::AppEntry;
use crate::runner::{Launcher, RealLauncher};

pub const PLUGIN_ID: &str = "launcher";
pub const PLUGIN_VERSION: &str = "0.1.5";

pub const ACTIONS: &[&str] = &["launch", "launch_list", "launch_providers"];

#[derive(Debug)]
struct CachedEntries {
    at: Instant,
    entries: Vec<AppEntry>,
}

#[derive(Debug, Default)]
struct ScanCache {
    desktop: Option<CachedEntries>,
    steam: Option<CachedEntries>,
}

pub struct LauncherPlugin<L: Launcher = RealLauncher> {
    pub config: Config,
    pub launcher: Arc<L>,
    cache: Arc<Mutex<ScanCache>>,
}

impl<L: Launcher> LauncherPlugin<L> {
    pub fn new(config: Config, launcher: Arc<L>) -> Self {
        Self {
            config,
            launcher,
            cache: Arc::new(Mutex::new(ScanCache::default())),
        }
    }

    fn cached_apps(&self, provider: &str, include_hidden: bool) -> Vec<AppEntry> {
        let ttl = Duration::from_millis(self.config.cache_ttl_ms);
        let now = Instant::now();
        let mut cache = self.cache.lock().unwrap();
        let mut out = Vec::new();
        if provider == "auto" || provider == "desktop" {
            let need_scan = if ttl.is_zero() {
                true
            } else {
                cache
                    .desktop
                    .as_ref()
                    .map(|c| now.duration_since(c.at) > ttl)
                    .unwrap_or(true)
            };
            if need_scan {
                let full = crate::desktop::scan_desktop_dirs(&self.config.desktop_dirs, true);
                let entries = full
                    .into_iter()
                    .map(AppEntry::from_desktop)
                    .collect::<Vec<_>>();
                cache.desktop = Some(CachedEntries {
                    at: now,
                    entries: entries.clone(),
                });
                let mut filtered = entries;
                if !include_hidden {
                    filtered.retain(|e| !e.hidden);
                }
                out.extend(filtered);
            } else {
                let entries = cache.desktop.as_ref().unwrap().entries.clone();
                let mut filtered = entries;
                if !include_hidden {
                    filtered.retain(|e| !e.hidden);
                }
                out.extend(filtered);
            }
        }
        if provider == "auto" || provider == "steam" {
            let need_scan = if ttl.is_zero() {
                true
            } else {
                cache
                    .steam
                    .as_ref()
                    .map(|c| now.duration_since(c.at) > ttl)
                    .unwrap_or(true)
            };
            if need_scan {
                let entries = crate::steam::scan_steam_roots(&self.config.steam_roots)
                    .into_iter()
                    .map(AppEntry::from_steam)
                    .collect::<Vec<_>>();
                cache.steam = Some(CachedEntries {
                    at: now,
                    entries: entries.clone(),
                });
                out.extend(entries);
            } else {
                out.extend(cache.steam.as_ref().unwrap().entries.clone());
            }
        }
        out
    }

    fn find_app_cached(&self, app_id: &str, provider: &str) -> Option<AppEntry> {
        let all = self.cached_apps(provider, true);
        if let Some(found) = all.iter().find(|a| a.id == app_id) {
            return Some(found.clone());
        }
        if let Some(found) = all
            .iter()
            .find(|a| a.id.to_lowercase() == app_id.to_lowercase())
        {
            return Some(found.clone());
        }
        let name_matches: Vec<_> = all
            .iter()
            .filter(|a| a.name.to_lowercase() == app_id.to_lowercase())
            .collect();
        if name_matches.len() == 1 {
            return Some(name_matches[0].clone());
        }
        None
    }

    pub async fn handle_action(
        &self,
        action: &str,
        params_json: &[u8],
    ) -> Result<serde_json::Value, LauncherError> {
        match action {
            "launch" => self.handle_launch(params_json).await,
            "launch_list" => self.handle_list(params_json).await,
            "launch_providers" => self.handle_providers(params_json).await,
            _ => Err(LauncherError::NotFound(format!("unknown action: {action}"))),
        }
    }

    async fn handle_launch(&self, params: &[u8]) -> Result<serde_json::Value, LauncherError> {
        let (app_id, provider, extra_args, dry_run) =
            request::parse_launch(params).map_err(LauncherError::BadParams)?;
        let app = if matches!(
            provider.as_str(),
            "tmux" | "kitty" | "alacritty" | "ghostty"
        ) {
            self.find_app_cached(&app_id, &provider)
                .unwrap_or(AppEntry {
                    id: app_id.clone(),
                    name: app_id.clone(),
                    provider: provider.clone(),
                    exec: Some(app_id.clone()),
                    path: std::path::PathBuf::from(""),
                    hidden: false,
                    terminal: false,
                    working_dir: None,
                })
        } else {
            self.find_app_cached(&app_id, &provider).ok_or_else(|| {
                LauncherError::NotFound(format!("app '{app_id}' not found (provider={provider})"))
            })?
        };
        let (bin, argv) = providers::resolve_launch_command(&self.config, &app, &extra_args)
            .map_err(|e| {
                if e.contains("ERR_LAUNCH_BLOCKED") {
                    LauncherError::Blocked(e)
                } else if e.contains("ERR_LAUNCH_PROVIDER_MISSING") {
                    LauncherError::ProviderMissing(e)
                } else if e.contains("ERR_LAUNCH_NOT_SUPPORTED") {
                    LauncherError::NotSupported(e)
                } else {
                    LauncherError::BadParams(e)
                }
            })?;
        if dry_run {
            return Ok(json!({
                "launched": false,
                "dry_run": true,
                "provider": app.provider,
                "app_id": app.id,
                "name": app.name,
                "command": bin,
                "argv": argv,
            }));
        }
        self.launcher
            .spawn_detached(&bin, &argv)
            .await
            .map_err(|e| {
                if e.contains("ERR_LAUNCH_PROVIDER_MISSING") {
                    LauncherError::ProviderMissing(e)
                } else if e.contains("ERR_LAUNCH_TIMEOUT") {
                    LauncherError::Timeout(e)
                } else {
                    LauncherError::SpawnFailed(e)
                }
            })?;
        Ok(json!({
            "launched": true,
            "dry_run": false,
            "provider": app.provider,
            "app_id": app.id,
            "name": app.name,
            "command": bin,
            "argv": argv,
        }))
    }

    async fn handle_list(&self, params: &[u8]) -> Result<serde_json::Value, LauncherError> {
        let (provider, query, limit, include_hidden) =
            request::parse_launch_list(params).map_err(LauncherError::BadParams)?;
        let mut apps = self.cached_apps(&provider, include_hidden);
        if let Some(q) = query.as_deref() {
            let ql = q.to_lowercase();
            let mut scored: Vec<(u32, AppEntry)> = apps
                .into_iter()
                .filter_map(|a| fuzzy_score(&ql, &a.id.to_lowercase(), &a.name.to_lowercase()).map(|s| (s, a)))
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));
            apps = scored.into_iter().map(|(_, a)| a).collect();
        } else {
            apps.sort_by(|a, b| a.id.cmp(&b.id));
        }
        apps.truncate(limit as usize);
        let providers_scanned: Vec<String> = if provider == "auto" {
            vec!["desktop".to_string(), "steam".to_string()]
        } else {
            vec![provider.clone()]
        };
        let json_apps: Vec<serde_json::Value> = apps
            .into_iter()
            .map(|a| {
                json!({
                    "id": a.id,
                    "name": a.name,
                    "provider": a.provider,
                    "exec": a.exec,
                    "path": a.path.display().to_string(),
                })
            })
            .collect();
        Ok(json!({ "apps": json_apps, "providers": providers_scanned }))
    }


    async fn handle_providers(&self, params: &[u8]) -> Result<serde_json::Value, LauncherError> {
        if !params.is_empty() && params != b"{}" && params != b"null" {
            // allow empty but reject non-empty malformed
            let v: serde_json::Value = serde_json::from_slice(params)
                .map_err(|e| LauncherError::BadParams(format!("invalid params_json: {e}")))?;
            if !v.is_object() || !v.as_object().unwrap().is_empty() {
                return Err(LauncherError::BadParams(
                    "launch_providers takes no params".to_string(),
                ));
            }
        }
        let infos = providers::list_providers(&self.config);
        let out: Vec<serde_json::Value> = infos
            .into_iter()
            .map(|p| {
                json!({
                    "id": p.id,
                    "name": p.name,
                    "available": p.available,
                    "description": p.description,
                    "roots": p.roots,
                })
            })
            .collect();
        Ok(serde_json::Value::Array(out))
    }
}

impl Plugin for LauncherPlugin<RealLauncher> {
    fn id(&self) -> &str {
        PLUGIN_ID
    }
    fn version(&self) -> &str {
        PLUGIN_VERSION
    }
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            permissions: vec!["launch".into()],
            actions: ACTIONS.iter().map(|s| s.to_string()).collect(),
            action_specs: crate::manifest::action_specs(),
            ..Default::default()
        }
    }
    async fn on_message(&mut self, envelope: Envelope) -> Result<Option<Envelope>, VynkorError> {
        let Some(envelope::Payload::ActionRequest(req)) = envelope.payload else {
            return Ok(None);
        };
        let result = self.handle_action(&req.action, &req.params_json).await;
        let reply = match result {
            Ok(value) => ActionResponse {
                action_id: req.action_id,
                status: ActionStatus::ActionOk as i32,
                data_json: value.to_string().into_bytes(),
                error: String::new(),
            },
            Err(e) => {
                let not_found = matches!(e, LauncherError::NotFound(_));
                ActionResponse {
                    action_id: req.action_id,
                    status: if not_found {
                        ActionStatus::ActionNotFound as i32
                    } else {
                        ActionStatus::ActionError as i32
                    },
                    data_json: Vec::new(),
                    error: e.to_string(),
                }
            }
        };
        Ok(Some(Envelope {
            payload: Some(envelope::Payload::ActionResponse(reply)),
            ..Default::default()
        }))
    }
}


fn fuzzy_score(query: &str, id: &str, name: &str) -> Option<u32> {
    let q = query.trim();
    if q.is_empty() { return Some(0); }
    if id == q || name == q { return Some(100); }
    if id.starts_with(q) { return Some(90); }
    if name.starts_with(q) { return Some(80); }
    if id.contains(q) { return Some(70); }
    if name.contains(q) { return Some(60); }
    if is_subsequence(q, id) { return Some(40); }
    if is_subsequence(q, name) { return Some(30); }
    let id_chars: std::collections::HashSet<char> = id.chars().collect();
    let name_chars: std::collections::HashSet<char> = name.chars().collect();
    let q_chars: std::collections::HashSet<char> = q.chars().collect();
    if q_chars.is_subset(&id_chars) || q_chars.is_subset(&name_chars) {
        return Some(10);
    }
    None
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut hay = haystack.chars();
    for c in needle.chars() {
        loop {
            match hay.next() {
                Some(h) if h == c => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, DesktopLauncher};
    use crate::runner::FakeLauncher;
    use std::sync::Arc;

    fn test_cfg(
        desktop_dir: Option<std::path::PathBuf>,
        steam_root: Option<std::path::PathBuf>,
    ) -> Config {
        Config {
            desktop_dirs: desktop_dir.into_iter().collect(),
            steam_roots: steam_root.into_iter().collect(),
            allowed_ids: None,
            timeout_ms: 5000,
            steam_binary: "steam".to_string(),
            desktop_launcher: DesktopLauncher::Exec,
            cache_ttl_ms: 60_000,
            terminal: crate::config::TerminalKind::None,
            tmux_session: "vynkor".to_string(),
        }
    }

    #[tokio::test]
    async fn launch_dry_run_desktop() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(
            td.path().join("myapp.desktop"),
            "[Desktop Entry]\nName=MyApp\nExec=/usr/bin/myapp %U\nType=Application\n",
        )
        .unwrap();
        let cfg = test_cfg(Some(td.path().to_path_buf()), None);
        let launcher = Arc::new(FakeLauncher::new_ok());
        let plugin = LauncherPlugin::new(cfg, launcher.clone());
        let v = plugin
            .handle_action("launch", br#"{"app_id":"myapp","dry_run":true}"#)
            .await
            .unwrap();
        assert_eq!(v["launched"], false);
        assert_eq!(v["dry_run"], true);
        assert_eq!(v["provider"], "desktop");
        assert_eq!(v["command"], "/usr/bin/myapp");
        // not actually launched
        assert!(launcher.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn launch_steam_dry_run() {
        let td = tempfile::tempdir().unwrap();
        let steamapps = td.path().join("steamapps");
        std::fs::create_dir_all(&steamapps).unwrap();
        std::fs::write(
            steamapps.join("appmanifest_123.acf"),
            r#""AppState" { "appid" "123" "name" "Test Game" }"#,
        )
        .unwrap();
        let cfg = test_cfg(None, Some(td.path().to_path_buf()));
        let launcher = Arc::new(FakeLauncher::new_ok());
        let plugin = LauncherPlugin::new(cfg, launcher.clone());
        let v = plugin
            .handle_action(
                "launch",
                br#"{"app_id":"123","provider":"steam","dry_run":true}"#,
            )
            .await
            .unwrap();
        assert_eq!(v["provider"], "steam");
        assert_eq!(v["command"], "steam");
        assert_eq!(v["argv"][0], "steam://rungameid/123");
    }

    #[tokio::test]
    async fn launch_actual_spawns() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(
            td.path().join("myapp.desktop"),
            "[Desktop Entry]\nName=MyApp\nExec=/usr/bin/myapp\nType=Application\n",
        )
        .unwrap();
        let cfg = test_cfg(Some(td.path().to_path_buf()), None);
        let launcher = Arc::new(FakeLauncher::new_ok());
        let plugin = LauncherPlugin::new(cfg, launcher.clone());
        let v = plugin
            .handle_action("launch", br#"{"app_id":"myapp"}"#)
            .await
            .unwrap();
        assert_eq!(v["launched"], true);
        let calls = launcher.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "/usr/bin/myapp");
    }

    #[tokio::test]
    async fn launch_not_found() {
        let cfg = test_cfg(None, None);
        let launcher = Arc::new(FakeLauncher::new_ok());
        let plugin = LauncherPlugin::new(cfg, launcher);
        let e = plugin
            .handle_action("launch", br#"{"app_id":"nope"}"#)
            .await
            .unwrap_err();
        assert!(matches!(e, LauncherError::NotFound(_)));
    }

    #[tokio::test]
    async fn launch_blocked_by_allowlist() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(
            td.path().join("allowed.desktop"),
            "[Desktop Entry]\nName=Allowed\nExec=allowed\nType=Application\n",
        )
        .unwrap();
        std::fs::write(
            td.path().join("blocked.desktop"),
            "[Desktop Entry]\nName=Blocked\nExec=blocked\nType=Application\n",
        )
        .unwrap();
        let mut cfg = test_cfg(Some(td.path().to_path_buf()), None);
        cfg.allowed_ids = Some(vec!["allowed".to_string()]);
        let launcher = Arc::new(FakeLauncher::new_ok());
        let plugin = LauncherPlugin::new(cfg, launcher);
        let e = plugin
            .handle_action("launch", br#"{"app_id":"blocked"}"#)
            .await
            .unwrap_err();
        assert!(matches!(e, LauncherError::Blocked(_)));
        // allowed passes
        let v = plugin
            .handle_action("launch", br#"{"app_id":"allowed","dry_run":true}"#)
            .await
            .unwrap();
        assert_eq!(v["app_id"], "allowed");
    }

    #[tokio::test]
    async fn launch_list_filters() {
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
        let cfg = test_cfg(Some(td.path().to_path_buf()), None);
        let launcher = Arc::new(FakeLauncher::new_ok());
        let plugin = LauncherPlugin::new(cfg, launcher);
        let v = plugin
            .handle_action("launch_list", br#"{"query":"alpha"}"#)
            .await
            .unwrap();
        assert_eq!(v["apps"].as_array().unwrap().len(), 1);
        assert_eq!(v["apps"][0]["id"], "a");
    }

    #[tokio::test]
    async fn launch_providers_returns_both() {
        let cfg = test_cfg(None, None);
        let launcher = Arc::new(FakeLauncher::new_ok());
        let plugin = LauncherPlugin::new(cfg, launcher);
        let v = plugin
            .handle_action("launch_providers", b"{}")
            .await
            .unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 6);
        assert!(arr.iter().any(|x| x["id"] == "desktop"));
        assert!(arr.iter().any(|x| x["id"] == "steam"));
        assert!(arr.iter().any(|x| x["id"] == "tmux"));
    }

    #[tokio::test]
    async fn unknown_action_is_not_found() {
        let cfg = test_cfg(None, None);
        let launcher = Arc::new(FakeLauncher::new_ok());
        let plugin = LauncherPlugin::new(cfg, launcher);
        let e = plugin.handle_action("bogus", b"{}").await.unwrap_err();
        assert!(matches!(e, LauncherError::NotFound(_)));
    }
}
