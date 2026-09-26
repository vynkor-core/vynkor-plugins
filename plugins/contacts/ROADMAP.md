# contacts plugin roadmap

> Audit 2026-09-27. Cross-plugin priorities live in root `PLANS.md`; this
> file holds the plugin-level backlog. **STAT-01** (rename a bare `status`
> action to `<slug>_status`) applies wherever this plugin declares `status`.

## Next

- **vCard import/export** — `contact_import_vcf {data}` /
  `contact_export_vcf`; the store is "vCard-ish" but has no vCard I/O.
- **Birthdays & dates** — `birthday` field; a daily scan (or `calendar`
  events) feeding the morning briefing.
- **Multiple values** — several emails/phones with labels; `telegram`
  handle and address fields.
- **Dedupe/merge** — `contact_merge {ids[]}`.
- **Resolve** — `contact_resolve {name}` → best match with email/phone/
  telegram peer, so "write to Anna" works across email and telegram.
