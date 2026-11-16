# Learned Routines Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The `agent` plugin learns repeated single-tool device commands overnight on a local model and afterwards serves them with zero LLM calls and a pre-generated, rotating reply.

**Architecture:** New `routines` module tree in `plugins/agent/src/routines/`: pure logic (`mod.rs`: types, eligibility, matching, correction), persistence (`store.rs`: one `database` doc), mining (`mine.rs`: candidate extraction + one local-model call per candidate), firing (`fire.rs`: hook called from `engine::run` before small talk). A tokio timer in `main.rs` runs mining at `AGENT_PLUGIN_ROUTINES_AT`.

**Tech Stack:** Rust 2021, tokio, serde_json, chrono 0.4 (`clock`, already used by calendar/filesystem), existing `Rpc` / `database` / `ai` actions.

**Spec:** `docs/superpowers/specs/2026-10-26-learned-routines-design.md`

## Global Constraints

- Feature is off unless `AGENT_PLUGIN_ROUTINES=on`.
- Learnable tools: exact names from `AGENT_PLUGIN_ROUTINE_TOOLS`; default `mqtt_device_command,pixel-6pro.flashlight,pixel-6pro.dnd,pixel-6pro.ringer,pixel-6pro.brightness`; additionally `requires_confirmation == false` and `risk != "high"` in the current catalog.
- Learn at ≥ 2 successes; one `tool_error` or one user correction disables.
- Mining uses `AGENT_PLUGIN_ROUTINES_AGENT_ID`, else `AGENT_PLUGIN_MEMORY_AGENT_ID`; neither set → mining refuses. Never the goal's own plan, never `chat_with_fallback`.
- Mining time `AGENT_PLUGIN_ROUTINES_AT=HH:MM` local, default `03:30`; lookback `AGENT_PLUGIN_ROUTINES_LOOKBACK_DAYS`, default 30.
- Up to 8 paraphrases, 4–6 replies, each reply non-empty and ≤ 160 chars.
- Correction window 120 s; markers `не то`, `не тот`, `не так`, `неправильно`, `отмена`, `отмени`, `верни` (prefix match on the normalized goal).
- Usage tag for mining calls: `purpose: "routine_mine"`.
- Text normalizer is shared with small talk (lowercase, `ё→е`, non-alphanumerics → space, collapse whitespace).

## Builder calls (deviations from the spec's "left to builder")

- Storage is **one** `database` doc `routines` = `{routines: [Routine], last_fire: {routine_id, at_ms} | null}` instead of per-routine docs + index: the set is dozens of entries, one read per goal, no index to drift. Lost-update race between concurrent goals only affects counters.
- No in-memory phrase map: the doc is read at each fresh goal (one local `db_get`), so there is nothing to refresh.
- `spec_hash` is FNV-1a 64 over `name \0 description \0 parameters-JSON` (stable across toolchains, unlike `DefaultHasher`).

## Review Focus

1. Goal text with emoji / punctuation only ("👍") — normalizes to empty; must never match a routine (empty phrase is never stored).
2. Same normalized text learned for two different `(tool, params)` — must be dropped from both, not fire either.
3. Tool removed from the catalog after learning — firing must disable the routine and fall through, not dispatch an unknown action.
4. Local model returns prose / broken JSON / replies over 160 chars — candidate skipped or replies filtered, never a routine with zero replies.
5. Routines doc missing or corrupt in `database` — goal runs normally; a corrupt doc logs and is treated as empty (never fails `goal_start`).

---

### Task 1: Shared normalizer + routine core types and pure logic

**Files:**
- Modify: `plugins/agent/src/smalltalk.rs` (extract `pub fn normalize(text: &str) -> String`; `is_small_talk` uses it)
- Create: `plugins/agent/src/routines/mod.rs`
- Modify: `plugins/agent/src/lib.rs` (`pub mod routines;`)

**Interfaces — Produces:**
- `smalltalk::normalize(&str) -> String`
- `routines::Routine { id: String, tool: String, params: Value, phrases: Vec<String>, replies: Vec<String>, spec_hash: String, status: String /* "active"|"disabled" */, disabled_reason: String, disabled_at_ms: i64, hits: u32, fails: u32, last_reply_idx: usize, created_at_ms: i64, updated_at_ms: i64 }` (serde, `#[serde(default)]` on all but id/tool/params)
- `routines::Book { routines: Vec<Routine>, last_fire: Option<LastFire> }`, `LastFire { routine_id: String, at_ms: i64 }`
- `routines::enabled() -> bool`, `routines::allowed_tools() -> Vec<String>`
- `routines::eligible(spec: &ToolSpec, allow: &[String]) -> bool`
- `routines::spec_hash(spec: &ToolSpec) -> String`
- `Book::find_active(&self, normalized: &str) -> Option<usize>` (index into `routines`)
- `Book::next_reply(&mut self, idx: usize) -> String` (rotates `last_reply_idx`, never repeats consecutively when ≥ 2 replies)
- `routines::is_correction(normalized: &str) -> bool`
- `Book::correction_target(&self, now_ms: i64) -> Option<usize>` (last fire within 120 000 ms and still active)
- `Book::disable(&mut self, idx: usize, reason: &str, now_ms: i64)`

- [ ] **Step 1: Tests** in `routines/mod.rs`:
  - `normalize("Включи СВЕТ!!") == "включи свет"`, `normalize("ёлка") == "елка"`, `normalize("👍") == ""`.
  - `eligible`: allowlisted + low risk → true; not allowlisted → false; `requires_confirmation` → false; `risk "high"` → false.
  - `spec_hash` equal for identical specs, differs when description or parameters change.
  - `find_active`: hit on phrase of active routine; `None` for disabled routine; `None` for `""`.
  - `next_reply` with 3 replies called 4× yields no two equal consecutive values; with 1 reply always returns it.
  - `is_correction("не тот свет")`, `is_correction("отмена")` true; `is_correction("включи свет")` false.
  - `correction_target` some at 119 s, none at 121 s, none if routine already disabled.
- [ ] **Step 2:** `cargo test -p agent-plugin routines` → fails to compile.
- [ ] **Step 3:** Implement. FNV-1a 64: offset `0xcbf29ce484222325`, prime `0x100000001b3`, hex output.
- [ ] **Step 4:** Tests pass; `small_talk` tests still pass.
- [ ] **Step 5:** Commit `feat(agent): routine core types and matching`.

### Task 2: Persistence

**Files:** Create `plugins/agent/src/routines/store.rs`

**Interfaces — Produces:**
- `pub const BOOK_KEY: &str = "routines";`
- `pub async fn load(rpc: &Rpc, timeout_ms: u32) -> Book` — missing → empty; corrupt → `eprintln!` + empty; RPC error → `eprintln!` + empty.
- `pub async fn save(rpc: &Rpc, timeout_ms: u32, book: &Book) -> Result<(), String>` — `db_set`, requires `{"ok": true}`.

- [ ] **Step 1:** Integration test in `main.rs` tests (`routines_book_roundtrip_and_corrupt_doc_reads_empty`) via shim: `save` then `load` equal; shim `db_set` of `"routines"` = `"garbage"` → `load` returns empty book.
- [ ] **Step 2–4:** fail → implement → pass.
- [ ] **Step 5:** Commit `feat(agent): routine book persistence`.

### Task 3: Firing + correction hook in the engine

**Files:** Create `plugins/agent/src/routines/fire.rs`; modify `plugins/agent/src/engine.rs` (call before small-talk check), `engine::dispatch_and_observe` becomes `pub(crate)`, returns `Result<bool, String>` (already does).

**Interfaces:**
- Consumes: Task 1/2 API, `engine::dispatch_and_observe`, `engine::persist`, `check_quota`.
- Produces: `pub async fn try_fire(db: &Db, rpc: &Rpc, catalog: &Catalog, doc: &mut GoalDoc) -> Result<bool, String>` — `true` = goal completed by routine.

Behaviour (in order): load book → if `is_correction(goal)` and `correction_target` → disable (`user_correction`), save, return `false` → `find_active` → none: `false` → catalog lookup; missing / not eligible / hash mismatch → disable (`tool_changed`), save, `false` → quota check fails → `false` → dispatch with `doc.transcript = [user: goal]`; step detail gets `"routine": id` (patch the last step after dispatch) → ok: final = `next_reply`, `hits += 1`, `last_fire = Some`, status completed, step `final {"fast_path":"routine"}`, persist, save, `true` → error: `fails += 1`, disable (`tool_error`), save, return `false` (engine continues on the normal path with the error observation already in the transcript — the engine must therefore seed `opening_messages` *before* the existing transcript content: on `false` after a dispatch, engine prepends the seed turns).

- [ ] **Step 1:** Integration tests (`main.rs`, env `AGENT_PLUGIN_ROUTINES=on`, `AGENT_PLUGIN_ROUTINE_TOOLS=notify_send` set inside each test, both test goals non-small-talk):
  - `routine_fires_without_llm`: seed book with routine `{tool: notify_send, params: {"title":"свет"}, phrases: ["включи свет"], replies: ["Готово.", "Сделано."], spec_hash: <hash of the shim's merged notify_send spec — compute by calling `tools_list` first and hashing>}`; `goal_start "Включи свет!"` → `ai_requests().len() == 0`, `dispatched == [("notify_send", {"title":"свет"})]`, `final_answer ∈ replies`, book `hits == 1`.
  - `routine_with_changed_spec_is_disabled_and_goal_runs_normally`: seeded `spec_hash: "stale"`, one scripted AI reply → status completed via LLM, book routine `status == "disabled"`, `disabled_reason == "tool_changed"`.
  - `correction_disables_last_fired_routine`: fire as above, then `goal_start "не тот"` with a scripted AI reply → routine disabled with `user_correction`.
- [ ] **Step 2–4:** fail → implement → pass (full `cargo test -p agent-plugin`).
- [ ] **Step 5:** Commit `feat(agent): serve learned routines without the LLM`.

### Task 4: Mining

**Files:** Create `plugins/agent/src/routines/mine.rs`; modify `plugins/agent/src/store.rs` (add `Db::mining_rows(since_ms: i64) -> Result<Vec<MiningRow>, String>`: one server-side `db_query` projecting `goal`, `status`, `steps`, `updated_at_ms` — full docs via `db_batch_get` timed out on the live store; `MiningRow::from_row` accepts `steps` as JSON text or array and skips corrupt rows with a log); `llm.rs` (`first_balanced_object` → `pub(crate)`).

**Interfaces — Produces:**
- `pub struct Candidate { pub tool: String, pub params: Value, pub texts: Vec<String> /* normalized, unique, sorted */, pub successes: u32 }`
- `pub fn candidates(goals: &[MiningRow], catalog: &Catalog, allow: &[String], book: &Book) -> Vec<Candidate>` — pure. Rules: status completed; exactly one `tool_ok`, zero `tool_error`; eligible tool; group key `(tool, serde_json canonical string of params)`; `successes >= 2`; a normalized text present in two groups removed from both (group then dropped if it has no texts left); goals with `updated_at_ms <= disabled_at_ms` of a disabled routine with the same `(tool, params)` ignored; empty normalized text ignored.
- `pub struct Enrichment { pub paraphrases: Vec<String>, pub replies: Vec<String> }`
- `pub fn parse_enrichment(content: &str) -> Option<Enrichment>` — first balanced JSON object; replies trimmed, empties and > 160 chars dropped, max 6; `None` if < 1 reply left; paraphrases normalized, empties dropped, max 8.
- `pub fn merge(book: &mut Book, c: &Candidate, e: &Enrichment, spec_hash: &str, now_ms: i64) -> MergeOutcome` (`Created | Updated | Dropped`) — `Dropped` when no phrase survives; replies and paraphrases pass `routines/guard.rs` first (see spec §2 step 6), replies topped up to 3 from stock; phrases = observed texts ∪ paraphrases, minus any phrase owned by another routine; existing active routine for same `(tool, params)`: union phrases, replace replies, refresh `spec_hash`.
- `pub async fn run(rpc: &Rpc, db: &Db, catalog: &Catalog) -> Result<Value, String>` → `{"candidates": n, "created": n, "updated": n, "dropped": n, "skipped": n}`; `Err` when no mining agent configured. LLM call via `llm::chat` (no fallback) with plan `llm::fallback_plan(&base, &agent_id)` and `CallTag { purpose: "routine_mine", goal_id: "" }`; prompt = `persona_block()`-free instruction + tool name/description/schema/params + observed texts, asking for the JSON shape above in the user's language; replies must fit text and voice.

- [ ] **Step 1:** Unit tests in `mine.rs`:
  - two successful goals "включи свет", "Включи свет." same params → one candidate, `texts == ["включи свет"]`, `successes == 2`.
  - one success only → none; non-allowlisted tool → none; goal with an extra `tool_error` step → ignored.
  - "свет" → params A twice, "свет" → params B twice → no candidates (conflict).
  - `parse_enrichment` on fenced JSON works; on prose → `None`; 200-char reply dropped; 10 paraphrases → 8.
  - `merge` filters on/off-contradicting replies and paraphrases (guard); paraphrase owned by another routine is excluded.
- [ ] **Step 2:** Integration test `routine_mine_learns_from_history` (`main.rs`): env `AGENT_PLUGIN_ROUTINES_AGENT_ID=memory`; run two goals each scripted `{"tool":"notify_send","params":{"title":"свет"}}` then a final; script mining reply JSON; call action `routine_mine` → `created == 1`; book has routine with phrase from the goal; last AI request has `agent_id == "memory"` and `purpose == "routine_mine"`.
- [ ] **Step 3–4:** implement → pass.
- [ ] **Step 5:** Commit `feat(agent): mine routines from goal history on a local model`.

### Task 5: Actions, timer, manifest, docs

**Files:** `plugins/agent/src/request.rs` (`RoutineList`, `RoutineDelete { id }`, `RoutineMine`), `plugins/agent/src/lib.rs` (`handle_action` arms), `plugins/agent/src/main.rs` (`ACTIONS` + spawn timer after registration), `plugins/agent/plugin.json` (3 action specs: `routine_list` risk low, `routine_delete` risk medium, `routine_mine` risk low), `plugins/agent/Cargo.toml` (chrono), `plugins/agent/README.md` + `config.example.yaml` (env vars).

**Interfaces:**
- `routine_list {}` → `{"total": n, "routines": [Routine]}`
- `routine_delete {"id"}` → `{"deleted": bool}`
- `routine_mine {}` → Task 4 `run` output.
- `pub fn next_run_delay(now: chrono::DateTime<Local>, at: (u32, u32)) -> Duration` in `routines/mod.rs` + `parse_at(&str) -> Option<(u32,u32)>`; timer loop: sleep delay → `mine::run` (log result/error) → repeat. Spawned only when `routines::enabled()`.

- [ ] **Step 1:** Tests: `parse_at("03:30") == Some((3,30))`, `parse_at("25:00") == None`; `next_run_delay` at 02:00 → 1h30m, at 04:00 → 23h30m; request parsing for the three actions; existing manifest-specs test (every action documented in `plugin.json`) passes.
- [ ] **Step 2–4:** implement → pass; `cargo clippy --all-targets` clean.
- [ ] **Step 5:** Commit `feat(agent): routine actions and nightly mining timer`.

### Task 6: Deploy and verify live

- [ ] Release build, back up and install `agent` binary, add `AGENT_PLUGIN_ROUTINES=on` to `agent.yaml`, `systemctl --user restart vyn.service`.
- [ ] `vyn-act routine_mine '{}'` → JSON counts (requires Ollama models reachable).
- [ ] `vyn-act routine_list '{}'` → inspect; if a routine exists, `goal_start` with its phrase → `steps` contain `fast_path: routine`, no new `usage` row.
