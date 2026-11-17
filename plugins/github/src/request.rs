use serde::Deserialize;

#[derive(Debug)]
pub enum GithubRequest {
    ListIssues {
        repo: String,
        state: String,
        limit: u32,
        pat_env: Option<String>,
    },
    CreateIssue {
        repo: String,
        title: String,
        body: Option<String>,
        pat_env: Option<String>,
    },
    ListPrs {
        repo: String,
        state: String,
        limit: u32,
        pat_env: Option<String>,
    },
    ListRuns {
        repo: String,
        branch: Option<String>,
        limit: u32,
        pat_env: Option<String>,
    },
}

#[derive(Deserialize)]
struct BaseRepo {
    repo: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    pat_env: Option<String>,
}

#[derive(Deserialize)]
struct CreateIssueParams {
    repo: String,
    title: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    pat_env: Option<String>,
}

#[derive(Deserialize)]
struct RunsParams {
    repo: String,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    pat_env: Option<String>,
}

fn validate_repo(repo: &str) -> Result<String, String> {
    let r = repo.trim().to_string();
    if r.is_empty() {
        return Err("params.repo must not be empty".into());
    }
    if !r.contains('/') {
        return Err("params.repo must be \"owner/repo\"".into());
    }
    if r.len() > 200 {
        return Err("params.repo exceeds 200 bytes".into());
    }
    Ok(r)
}

fn normalize_state(s: Option<String>) -> String {
    match s.as_deref().map(|v| v.trim().to_ascii_lowercase()) {
        Some(v) if v == "closed" || v == "all" => v,
        _ => "open".to_string(),
    }
}

pub fn parse_request(action: &str, params_json: &[u8]) -> Result<GithubRequest, String> {
    match action {
        "gh_list_issues" => {
            let p: BaseRepo = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for gh_list_issues: {e}"))?;
            Ok(GithubRequest::ListIssues {
                repo: validate_repo(&p.repo)?,
                state: normalize_state(p.state),
                limit: p.limit.unwrap_or(20).clamp(1, 100),
                pat_env: p.pat_env,
            })
        }
        "gh_list_prs" => {
            let p: BaseRepo = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for gh_list_prs: {e}"))?;
            Ok(GithubRequest::ListPrs {
                repo: validate_repo(&p.repo)?,
                state: normalize_state(p.state),
                limit: p.limit.unwrap_or(20).clamp(1, 100),
                pat_env: p.pat_env,
            })
        }
        "gh_create_issue" => {
            let p: CreateIssueParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for gh_create_issue: {e}"))?;
            if p.title.trim().is_empty() {
                return Err("params.title must not be empty".into());
            }
            if p.title.len() > 256 {
                return Err("params.title exceeds 256 bytes".into());
            }
            Ok(GithubRequest::CreateIssue {
                repo: validate_repo(&p.repo)?,
                title: p.title.trim().to_string(),
                body: p.body,
                pat_env: p.pat_env,
            })
        }
        "gh_list_runs" => {
            let p: RunsParams = serde_json::from_slice(params_json)
                .map_err(|e| format!("invalid params for gh_list_runs: {e}"))?;
            Ok(GithubRequest::ListRuns {
                repo: validate_repo(&p.repo)?,
                branch: p.branch.and_then(|b| {
                    let t = b.trim().to_string();
                    if t.is_empty() {
                        None
                    } else {
                        Some(t)
                    }
                }),
                limit: p.limit.unwrap_or(10).clamp(1, 100),
                pat_env: p.pat_env,
            })
        }
        other => Err(format!("unknown action: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn list_issues_defaults() {
        let r = parse_request("gh_list_issues", br#"{"repo":"a/b"}"#).unwrap();
        match r {
            GithubRequest::ListIssues {
                repo, state, limit, ..
            } => {
                assert_eq!(repo, "a/b");
                assert_eq!(state, "open");
                assert_eq!(limit, 20);
            }
            _ => panic!(),
        }
    }
    #[test]
    fn rejects_bad_repo() {
        assert!(parse_request("gh_list_issues", br#"{"repo":"bad"}"#).is_err());
        assert!(parse_request("gh_create_issue", br#"{"repo":"a/b","title":""}"#).is_err());
    }
    #[test]
    fn create_issue_ok() {
        let r = parse_request(
            "gh_create_issue",
            br#"{"repo":"o/r","title":"hello","body":"world"}"#,
        )
        .unwrap();
        match r {
            GithubRequest::CreateIssue { title, .. } => assert_eq!(title, "hello"),
            _ => panic!(),
        }
    }
}
