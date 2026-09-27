# rss plugin roadmap

> Audit 2026-09-27. Cross-plugin priorities live in root `PLANS.md`; this
> file holds the plugin-level backlog. **STAT-01** (rename a bare `status`
> action to `<slug>_status`) applies wherever this plugin declares `status`.

## Next

- **Batch read state** — `rss_mark_read {ids[]}` / `{feed_id, before_ms}`;
  the digest playbook currently spends one agent step per article.
- **Background refresh** — optional `RSS_PLUGIN_REFRESH_SECS` loop with
  `plugin.rss.new_articles {feed_id, count}` events (today: pull-only,
  scheduled through `scheduler` → `rss_fetch_all`).
- **OPML import/export** — bring an existing subscription list in one call.
- **Per-feed metadata** — title/site from the feed itself, last error,
  last fetch time in `rss_list`.
- **Semantic dedupe/search** over `vector-db` (INT-05's "what's new" angle).
