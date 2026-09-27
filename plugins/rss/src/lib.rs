pub mod request;
pub mod store;

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use store::{Article, Feed};

#[derive(Debug, Clone)]
pub struct Config { pub db_timeout_ms: u32, pub fetch_timeout_ms: u32 }
impl Default for Config { fn default() -> Self { Self { db_timeout_ms: 5000, fetch_timeout_ms: 10000 } } }
impl Config {
    pub fn from_env() -> Self {
        let db_timeout_ms = std::env::var("RSS_PLUGIN_DB_TIMEOUT_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(5000);
        let fetch_timeout_ms = std::env::var("RSS_PLUGIN_FETCH_TIMEOUT_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(10000);
        Self { db_timeout_ms, fetch_timeout_ms }
    }
}

pub struct RpcCall { pub action: String, pub params_json: Vec<u8>, pub timeout_ms: u32, pub reply: oneshot::Sender<Result<Value, String>> }
#[derive(Clone)]
pub struct Rpc { tx: mpsc::Sender<RpcCall> }
impl Rpc {
    pub fn new(tx: mpsc::Sender<RpcCall>) -> Self { Self { tx } }
    pub async fn call(&self, action: &str, params: Value, timeout_ms: u32) -> Result<Value, String> {
        let params_json = serde_json::to_vec(&params).map_err(|e| format!("failed to encode {action} params: {e}"))?;
        let (reply, rx) = oneshot::channel();
        self.tx.send(RpcCall { action: action.to_string(), params_json, timeout_ms, reply }).await.map_err(|_| format!("{action} aborted"))?;
        let effective = if timeout_ms==0 {30_000} else {timeout_ms};
        match tokio::time::timeout(std::time::Duration::from_millis(effective as u64), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(format!("{action} aborted")),
            Err(_) => Err(format!("{action} timed out after {effective} ms")),
        }
    }
}

#[derive(Debug)]
pub struct ActionResult { pub data: Vec<u8>, pub event: Option<(String, Value)> }

pub async fn handle_action(rpc: Rpc, config: &Config, action: &str, params_json: &[u8], start: std::time::Instant) -> Result<ActionResult, String> {
    let req = request::parse_request(action, params_json)?;
    let db = store::Db::new(rpc.clone(), config.db_timeout_ms);
    match req {
        request::RssRequest::Add { url } => {
            let existing = db.list_feeds().await?;
            if let Some(f) = existing.iter().find(|f| f.url==url) {
                return ok(json!({"id": f.id, "feed": f}), None);
            }
            let id = db.next_id(store::NEXT_FEED_ID).await?.to_string();
            let now = store::now_ms();
            let feed = Feed { id: id.clone(), url: url.clone(), title: url.clone(), created_at_ms: now };
            db.put_feed(&feed).await?;
            ok(json!({"id": id, "feed": feed}), Some(("feed_added".into(), json!({"id": id, "url": url}))))
        }
        request::RssRequest::Remove { id } => {
            let removed = db.delete_feed(&id).await?;
            // also delete articles for that feed
            if removed {
                let articles = db.list_articles().await?;
                for a in articles.iter().filter(|a| a.feed_id==id) {
                    let _ = db.call("db_delete", json!({"key": format!("article:{}", a.id)})).await;
                }
            }
            ok(json!({"removed": removed}), removed.then(|| ("feed_removed".into(), json!({"id": id}))))
        }
        request::RssRequest::List => {
            let mut feeds = db.list_feeds().await?;
            feeds.sort_by(|a,b| a.created_at_ms.cmp(&b.created_at_ms));
            ok(json!({"feeds": feeds}), None)
        }
        request::RssRequest::Fetch { id, timeout_ms } => {
            let feed = db.get_feed(&id).await?.ok_or_else(|| format!("feed not found: {id}"))?;
            let (fetched, new) = fetch_feed(rpc.clone(), &db, &feed, timeout_ms).await?;
            ok(json!({"fetched": fetched, "new": new}), Some(("feed_fetched".into(), json!({"id": id, "fetched": fetched, "new": new}))))
        }
        request::RssRequest::FetchAll { timeout_ms } => {
            let feeds = db.list_feeds().await?;
            let mut total_fetched=0; let mut total_new=0;
            for feed in feeds.iter() {
                let (f,n) = fetch_feed(rpc.clone(), &db, feed, timeout_ms).await?;
                total_fetched+=f; total_new+=n;
            }
            ok(json!({"fetched": total_fetched, "new": total_new, "feeds": feeds.len()}), Some(("fetch_all".into(), json!({"fetched": total_fetched, "new": total_new}))))
        }
        request::RssRequest::Articles { feed_id, unread_only, query, limit, offset } => {
            let mut articles = db.list_articles().await?;
            if let Some(fid) = feed_id { articles.retain(|a| a.feed_id==fid); }
            if unread_only { articles.retain(|a| !a.read); }
            if let Some(q) = query { let ql=q.to_lowercase(); articles.retain(|a| a.title.to_lowercase().contains(&ql) || a.link.to_lowercase().contains(&ql)); }
            articles.sort_by(|a,b| b.published_ms.unwrap_or(b.created_at_ms).cmp(&a.published_ms.unwrap_or(a.created_at_ms)));
            let total = articles.len();
            let page: Vec<&Article> = articles.iter().skip(offset).take(limit).collect();
            ok(json!({"articles": page, "total": total}), None)
        }
        request::RssRequest::MarkRead { id, read } => {
            let mut article = db.get_article(&id).await?.ok_or_else(|| format!("article not found: {id}"))?;
            article.read = read;
            db.put_article(&article).await?;
            ok(json!({"updated": true, "article": article}), None)
        }
        request::RssRequest::Status => {
            let uptime_ms = start.elapsed().as_millis() as u64;
            ok(json!({"version": env!("CARGO_PKG_VERSION"), "uptime_ms": uptime_ms, "engine_ready": true, "last_error": Value::Null, "counters": {}}), None)
        }
    }
}

async fn fetch_feed(rpc: Rpc, db: &store::Db, feed: &Feed, timeout_ms: u64) -> Result<(usize, usize), String> {
    let http_req = json!({"url": feed.url, "method": "GET", "timeout_ms": timeout_ms});
    let res = rpc.call("http_request", http_req, timeout_ms as u32).await;
    let v = match res {
        Ok(v) => v,
        Err(e) => return Err(format!("http_request failed: {e}")),
    };
    let status = v.get("status").and_then(Value::as_i64).unwrap_or(0);
    if !(200..300).contains(&status) {
        let body = v.get("body").and_then(Value::as_str).unwrap_or("");
        return Err(format!("feed returned HTTP {status}: {body}"));
    }
    let body = v.get("body").and_then(Value::as_str).unwrap_or("").to_string();
    let encoding = v.get("body_encoding").and_then(Value::as_str).unwrap_or("utf8");
    let bytes = if encoding=="base64" {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.decode(&body).map_err(|e| format!("base64 decode: {e}"))?
    } else { body.into_bytes() };
    let text = String::from_utf8_lossy(&bytes).to_string();
    let channel = rss::Channel::read_from(text.as_bytes()).map_err(|e| format!("rss parse: {e}"))?;
    let mut fetched=0; let mut new=0;
    for item in channel.items() {
        fetched+=1;
        let title = item.title().unwrap_or("untitled").to_string();
        let link = item.link().unwrap_or("").to_string();
        if link.is_empty() { continue; }
        if db.find_article_by_link(&link).await?.is_some() { continue; }
        let published_ms = item.pub_date().and_then(|d| chrono::DateTime::parse_from_rfc2822(d).ok()).map(|dt| dt.timestamp_millis());
        let id = db.next_id(store::NEXT_ARTICLE_ID).await?.to_string();
        let now = store::now_ms();
        let article = Article { id, feed_id: feed.id.clone(), title, link, published_ms, read: false, created_at_ms: now };
        db.put_article(&article).await?;
        new+=1;
    }
    // update feed title if channel has title
    let channel_title = channel.title();
    if !channel_title.is_empty() && channel_title != &feed.title {
        let mut updated = feed.clone();
        updated.title = channel_title.to_string();
        let _ = db.put_feed(&updated).await;
    }
    Ok((fetched, new))
}

fn ok(data: Value, event: Option<(String, Value)>) -> Result<ActionResult, String> {
    let data = serde_json::to_vec(&data).map_err(|e| format!("encode: {e}"))?;
    Ok(ActionResult { data, event })
}
