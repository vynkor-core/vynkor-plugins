# Agent Prompt & Catalog Architecture — Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Put every layer of the agent's prompt and tool catalog under its correct owner — plugins own tool descriptions, operators own persona and facts — so adding a plugin no longer requires hand-editing a parallel catalog that silently drifts.

**Architecture:** Two independent tracks landing in one repo. Track A (Tasks 1–3) moves the agent's prompt layers out of a single unmaintainable env string and out of Rust `const`s into a `prompts.d/`-style directory with explicit precedence. Track B (Tasks 4–6) revives the already-designed-but-dead kernel-manifest catalog layer by making plugins actually ship `action_specs`, then migrates schemas into `plugin.json` so the hand-written operator catalog can shrink. Task 7 writes the resulting rules down as the standard for future plugins.

**Tech Stack:** Rust 2021 (agent + telegram plugins, `vynkor-sdk` 0.0.3 from crates.io), `serde_json`, `tempfile` (agent dev-dep), Python 3 for repo scripts, GitHub Actions for CI.

**Spec:** This document's [Background & Findings](#background--findings) section. There is no separate spec doc; the findings below were established by direct code inspection during the 2026-09-17 session and every task argues from them.

## Global Constraints

- **No changes to `vynkor-wire` or `vynkor-sdk` in this plan.** Plugins depend on published crates (`vynkor-sdk = "0.0.3"`, `vynkor-wire = "0.0.3"`), not path deps. Everything here must work against those published versions. `vynkor_sdk::proto::ActionSpec` already carries `name` / `description` / `params_schema` / `risk` / `requires_confirmation` — no new field is needed.
- **No kernel (`vynkor`) changes.** `kernel/commands.rs:94-106` already serves all five `ActionSpec` fields.
- **No `agent` plugin changes for Track B.** `plugins/agent/src/discovery.rs:44-73` already consumes `description` / `params_schema` / `risk` / `requires_confirmation`. Track B is plugin-side only.
- **Rust edition 2021**, `rust-version` 1.85 floor (inherited from `vynkor-sdk`).
- **There is no Cargo workspace.** Each plugin under `plugins/*/` is a standalone crate with its own `Cargo.toml`, and the repo root has none — so `cargo test -p <crate>` from the root fails with "could not find `Cargo.toml`". Every cargo command in this plan uses `--manifest-path plugins/<name>/Cargo.toml`, matching how CI does it (`.github/workflows/ci.yml`, the "Run cargo test for each plugin" step). All commands in this plan, cargo and otherwise, are written to run from the repo root.
- **Telegram's test suite must stay green without a live Telegram account**: `cargo test --manifest-path plugins/telegram/Cargo.toml` (49 tests today). Never add a test that needs MTProto or a running kernel.
- **New env-reading code must be testable without mutating process env.** Existing tests in `plugins/agent/src/llm.rs:990-1014` call `std::env::set_var` directly with no lock; the repo has already been bitten by cross-module env races (see commit `244de69`, "note cross-module ENV_LOCK race, use `--test-threads=1`"). Every new helper in this plan therefore splits into a pure `*_at(...)` function that takes its inputs as parameters (tested) and a thin env-reading wrapper (untested). Do not add new `set_var` tests.
- **Backward compatibility is mandatory at every step.** `AGENT_PLUGIN_PLUGIN_PROMPTS` keeps working unchanged and keeps winning; a missing prompts directory, a missing `plugin.json`, or a malformed file must always degrade to today's behavior, never blank a prompt or block registration.
- **Security boundary is untouched.** `AGENT_PLUGIN_ALLOWED_ACTIONS` remains the only thing that decides what may be dispatched. Nothing in this plan widens it.

---

## Background & Findings

Read this before starting. Every task depends on it.

### The prompt is assembled in six layers

`plugins/agent/src/llm.rs:364-522` (`opening_messages_with_full`) builds the instruction message in this order:

| # | Layer | Source today | Owner it *should* have |
|---|-------|--------------|------------------------|
| 1 | `IDENTITY` | `const`, `llm.rs:431` | operator (config) |
| 2 | `PERSONALITY` | `const`, `llm.rs:432` | operator (config) |
| 3 | `CHANNEL` | `const`, `llm.rs:433` | operator (config) |
| 4 | groups overview (names only) | auto-generated, `llm.rs:275-315` | ✅ already correct |
| 5 | plugin prompts (per group) | `AGENT_PLUGIN_PLUGIN_PROMPTS` env JSON, `llm.rs:317-358` | operator, but needs a sane file format |
| 6 | detailed tool JSON | `agent-tools.json` + kernel manifests + minimal, `tools.rs` | plugin author |

Layers 1–3 require a **recompile** to change. Layer 5 lives as a single-line JSON blob inside a single-quoted YAML scalar — no newlines, escaping hell, unreviewable diffs.

### Group keys are derived from tool-name prefixes

`llm.rs:325-342` maps the first `_`-delimited segment of a tool name to a group: `tg_*` → `telegram`, `contact_*` → `contacts`, `db_*` → `database`, `vec_*` → `vector-db`, `note_*` → `notes`, `email_*` → `email`, `event_*`/`schedule_*` → `calendar/scheduler`, `fs_*` → `filesystem`, `media_*` → `media`, `sys_*` → `system`, `web_*`/`http_*` → `web/network`, `secret_*` → `secrets`, `notify_*` → `notifications`, `tts_*`/`stt_*`/`mic_*`/`sound_*`/`daemon_*` → `audio/voice`, `launch_*`/`clipboard_*`/`hotkey_*` → `desktop`, anything else → the bare prefix.

**Three group names contain a `/`** (`audio/voice`, `calendar/scheduler`, `web/network`). Any file-per-group scheme must flatten that character.

### Layer 5 replaces, never merges

`llm.rs:347`: `env_map.get(&g).cloned().or_else(|| default_plugin_prompt(&g))`. Because `AGENT_PLUGIN_PLUGIN_PROMPTS` defines `telegram`, the built-in default at `llm.rs:265-267` is dead code in production. There is no way to *extend* a default — only to replace it.

### The root cause: the kernel-manifest catalog layer is dead

`plugins/agent/README.md:121-145` documents a three-layer catalog merged per action name:

1. operator tools file (`AGENT_PLUGIN_TOOLS_FILE`) — wins
2. **kernel manifests** — "This is the default path; no file is needed"
3. minimal spec — name only

Layer 2 never fires, because **no plugin populates `action_specs`**. Of 38 plugins, only `agent` even mentions the field, and only as a consumer. Telegram registers like this (`plugins/telegram/src/main.rs:46-57`):

```rust
fn manifest() -> vynkor_sdk::proto::PluginManifest {
    vynkor_sdk::proto::PluginManifest {
        permissions: vec![/* … */],
        actions: ACTIONS.iter().map(|s| s.to_string()).collect(),
        ..Default::default()   // ← action_specs stays empty
    }
}
```

So `get_manifest` returns an empty `action_specs` array, layer 2 contributes nothing, and every tool the model sees must be hand-written into `agent-tools.json` — a file with no link to the plugins it describes. That is exactly how eight catalog entries came to reference telegram actions that do not exist.

`src/confirmation_gate.rs:38-39` shows the intended idiom (`let (actions, action_specs) = gate.manifest_entries();` merged into the manifest) — the capability was always there, plugins just never used it.

### The descriptions already exist and are being thrown away

`plugins/telegram/plugin.json` declares **29 actions, all 29 with a `description`** — and **0 with an `input` schema**. None of those descriptions reach the model, because the on-disk manifest is never forwarded as `action_specs`.

Repo-wide: 236 actions declared across `plugins/*/plugin.json`, only 31 with a top-level `description`.

Meanwhile `discovery.rs:22-42` carries a workaround — `description_from_parameters()` stitches a pseudo-description by joining per-parameter docs — whose doc comment explains the stakes: without *some* description, a tool embeds as `"name — "` and `AGENT_PLUGIN_EMBEDDING_FILTER=on` (which is set in production) reliably drops it from the catalog for any goal that does not literally contain the tool's name.

### Consequence for Task ordering

Because telegram's `plugin.json` has descriptions but no schemas, Task 4 alone does **not** let you delete `tg_*` entries from `agent-tools.json` — the merge is per-action-name and whole-record, so the file layer still wins outright for those names. Task 4's immediate payoff is for every action *not* in the operator file (the other ~200). Task 5 closes the gap by migrating the good schemas from the operator file into `plugin.json`, after which operator entries become deletable.

### Explicitly out of scope for this plan

**Plugin-supplied behavior prompts** (a plugin shipping its own `agent_prompt` that lands in the system prompt) are deliberately *not* in this plan. Once a plugin can inject text into the system prompt, a compromised or hostile plugin can instruct the agent to misuse *other* plugins' tools — for instance to read a secret via `secret_get` and exfiltrate it via `http_request`. That is a materially larger blast radius than merely exposing an action, and it needs its own design: a per-plugin opt-in allowlist mirroring `ALLOWED_ACTIONS`' default-deny, a length cap, and clear source-attributed fencing around the injected text. Track A gives operators the file-based layering they need today; the plugin-supplied variant comes after a dedicated security design. Task 7 records this decision so the next person does not casually implement it.

---

## File Structure

**Track A — agent prompt layers** (all in `plugins/agent/`)

- Modify `src/llm.rs` — add the prompts-directory loader, core-block overrides, and facts block. All three are small, cohesive additions to the module that already owns prompt assembly; splitting them into a new module would separate them from the `const`s and the `format!` they feed.
- Modify `README.md` — document the new env vars in the existing Configuration section.

**Track B — catalog ownership** (in `plugins/telegram/` and `scripts/`)

- Create `plugins/telegram/src/manifest.rs` — converts the on-disk `plugin.json` into wire `ActionSpec`s. New file because it is self-contained, has no dependency on MTProto, is unit-testable without a kernel, and is written to be lifted into `vynkor-sdk` verbatim when a release cycle next happens.
- Modify `plugins/telegram/src/lib.rs:5-6` — declare the new module.
- Modify `plugins/telegram/src/main.rs:46-57` — populate `action_specs`.
- Modify `plugins/telegram/plugin.json` — gains `input` schemas (Task 5).
- Create `scripts/schemas-into-manifest.py` — one-off migration from an operator tools file into `plugin.json`.
- Create `scripts/check-action-docs.py` — CI guard that every declared action is documented.
- Modify `.github/workflows/ci.yml` — wire the guard into the existing `lint` job.

**Documentation**

- Modify `docs/PLUGIN_AUTHORING.md` — the standard for plugin authors.

Track A and Track B touch disjoint files and can be executed in either order or in parallel.

---

### Task 1: Per-group prompt files (`prompts.d/`)

Replaces the unmaintainable single-line JSON env blob with one Markdown file per group, while keeping the env var working and winning.

**Files:**
- Modify: `plugins/agent/src/llm.rs` (add helpers near `llm.rs:239-358`; wire into `plugin_prompt_section` at `llm.rs:347`)
- Test: `plugins/agent/src/llm.rs` (inline `#[cfg(test)] mod tests`, which starts at `llm.rs:754`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces, used by Tasks 2 and 3:
  - `const PROMPTS_DIR_ENV: &str = "AGENT_PLUGIN_PROMPTS_DIR";`
  - `fn prompts_dir() -> Option<std::path::PathBuf>`
  - `fn prompt_file_at(dir: &std::path::Path, stem: &str) -> Option<String>`
  - `fn group_file_stem(group: &str) -> String`

- [x] **Step 1: Write the failing tests**

Add to the existing `#[cfg(test)] mod tests` block in `plugins/agent/src/llm.rs`:

```rust
    #[test]
    fn group_file_stem_flattens_slash_bearing_groups() {
        assert_eq!(group_file_stem("telegram"), "telegram");
        assert_eq!(group_file_stem("audio/voice"), "audio-voice");
        assert_eq!(group_file_stem("calendar/scheduler"), "calendar-scheduler");
        assert_eq!(group_file_stem("web/network"), "web-network");
    }

    #[test]
    fn prompt_file_at_reads_and_trims_contents() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("telegram.md"), "\n  be terse  \n").unwrap();
        assert_eq!(prompt_file_at(dir.path(), "telegram").as_deref(), Some("be terse"));
    }

    #[test]
    fn prompt_file_at_treats_missing_and_blank_files_as_unset() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("blank.md"), "   \n\t\n").unwrap();
        assert!(prompt_file_at(dir.path(), "absent").is_none());
        assert!(prompt_file_at(dir.path(), "blank").is_none());
    }
```

- [x] **Step 2: Run the tests to verify they fail**

Run, as two invocations (cargo takes one positional filter):

```bash
cargo test --manifest-path plugins/agent/Cargo.toml group_file_stem
cargo test --manifest-path plugins/agent/Cargo.toml prompt_file_at
```

Expected: FAIL — `cannot find function 'group_file_stem' in this scope` and `cannot find function 'prompt_file_at' in this scope`.

- [x] **Step 3: Write the implementation**

Add `use std::path::{Path, PathBuf};` to the imports at the top of `plugins/agent/src/llm.rs` (after `use serde_json::{json, Value};` at `llm.rs:15`).

Insert directly after `plugin_prompts_map()` ends at `llm.rs:261`:

```rust
/// Operator env var: directory of per-layer prompt files. A group's prompt
/// lives at `<dir>/<group>.md`; the reserved `_`-prefixed stems carry the
/// core persona (`_identity`, `_personality`, `_channel`) and the facts
/// block (`_facts`).
///
/// This exists because `AGENT_PLUGIN_PLUGIN_PROMPTS` is a single-line JSON
/// object embedded in a YAML scalar: no newlines, heavy escaping, and a
/// diff nobody can review. The env var still wins so an operator can hot-fix
/// a prompt without touching the filesystem.
pub const PROMPTS_DIR_ENV: &str = "AGENT_PLUGIN_PROMPTS_DIR";

/// Three group keys contain a `/` (`audio/voice`, `calendar/scheduler`,
/// `web/network` — see the prefix map in `plugin_prompt_section`), which
/// cannot appear in a filename. Flatten to `-`.
fn group_file_stem(group: &str) -> String {
    group.replace('/', "-")
}

/// Read `<dir>/<stem>.md`. A missing file, an unreadable file, and a
/// whitespace-only file all read as "not configured", so a half-populated
/// directory degrades to the next layer instead of blanking the prompt.
fn prompt_file_at(dir: &Path, stem: &str) -> Option<String> {
    let raw = std::fs::read_to_string(dir.join(format!("{stem}.md"))).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn prompts_dir() -> Option<PathBuf> {
    let raw = std::env::var(PROMPTS_DIR_ENV).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

fn prompt_from_dir(group: &str) -> Option<String> {
    prompt_file_at(&prompts_dir()?, &group_file_stem(group))
}
```

- [x] **Step 4: Run the tests to verify they pass**

Run, as two invocations (cargo takes one positional filter):

```bash
cargo test --manifest-path plugins/agent/Cargo.toml group_file_stem
cargo test --manifest-path plugins/agent/Cargo.toml prompt_file_at
```

Expected: PASS, 3 tests.

- [x] **Step 5: Wire the directory into the group-prompt precedence chain**

In `plugins/agent/src/llm.rs`, replace the single line at `llm.rs:347`:

```rust
        if let Some(prompt) = env_map.get(&g).cloned().or_else(|| default_plugin_prompt(&g)) {
```

with:

```rust
        // Precedence: env JSON (hot-fix, wins) > prompts dir (the normal
        // place) > built-in default. Each layer replaces rather than
        // appends — see this plan's follow-on notes for merge semantics.
        let resolved = env_map
            .get(&g)
            .cloned()
            .or_else(|| prompt_from_dir(&g))
            .or_else(|| default_plugin_prompt(&g));
        if let Some(prompt) = resolved {
```

- [x] **Step 6: Run the full agent suite to verify nothing regressed**

Run: `cargo test --manifest-path plugins/agent/Cargo.toml`

Expected: PASS, all tests. Note that `plugin_prompt_section` early-returns `None` under `cfg!(test)` (`llm.rs:318-320`), so no existing test exercises the chain you just edited — the new unit tests on `prompt_file_at` are the coverage.

- [x] **Step 7: Commit**

```bash
git add plugins/agent/src/llm.rs
git commit -m "feat(agent): load per-group prompts from AGENT_PLUGIN_PROMPTS_DIR

Group prompts can live as <dir>/<group>.md instead of a single-line JSON
blob inside a YAML scalar. AGENT_PLUGIN_PLUGIN_PROMPTS still wins so
operators keep a hot-fix path."
```

---

### Task 2: Core persona blocks overridable from the prompts directory

Moves `IDENTITY` / `PERSONALITY` / `CHANNEL` out of compile-time `const`s. Changing the agent's tone currently requires rebuilding the binary.

**Files:**
- Modify: `plugins/agent/src/llm.rs` (helper next to Task 1's; call site at `llm.rs:489`)
- Test: `plugins/agent/src/llm.rs` (inline tests)

**Interfaces:**
- Consumes: `prompt_file_at`, `prompts_dir` from Task 1.
- Produces, used by Task 3's call-site edit: `fn core_block_at(dir: Option<&std::path::Path>, stem: &str, fallback: &str) -> String` and its wrapper `fn core_block(stem: &str, fallback: &str) -> String`.

- [x] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `plugins/agent/src/llm.rs`:

```rust
    #[test]
    fn core_block_prefers_file_over_builtin_const() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("_identity.md"), "custom identity").unwrap();
        assert_eq!(
            core_block_at(Some(dir.path()), "_identity", "builtin identity"),
            "custom identity"
        );
    }

    #[test]
    fn core_block_falls_back_when_dir_unset_or_file_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(core_block_at(None, "_identity", "builtin identity"), "builtin identity");
        assert_eq!(
            core_block_at(Some(dir.path()), "_identity", "builtin identity"),
            "builtin identity"
        );
    }

    #[test]
    fn core_block_falls_back_on_blank_file_rather_than_blanking_persona() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("_channel.md"), "  \n ").unwrap();
        assert_eq!(core_block_at(Some(dir.path()), "_channel", "builtin channel"), "builtin channel");
    }
```

- [x] **Step 2: Run the tests to verify they fail**

Run: `cargo test --manifest-path plugins/agent/Cargo.toml core_block`

Expected: FAIL — `cannot find function 'core_block_at' in this scope`.

- [x] **Step 3: Write the implementation**

Insert after `prompt_from_dir` (added in Task 1) in `plugins/agent/src/llm.rs`:

```rust
/// Resolve one core persona block: the prompts directory overrides the
/// compile-time default. Taking `dir` as a parameter keeps this testable
/// without mutating process env — the repo has an open cross-module env
/// race in tests (see commit 244de69).
fn core_block_at(dir: Option<&Path>, stem: &str, fallback: &str) -> String {
    dir.and_then(|d| prompt_file_at(d, stem)).unwrap_or_else(|| fallback.to_string())
}

fn core_block(stem: &str, fallback: &str) -> String {
    core_block_at(prompts_dir().as_deref(), stem, fallback)
}
```

- [x] **Step 4: Run the tests to verify they pass**

Run: `cargo test --manifest-path plugins/agent/Cargo.toml core_block`

Expected: PASS, 3 tests.

- [x] **Step 5: Wire the blocks into the call site**

In `plugins/agent/src/llm.rs`, replace the line at `llm.rs:489`:

```rust
    let identity_block = format!("{IDENTITY}\n\n{PERSONALITY}\n\n{CHANNEL}");
```

with:

```rust
    let identity_block = format!(
        "{}\n\n{}\n\n{}",
        core_block("_identity", IDENTITY),
        core_block("_personality", PERSONALITY),
        core_block("_channel", CHANNEL),
    );
```

Leave the three `const` declarations at `llm.rs:486-488` in place — they are now the documented fallback, not dead code.

- [x] **Step 6: Run the full agent suite**

Run: `cargo test --manifest-path plugins/agent/Cargo.toml`

Expected: PASS, all tests.

- [x] **Step 7: Commit**

```bash
git add plugins/agent/src/llm.rs
git commit -m "feat(agent): allow prompts dir to override identity/personality/channel

Changing the agent's tone no longer needs a rebuild. The consts stay as
the fallback when no file is present."
```

---

### Task 3: Facts block as its own layer

Hard data (owner id, account names, who-is-who) currently lives inline in the prose of the `telegram` group prompt, so it cannot be reused by `notify` or `email`, and updating a chat id means editing a paragraph.

**Files:**
- Modify: `plugins/agent/src/llm.rs` (helper + call site at `llm.rs:489`)
- Test: `plugins/agent/src/llm.rs` (inline tests)

**Interfaces:**
- Consumes: `prompt_file_at`, `prompts_dir` from Task 1; the `identity_block` binding edited in Task 2.
- Produces: `const FACTS_ENV: &str = "AGENT_PLUGIN_FACTS";`, `fn facts_block_at(dir: Option<&std::path::Path>, literal: Option<&str>) -> Option<String>`.

- [x] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `plugins/agent/src/llm.rs`:

```rust
    #[test]
    fn facts_literal_env_wins_over_facts_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("_facts.md"), "from file").unwrap();
        assert_eq!(
            facts_block_at(Some(dir.path()), Some("from env")).as_deref(),
            Some("from env")
        );
    }

    #[test]
    fn blank_facts_env_falls_through_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("_facts.md"), "from file").unwrap();
        assert_eq!(facts_block_at(Some(dir.path()), Some("   ")).as_deref(), Some("from file"));
        assert_eq!(facts_block_at(Some(dir.path()), None).as_deref(), Some("from file"));
    }

    #[test]
    fn facts_absent_everywhere_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(facts_block_at(None, None).is_none());
        assert!(facts_block_at(Some(dir.path()), None).is_none());
    }
```

- [x] **Step 2: Run the tests to verify they fail**

Run: `cargo test --manifest-path plugins/agent/Cargo.toml facts`

Expected: FAIL — `cannot find function 'facts_block_at' in this scope`.

- [x] **Step 3: Write the implementation**

Insert after `core_block` (added in Task 2) in `plugins/agent/src/llm.rs`:

```rust
/// Operator env var: literal facts text. Overrides `<prompts_dir>/_facts.md`
/// so a fact can be corrected without filesystem access.
pub const FACTS_ENV: &str = "AGENT_PLUGIN_FACTS";

/// Authoritative data about the user and environment — ids, account names,
/// who-is-who — kept out of behavioral prose so it can be corrected in one
/// line and reused by every group rather than duplicated per group prompt.
fn facts_block_at(dir: Option<&Path>, literal: Option<&str>) -> Option<String> {
    if let Some(text) = literal {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    dir.and_then(|d| prompt_file_at(d, "_facts"))
}

fn facts_block() -> Option<String> {
    let literal = std::env::var(FACTS_ENV).ok();
    facts_block_at(prompts_dir().as_deref(), literal.as_deref())
}
```

- [x] **Step 4: Run the tests to verify they pass**

Run: `cargo test --manifest-path plugins/agent/Cargo.toml facts`

Expected: PASS, 3 tests.

- [x] **Step 5: Wire the facts block into the instructions**

In `plugins/agent/src/llm.rs`, replace the `identity_block` binding you produced in Task 2 with:

```rust
    // Appended to the persona rather than threaded through both format!
    // arms below: the facts are context for every group, not a group of
    // their own.
    let facts_section = facts_block()
        .map(|facts| {
            format!(
                "\n\nFacts (authoritative data about the user and environment — \
                 prefer these over inference, and never contradict them):\n{facts}"
            )
        })
        .unwrap_or_default();
    let identity_block = format!(
        "{}\n\n{}\n\n{}{}",
        core_block("_identity", IDENTITY),
        core_block("_personality", PERSONALITY),
        core_block("_channel", CHANNEL),
        facts_section,
    );
```

- [ ] **Step 6: Run the full agent suite**

Run: `cargo test --manifest-path plugins/agent/Cargo.toml`

Expected: PASS, all tests.

- [x] **Step 7: Document the new env vars**

In `plugins/agent/README.md`, find the `## Configuration` section (around `README.md:147`) and add these three rows/entries in the same style the surrounding entries use:

```markdown
- `AGENT_PLUGIN_PROMPTS_DIR` — directory of prompt files. Group prompts at
  `<dir>/<group>.md` (`/` in a group name becomes `-`, so `audio/voice` →
  `audio-voice.md`). Reserved stems: `_identity.md`, `_personality.md`,
  `_channel.md` override the built-in persona constants; `_facts.md`
  supplies the facts block. A missing or blank file falls through to the
  next layer, never to an empty prompt.
- `AGENT_PLUGIN_FACTS` — literal facts text; overrides `_facts.md`.
- Group-prompt precedence: `AGENT_PLUGIN_PLUGIN_PROMPTS` (wins) >
  `<prompts dir>/<group>.md` > built-in default. Each layer replaces the
  next; there is no append.
```

- [x] **Step 8: Commit**

```bash
git add plugins/agent/src/llm.rs plugins/agent/README.md
git commit -m "feat(agent): add a facts layer separate from behavioral prompts

Ids, account names and who-is-who move out of per-group prose into
<prompts_dir>/_facts.md (or AGENT_PLUGIN_FACTS), so they are corrected in
one place and shared across every group."
```

---

### Task 4: Telegram ships `action_specs` from its own `plugin.json`

The root-cause fix. Telegram already documents all 29 of its actions; none of those descriptions currently reach the model.

**Files:**
- Create: `plugins/telegram/src/manifest.rs`
- Modify: `plugins/telegram/src/lib.rs:5-6` (module declaration)
- Modify: `plugins/telegram/src/main.rs:46-57` (`manifest()`)
- Test: `plugins/telegram/src/manifest.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing from earlier tasks. `vynkor_sdk::proto::{ActionRisk, ActionSpec}` come from the published SDK.
- Produces, used by Task 6's CI guard (same field names) and by Task 5 (which fills the `input` field this reads):
  - `pub fn specs_from_manifest(manifest: &serde_json::Value) -> Vec<vynkor_sdk::proto::ActionSpec>`
  - `pub fn action_specs() -> Vec<vynkor_sdk::proto::ActionSpec>`

- [x] **Step 1: Write the failing tests**

Create `plugins/telegram/src/manifest.rs` containing only the test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spec_carries_description_schema_risk_and_confirmation() {
        let manifest = json!({
            "actions": [{
                "name": "tg_send_message",
                "description": "Send a message to a peer",
                "input": {"type": "object"},
                "risk": "medium",
                "requires_confirmation": true
            }]
        });
        let specs = specs_from_manifest(&manifest);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "tg_send_message");
        assert_eq!(specs[0].description, "Send a message to a peer");
        assert_eq!(specs[0].params_schema, r#"{"type":"object"}"#);
        assert_eq!(specs[0].risk, ActionRisk::Medium as i32);
        assert!(specs[0].requires_confirmation);
    }

    #[test]
    fn nameless_entries_are_skipped_and_optional_fields_default() {
        let manifest = json!({"actions": [{"description": "orphan"}, {"name": "tg_ping"}]});
        let specs = specs_from_manifest(&manifest);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "tg_ping");
        assert_eq!(specs[0].description, "");
        assert_eq!(specs[0].params_schema, "");
        assert_eq!(specs[0].risk, ActionRisk::Unknown as i32);
        assert!(!specs[0].requires_confirmation);
    }

    #[test]
    fn unknown_risk_strings_degrade_to_unknown() {
        let manifest = json!({"actions": [{"name": "a", "risk": "SPICY"}]});
        assert_eq!(specs_from_manifest(&manifest)[0].risk, ActionRisk::Unknown as i32);
    }

    #[test]
    fn risk_parsing_is_case_insensitive() {
        let manifest = json!({"actions": [
            {"name": "a", "risk": "LOW"},
            {"name": "b", "risk": " Critical "}
        ]});
        let specs = specs_from_manifest(&manifest);
        assert_eq!(specs[0].risk, ActionRisk::Low as i32);
        assert_eq!(specs[1].risk, ActionRisk::Critical as i32);
    }

    #[test]
    fn manifest_without_actions_yields_no_specs() {
        assert!(specs_from_manifest(&json!({})).is_empty());
        assert!(specs_from_manifest(&json!({"actions": "not an array"})).is_empty());
    }

    // Regression guard for the defect this module exists to fix: an action
    // that reaches the model with an empty description gets dropped by the
    // agent's embedding filter (see the agent plugin's
    // discovery.rs::description_from_parameters doc comment).
    #[test]
    fn shipped_manifest_documents_every_declared_action() {
        let parsed: Value = serde_json::from_str(include_str!("../plugin.json")).unwrap();
        let declared = parsed["actions"].as_array().unwrap().len();
        let specs = specs_from_manifest(&parsed);
        assert_eq!(specs.len(), declared, "every declared action must produce a spec");
        let undocumented: Vec<&str> = specs
            .iter()
            .filter(|s| s.description.is_empty())
            .map(|s| s.name.as_str())
            .collect();
        assert!(undocumented.is_empty(), "actions missing a description: {undocumented:?}");
    }
}
```

- [x] **Step 2: Declare the module so the tests compile**

In `plugins/telegram/src/lib.rs`, add alongside the existing declarations at lines 5-6:

```rust
pub mod events;
pub mod manifest;
pub mod mtproto;
```

- [x] **Step 3: Run the tests to verify they fail**

Run: `cargo test --manifest-path plugins/telegram/Cargo.toml manifest::`

Expected: FAIL to compile — `cannot find function 'specs_from_manifest' in this scope`, `cannot find type 'ActionRisk' in this scope`.

- [x] **Step 4: Write the implementation**

Prepend to `plugins/telegram/src/manifest.rs`, above the test module:

```rust
//! Ship the on-disk `plugin.json` action docs to the kernel as
//! `action_specs` on registration.
//!
//! Without this the kernel's `get_manifest` returns an empty `action_specs`
//! array, which silently disables the agent plugin's kernel-manifest
//! catalog layer (`plugins/agent/README.md`, layer 2) and forces every tool
//! description into a hand-maintained operator file that has no link back
//! to this plugin — the exact source of the catalog drift that shipped
//! eight references to actions this plugin never had.
//!
//! Deliberately free of MTProto and kernel dependencies so it unit-tests
//! offline, and written to be lifted into `vynkor-sdk` unchanged when a
//! release cycle next allows it.

use serde_json::Value;
use vynkor_sdk::proto::{ActionRisk, ActionSpec};

fn risk_from_str(raw: &str) -> ActionRisk {
    match raw.trim().to_ascii_lowercase().as_str() {
        "low" => ActionRisk::Low,
        "medium" => ActionRisk::Medium,
        "high" => ActionRisk::High,
        "critical" => ActionRisk::Critical,
        _ => ActionRisk::Unknown,
    }
}

/// Convert a parsed `plugin.json` into wire `ActionSpec`s. Entries without a
/// `name` are skipped; every other field is optional and degrades to the
/// proto default, so an under-documented manifest still registers.
pub fn specs_from_manifest(manifest: &Value) -> Vec<ActionSpec> {
    let Some(actions) = manifest.get("actions").and_then(Value::as_array) else {
        return Vec::new();
    };
    actions
        .iter()
        .filter_map(|action| {
            let name = action.get("name").and_then(Value::as_str)?;
            Some(ActionSpec {
                name: name.to_string(),
                description: action
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                // The kernel carries params_schema as a JSON-encoded string
                // and does not parse it (see the proto's D-01 note).
                params_schema: action.get("input").map(Value::to_string).unwrap_or_default(),
                risk: risk_from_str(action.get("risk").and_then(Value::as_str).unwrap_or_default())
                    as i32,
                requires_confirmation: action
                    .get("requires_confirmation")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}

/// Load `plugin.json` from the installed plugin directory (it sits next to
/// this binary — the manifest's own `files` list installs it there). Any
/// failure degrades to an empty spec list: registration must never depend
/// on the doc file being readable.
pub fn action_specs() -> Vec<ActionSpec> {
    let Some(path) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("plugin.json")))
    else {
        tracing::warn!("current_exe unavailable; registering without action_specs");
        return Vec::new();
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) => {
            tracing::warn!(
                path = %path.display(), %err,
                "plugin.json unreadable; registering without action_specs"
            );
            return Vec::new();
        }
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(parsed) => specs_from_manifest(&parsed),
        Err(err) => {
            tracing::warn!(
                path = %path.display(), %err,
                "plugin.json malformed; registering without action_specs"
            );
            Vec::new()
        }
    }
}
```

- [x] **Step 5: Run the tests to verify they pass**

Run: `cargo test --manifest-path plugins/telegram/Cargo.toml manifest::`

Expected: PASS, 6 tests. If `shipped_manifest_documents_every_declared_action` fails, an action in `plugins/telegram/plugin.json` is missing its `description` — add it rather than weakening the test.

- [x] **Step 6: Populate `action_specs` at registration**

In `plugins/telegram/src/main.rs`, replace `manifest()` at lines 46-57:

```rust
fn manifest() -> vynkor_sdk::proto::PluginManifest {
    vynkor_sdk::proto::PluginManifest {
        permissions: vec![
            "PERMISSION_NETWORK".into(),
            "PERMISSION_STORAGE".into(),
            "PERMISSION_SECRETS".into(),
            "PERMISSION_EVENT_PUBLISH".into(),
        ],
        actions: ACTIONS.iter().map(|s| s.to_string()).collect(),
        action_specs: telegram_plugin::manifest::action_specs(),
        ..Default::default()
    }
}
```

- [x] **Step 7: Run the full telegram suite**

Run: `cargo test --manifest-path plugins/telegram/Cargo.toml`

Expected: PASS, 55 tests (49 existing + 6 new).

- [ ] **Step 8: Verify against the live kernel** — deferred to operator

Deploy per `plugins/telegram/CLAUDE.md`:

```bash
cargo build --manifest-path plugins/telegram/Cargo.toml --release
/home/behzod/.local/bin/vyn stop -c ~/.config/vyn/config.yaml
\cp -f target/release/telegram ~/.local/lib/vyn/plugins/telegram/telegram
\cp -f plugins/telegram/plugin.json ~/.local/lib/vyn/plugins/telegram/plugin.json
/home/behzod/.local/bin/vyn start -c ~/.config/vyn/config.yaml
```

Then confirm the descriptions now travel. Ask the agent plugin for its effective catalog via its `tools_list` action and check that `tg_*` entries report a non-empty `description`. Expected: every `tg_*` action has a description, and entries not present in `~/.config/vyn/agent-tools.json` now report `source: "kernel"` instead of `source: "minimal"`.

Note what this does *not* yet do: `tg_*` names that appear in `agent-tools.json` still report `source: "file"`, because the merge is per-action-name and whole-record. Task 5 fixes that.

- [x] **Step 9: Commit**

```bash
git add plugins/telegram/src/manifest.rs plugins/telegram/src/lib.rs plugins/telegram/src/main.rs
git commit -m "feat(telegram): register action_specs from plugin.json

The plugin documents all 29 actions but shipped none of it: action_specs
was left at Default::default(), so the kernel served an empty array and
the agent's kernel-manifest catalog layer contributed nothing. Descriptions
now reach the model from the plugin that owns them."
```

---

### Task 5: Migrate operator-file schemas into `plugin.json`

After Task 4, telegram ships descriptions but no schemas — `plugin.json` has zero `input` blocks, while `agent-tools.json` has good hand-written ones. Move them to the owner so the operator file can shrink.

**Files:**
- Create: `scripts/schemas-into-manifest.py`
- Modify: `plugins/telegram/plugin.json` (generated by the script, reviewed by hand)
- Test: `scripts/schemas-into-manifest.py` self-check via `--dry-run`, plus Task 4's existing `shipped_manifest_documents_every_declared_action`

**Interfaces:**
- Consumes: `specs_from_manifest` from Task 4 reads the `input` key this task fills.
- Produces: nothing consumed by later tasks in code; Task 6's guard validates the result.

- [x] **Step 1: Write the migration script**

Create `scripts/schemas-into-manifest.py`:

```python
#!/usr/bin/env python3
"""Copy JSON-Schema params from an agent tools file into a plugin manifest.

The agent plugin's operator tools file (AGENT_PLUGIN_TOOLS_FILE) holds
hand-written `parameters` schemas that duplicate what the owning plugin
should declare. This lifts them into the plugin's own plugin.json as
`input`, so the kernel-manifest catalog layer becomes complete and the
operator file entry can be deleted.

Only actions the manifest already declares are touched, and an existing
`input` is never overwritten — the plugin stays the authority.

    python3 scripts/schemas-into-manifest.py \\
        --tools-file ~/.config/vyn/agent-tools.json \\
        --manifest plugins/telegram/plugin.json --dry-run
"""
import argparse
import json
import sys


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tools-file", required=True)
    ap.add_argument("--manifest", required=True)
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()

    with open(args.tools_file) as fh:
        schemas = {
            t["name"]: t["parameters"]
            for t in json.load(fh).get("tools", [])
            if t.get("name") and t.get("parameters")
        }
    with open(args.manifest) as fh:
        manifest = json.load(fh)

    filled, skipped_present, skipped_absent = [], [], []
    for action in manifest.get("actions", []):
        name = action.get("name")
        if name not in schemas:
            skipped_absent.append(name)
        elif "input" in action:
            skipped_present.append(name)
        else:
            action["input"] = schemas[name]
            filled.append(name)

    print(f"fill:            {len(filled)} {sorted(filled)}")
    print(f"already had input: {len(skipped_present)} {sorted(skipped_present)}")
    print(f"no schema in tools file: {len(skipped_absent)} {sorted(skipped_absent)}")

    if args.dry_run:
        print("dry run — manifest not written")
        return 0
    with open(args.manifest, "w") as fh:
        json.dump(manifest, fh, indent=2, ensure_ascii=False)
        fh.write("\n")
    print(f"wrote {args.manifest}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
```

- [x] **Step 2: Dry-run it to verify the match set**

Run:

```bash
python3 scripts/schemas-into-manifest.py \
  --tools-file ~/.config/vyn/agent-tools.json \
  --manifest plugins/telegram/plugin.json --dry-run
```

Expected, verified against today's files:

```
fill:            16 ['tg_add_reaction', 'tg_delete_message', 'tg_download_media', 'tg_edit_message', 'tg_forward_message', 'tg_get_chat_info', 'tg_get_history', 'tg_get_message', 'tg_list_dialogs', 'tg_list_unread', 'tg_mark_all_read', 'tg_mark_read', 'tg_pin_message', 'tg_search', 'tg_send_message', 'tg_upload_media']
already had input: 0 []
no schema in tools file: 13 ['status', 'tg_delete_messages', 'tg_forward_messages', 'tg_get_contact', 'tg_get_unread', 'tg_get_user', 'tg_list_contacts', 'tg_send_action', 'tg_send_animation', 'tg_send_sticker', 'tg_send_voice', 'tg_set_typing', 'tg_transcribe_voice']
```

The 13 in the last group keep a description but no schema — the agent can still call them, the model just gets no parameter docs. Writing those schemas by hand is worthwhile but is not part of this task.

- [x] **Step 3: Apply the migration**

Run:

```bash
python3 scripts/schemas-into-manifest.py \
  --tools-file ~/.config/vyn/agent-tools.json \
  --manifest plugins/telegram/plugin.json
```

- [x] **Step 4: Review the diff by hand**

Run: `git diff plugins/telegram/plugin.json`

Check each added `input` block: it must describe *this* plugin's real parameters. The operator file was written against a partly imaginary API, so treat every schema as a claim to verify against `plugins/telegram/src/lib.rs`'s handler for that action, not as ground truth. Delete or correct anything that does not match the handler.

- [x] **Step 5: Run the telegram suite**

Run: `cargo test --manifest-path plugins/telegram/Cargo.toml`

Expected: PASS. `shipped_manifest_documents_every_declared_action` still passes (it asserts on descriptions, which this task did not touch), and `specs_from_manifest` now emits non-empty `params_schema` for the migrated actions.

- [x] **Step 6: Add a test pinning the migrated schemas**

Add to the `#[cfg(test)] mod tests` block in `plugins/telegram/src/manifest.rs`:

```rust
    #[test]
    fn migrated_actions_ship_a_params_schema() {
        let parsed: Value = serde_json::from_str(include_str!("../plugin.json")).unwrap();
        let specs = specs_from_manifest(&parsed);
        let send = specs.iter().find(|s| s.name == "tg_send_message").unwrap();
        assert!(
            !send.params_schema.is_empty(),
            "tg_send_message must ship its own schema so the operator catalog entry can be deleted"
        );
        let schema: Value = serde_json::from_str(&send.params_schema).unwrap();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].get("text").is_some());
    }
```

- [x] **Step 7: Run it**

Run: `cargo test --manifest-path plugins/telegram/Cargo.toml manifest::`

Expected: PASS, 7 tests.

- [x] **Step 8: Deploy and trim the operator entries to locale overlays**

Redeploy per Task 4 Step 8 (binary plus `plugin.json`). Then, for each `tg_*` action whose schema now lives in `plugins/telegram/plugin.json`, delete its entry from `~/.config/vyn/agent-tools.json`.

Verify before and after with the agent's `tools_list` action: the tool must keep a non-empty `description` and an equivalent `parameters` object, with `source` flipping from `"file"` to `"kernel"`. If any tool loses its schema, restore that entry and fix the manifest instead.

**Resolved 2026-09-17 by adding field-level merge first (commit `bfef99f`).**

The original blocker: `Catalog::load_with_discovery` replaced a spec *wholesale* and only when
it was still `Source::Minimal`, so deleting a file entry was all-or-nothing — the operator
descriptions carry Russian trigger phrases that `embedding_filtered_catalog` matches goals
against (`plugins/agent/src/engine.rs:78,121,137`, threshold 0.35), while manifest descriptions
are terse English. Deleting would have swapped the retrieval signal into the wrong language;
putting Russian into `plugin.json` would have re-inverted the ownership this plan fixes.

`merge_from_kernel` now fills only the fields a file entry left empty and OR-s
`requires_confirmation`, so the entries were *trimmed* rather than deleted: each keeps
`{name, description}` as a pure locale overlay and takes its schema/risk from the owner. The 16
`parameters` blocks are gone from `~/.config/vyn/agent-tools.json`; `tools_list` reports all 16
as `source: merged` with the operator description intact and the corrected schema attached, and
no `tg_*` tool is left without either.

One operator fact lived only in the deleted schemas — the account slugs. The claim there
(`loner42 or loner80`) was itself wrong: `~/.config/vyn/plugins.d/telegram.yaml` configures only
`loner42`. The corrected fact moved to `~/.config/vyn/prompts.d/_facts.md`, which is where
operator facts belong.

This edits an operator config outside the repo — back it up first — back it up first:

```bash
cp ~/.config/vyn/agent-tools.json ~/.config/vyn/agent-tools.json.bak-$(date +%Y%m%d-%H%M%S)
```

- [ ] **Step 9: Commit**

```bash
git add scripts/schemas-into-manifest.py plugins/telegram/plugin.json plugins/telegram/src/manifest.rs
git commit -m "feat(telegram): own the action schemas in plugin.json

Lifts the hand-written params schemas out of the operator tools file into
the manifest of the plugin that implements them, so the catalog layer the
kernel serves is complete and the operator file stops being a second,
drifting source of truth."
```

---

### Task 6: CI guard against undocumented actions

Makes the invariant permanent for all 38 plugins, so the next plugin cannot repeat the defect.

**Files:**
- Create: `scripts/check-action-docs.py`
- Modify: `.github/workflows/ci.yml` (the existing `lint` job, after the "Check plugin manifests" step at line 22)

**Interfaces:**
- Consumes: the `description` convention Task 4 depends on.
- Produces: nothing consumed by later tasks.

- [ ] **Step 1: Write the guard**

Create `scripts/check-action-docs.py`, matching the plain-assert style of the existing `scripts/check-registry.py`:

```python
#!/usr/bin/env python3
"""Every declared action needs a model-facing description.

An action that registers with an empty description embeds as "name — " and
the agent plugin's embedding filter then drops it for any goal that does
not literally contain the tool name (see the agent plugin's
discovery.rs::description_from_parameters doc comment). Undocumented
actions are therefore not merely untidy — they are unreachable.

Reports every offender, then fails. Exit 0 = clean.
"""
import glob
import json
import sys

# Plugins whose actions predate this rule. Shrink this list; never grow it.
GRANDFATHERED = set()

def main():
    offenders = []
    checked = 0
    for path in sorted(glob.glob("plugins/*/plugin.json")):
        with open(path) as fh:
            manifest = json.load(fh)
        plugin_id = manifest.get("plugin_id", path.split("/")[1])
        if plugin_id in GRANDFATHERED:
            continue
        for action in manifest.get("actions", []):
            if not isinstance(action, dict):
                continue  # legacy string form: nothing to document yet
            checked += 1
            if not action.get("description", "").strip():
                offenders.append(f"{plugin_id}.{action.get('name', '<unnamed>')}")

    print(f"check-action-docs: {checked} declared actions checked")
    if offenders:
        print(f"\n{len(offenders)} action(s) missing a description:", file=sys.stderr)
        for name in offenders:
            print(f"  - {name}", file=sys.stderr)
        print(
            "\nAdd a one-line `description` to each action in its plugin.json. "
            "It is what the model reads to decide whether to call the tool.",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
```

- [ ] **Step 2: Run it to see the current damage**

Run: `python3 scripts/check-action-docs.py`

Expected: FAIL, listing roughly 205 undocumented actions across the repo (236 declared, 31 documented today).

- [ ] **Step 3: Grandfather the existing offenders so CI can go green**

Run this to get the plugin ids that currently fail:

```bash
python3 - <<'EOF'
import glob, json
bad = set()
for path in glob.glob("plugins/*/plugin.json"):
    m = json.load(open(path))
    pid = m.get("plugin_id", path.split("/")[1])
    for a in m.get("actions", []):
        if isinstance(a, dict) and not a.get("description", "").strip():
            bad.add(pid)
print(sorted(bad))
EOF
```

Paste the result into `GRANDFATHERED` in `scripts/check-action-docs.py`, keeping the "shrink, never grow" comment above it. Telegram must **not** appear in the list — Task 4 already documents all 29 of its actions, and that is the point of the guard.

- [ ] **Step 4: Run it again to verify it passes**

Run: `python3 scripts/check-action-docs.py`

Expected: PASS, exit 0, prints the checked count.

- [ ] **Step 5: Verify the guard actually catches a regression**

A guard that never fails is not a guard. Blank one telegram description, confirm the script catches it, then restore from git rather than from a copy — git is the safer restore because it is verifiable:

```bash
git diff --quiet plugins/telegram/plugin.json || { echo "ABORT: uncommitted manifest changes"; exit 1; }
python3 - <<'EOF'
import json
path = "plugins/telegram/plugin.json"
m = json.load(open(path))
m["actions"][1]["description"] = ""
json.dump(m, open(path, "w"), indent=2, ensure_ascii=False)
EOF
python3 scripts/check-action-docs.py; echo "exit=$?"
git checkout -- plugins/telegram/plugin.json
git diff --stat plugins/telegram/plugin.json
```

Expected: `exit=1` naming `telegram.tg_list_dialogs`, then `git diff --stat` prints nothing — the manifest is back to its committed state.

The leading `git diff --quiet` check matters: the restore is a `git checkout --`, which discards *all* uncommitted changes to that file. If you have not yet committed Task 5's migrated schemas, commit them before running this step or you will lose them.

- [ ] **Step 6: Wire it into CI**

In `.github/workflows/ci.yml`, in the `lint` job immediately after the "Check plugin manifests" step (line 22), add:

```yaml
      - name: Check action descriptions
        run: python3 scripts/check-action-docs.py
```

- [ ] **Step 7: Commit**

```bash
git add scripts/check-action-docs.py .github/workflows/ci.yml
git commit -m "ci: require a description on every declared action

An action registering with an empty description is dropped by the agent's
embedding filter, so it is unreachable rather than merely undocumented.
Existing offenders are grandfathered; the list only shrinks."
```

---

### Task 7: Write down the standard

Without this, the next plugin repeats the defect and the next session re-derives the whole analysis.

**Files:**
- Modify: `docs/PLUGIN_AUTHORING.md` (new section)

**Interfaces:**
- Consumes: everything from Tasks 1–6.
- Produces: the convention future plugins follow.

- [ ] **Step 1: Add the authoring section**

Append to `docs/PLUGIN_AUTHORING.md`:

```markdown
## Action documentation and the agent catalog

The `agent` plugin builds the tool catalog the model sees by merging three
layers per action name (see `plugins/agent/README.md`): the operator's
tools file wins, the **kernel manifest** fills anything the file omits, and
a bare allowlisted name still dispatches with no schema.

The kernel-manifest layer is the one you own, and it is the one that scales.
It is fed by the `action_specs` field of the `PluginManifest` you send at
registration — **not** by your `plugin.json` alone. A plugin that leaves
`action_specs` at `Default::default()` contributes nothing to the catalog
and forces its tools to be hand-copied into an operator file that has no
link back to this repo. That is how a catalog comes to reference actions a
plugin never had.

### Required of every plugin

1. **Declare a `description` on every action in `plugin.json`.** One line,
   written for the model, not for a changelog: what the action does and
   when to reach for it. `scripts/check-action-docs.py` enforces this in
   CI. An empty description is worse than a bad one — a tool that embeds as
   `"name — "` is dropped outright by the agent's embedding filter
   (`AGENT_PLUGIN_EMBEDDING_FILTER`).
2. **Declare an `input` JSON Schema on every action that takes params**,
   with a `description` on each property. This becomes `params_schema`.
3. **Populate `action_specs` at registration.** Copy
   `plugins/telegram/src/manifest.rs` — it reads the installed `plugin.json`
   next to the binary and converts it, degrading to an empty list on any
   failure so registration never depends on the doc file. Add `plugin.json`
   to your manifest's `files` list so it is installed.
4. **Set `risk` and `requires_confirmation`** on anything destructive. For
   the two-step request/confirm pattern, use
   `vynkor_sdk::confirmation_gate::ConfirmationGate` and merge its
   `manifest_entries()` output instead of hand-writing the specs.

### Not required of you

Do not write entries into the operator's `agent-tools.json`. That file is an
operator override for when a plugin's own docs are wrong or missing; it is
not where a plugin publishes itself. If you find yourself editing it to make
your plugin usable, the fix belongs in your `plugin.json`.

### Operator-side prompt layering

Persona and facts belong to the operator, not to plugins. The `agent`
plugin resolves them, in order:

| Layer | Source | Override |
|---|---|---|
| identity / personality / channel | `const` in `llm.rs` | `<prompts_dir>/_identity.md`, `_personality.md`, `_channel.md` |
| facts (ids, account names, who-is-who) | none | `<prompts_dir>/_facts.md`, or `AGENT_PLUGIN_FACTS` |
| per-group behavior | built-in default | `<prompts_dir>/<group>.md`, then `AGENT_PLUGIN_PLUGIN_PROMPTS` (wins) |

`<prompts_dir>` is `AGENT_PLUGIN_PROMPTS_DIR`. Group names are derived from
the tool-name prefix (`tg_*` → `telegram`); `/` in a group name becomes `-`
in the filename.

### Why plugins do not ship behavior prompts

A plugin supplying free text that lands in the agent's system prompt could
instruct the agent to misuse *other* plugins' tools — read a credential via
`secret_get`, send it via `http_request`. That is a much larger blast radius
than exposing an action, which stays bounded by
`AGENT_PLUGIN_ALLOWED_ACTIONS`. If this capability is added later it needs,
at minimum: a per-plugin opt-in allowlist that is default-deny like the
action allowlist, a hard length cap, and source-attributed fencing around
the injected text so it is never mistaken for host instruction. Until that
design exists, behavior prompts stay operator-owned.
```

- [ ] **Step 2: Verify the cross-references are accurate**

Run: `python3 scripts/check-action-docs.py && cargo test --manifest-path plugins/telegram/Cargo.toml manifest:: && cargo test --manifest-path plugins/agent/Cargo.toml`

Expected: all PASS. Then confirm by eye that every path and env var named in the new section exists:
`plugins/telegram/src/manifest.rs`, `scripts/check-action-docs.py`, `AGENT_PLUGIN_PROMPTS_DIR`, `AGENT_PLUGIN_FACTS`, `AGENT_PLUGIN_PLUGIN_PROMPTS`, `AGENT_PLUGIN_EMBEDDING_FILTER`, `AGENT_PLUGIN_ALLOWED_ACTIONS`, `vynkor_sdk::confirmation_gate::ConfirmationGate`.

- [ ] **Step 3: Commit**

```bash
git add docs/PLUGIN_AUTHORING.md
git commit -m "docs: standard for action docs and agent prompt layering

Records why action_specs must be populated, what the operator tools file
is and is not for, and why plugin-supplied behavior prompts are deferred
pending a security design."
```

---

## Operator migration (after Tasks 1–3 land)

Not a code task — the config move that makes Track A worth having. Do it once the agent plugin is rebuilt and deployed.

```bash
mkdir -p ~/.config/vyn/prompts.d
cp ~/.config/vyn/plugins.d/agent.yaml ~/.config/vyn/plugins.d/agent.yaml.bak-$(date +%Y%m%d-%H%M%S)
```

Then, one file at a time:

1. Extract each key of `AGENT_PLUGIN_PLUGIN_PROMPTS` into `~/.config/vyn/prompts.d/<group>.md` — `telegram` → `telegram.md`, `audio/voice` → `audio-voice.md`, `pulse` → `pulse.md`. Unescape the `\"` sequences and break the wall of text into real paragraphs.
2. Move the hard data out of `telegram.md` into `~/.config/vyn/prompts.d/_facts.md` — the owner id, the agent's own account, and the who-is-who list. What stays in `telegram.md` is behavior only.
3. Add `AGENT_PLUGIN_PROMPTS_DIR=/home/behzod/.config/vyn/prompts.d` to the plugin's `env:` list.
4. Delete the `AGENT_PLUGIN_PLUGIN_PROMPTS` line **last**, and only after a restart confirms the files are being read — it wins over the directory, so while it is present the files have no effect and you cannot tell whether they loaded.

Verify with a real goal that exercises a group prompt before deleting the env line.

---

## Follow-on plans (not in scope here)

Each needs its own plan; none blocks the other.

1. **Roll Task 4 across the remaining 37 plugins** and shrink `GRANDFATHERED` in `scripts/check-action-docs.py` to empty. Mechanical but large; best done a few plugins at a time, each with its own commit.
2. **Lift `manifest.rs` into `vynkor-sdk`** as `vynkor_sdk::manifest::action_specs()` when a release cycle allows, and add `description` to `vynkor-wire`'s `ActionSpecV2` so the on-disk and wire manifest shapes stop disagreeing. Removes the per-plugin copy and lets `discovery.rs::description_from_parameters` be deleted.
3. **Merge semantics for prompt layers** — an explicit append marker so an operator can extend a built-in or plugin default instead of replacing it wholesale. Only worth building once there is more than one source of defaults.
4. **Plugin-supplied behavior prompts**, gated by the opt-in allowlist described in Task 7. Needs a security design first.
5. **Widen `AGENT_PLUGIN_ALLOWED_ACTIONS`** to the telegram actions the plugin implements but the agent cannot reach: `tg_get_user`, `tg_get_contact`, `tg_list_contacts` (these directly serve "who is X" questions), `tg_set_typing`, `tg_send_action`, `tg_send_voice`, `tg_send_sticker`, `tg_send_animation`, `tg_delete_messages`, `tg_forward_messages`. Each is a security decision, so it stays a deliberate operator edit rather than a code change.
