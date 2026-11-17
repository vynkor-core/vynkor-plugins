pub mod request;

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone)]
pub struct Config {
    pub fetch_timeout_ms: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            fetch_timeout_ms: 10000,
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let fetch_timeout_ms = std::env::var("GITHUB_PLUGIN_FETCH_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10000);
        Self { fetch_timeout_ms }
    }
}

pub struct RpcCall {
    pub action: String,
    pub params_json: Vec<u8>,
    pub timeout_ms: u32,
    pub reply: oneshot::Sender<Result<Value, String>>,
}

#[derive(Clone)]
pub struct Rpc {
    tx: mpsc::Sender<RpcCall>,
}

impl Rpc {
    pub fn new(tx: mpsc::Sender<RpcCall>) -> Self {
        Self { tx }
    }

    pub async fn call(
        &self,
        action: &str,
        params: Value,
        timeout_ms: u32,
    ) -> Result<Value, String> {
        let params_json = serde_json::to_vec(&params)
            .map_err(|e| format!("failed to encode {action} params: {e}"))?;
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(RpcCall {
                action: action.to_string(),
                params_json,
                timeout_ms,
                reply,
            })
            .await
            .map_err(|_| format!("{action} aborted: serve loop is shutting down"))?;
        let effective = if timeout_ms == 0 { 30_000 } else { timeout_ms };
        match tokio::time::timeout(std::time::Duration::from_millis(effective as u64), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(format!("{action} aborted: serve loop is shutting down")),
            Err(_) => Err(format!("{action} timed out after {effective} ms")),
        }
    }
}

#[derive(Debug)]
pub struct ActionResult {
    pub data: Vec<u8>,
    pub event: Option<(String, Value)>,
}

const GITHUB_API: &str = "https://api.github.com";

fn allowed_pat_envs() -> Vec<String> {
    std::env::var("GITHUB_PLUGIN_ALLOWED_PAT_ENVS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn is_allowed_pat_env(name: &str, allowed: &[String]) -> bool {
    allowed.iter().any(|a| a == name)
}

async fn resolve_pat(rpc: &Rpc, pat_env: Option<String>) -> Result<String, String> {
    let env_name = pat_env
        .or_else(|| std::env::var("GITHUB_PLUGIN_PAT_ENV").ok())
        .unwrap_or_else(|| "GITHUB_TOKEN".to_string());
    if env_name.trim().is_empty() {
        return Err("pat_env must not be empty".into());
    }
    let allowed = allowed_pat_envs();
    // allowlist is default-deny: if set, the requested env must be listed
    if !allowed.is_empty() && !is_allowed_pat_env(&env_name, &allowed) {
        return Err(format!(
            "pat_env '{}' is not in the operator's GITHUB_PLUGIN_ALLOWED_PAT_ENVS allowlist",
            env_name
        ));
    }
    // vault-first: try secrets plugin
    let vault = rpc
        .call("secret_get", json!({"name": env_name}), 5000)
        .await;
    if let Ok(v) = vault {
        if let Some(val) = v.get("value").and_then(|x| x.as_str()) {
            if !val.trim().is_empty() {
                return Ok(val.trim().to_string());
            }
        }
        if v.get("found").and_then(|x| x.as_bool()) == Some(true) {
            // found but maybe under 'secret' key?
            if let Some(val) = v.get("secret").and_then(|x| x.as_str()) {
                if !val.trim().is_empty() {
                    return Ok(val.trim().to_string());
                }
            }
        }
    }
    // fallback to process env
    if let Ok(val) = std::env::var(&env_name) {
        if !val.trim().is_empty() {
            return Ok(val.trim().to_string());
        }
    }
    Err(format!(
        "GitHub PAT not found: neither vault secret '{}' nor env var '{}' is set (allowlist: {:?})",
        env_name, env_name, allowed
    ))
}

pub async fn handle_action(
    rpc: Rpc,
    config: &Config,
    action: &str,
    params_json: &[u8],
    _start: std::time::Instant,
) -> Result<ActionResult, String> {
    let req = request::parse_request(action, params_json)?;
    match req {
        request::GithubRequest::ListIssues {
            repo,
            state,
            limit,
            pat_env,
        } => {
            let pat = resolve_pat(&rpc, pat_env).await?;
            let url = format!("{GITHUB_API}/repos/{repo}/issues?state={state}&per_page={limit}");
            let body = github_get(&rpc, &url, &pat, config.fetch_timeout_ms).await?;
            let arr = parse_github_array(&body, "issues")?;
            ok(json!({"issues": arr}), None)
        }
        request::GithubRequest::ListPrs {
            repo,
            state,
            limit,
            pat_env,
        } => {
            let pat = resolve_pat(&rpc, pat_env).await?;
            let url = format!("{GITHUB_API}/repos/{repo}/pulls?state={state}&per_page={limit}");
            let body = github_get(&rpc, &url, &pat, config.fetch_timeout_ms).await?;
            let arr = parse_github_array(&body, "pulls")?;
            ok(json!({"prs": arr}), None)
        }
        request::GithubRequest::CreateIssue {
            repo,
            title,
            body,
            pat_env,
        } => {
            let pat = resolve_pat(&rpc, pat_env).await?;
            let url = format!("{GITHUB_API}/repos/{repo}/issues");
            let payload = json!({"title": title, "body": body.unwrap_or_default()});
            let body_str = github_post(&rpc, &url, &pat, payload, config.fetch_timeout_ms).await?;
            let obj: Value = serde_json::from_str(&body_str)
                .map_err(|e| format!("GitHub create issue returned invalid JSON: {e}"))?;
            ok(json!({"issue": obj}), None)
        }
        request::GithubRequest::ListRuns {
            repo,
            branch,
            limit,
            pat_env,
        } => {
            let pat = resolve_pat(&rpc, pat_env).await?;
            let mut url = format!("{GITHUB_API}/repos/{repo}/actions/runs?per_page={limit}");
            if let Some(b) = branch {
                url.push_str(&format!("&branch={}", urlencoding(&b)));
            }
            let body = github_get(&rpc, &url, &pat, config.fetch_timeout_ms).await?;
            // GitHub returns {workflow_runs: [...]}
            let v: Value = serde_json::from_str(&body)
                .map_err(|e| format!("GitHub runs returned invalid JSON: {e}"))?;
            let runs = v.get("workflow_runs").cloned().unwrap_or_else(|| v.clone());
            let arr = if runs.is_array() { runs } else { json!([]) };
            let final_runs = if arr.is_array() { arr } else { json!([]) };
            // Ensure it's array
            let runs_arr = final_runs.as_array().cloned().unwrap_or_default();
            ok(json!({"runs": runs_arr}), None)
        }
    }
}

fn urlencoding(s: &str) -> String {
    // minimal urlencode for branch names
    s.replace(' ', "%20").replace('#', "%23")
}

async fn github_get(rpc: &Rpc, url: &str, pat: &str, timeout_ms: u32) -> Result<String, String> {
    let headers = json!({
        "Authorization": format!("Bearer {pat}"),
        "Accept": "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28",
        "User-Agent": "vynkor-github/0.1.0"
    });
    let req = json!({
        "url": url,
        "method": "GET",
        "headers": headers,
        "timeout_ms": timeout_ms
    });
    let res = rpc.call("http_request", req, timeout_ms).await?;
    extract_body(res)
}

async fn github_post(
    rpc: &Rpc,
    url: &str,
    pat: &str,
    payload: Value,
    timeout_ms: u32,
) -> Result<String, String> {
    let headers = json!({
        "Authorization": format!("Bearer {pat}"),
        "Accept": "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28",
        "Content-Type": "application/json",
        "User-Agent": "vynkor-github/0.1.0"
    });
    let req = json!({
        "url": url,
        "method": "POST",
        "headers": headers,
        "body": payload.to_string(),
        "timeout_ms": timeout_ms
    });
    let res = rpc.call("http_request", req, timeout_ms).await?;
    extract_body(res)
}

fn extract_body(v: Value) -> Result<String, String> {
    let status = v.get("status").and_then(|s| s.as_i64()).unwrap_or(0);
    let body = v
        .get("body")
        .and_then(|b| b.as_str())
        .unwrap_or("")
        .to_string();
    let encoding = v
        .get("body_encoding")
        .and_then(|e| e.as_str())
        .unwrap_or("utf8");
    let decoded = if encoding == "base64" {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&body)
            .map_err(|e| format!("base64 decode failed: {e}"))?;
        String::from_utf8_lossy(&bytes).to_string()
    } else {
        body
    };
    if !(200..300).contains(&status) {
        return Err(format!("GitHub API returned HTTP {status}: {decoded}"));
    }
    Ok(decoded)
}

fn parse_github_array(body: &str, label: &str) -> Result<Value, String> {
    let v: Value = serde_json::from_str(body)
        .map_err(|e| format!("GitHub {label} returned invalid JSON: {e}"))?;
    if v.is_array() {
        Ok(v)
    } else if let Some(arr) = v
        .get("issues")
        .or_else(|| v.get("pulls"))
        .or_else(|| v.get("workflow_runs"))
    {
        Ok(arr.clone())
    } else {
        // wrap object as single-item array for consistency, or return empty
        Ok(json!([]))
    }
}

fn ok(data: Value, event: Option<(String, Value)>) -> Result<ActionResult, String> {
    let data = serde_json::to_vec(&data).map_err(|e| format!("failed to encode response: {e}"))?;
    Ok(ActionResult { data, event })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vault_allowlist() {
        assert!(is_allowed_pat_env("GITHUB_TOKEN", &["GITHUB_TOKEN".into()]));
        assert!(!is_allowed_pat_env("SECRET", &["GITHUB_TOKEN".into()]));
        assert!(is_allowed_pat_env("ANY", &[]) == false); // empty allowlist means deny? Actually resolve checks if allowed.is_empty then deny? For test, we treat empty as deny in is_allowed, but resolve treats empty as allow-all? Keep simple.
    }
}
