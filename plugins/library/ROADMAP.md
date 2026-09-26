# library plugin roadmap

> Audit 2026-09-27. Cross-plugin priorities live in root `PLANS.md`; this
> file holds the plugin-level backlog. **STAT-01** (rename a bare `status`
> action to `<slug>_status`) applies wherever this plugin declares `status`.

## Next

- **Metadata** — ID3/Vorbis tags (artist/album/duration) for audio, EXIF
  date/location for photos; search by them.
- **Incremental rescans** — mtime checkpoints instead of full walks (the
  shared crawl crate, ARCH-03).
- **Semantic search** — embed titles/tags into `vector-db` ("something
  calm", "photos from the sea") — CAP-09's vector-db dependency is not used
  yet.
- **Playlists** — `library_playlist {query}` → ordered paths for a `sound`
  queue or an MPRIS player.
- **"On this day"** — photos from this date in past years for the briefing.
