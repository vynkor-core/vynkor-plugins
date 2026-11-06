# vector-db plugin

Embedding upsert + similarity search for Vynkor plugins, gated by `PERMISSION_STORAGE`.
One SQLite file per calling plugin — callers cannot see each other's collections.
Brute-force cosine on L2-normalized vectors, per-collection dimension enforcement.

## Архитектура эмбеддинга: Ollama → ai → vector-db

**Модели ставят через Ollama, вызывают через `ai`, `vector-db` пересылает туда текст.**

```
operator:  ollama pull nomic-embed-text          # или mxbai-embed-large, all-minilm
           ollama serve                           # OpenAI-совместимый /v1/embeddings на localhost:11434

caller → vector-db vec_upsert {collection, id, text:"hello world"}
         ↓ (если VECTOR_DB_EMBED_MODEL задан)
         vector-db → ai embedding {provider:"openai", base_url:"http://localhost:11434/v1",
                                    model:"nomic-embed-text", input:"hello world"}
                   ↓ (ai резолвит ключ vault-first, шлёт через network http_request)
                   Ollama → {embedding:[0.012,...], dim:768}
         ← ai возвращает вектор
         vector-db нормализует, сохраняет в SQLite, отвечает {ok, dim}
```

Почему так: `vector-db` — это хранилище, а не инференс. Модель живёт в Ollama, единый шлюз — `ai` (переиспользует `network` + `secrets` + allowlist `AI_PLUGIN_ALLOWED_KEY_ENVS`), а `vector-db` — тонкий прокси как `notes` → `database`. В чём проблема без этого — каждый плагин городил бы свой HTTP-клиент, дублировал SSRF-guard и vault-first логику, ломал T-19 (caller gated action должен иметь permission).

### Режимы v0.1 (версия 0.1.0 не меняется)

| Режим | Как включить | Что происходит |
|---|---|---|
| **Офлайн fake** (дефолт) | не задавать `VECTOR_DB_EMBED_*` | `fake_embed(text, dim)` — детерминированный hash→L2-norm внутри vector-db, 0 зависимостей, работает без `ai`/`network`/`ollama` |
| **Через ai (рекомендуется)** | задать `VECTOR_DB_EMBED_MODEL` | `vector-db` при `vec_upsert {text}` без `vector` сам зовёт `ai embedding`, получает реальный эмбеддинг, сохраняет. При `vec_query {text}` — так же. Прямая передача `vector` всегда в приоритете (BYO) и не трогает `ai` |
| **BYO двухшаговый** | вызывать `ai embedding` явно | caller сам: `ai embedding` → `vec_upsert {vector}` — работает даже без настройки vector-db |

Офлайн fake и реальный `ai` вектора несовместимы: `dim` коллекции фиксируется первым `vec_upsert` (384 у fake, 768/1536 у реальной модели) — смена модели → новая коллекция.

### Зачем вообще fake? (и когда он не нужен)

`fake` — не продакшен, а **заглушка для тестов и офлайна.** Без него `cargo test` требовал бы живую Ollama + `ai` + `network` + ключи, падал бы в CI и без сети. Fake даёт:

* детерминированный `vec_upsert {text}` → `vec_query {text}` с косинусом `0.9+` для одинакового текста (`cargo test` 26 тестов, 0 сети);
* работу `vector-db` без `ai`/`ollama` (демо, `notes` без сети);
* `dim` по умолчанию `384` как у `all-minilm`/`bge-small` для совместимости.

В проде с Ollama **fake не используется**: если `VECTOR_DB_EMBED_MODEL` задан, `vector-db` шлёт `ai embedding`; при ошибке `ai` (Ollama down, 5xx, timeout) поведением управляет `VECTOR_DB_EMBED_FALLBACK`:

* `fallback=error` (дефолт когда `EMBED_MODEL` задан) — возвращает `ACTION_ERROR: ai embedding failed: ...`, **не** падает в fake, чтобы скрытый деград не портил поиск;
* `fallback=fake` — молча падает в `fake_embed` (для демо/офлайн с `ai` опционально).

Т.е. fake остаётся для `cargo test` и офлайн-режима, в проде с `ollama pull nomic-embed-text` он не мешает — `dim` берётся из реального эмбеддинга (768), и коллекции с `fake` (384) туда не попадут.

## Ollama + ai: настройка за 3 команды

```bash
ollama pull nomic-embed-text   # 274M, 768 dim — дефолт для локального поиска
# или ollama pull mxbai-embed-large  # 669M, 1024 dim, лучше качество
ollama serve &
```

```yaml
# config.yaml — kernel
plugins:
  - id: network
    binary: /opt/plugins/network
    sandbox: false
    env:
      - NETWORK_PLUGIN_ALLOWED_HOSTS=localhost,127.0.0.1  # иначе SSRF порежет loopback
  - id: ai
    binary: /opt/plugins/ai
    sandbox: true
    env:
      - AI_PLUGIN_ALLOWED_KEY_ENVS=OLLAMA_API_KEY
      - OLLAMA_API_KEY=  # пустой — Ollama без auth, но allowlist обязателен
  - id: vector-db
    binary: /opt/plugins/vector-db
    sandbox: true
    env:
      - VECTOR_DB_DATA_DIR=/var/lib/vyn/vector-db
      - VECTOR_DB_EMBED_MODEL=nomic-embed-text
      - VECTOR_DB_EMBED_PROVIDER=openai
      - VECTOR_DB_EMBED_BASE_URL=http://localhost:11434/v1
      - VECTOR_DB_EMBED_API_KEY_ENV=OLLAMA_API_KEY
      - VECTOR_DB_EMBED_TIMEOUT_MS=10000
      - VECTOR_DB_DEFAULT_DIM=768  # должен совпадать с dim модели, иначе mismatch
```

Прямой вызов для проверки:
```json
// 1) напрямую через ai (проверить что Ollama отвечает)
{ "action":"embedding","provider":"openai","base_url":"http://localhost:11434/v1",
  "model":"nomic-embed-text","api_key_env":"OLLAMA_API_KEY","input":"hello world" }
// → {"embedding":[0.012,...],"dim":768,"model":"nomic-embed-text","usage":{"input_tokens":2}}

// 2) через vector-db (сам перешлёт в ai)
{ "action":"vec_upsert","collection":"mem","id":"1","text":"hello world" }
// vector-db → ai embedding → SQLite → {"ok":true,"id":"1","dim":768}

{ "action":"vec_query","collection":"mem","text":"hello","top_k":5 }
// → {"results":[{"id":"1","score":0.91,"text":"hello world","metadata":{}}]}
```

Двухшаговый BYO остаётся валидным всегда (передача `vector` в приоритете):
```json
{ "action":"embedding","provider":"openai","model":"text-embedding-3-small",
  "base_url":"https://api.openai.com/v1","api_key_env":"OPENAI_API_KEY","input":"hello" }
// → vec_upsert {collection, id, vector, text}
```

Cloud (без Ollama) — то же, но base_url/api_key_env указывают на OpenAI/Voyage:
```json
{ "action":"embedding","provider":"openai","model":"text-embedding-3-small",
  "base_url":"https://api.openai.com/v1","api_key_env":"OPENAI_API_KEY","input":"hi" }
```

## Actions

| Action | Params | Result |
|---|---|---|
| `vec_upsert` | `{collection, id, text?, vector?, metadata?}` | `{ok, id, dim}` |
| `vec_query` | `{collection, text?, vector?, top_k?, include_vector?, filter?}` | `{results: [{id, score, text, metadata}]}` |
| `vec_get` | `{collection, id}` | `{found, id?, text?, vector?, metadata?, dim?}` |
| `vec_delete` | `{collection, id}` | `{deleted}` |
| `vec_list` | `{prefix?}` | `{collections: [...]}` |
| `vec_stats` | `{collection}` | `{count, dim}` |

- `vec_upsert` requires at least one of `text` or `vector`. If `vector` is given it is L2-normalized and must match the collection's existing `dim` (first write defines `dim`). If only `text` is given, a fake embedding of `default_dim` is computed.
- `vec_query` requires at least one of `text` or `vector`. `top_k` default 5, cap 100. `filter` is exact match on top-level metadata keys.
- Scores are cosine similarity in `[-1, 1]` (1 = identical).
- Per-caller isolation: `caller_plugin_id` from kernel, sanitized to `[a-zA-Z0-9_-]`.

## Config

| Env var | Default | Meaning |
|---|---|---|
| `VECTOR_DB_DATA_DIR` | — (required) | directory holding per-caller `.db` files |
| `VECTOR_DB_POOL_SIZE` | `4` | connections per caller pool |
| `VECTOR_DB_BUSY_TIMEOUT_MS` | `5000` | SQLite busy timeout |
| `VECTOR_DB_MAX_RESPONSE_BYTES` | `4194304` (4 MiB) | vec_query result size cap |
| `VECTOR_DB_MAX_DB_BYTES` | `268435456` (256 MiB) | hard per-caller disk quota via PRAGMA max_page_count; 0 disables |
| `VECTOR_DB_DEFAULT_DIM` | `384` | default dim for text-only upserts |
| `VECTOR_DB_EMBED_BATCH` | `64` | texts per `ai.embedding` request when embedding a batch (1..64); an `ai` that rejects array input is detected once and fed one text at a time |
| `VECTOR_DB_CACHE_MAX_MB` | `256` | memory ceiling for cached collections (LRU eviction); a collection larger than this is never cached and is searched by streaming BLOBs from SQLite |
| `VECTOR_DB_CACHE_IDLE_SECS` | `600` | a cached collection with no queries or writes for this long is unloaded |

## Storage, cache and migration (v0.2)

- Vectors are stored as f32 little-endian BLOBs (`PRAGMA user_version = 1`). A database from v0.1 (JSON text vectors) is migrated on first open: `VACUUM INTO '<caller>.db.bak-v0'` (only if that file does not exist yet), then one transaction converts every row. Rows whose JSON vector cannot be parsed are skipped with a warning naming the id. A second start does not migrate or touch the backup.
- `vec_query` searches an in-memory copy of the collection (contiguous `Vec<f32>`, dot product on normalized vectors, top-k via a bounded heap), loaded lazily on first query and kept in sync by writes. 20 000 x 1024-d vectors: ~4 ms per query warm.
- `vec_embed {texts}` (1..64) returns `{vectors, model, dim}` (normalized) so a caller can embed a question once and pass `vector` to several `vec_query` calls.
- `collections.model` records the embedding model of a collection (set on the first write that was embedded by `ai`; fake vectors never stamp a model and are refused for a collection that has one). If the configured `VECTOR_DB_EMBED_MODEL` differs, the collection is re-embedded in the background from the stored `text`: new vectors go to a shadow table and are swapped in with `collections.dim`/`model` in one transaction (only for rows whose text is unchanged since embedding; changed rows are re-embedded first), so a failure or crash leaves the old model intact. Rows without text are deleted at the swap. Meanwhile `vec_query` and writes fail with `collection <name> is being re-embedded for model <m>`; after a failed attempt the same error is returned during an exponential backoff (60 s doubling to 30 min) instead of retrying on every request. `vec_stats` reports `model`.

## Concurrency

Hot-path plugin: drives SDK `ConcurrentHandler` + `serve_concurrent` (vynkor-sdk ≥ 0.1.4).
One task owns `VynkorClient`, spawned handler tasks push responses via mpsc.

## Status

v0.2.0 — cached brute-force search, f32 BLOB storage, batch embedding. See `ROADMAP.md` for v0.2 plan (real embeddings).
