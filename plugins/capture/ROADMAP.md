# capture plugin roadmap

> Audit 2026-09-27. Cross-plugin priorities live in root `PLANS.md`; this
> file holds the plugin-level backlog. **STAT-01** (rename a bare `status`
> action to `<slug>_status`) applies wherever this plugin declares `status`.

## Next

- **Webcam** — V4L2 frame grab + recording; needs `PERMISSION_CAMERA`
  (enum 23, same wire bump as 20–24).
- **Active-window capture** — once the `window` plugin can report geometry
  (today: `full` or an explicit `region`).
- **OCR languages** — `CAPTURE_PLUGIN_OCR_LANGS` (e.g. `eng+rus`) and a
  per-call `lang`.
- **Screen → agent** — `capture_ocr` of the focused window as a one-shot
  "what am I looking at" tool; cloud vision via `ai` for images tesseract
  can't read.
- **Retention** — auto-delete captures older than N days in
  `CAPTURE_PLUGIN_DIR`.
