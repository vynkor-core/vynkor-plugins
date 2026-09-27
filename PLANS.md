# PLANS.md — план развития экосистемы vynkor

> **Статус 2026-09-27 (аудит документации против кода, ветка
> `chore/docs-registry-sync`):** колонка «Статус» в сводной таблице отражает
> фактический код — ✅ сделано, 🟡 частично, — не начато. Секции сделанных
> пунктов ниже оставлены как история решений; источник правды о Shipped —
> root `ROADMAP.md`. Новые пункты аудита: **STAT-01**, **SIG-01**, **REL-01**,
> **EXI-11**, **AGT-10** (в конце таблицы). Задачи уровня плагина — в
> `plugins/<slug>/ROADMAP.md`.
>
> Идеи по плагинам, клиентам, интеграциям и инфраструктуре. Составлено
> 2026-08-25 после релиза `agent@0.1.3` / `ai@0.1.1`. Живой документ:
> реализованное переносить в root `ROADMAP.md` (Shipped) и удалять отсюда.
>
> **Приоритет:** P0 = делать сейчас · P1 = следующий эшелон · P2 = когда
> дойдут руки · P3 = someday/maybe.
> **Сложность:** S < 1 дня · M = 1–3 дня · L ≈ неделя · XL > недели.
> Оценки для одного разработчика при готовых примитивах (network/ai/
> database/secrets/scheduler/event bus уже работают и проверены live-audit'ом).

## Принципы отбора

1. **Outbound-only интеграции сначала** — SSRF-guarded `network` делает их
   безопасными по построению (никаких входящих портов).
2. **Тонкие схемы над примитивами** — как `notes` над `database`: новый
   плагин держит только бизнес-логику и чужие permissions (T-19).
3. **Ключи — vault-first через `secrets`**, литералов в конфигах нет.
4. **События — `plugin.<slug>.<event>`**, best-effort публикация.
5. Сначала то, что превращает набор плагинов в *систему* (клей: automations,
   memory, клиенты), потом отдельные возможности.

## Сводная таблица

| ID | Идея | Категория | Приоритет | Сложность | Зависит от | Статус |
|---|---|---|---|---|---|---|
| INF-01 | Релизы по тегу в CI (package+sign) | Инфра | **P0** | S | GH Secrets | ✅ |
| EXI-01 | ICS импорт/экспорт в `calendar` | Существующие | **P0** | M | — | ✅ |
| AGT-01 | Memory агента на `vector-db` | Agent | **P0** | M | — | ✅ |
| INT-01 | Telegram-бот как клиент агента | Интеграции | **P0** | M | — | 🟡 user-клиент `telegram` (MTProto) есть; бот-клиент агента — нет |
| EXI-02 | `tts`: синтез по предложениям | Существующие | **P0** | M | daemon | ✅ |
| CAP-01 | `automations` — rules engine | Новый плагин | **P1** | L | event bus | ✅ |
| EXI-03 | `stt`: wake-word + промежуточные гипотезы | Существующие | **P1** | L | sherpa | 🟡 VAD и события начала/конца речи есть; wake-word и partial-гипотезы — нет |
| CAP-02 | `capture` — экран/камера/OCR | Новый плагин | **P1** | XL | PipeWire | ✅ |
| CAP-03 | `metrics` — хост-метрики для графиков | Новый плагин | **P1** | M | — | ✅ |
| EXI-04 | `calendar`: повторяющиеся события (RRULE) | Существующие | **P1** | M | — | ✅ |
| INT-02 | ntfy/Gotify push на телефон | Интеграции | **P1** | S | network | ✅ |
| EXI-05 | `filesystem`: delete/rename/move/trash | Существующие | **P1** | M | — | ✅ |
| CLI-01 | `vyn ask` — терминальный клиент агенту | Клиенты | P2 | S | WS API | ✅ |
| CLI-02 | vynkor-web: чат + inbox + графики | Клиенты | P2 | L | metrics | — |
| INT-03 | Google Calendar полная синхронизация | Интеграции | P2 | L | EXI-01 | — |
| INT-04 | CalDAV (Nextcloud/iCloud/Fastmail) | Интеграции | P2 | L | EXI-01 | — |
| INT-05 | `rss` читалка + «что нового» | Интеграции | P2 | M | vector-db | ✅ |
| INT-06 | `github` — issues/CI/PR голосом | Интеграции | P2 | M | network | ✅ |
| INT-07 | `email`: IMAP-тело + триаж агентом | Интеграции | P2 | M | — | — |
| EXI-06 | `clipboard`: история + поиск | Существующие | P2 | S | database | ✅ |
| AGT-02 | Background goals (detach >30 с) | Agent | P2 | L | — | — |
| AGT-03 | Playbooks — декларативные макро без LLM | Agent | P2 | M | — | — |
| INF-02 | vynm: авто-deps, rollback, каналы, search | Инфра | P2 | M | kernel | — |
| INT-08 | `weather` + плейбук «брифинг» | Интеграции | P2 | S | search | ✅ |
| CAP-04 | `window` — список/фокус окон | Новый плагин | P2 | M | — | — |
| CAP-05 | `input` — виртуальные клавиатура/мышь | Новый плагин | P2 | M | — | — |
| CAP-06 | `wifi` — NetworkManager D-Bus | Новый плагин | P2 | M | wire bump | — |
| CAP-07 | `bluetooth` — BlueZ D-Bus | Новый плагин | P2 | M | wire bump | — |
| EXI-07 | `scheduler`: IANA-таймзоны вместо offset | Существующие | P2 | S | — | ✅ |
| AGT-04 | Eval-harness: регресс-цели на fake-LLM | Agent | P2 | M | — | — |
| HW-01  | ESP32 voice-satellite (wake-word → stt) | Железо | P3 | XL | stt | — |
| INT-09 | `mqtt` — Zigbee2MQTT/Tasmota/ESPHome | Интеграции | P3 | M | network* | ✅ |
| INT-10 | Home Assistant bridge | Интеграции | P3 | M | network | — |
| INT-11 | `spotify` Web API | Интеграции | P3 | M | media | — |
| INT-12 | `contacts` — vCard-хранилище | Интеграции | P3 | S | database | ✅ |
| INT-13 | `files-index` — RAG по файлам | Интеграции | P3 | L | vector-db | — |
| INT-14 | Email→agent шлюз (управление почтой) | Интеграции | P3 | M | INT-07 | — |
| CLI-03 | Android: PTT-кнопка + зеркало нотификаций | Клиенты | P3 | M | device-agent | — |
| CLI-04 | Трей-индикатор десктопа | Клиенты | P3 | M | — | — |
| CLI-05 | Браузерное расширение «вкладка → агенту» | Клиенты | P3 | M | WS API | — |
| AGT-05 | Мультиагент: planner/worker | Agent | P3 | XL | AGT-02 | — |
| AGT-06 | Бюджеты токенов/стоимости per goal/day | Agent | P3 | S | — | — |
| INF-03 | Мультихост: fleet/mesh между ядрами | Инфра | P3 | XL | D-13 | — |
| INF-04 | `backup` — снапшоты состояния vyn | Инфра | P3 | M | scheduler | — |
| EXI-08 | `ai`: батч-эмбеддинги + SSE-стриминг | Существующие | P3 | M/L | — | — |
| EXI-09 | `network`: WebSocket-действия | Существующие | P3 | XL | kernel proto | — |
| EXI-10 | Мелочи: media Raise/Quit, sound devices, launcher fuzzy/recents | Существующие | P3 | S | — | 🟡 media Raise/Quit, `sound_devices`, fuzzy в launcher — есть; recents — нет |
| INT-20 | Провайдер-рецепты `ai`: Gemini/Groq/OpenRouter (почти только доки) | Интеграции | **P0** | S | — | ✅ |
| INT-19 | `tasks` локально + google-tasks синк | Интеграции | P1 | M | INT-03 (OAuth-инфраструктура) | 🟡 локальный `tasks` есть; google-tasks синк — нет |
| CAP-08 | Dictation mode — системный голосовой ввод (hold-hotkey → речь → текст в курсор) | Режим daemon'а | P1 | M | CAP-05 (`input`) | — |
| PLAY-01 | Sleep timer («подкаст на полчаса») | Плейбук | P1 | S | scheduler+sound | ✅ |
| PLAY-02 | Умный будильник (крон + нарастающий volume + брифинг) | Плейбук | P1 | S | PLAY-01, INT-08 | ✅ |
| PLAY-03 | Focus mode («не беспокоить час» → silent-inbox + таймер) | Плейбук | P2 | S | notify inbox | ✅ |
| PLAY-04 | Голосовой DJ («что-нибудь для работы») | Плейбук | P2 | S | ai+media | ✅ |
| PLAY-05 | Языковой тренажёр (persona-pack агента) | Плейбук | P3 | S | tts+stt | — |
| INT-21 | Obsidian-мост для notes (notes ↔ .md файлы vault'а) | Интеграции | P2 | M | filesystem | — |
| INT-22 | Email→календарь: извлечение событий из писем | Интеграции | P2 | M | INT-07 | — |
| CAP-09 | `library` — индекс фото/музыки/видео (слой индексации, НЕ плеер) | Новый плагин | P2 | M | database, vector-db | ✅ |
| INT-23 | YouTube «включи X» (piped/invidious поиск → открытие) | Интеграции | P3 | S-M | launcher | — |
| INT-24 | Google Photos «этот день N лет назад» в брифинге | Интеграции | P3 | M | INT-03 | — |
| MRG-01 | Слияние `tts`+`stt` → **`speech`** | Рефакторинг | P1 | M | приурочить к следующей содержательной правке tts/stt | ✅ |
| MRG-02 | Слияние `wifi`+`bluetooth` → **`connectivity`** | Рефакторинг | P2 | M | CAP-06/07, wire bump | — |
| MRG-03 | Push-каналы (ntfy/Gotify) внутрь `notify` | Рефакторинг | P1 | S | INT-02 | ✅ |
| MRG-04 | `window` внутрь `system` (опционально) | Рефакторинг | P3 | S | CAP-04 | — |
| MRG-05 | `fs_delete{to_trash}` вытесняет gated-write из production | Рефакторинг | — | — | EXI-05 (не отдельная работа) | — |
| ARCH-01 | Крейт scan-harness: общий скан-цикл (calendar/scheduler/metrics/automations/rss/backup) | Архитектура | P1 | M | — | — |
| ARCH-02 | Крейт `gauth`: OAuth refresh-цикл для всех Google-интеграций | Архитектура | P2 | M | INT-03 первый | — |
| ARCH-03 | Крейт crawl: обход allowlist-корней + mtime-чекпоинты (library, files-index) | Архитектура | P3 | M | CAP-09 второй индексатор | — |
| ARCH-04 | Крейт thin-schema helpers: doc CRUD + atomic id counter + event boilerplate | Архитектура | P2 | S | notes/calendar/tasks | — |
| INF-07 | Конвенция `plugin_status` + дашборд здоровья плагинов | Инфра | P1 | S-M | — | 🟡 конвенция описана, все 9 плагинов на `<slug>_status` (STAT-01); `vyn status`-агрегатора нет |
| INF-08 | `vyn doctor` — предзапускная проверка окружения | Инфра | P1 | M | kernel/tooling | — |
| INF-09 | Ночной fake-kernel e2e по ВСЕМ плагинам в CI | Инфра | P1 | M | INF-01 | — |
| AGT-09 | Per-tool квоты/кулдауны в каталоге агента | Agent | P2 | S | — | ✅ |
| CLI-07 | Голосовые шорткаты: hotkey → playbook binding | Клиенты | P2 | S | CAP-01, hotkey | ✅ |
| INF-10 | Граф зависимостей плагинов в web (requires + ipc_targets из манифестов) | Инфра | P3 | S | list_plugins | — |
| INF-11 | Perf-бенчмарки в CI: time-to-first-audio, latency шага goal | Инфра | P3 | M | — | — |
| SEC-01 | Threat-model документ (STRIDE по плагинам: input/mic/hotkey/network) | Безопасность | P2 | M | — | — |
| HYG-01 | Репо-гигиена: ping-pong-rs → examples/, gated-write → reference/ | Инфра | P3 | S | — | ✅ |
| STAT-01 | `status` → `<slug>_status`: голый `status` объявлен в 9 плагинах, ядро отказывается роутить неоднозначные действия (`router.rs` ActionLookup::Ambiguous → ACTION_NOT_FOUND) | Инфра | **P0** | S-M | — | ✅ |
| SIG-01 | Переподписать реестр: `resign.py --check` — 23 версии invalid, launcher 0.1.1–0.1.5 unsigned | Инфра | **P0** | S | ключ мейнтейнера | — |
| REL-01 | Первый релиз 13 плагинов из `scripts/registry-meta.json` (тег `<slug>-v<ver>`) | Инфра | **P0** | S | SIG-01 | — |
| EXI-11 | Алерты: `metrics` пороги → событие, `uptime` recovered + N-подряд | Существующие | P1 | S | — | — |
| AGT-10 | `goal_cancel` + пошаговые события цели для UI | Agent | P1 | M | — | — |

---

## P0 — сейчас

### INF-01 · Релизы по тегу в CI (package+sign)

- **Проблема:** релизы руками через `scripts/package.sh` отстают от кода —
  `agent` дошёл до 0.1.7 в git при 0.1.2 в registry (закрыто вручную
  2026-08-25). Воспроизведётся гарантированно.
- **Что делает:** `git tag agent-v0.1.4 && git push --tags` → GitHub Action
  собирает оба архива, подписывает Ed25519-ключом из Secrets, коммитит
  `dist/` + `registry.json`, вешает артефакты на release.
- **Как реализовать:** workflow `on: push: tags: ['*-v*']`; парсит
  `<slug>-v<version>`; шаги = тело `package.sh` минус локальная сборка
  (`cargo build --release`, zip, python-sign, upsert registry). Секреты:
  `SIGNING_KEY_HEX` (= seed из `~/.ssh/vynkor_sign`). Защита: environment
  с required reviewer, ключ только в Secrets. Проверка: `vynm install`
  свежевыпущенной версии на локальной машине.
- **Зачем:** убирает единственный ручной шаг дистрибуции; подпись тем же
  pinned-ключом сохраняется.

### EXI-01 · ICS импорт/экспорт в `calendar`

- **Проблема:** календарь vynkor изолирован — перенести события из Google
  Calendar / Nextcloud / iCloud можно только руками.
- **Что делает:** `calendar_ics_import {ics_base64}` и
  `calendar_ics_export {}` — парсинг/генерация VEVENT (DTSTART/DTEND,
  SUMMARY, DESCRIPTION, LOCATION, VALARM → `remind_before_ms`).
- **Как реализовать:** крейт `icalendar` (парсер+генератор); импорт идёт в
  существующий store (`cal:<id>`), каждому импортированному событию —
  `ics_uid` поле для идемпотентности повторного импорта; экспорт — обход
  `event_list`. Файл передаётся base64 (лимит как у fs_write ~несколько МБ)
  или читается через `filesystem.fs_read` из allowlist-корня.
- **Риск:** RRULE в чужих .ics — на первом этапе разворачивать в N
  конкретных событий (например, вперёд на 90 дней), полноценный RRULE —
  отдельно (EXI-04).

### AGT-01 · Memory агента на `vector-db`

- **Проблема:** агент амнезиак между целями — каждый goal_start с нуля;
  пользователь повторяет контекст («мой сервер такой-то…») каждый раз.
- **Что делает:** после завершённого goal — извлечь факты и записать в
  `vector-db` (`vec_upsert`, namespace=`agent-memory`); перед запуском —
  семантический поиск top-K фактов и подстановка в системный промпт
  («Известный контекст: …»). Команды оператору: `memory_forget`.
- **Как реализовать:** новый модуль в agent (без нового плагина): ветка в
  serve-loop после терминального статуса; экстракция фактов — отдельный
  дешёвый вызов `ai.chat_completion` («выпиши устойчивые факты списком,
  JSON»); эмбеддинг текстов через `ai.embedding` (Ollama локально);
  recall — `vec_query` по тексту цели. Флаг `AGENT_PLUGIN_MEMORY=on/off`,
  namespace настраиваемый. Персистентность бесплатно — SQLite vector-db.
- **Риски:** шум в промпте — ограничить K=5 и min-score; приватность —
  память живёт локально в per-caller SQLite, но добавить `memory_clear`.

### INT-01 · Telegram-бот как клиент агента

- **Проблема:** управлять машиной можно только сидя за ней или с
  headless-daemon'а; нужен удалённый канал без проброса портов и VPN.
- **Что делает:** long-poll Bot API (чистый outbound!) → входящий текст =
  `goal_start` (allowlist действий оператора) → ответ модели в чат;
  голосовые сообщения → `stt_transcribe` → тот же путь; опция озвучки
  ответов `tts_synthesize` и отправка voice-сообщением. Фото от
  пользователя → `ai` vision.
- **Как реализовать:** плагин `telegram` (permissions: caller-side —
  network-зависимости через T-19): tokio-цикл `getUpdates` c `offset`,
  таймаут лонг-полла 50 с; команды `/goals`, `/cancel` поверх; чат-id
  allowlist в env (`TELEGRAM_PLUGIN_ALLOWED_CHATS`) — иначе бот отвечает
  кому угодно; токен vault-first через `secrets`. Длинные цели — через
  `goal_start` синхронно до появления AGT-02 (Telegram прощает 30–60 с,
  слать «думаю…» сразу). Событие `plugin.telegram.message_received` — чтобы
  automations (CAP-01) могли реагировать на сообщения.
- **Риски:** лимиты Bot API (~30 msg/s) — не проблема для личного бота;
  секрет чат-строки не светить в логах.

### EXI-02 · `tts`: синтез по предложениям (стриминг латентности)

- **Проблема:** `daemon_turn` ждёт полного синтеза ответа; для абзаца это
  секунды тишины — главный вкладчик в воспринимаемую latency голосового
  цикла (постмортем launcher упоминает 30–60 с ожиданий в связке с LLM).
- **Что делает:** `tts_speak_stream` (или режим в `tts_speak`): принимает
  полный текст, сам режет на предложения, синтезирует и стримит Opus-чанки
  пиру по мере готовности — первый звук через время одной фразы.
- **Как реализовать:** сплиттер предложений (RU/EN сокращения учесть —
  «т.д.», «etc.»); очередь: synthesize(s1) → start stream → параллельно
  synthesize(s2)...; переиспользовать существующий D-12 AudioStreamChunk-
  конвейер `tts_speak`; для cloud-провайдеров оставить как есть (у них
  свой стриминг позже). Daemon просто вызывает новое действие — изменений
  с его стороны почти нет.
- **Проверка:** замер time-to-first-audio до/после на абзаце из 4–5 фраз.

### INT-20 · Провайдер-рецепты `ai`: Gemini / Groq / OpenRouter (P0, почти только доки)

- **Проблема:** мозг ассистента воспринимается как «платный Claude или
  возня с Ollama»; барьер цены мешает пробовать.
- **Что делает:** страничка «варианты мозга по цене/скорости» — готовые
  конфиги `ai` для: локальный Ollama (0 ₽), Gemini free tier (0 ₽,
  OpenAI-совместимый endpoint `https://generativelanguage.googleapis.com/v1beta/openai/`),
  Groq (быстро, free tier), OpenRouter (выбор моделей). Плюс прогон
  agent'а на каждом.
- **Как реализовать:** `ai.provider:"openai"` уже принимает произвольный
  `base_url` и vault-first `api_key_env` — скорее всего это **чистая
  конфигурация без кода**. Проверить нюансы совместимости Gemini
  (max_tokens, tool-calls через compat-слой) одним e2e-прогоном; если
  что-то расходится — точечные фиксы в openai-адаптере. Рецепт в USAGE.md +
  `config.example.yaml`.
- **Зачем P0:** убирает главный барьер входа для новичков; час работы.

---

## P1 — следующий эшелон

### CAP-01 · `automations` — rules engine (клей системы)

- **Проблема:** каждое «если X, то Y» сегодня — код; система реагирует
  только на прямые команды.
- **Что делает:** декларативные правила `trigger → [conditions] → action`:
  триггеры cron (`scheduler`) и подписки на события (hotkey_pressed,
  calendar.due, telegram.message_received, sys-события, sync.delta);
  действия — kernel-routed action call или `goal_start`. Пример: «battery
  < 20% после 21:00 → notify + dim brightness».
- **Как реализовать:** правила как JSON-документы в `database`
  (`rule:<id>`), CRUD-действия; цикл как в calendar: select-ветка на
  события + тикер; dispatch через те же kernel-routed calls, что у
  scheduler'а (пермишены оператора, no laundering — механика уже есть).
  Cooldown/dedup на правило (не спамить одним срабатыванием). Опасные
  действия — помечать `requires_confirmation` и звать оператора.
- **Зачем первым среди P1:** превращает плагины в платформу; всё нужное
  (event bus, scheduler-dispatch, gated calls) уже отлажено в scheduler.

### EXI-03 · `stt`: wake-word + промежуточные гипотезы

- **Проблема:** hands-free работает только через VAD после начала записи;
  «Эй, vyn» требует облачных сервисов или кнопки. Плюс daemon молчит во
  время речи — нет фидбэка, что вас слышат.
- **Что делает:** (a) локальный детектор wake-word (openWakeWord /
  microWakeWord onnx) на входящем PCM-потоке → публикует
  `stt_wake_detected`, daemon в vad-режиме начинает транскрипцию; (b)
  интерим-события `stt_text_partial` каждые ~500 мс — UI/tray может
  показывать живую расшифровку.
- **Как реализовать:** (a) модель onnx ~2–8 МБ, инференс в том же
  spawn_blocking-пуле, что и основной движок; окно 1.5–3 с скользящее по
  потоку от mic; порог + refractory-период 2 с. (b) тривиально поверх
  текущего listen-цикла — публиковать частичный transcript буфера.
- **Риск:** ложные срабатывания — калибруемый порог в env; CPU-цена
  непрерывного окна замерить (ожидаемо <5% одного ядра int8).

### CAP-02 · `capture` — экран/окно/камера/OCR (уже в root ROADMAP)

- **Проблема:** у агента нет глаз: «что на моём экране?», «отсканируй
  документ», «что за ошибка в этом окне?».
- **Что делает:** PipeWire ScreenCast (портал) для экрана/окна, V4L2 для
  камеры; действия: `capture_screen` → PNG base64, `capture_ocr` → текст
  (локальный tesseract argv-only, как clipboard/notify), видео-запись
  позже. Связка с `ai` vision: скриншот напрямую в multimodal-запрос.
- **Как реализовать:** портал-DBus API через zbus (стек обкатан в
  media/hotkey); токен restore для бесшовных повторных запросов;
  PERMISSION_SCREEN/CAMERA из wire v1.4; OCR — child-process tesseract с
  stdin/stdout png, никаких сетей.
- **Почему XL:** портал-сессии, форматы пиксель-буферов, разрешения
  Wayland — много краевых случаев; начать с MVP «активный экран → PNG».

### CAP-03 · `metrics`

- **Проблема:** нет истории нагрузки — веб-клиенту нечего рисовать,
  диагносировать «тормозило в 3 часа ночи» нечем.
- **Что делает:** тикер (как calendar scan): раз в N сек сэмплит
  CPU/RAM/disk/battery/net (расширить `system`-парсеры /proc+sysfs) →
  строка в собственную SQLite (per-caller изоляция как у vector-db);
  действия `metrics_query {from,to,resolution}` / `metrics_latest`.
- **Как реализовать:** retention-политика (raw 24 ч → downsampling 1 мин
  30 дней); публикация `plugin.metrics.sample` для live-графиков по WS.
  Данные уже частично умеет добывать `system.sys_info/sys_battery` —
  вынести общие парсеры в общий модуль или дублировать узко.

### EXI-04 · `calendar`: повторяющиеся события (RRULE)

- **Проблема:** еженедельники/ежедневники приходится создавать руками на
  каждый случай (или разворачивать на импорте, см. EXI-01).
- **Что делает:** поле `rrule` ("FREQ=WEEKLY;BYDAY=MO,WE") у события;
  materialization по окну при `event_list`/скане напоминаний.
- **Как реализовать:** крейт `rrule`; хранить master-event + генерить
  виртуальные вхождения на лету (окно ±90 дней); напоминания — по
  вхождениям; редактирование «этого вхождения» → split в обычное событие
  (exdate у мастера). Не забыть `late`-семантику напоминаний как у
  одноразовых.

### INT-02 · ntfy/Gotify push на телефон

- **Проблема:** desktop-notify виден только за компом; нужен пуш на телефон
  без своего приложения-магазина и без Telegram.
- **Что делает:** `push_send {title, body, topic}` → POST на
  ntfy.sh/self-hosted Gotify; приоритеты, теги. Автоматически становится
  транспортом для automations («ушёл из дома → пуш»).
- **Как реализовать:** S — один action поверх `http_request` (тот же
  паттерн, что search/ai: gated http_request, vault-first ключ, allowlist
  хостов через NETWORK_PLUGIN_ALLOWED_HOSTS). Self-hosted вариант решает
  приватность.

### EXI-05 · `filesystem`: delete/rename/move/trash

- **Проблема:** плагин только создаёт/читает — агент не может ни
  переименовать файл, ни прибрать мусор; «sandboxed» оборачивается
  бесполезностью для реальных задач уборки.
- **Что делает:** `fs_delete {path, to_trash=true}`, `fs_rename`,
  `fs_move`, `fs_mkdir` — всё внутри существующего root-allowlist.
- **Как реализовать:** default — перемещение в trash freedesktop spec
  (~/.local/share/Trash, файловая info-запись с путём-оригиналом) —
  обратимо; жёсткое удаление — только `to_trash:false` + отдельная
  пермиссия `PERMISSION_FILES_WRITE` уже покрывает, но action пометить
  `requires_confirmation: true` для agent-каталога. Переиспользовать
  canonicalize-защиту от symlink-побегов (она уже написана для fs_write).

### INT-19 · `tasks` локально + google-tasks синк

- **Проблема:** голосового GTD нет: «задача: перезвонить бухгалтеру
  завтра» некуда записать; задачи живут в голове или в чужом облаке.
- **Что делает:** локальный плагин `tasks` — thin schema над `database`
  (как notes): `task_create/task_list/task_done/task_delete`, поля
  title/due/notes/list; события `plugin.tasks.changed`. Позже — синк-
  транспорт google-tasks поверх OAuth-инфраструктуры INT-03 (Tasks API
  простой REST, idempotent mapping через `gtask_id` поле).
- **Как реализовать:** v1 — копия паттерна notes за вечер. Синк — после
  INT-03: pull/push по updated-меткам, конфликт last-writer-wins.
- **Почему так:** как calendar/ics/gcal — офлайн-first с опциональным
  облаком, а не облако-как-хранилище.

### CAP-08 · Dictation mode — системный голосовой ввод

- **Проблема:** печатать длинный текст на клавиатуре там, где нет окна
  vynkor (браузер, IDE, мессенджер); whisper-тулзы платные/облачные.
- **Что делает:** зажатая hotkey-кнопка → `mic_start{target:"stt"}` →
  отпустил → транскрипт печатается в фокус-окно через `input_type`.
  Локальный whisper-класс ввода везде, где есть курсор.
- **Как реализовать:** режим daemon'а (`daemon_dictate`) поверх уже
  существующих mic/stt/hotkey — единственная новая зависимость CAP-05
  (`input`, ydotool/wtype). PTT-механика hold/release у hotkey уже есть
  (Activated/Deactivated). Индикация состояния — трей (CLI-04).
- **Вау/польза:** самый высокий совместный балл из всего списка;
  используется ежедневно.

### INF-07 · Конвенция `plugin_status` + дашборд здоровья

- **Проблема:** жив ли плагин, загрузился ли движок, какая последняя
  ошибка — видно только по логам; live-audit тратил время на диагностику
  вслепую.
- **Что делает:** конвенция: каждый plugin отвечает на стандартное
  действие `status {}` → `{version, uptime_ms, engine_ready, last_error,
  counters}`; `vyn status` опрашивает всех и рисует таблицу green/red;
  позже — карточки в web.
- **Как реализовать:** сначала конвенция в PLUGIN_AUTHORING.md +
  реализация в 2–3 плагинах (speech, network, database); затем хелпер в
  SDK (`Plugin::status()` дефолт). Kernel-side агрегатор не нужен —
  CLI/web обходит плагинов через kernel-routed calls.

### INF-08 · `vyn doctor`

- **Проблема:** secured-bootstrap молча крэш-лупится без JWT env
  (live-audit Ops/DX #1) — оператор узнаёт по пустому логу.
- **Что делает:** одна команда проверяет ВСЁ до запуска: config.yaml
  парсится, socket существует, TLS-серт на месте, для каждого drop-in —
  бинарник существует, JWT sub == plugin_id, permissions из манифеста
  покрыты токеном, нужные env присутствуют (VYN_JWT_SECRET и пр.),
  vault открыт master-key'ем. Вывод: чеклист OK/FAIL с фикс-подсказками.
- **Как реализовать:** CLI-side (vynkor-core репо) поверх существующих
  структур config/drop-ins; манифесты берёт через registry/dist или
  напрямую из установленных. Без ядра — работает и при остановленном vyn.

### INF-09 · Ночной fake-kernel e2e по всем плагинам

- **Проблема:** cross-plugin регрессии ловятся руками: смена нумерации
  enum'ов сломала ping-pong status=0 незаметно для всех остальных.
- **Что делает:** CI-job раз в ночь + на PR к wire/kernel: поднимает
  fake-kernel harness, регистрирует ВСЕ 27 плагинов, гоняет smoke-матрицу
  (по одному ключевому действию на плагин — основа уже лежит в
  scripts/live-audit/).
- **Как реализовать:** переупаковать live-audit матрицы в cargo-test с
  fake-kernel (UnixStream::pair паттерн уже есть у каждого плагина);
  красный билд = кто-то сломал контракт.

---

## Плейбуки-композиции (PLAY) — новый функционал без нового кода

> Всё ниже собирается из существующих плагинов через AGT-03 (playbooks)
> или CAP-01 (automations). Дёшево забывается — записано, чтобы не забыть.

### PLAY-01 · Sleep timer — P1, S

«Включи подкаст на полчаса»: `sound_play` + scheduler one-shot на 30 мин →
`sound_stop`. Два вызова, ноль кода после AGT-03.

### PLAY-02 · Умный будильник — P1, S

Cron (scheduler) + `sound_play` с постепенным volume + PLAY-briefing
(погода INT-08 + calendar.due голосом). Заменяет звонок телефона.

### PLAY-03 · Focus mode — P2, S

«Не беспокой меня час» → все notify в silent-inbox (уже есть), таймер
возвращает; опционально media pause + статус. Automations rule.

### PLAY-04 · Голосовой DJ — P2, S

«Включи что-нибудь для работы» → ai выбирает по времени суток/контексту →
media играет (MPRIS) или sound_play локального файла. Всё существует.

### PLAY-05 · Языковой тренажёр — P3, S

Persona-pack агента: разговорная практика голосом (tts+stt), исправления,
новые слова в tasks. Только промпты + профиль ai (`agent_id`).

---

## P2 — когда дойдут руки

### CLI-01 · `vyn ask` — терминальный клиент

- **Проблема:** к агенту нельзя обратиться вне голосового цикла; vyn-act —
  одно действие, не диалог.
- **Что делает:** REPL/one-shot: `vyn ask "почему упал CI"` → WS к ядру →
  `goal_start` → печатает ответ; история сессии в `~/.local/share/vyn`.
  **Обязательный stdin-режим:** `git diff | vyn ask "напиши commit
  message"` — тогда vyn становится unix-тулом для ежедневной работы
  разработчика (commit messages, review, explain).
- **Как реализовать:** тонкий WS-клиент в стиле scripts/vyn-act (JWT +
  frame MAC уже реализованы там же); target=`kernel` envelope (урок
  live-audit №3); rust readline, ~300 LOC. Идеальный полигон для AGT-04.

### CLI-02 · vynkor-web: чат + inbox + графики

- Чат к agent по WS (та же схема, что CLI-01), inbox тихих нотификаций
  (`notify_list/mark_read` уже есть), графики `metrics_query`, CRUD
  notes/calendar. Строится по мере появления зависимостей; WS-обёртку для
  браузера ядро уже имеет (TLS + JWT).

### INT-03 · Google Calendar полная синхронизация

- После EXI-01: OAuth device-flow через `http_request` (refresh-токен в
  `secrets`), incremental sync (`syncToken`), watch-push опционально.
  Маппинг gcal id ↔ `cal:<id>` через `ics_uid`. Конфликт-политика:
  last-writer-wins + журнал расхождений в events.

### INT-04 · CalDAV

- Тот же контракт обмена, что INT-03, но PROPFIND/REPORT по CalDAV —
  Nextcloud/Fastmail/Radicale без OAuth-зоопарка (Basic + app-password в
  vault). Один модуль синка, два транспорта.

### INT-05 · `rss` читалка

- Фиды по cron (`automations`/`scheduler` дергает `rss_refresh`), статьи в
  `database` + эмбеддинги заголовков в `vector-db`; действия
  `rss_list/rss_mark_read`; агенту: «что нового по Rust» → vec_query +
  summarize через `ai`. Полностью outbound, ключей не нужно.

### INT-06 · `github`

- REST через `http_request`, PAT vault-first: `gh_issues`, `gh_runs`,
  `gh_issue_create`... Голосовые сценарии: «статус CI», «заведи багу:
  ...». Осторожно с write-действиями — `requires_confirmation`.

### INT-07 · `email`: тела писем + триаж

- Расширить существующий `email`: IMAP BODY.FETCH, `email_read`,
  `email_move` (архив/метки); затем цель-шаблон для агента «разбери
  входящие, важное — в notes». Пароли уже vault-first.

### EXI-06 · `clipboard`: история + поиск

- Хук в read/write → append в `database` (кольцо N=1000, TTL) +
  опционально векторизация текста; `clipboard_history {query?, limit}`.
  Приватность: выключаемая, exclude-паттерны (пароли из менеджеров).

### AGT-02 · Background goals

- Как в ROADMAP: detach длинных целей, прогресс-события
  `plugin.agent.goal_progress`, `goal_status` polling. Разблокируется
  реальным потребителем (telegram-бот/web-чат). Реализация: внутренняя
  задача на goal (calendar-style select branch), статус в goal-doc.

### AGT-03 · Playbooks (детерминированные макро)

- YAML-файлы: последовательность шагов с параметрами и условиями,
  исполняются БЕЗ LLM — дёшево и мгновенно («good_morning»: weather+
  calendar.due+ntfy). Реестр playbook'ов в `tools_file`-стиле;
  `playbook_run {name, params}`; LLM зовётся только где явно указано
  (`step: llm`). Marketplace-ready (vynm ставит файл рядом с бинарём).

### INF-02 · vynm: зависимости, rollback, каналы, search

- `install` резолвит `dependencies` из registry рекурсивно; `rollback
  <slug>` ставит предыдущую версию (versions-map уже хранит всю историю);
  каналы stable/beta — поле в slug-entry; `search` — grep по
  name/description/tags. Всё локально, registry-формат менять не нужно.

### INT-08 · `weather` + плейбук «брифинг»

- open-meteo — без ключа: `weather_now/weather_forecast {lat,lon}`;
  координаты/город в config. Плейбук AGT-03 «good_morning»: погода +
  calendar.due + rss-top → notify/голос. Первая «магия» для новичка.
- **Дополнение (commute):** время в пути/пробки через OpenRouteService
  (free key) или 2GIS — «выезжай через 15 минут» в том же брифинге.
  Air-quality/pollen у open-meteo тоже есть — бонус-строка брифа.

### CAP-04 · `window` — управление окнами

- wlr-protocols/foreign-toplevel (Hyprland: hyprctl IPC или
  zwlr_foreign_toplevel_management): `win_list/win_focus/win_minimize`.
  Замыкает цикл с launcher'ом («запусти и разверни»). PERMISSION_SYSTEM.

### CAP-05 · `input` — виртуальный ввод

- ydotool/wtype/xdotool spawn argv-only: `input_type/input_key/
  input_click/input_scroll`. Руки агента вместе с capture (глаза).
  ОБЯЗАТЕЛЬНО `requires_confirmation` на всё + rate-limit — это самый
  опасный плагин из списка.

### CAP-06 / CAP-07 · `wifi` / `bluetooth`

- NM D-Bus: scan/connect/forget/radio; BlueZ: devices/pair/battery.
  zbus-стек готов; PSK остаётся в NM-профилях (плагин паролей не видит).
  Оба ждут PERMISSION_WIFI(20)/BLUETOOTH(21) — делать в один wire bump с
  PERMISSION_HOTKEY(24)/SCREEN/CAMERA/INPUT.

### EXI-07 · `scheduler`: IANA-таймзоны

- Сейчас фиксированный `tz_offset_min` — ломается на DST. Добавить
  `tz: "Europe/Berlin"`: chrono-tz, конверсия при материализации cron;
  старое поле оставить для совместимости.

### AGT-04 · Eval-harness агента

- Скриптованные fake-LLM сценарии (как в тестах agent: scripted
  chat_completion) в виде матрицы целей → golden-ответы (шаги, статусы).
  Прогон на PR к agent/ai. Проблема: регрессии поведения loop'а
  обнаруживаются только руками. Это дёшево — инфраструктура уже есть в
  тестах, нужно вынести наружу.

### INT-21 · Obsidian-мост для notes

- **Проблема:** notes лежат JSON-документами в database — их не видно в
  файловой системе, нельзя редактировать в Obsidian/редакторе и
  версионировать git'ом.
- **Что делает:** режим, при котором CRUD notes отображается на `.md`
  файлы в operator-defined vault-каталоге: front-matter (id/tags/created)
  + тело; `plugin.notes.changed` остаётся.
- **Как реализовать:** через PERMISSION_FILES_READ/WRITE и root-allowlist
  filesystem (vault-папка = корень), либо прямой файловый IO в самом
  notes с теми же пермиссиями. Импорт существующего vault'а — разовый
  скан + индексация заголовков в vector-db («о чём я писал»). Атомарная
  запись — tmp-file + rename.

### INT-22 · Email→календарь: извлечение событий из писем

- **Проблема:** бронирования/приглашения приходят письмами без .ics —
  переносить даты руками лень.
- **Что делает:** цель-шаблон агента или automations rule: письмо от
  паттерна → ai извлекает {title, start, end, location} → черновик
  события оператору → `calendar_create`.
- **Как реализовать:** после INT-07 (тела писем); extraction —
  структурированный JSON-вывод chat_completion; подтверждение через
  requires_confirmation механику агента. Композиция существующих кусков.

### CAP-09 · `library` — индекс фото/музыки/видео (слой индексации, НЕ плеер)

- **Проблема:** «включи мою музыку», «покажи фото с отпуска» — sound/media
  играют то, что дали, но коллекции никто не индексирует. ВАЖНО: ответ не
  в слиянии плееров (см. раздел «Объединение плагинов»), а в общем
  индексе контента.
- **Что делает:** обход allowlist-корней (механика filesystem): аудио
  (теги), фото (EXIF: дата/GPS), видео (длительность); метаданные в
  database, эмбеддинги названий/тегов в vector-db. Действия:
  `lib_search/lib_random/lib_recent`. Плейбек остаётся у существующих:
  музыка → sound_play, просмотр → launcher открывает viewer, управление →
  media (MPRIS).
- **Как реализовать:** инкрементальный скан по mtime-чекпоинту; OCR фото
  позже через capture; дедуп по sha256 первых N KB.

---

## P3 — someday / maybe

### HW-01 · ESP32 voice-satellite

- ESP-SR wake-word на железе → PCM-поток в домашний `stt_listen_start`
  (WebSocket/UDS→WiFi мостик). «Alexa, своя». Требует стабильного
  аудио-транспорта по сети — возможно, дождаться EXI-09 (WS в network)
  или писать прямой WS-клиент в ядро. Отдельный репозиторий
  `vynkor-satellite`.

### INT-09 · `mqtt`

- Постоянное соединение ломает модель request/response плагинов (как WS в
  KERNEL_PROTOCOL_TODO #4). Вариант без изменения протокола: короткие
  connect/publish/disconnect на каждый вызов (годится для команд, плохо
  для подписок). Подписки датчиков — только после stream-модели в ядре.

### INT-10 · Home Assistant bridge

- REST API HA + долгоживущий токен в vault: `ha_call {domain, service,
  entity}`. Для тех, у кого HA уже есть — не конкурирует с home-протоколом
  из root ROADMAP, а дополняет.

### INT-11 · Spotify Web API

- Queue/playlists/search там, где нет MPRIS (web-плеер, remote). OAuth
  PKCE через http_request, refresh в vault. Действия зеркалят media.

### INT-12 · `contacts`

- vCard-хранилище в database: `contact_upsert/contact_find`; нужно для
  осмысленного «напиши Джону» (email) и будущих SMS/звонков через Android
  relay. S — потому что это ещё одна thin schema над database, как notes.

### INT-13 · `files-index` — RAG по личным файлам

- Обход allowlist-корней filesystem, извлечение текста (md/txt/pdf —
  pdf через pdftotext argv-only), эмбеддинги в vector-db, `find_about
  {query}`. Агент: «где мой договор с X?» → путь + цитата. L из-за
  форматов и инкрементального переиндексирования (mtime-чекпоинт).

### INT-14 · Email→agent шлюз

- Спец-адрес; poll IMAP → письмо от хозяина → goal_start → ответ письмом.
  Управление машиной там, где Telegram заблокирован. Только после
  жёсткой аутентификации отправителя (PGP-подпись или secret-маркер).

### CLI-03 · Android: PTT и зеркала

- Consumer-сторона device-agent: кнопка PTT → mic-поток на домашний stt;
  sync.delta → локальные нотификации; виджет «спроси vyn». Kotlin-часть
  поверх существующего UniFFI-моста.

### CLI-04 · Трей-индикатор

- StatusNotifierItem: статус daemon (idle/listening/thinking), mute микрофона
  (mic_stop/start), последние turns. Данные через подписку на
  `plugin.daemon.state.changed`. Sway/Hyprland: waybar-модуль как первая
  реализация.

### CLI-05 · Браузерное расширение

- Native messaging → локальный vyn CLI, или прямо WS к ядру: «отправь
  вкладку агенту» (текст страницы → goal с контекстом), выделенный текст
  → notes. MV3 + офлайн-разрешения.

### AGT-05 · Мультиагент planner/worker

- Planner-цель порождает воркер-цели (research/search vs household),
  сводит результаты. Пока не доказана потребность — сложность координации
  выше пользы. Предпосылка: AGT-02 (background goals).

### AGT-06 · Бюджеты токенов

- `max_total_tokens` на goal + дневной лимит; счётчик из `usage` ответов
  ai копится в goal-doc. Stop-loss от расходования квоты провайдера.

### INF-03 · Мультихост mesh

- Несколько ядер (домашний ПК + сервер + ноут) видят друг друга: D-13
  sync как транспорт состояния + маршрутизация действий на удалённое
  ядро («включи на сервере»). XL, архитектурный документ обязателен;
  кандидат-транспорт — тот же Telegram/ntfy-стиль outbound или прямой
  mTLS-Windows между ядрами.

### INF-04 · `backup`

- Cron-задача: tar-снапшоты `~/.local/share/vyn/` + drop-ins + базы
  database/vector-db → локальная ротация + опционально restic/rclone на
  S3 (ключи в vault). `backup_now/backup_list`. Продаётся одной фразой:
  «вся память ассистента переживает смерть диска».

### EXI-08 · `ai`: батч-эмбеддинги + SSE

- `embedding` принимает массив input (README обещает «batch в будущем») —
  ускоряет files-index/memory в разы; стриминг ответов chat_completion
  (SSE через http_request chunked) — снизит latency первого токена для
  web/CLI клиентов. Второе зависит от стриминговой модели ядра (см.
  KERNEL_PROTOCOL_TODO #2) — иначе только батчи.

### EXI-09 · `network`: WebSocket

- Самый большой kernel-side айтем (KERNEL_PROTOCOL_TODO #4): дизайн-док в
  vynkor-core (ActionStreamChunk либо raw-канал), потом `ws_request/ws_subscribe`.
  Разблокирует mqtt/HA-realtime/home-протоколы.

### EXI-10 · Россыпь мелких улучшений

- `media`: actions Raise/Quit (MPRIS Root interface) — «закрой плеер»;
  `sound`: `sound_devices` листинг sink'ов; `launcher`: fuzzy-ранжирование
  по частоте запусков (частоты в database); `system`: температуры CPU
  (фундамент для metrics); `notify`: action-кнопки libnotify (сложно,
  idle-callback).

### INT-23 · YouTube «включи X»

- Поиск через piped/invidious public API (outbound, без ключей) →
  `launch` браузера с URL или mpv через launcher. Голосовое «включи
  лекцию про rust» без открытия браузера руками.

### INT-24 · Google Photos «этот день N лет назад»

- Library API (фото-скоуп того же OAuth из INT-03): раз в день искать
  медиа этого дня прошлых лет → вложить в брифинг (ссылка/миниатюра в
  ntfy-push, голосом — подпись). Эмоциональный вау-слой брифинга.

---

## Дополнительные идеи (второй проход)

| ID | Идея | Приоритет | Сложность | Суть | Статус |
|---|---|---|---|---|---|
| AGT-07 | Персистентные approval-политики per tool | P2 | S | Вместо only-per-goal `requires_confirmation`: operator-конфиг «для notify_send — всегда, для fs_delete — всегда спрашивать, для media — никогда». Файл рядом с `AGENT_PLUGIN_TOOLS_FILE`, читается при построении каталога | — |
| AGT-08 | Scheduled goals как документированный паттерн | P2 | S | Уже работает сегодня: scheduler dispatch → `goal_start`. Нужен только рецепт в USAGE.md + плейбук-примеры («каждое утро goal: брифинг») | ✅ docs/playbooks/scheduled-goals.md |
| INT-15 | `uptime`/health-monitor | P2 | M | Cron-ping хостов/URL (`http_request`), деградация → notify/ntfy; история статусов в database. Личный mini-status-page | ✅ плагин `uptime` |
| INT-16 | `speedtest` | P3 | S | Раз в N минут через известные endpoints, результат в metrics — корреляция «лагал голос = упал канал» | — |
| INT-17 | `printer` (CUPS) | P3 | S | `print_file` через lp/lpr argv-only в allowlist-очередь. Голосом распечатать PDF — нишево, но смешно дёшево | — |
| INT-18 | SMS/calls через Android-relay | P3 | L | Device-agent читает входящие SMS → sync.delta → automations; отправка через telephony API. «Скажи vyn, что я опаздываю» без телефона в руках | — |
| CLI-06 | GNOME/KDE global search provider | P3 | S | Desktop-search-provider интеграция: ввод в системном поиске → launcher/search/agent. Делает vyn частью ОС | — |
| INF-05 | Sandbox-hardening: seccomp-профили плагинов | P2 | L | Kernel-side: профиль syscall-фильтра per-plugin категорий (offline-плагинам сетевые syscalls не нужны вовсе). Усиливает M-03 политику лимитов | — |
| INF-06 | Документационный сайт из README/USAGE | P2 | M | mdBook/Zola поверх существующих README+USAGE+docs/*, автодеплой GH Pages; единый toc по плагинам | — |
| AGT-09 | Per-tool квоты/кулдауны в каталоге агента (`cooldown_ms`, `max_per_goal`) | P2 | S | Спам-петля LLM («notify_send ×20») отсекается на dispatch, не в плагине | ✅ |
| CLI-07 | Голосовые шорткаты: hotkey binding → playbook/цель | P2 | S | automations-rule типа «hotkey_pressed{binding:X} → playbook_run» — физические кнопки для сценариев | ✅ docs/playbooks/hotkey-shortcuts.md |
| INF-10 | Граф зависимостей плагинов в web (requires + ipc_targets из манифестов) | P3 | S | Кто кого вызывает — визуально; вау для README и отладки прав | — |
| INF-11 | Perf-бенчмарки в CI: time-to-first-audio (tts), latency шага goal | P3 | M | Ловим деградации до пользователей; критерий EXI-02 | — |
| SEC-01 | Threat-model документ: STRIDE по опасным плагинам (input/mic/hotkey/network/filesystem) | P2 | M | Что злоумышленник получает при компрометации каждого; обоснование границ «слушатель ≠ инъектор» | — |
| HYG-01 | ping-pong-rs → examples/, gated-write → reference/, README-ссылки обновить | P3 | S | Репо-гигиена после MRG-05 | ✅ |

### Детализация второго прохода

**AGT-09 · Per-tool квоты — P2, S.** Проблема: LLM в петле может зациклиться
на дешёвом действии («notify_send ×20 подряд») — пользователь завален,
провайдер платит за токены. Что делает: поля `cooldown_ms` и
`max_per_goal` в spec инструмента каталога; dispatch отсекает вызов с
ошибкой «quota exceeded» (модель видит её как обычный tool error и
меняет тактику). Как: счётчики per goal-doc (уже персистится), таймштамп
последнего вызова per tool в памяти процесса. Дефолты: cooldown=0,
max=16 — ничего не ломает у существующих операторов.

**CLI-07 · Голосовые шорткаты — P2, S.** Проблема: сценарии запускаются
голосом через цели агента — медленно и не бесплатно для рефлексных
действий («брифинг», «стоп музыка»). Что делает: automations-rule нового
типа `hotkey_pressed{binding} → playbook_run{...}` / прямая action-call;
hotkey уже публикует события, CAP-01 даёт движок правил. Настройка —
YAML правила или действие `shortcut_bind`. Физическая кнопка (F13–F24
через macro-pad) = сценарий.

**INF-10 · Граф зависимостей — P3, S.** Проблема: кто кого вызывает
(network←ai←agent←daemon...), не видно ни в одном месте — права и
blast radius приходится держать в голове. Что делает: vynkor-web читает
`list_plugins`+`get_manifest`, строит граф по `requires` + чьим действиям
соответствуют permissions манифеста; экспорт SVG для README. Без
kernel-side изменений.

**INF-11 · Perf-бенчмарки в CI — P3, M.** Проблема: латентность голосового
цикла никто не меряет систематически — регрессии замечаются ушами.
Что делает: cargo-bench/criterion набор: time-to-first-audio на
эталонном абзаце (после EXI-02 — ключевой критерий), latency одного шага
goal на scripted-LLM, p50/p95 `http_request`. Порог-гейт опционально
(±20% → warn). Сначала локально по руками, в CI после INF-09.

**SEC-01 · Threat-model — P2, M.** Проблема: плагинам раздаётся
потенциально опасное железо (mic, input-инъекция, global hotkeys,
файловая система) — модель угроз не формализована, решения принимаются
интуитивно. Что делает: документ docs/THREAT_MODEL.md: по каждому
опасному плагину — STRIDE-таблица (что получает атакующий при
компрометации процесса, какие lateral moves через ipc_targets/T-19),
обоснования границ (sound|mic раздельно, hotkey|input раздельно,
confirmation-gate на инъекцию ввода). Как: начать с input/mic/hotkey;
обновлять при каждом новом permission. Польза ещё и как маркетинг
(проект с threat-model вызывает доверие).

**HYG-01 · Репо-гигиена — P3, S.** Перенос ping-pong-rs → `examples/`,
gated-write → `reference/gated-write/` после MRG-05; обновить ссылки в
README/root ROADMAP/PLUGIN_AUTHORING; registry-записи НЕ трогать
(история версий остаётся). Полчаса работы одним коммитом.

## Объединение плагинов — анализ (2026-08-25)

> Вопрос: что слить в один плагин (пример: photo/music/videos → media)?
> Принципы слияния: **объединяем** при общем OS-ресурсе-владельце, одном
> пермишен-скоупе, общей кодовой базе/движке; **не объединяем** при разных
> наборах permissions, разных failure-domain'ах (краш одного убивает
> другое), ролях протокола, или когда раздельность даёт агенту
> независимое allowlisting. Дедупликация КОДА решается общими крейтами,
> а не слиянием процессов.

### Слияния, которые СТОИТ сделать

| Что | Во что | Почему | Оговорки |
|---|---|---|---|
| `tts` + `stt` | **`speech`** | Один движок (sherpa ONNX in-process), одинаковая обвязка cloud-провайдеров через gated `http_request`, spawn_blocking-паттерн, model-config; live-audit показал идентичный deadlock в обоих — фиксить один раз. Минус процесс (~4–8 МБ RSS) и половина дублированного кода | Разный failure-domain: сегфолт ONNX на битой модели хоронит и синтез, и распознавание. Mitigation: lazy-load движков внутри процесса; откат слияния дёшев (два манифеста поверх одного кода) |
| `wifi` + `bluetooth` (планируемые) | **`connectivity`** | Оба — zbus D-Bus стек (NM / BlueZ), оба ждут wire-bump пермишенов (enum 20/21) — одним плагином и одним bump'ом; одинаковая форма действий (scan/list/connect/disconnect/status) | Оператор грантует оба permission одному токену — ок |
| ntfy/Gotify push (INT-02) | **внутрь `notify`** (channels) | «Сообщить человеку» — один домен: `notify_send {channels:["desktop","ntfy"]}`. Агент не должен жонглировать двумя плагинами ради «сказать» | Desktop-канал остаётся дефолтом; inbox общий |
| screenshot/camera/OCR | уже поглощены `capture` (CAP-02) | Решено в root ROADMAP | — |
| `window` (CAP-04) | **опционально внутрь `system`** | Тот же PERMISSION_SYSTEM («shares scope» по root ROADMAP); ещё один backend-trait рядом с wpctl/brightnessctl | Если появятся compositor-specific ветки (Hyprland IPC + wlr + X11) и код распухнет — оставить отдельным |
| EXI-05 (fs delete/trash) | вытесняет `gated-write` как production | gated-write был reference-имплементацией confirmation-gate; `fs_delete{to_trash}` + requires_confirmation делает то же в основном filesystem | gated-write остаётся в репо как учебный референс |

### НЕ объединять (и почему)

| Пара | Почему нет |
|---|---|
| `media` ↔ `sound` | Разные владельцы: sound — ЕДИНОЛИЧНЫЙ владелец колонок для аудио vynkor (спавнит плееры), media — пульт к ЧУЖИМ плеерам через MPRIS (браузер, Spotify). Слияние ломает модель владения аудио — фундамент архитектуры |
| `notes` / `calendar` / `tasks`(INT-19) | Намеренно тонкие схемы над database; слияние = монолит с чужими permissions (calendar держит NOTIFY). Вместо этого — общий крейт-хелпер (doc CRUD, atomic id counter, event-publish boilerplate), как vynkor-wire для протокола |
| `sync` ↔ `sync-client` | Роли протокола D-13: хост и клиент сознательно разведены (клиенту не нужен STORAGE); устройство ставит только клиента |
| `search` ↔ `ai` | Гигиена прав: search держит SECRETS под ключи brave/tavily; раздельно агент allowlist'ит независимо («может искать, но не ходить в LLM») |
| `hotkey` ↔ `input`(CAP-05) | Противоположные направления безопасности: hotkey только СЛУШАЕТ события, input ИНЪЕКТИРУЕТ ввод. Один процесс с обоими правами = keylogger + remote control в одном бинаре |
| `metrics`(CAP-03) ↔ `system` | Разные permissions (STORAGE+EVENT_PUBLISH у metrics); общие парсеры /proc/sysfs вынести в общий крейт, плагины раздельно |
| `email` ↔ contacts (если будет) | Контакты нужны шире почты (agent addressing, дни рождения в брифинге). Пока YAGNI — agent спрашивает пользователя |

### Вместо слияния «photo/music/videos → media»: CAP-09 `library`

Прямое слияние плееров неверно (media↔sound выше). Правильный ход — слой
НАД ними: `library` индексирует коллекции (теги аудио, EXIF фото,
метаданные видео → database + vector-db), воспроизведение остаётся у
существующих владельцев: музыка → sound_play, видео/фото → launcher
открывает viewer, управление воспроизведением → media (MPRIS). Один
индекс обслуживает все три типа контента и сценарии «включи мою музыку» /
«покажи фото с отпуска».

### Экономика процесса

27 процессов ≈ 165 МБ RSS — само по себе не проблема. Реальная выгода
слияний — меньше дублированного кода и один тестовый контур на движок;
RAM — следствие, не цель. Не сливать ради цифр.

### Слияния как план действий (MRG)

**MRG-01 · `speech` = tts+stt — P1, M.** Решение принято (2026-08-25).
Шаги: (1) новый крейт `plugins/speech` переиспользует модули tts/stt
(движки sherpa, провайдеры, model-config) без переписывания; (2) манифест
`speech` с объединённым набором действий (`tts_*` + `stt_*` имена СОХРАТЬ
— это стабильная поверхность для daemon/agent); (3) lazy-load обоих
движков (второй грузится при первом обращении) против VA-пиков RLIMIT_AS
(урок launcher); (4) e2e: оба набора действий на fake-kernel; (5) старые
tts/stt пометить `deprecated` в registry, drop-in оператора меняется один
раз; (6) daemon/notify-`speak` переключить на speech (или оставить имена
действий — тогда НЕ трогать вообще). Ключевой инвариант: имена действий
не меняются → потребители не знают о слиянии.

**MRG-02 · `connectivity` = wifi+bluetooth — P2, M.** Делать сразу при
старте CAP-06/07, не после: один плагин с бэкенд-trait'ами NmBackend /
BluezBackend, пермишены PERMISSION_WIFI+PERMISSION_BLUETOOTH в одном
wire-bump вместе с 22–24.

**MRG-03 · notify channels — P1, S.** Внутрь notify: параметр
`channels:["desktop"(default),"ntfy","wall"]`, конфиг каналов в env
(`NOTIFY_PLUGIN_NTFY_URL`, ключ vault-first); inbox общий на все каналы.
INT-02 поглощается этим пунктом — отдельного ntfy-плагина не будет.

**MRG-04 · window → system — P3, S.** Только если код compositor-backends
умещается в ~третью часть system's backend-модулей; иначе раздельно.

**MRG-05 · gated-write → reference.** После EXI-05 перенести в
`reference/gated-write/` (HYG-01), README ссылается как на учебный пример
confirmation-gate; из registry не удалять (история версий).

### Архитектурные крейты (ARCH) — дедуп БЕЗ слияния плагинов

**ARCH-01 · scan-harness — P1, M.** Один и тот же скелёт уже написан
трижды (calendar reminders, scheduler) и понадобится ещё минимум трижды
(metrics, automations, rss-refresh, backup): тикер с catch-up → выборка
due-работы → dispatch → отметка до/после → `late`-флаг. Вынести в крейт
`vynkor-scanner`: trait `ScanSource { due(now) -> Vec<Work>, complete(w),
mark_fired_before_dispatch() }` + tokio select-ветка с настраиваемым
интервалом и startup-catchup семантикой. Начать при CAP-03 или CAP-01,
переводя calendar/scheduler постепенно.

**ARCH-02 · gauth — P2, M.** OAuth refresh-цикл (refresh-token в vault,
авто-refresh, clock-skew) понадобится INT-03, INT-19-sync, INT-24 —
написать один раз крейтом `gauth`; sync-модули только вызывают.

**ARCH-03 · crawl — P3, M.** Обход allowlist-корней + mtime-чекпоинт +
дедуп: общий для CAP-09 library и INT-13 files-index; извлечение
метаданных — колбэком.

**ARCH-04 · thin-schema helpers — P2, S.** Doc CRUD над database
(`note:`/`cal:`/`sched:` паттерн), atomic id counter, best-effort event
publish — сейчас копипаста между notes/calendar/scheduler/tasks; крейт
сокращает новый schema-плагин до сотен строк.

## Порядок работ (рекомендация)

0. **INT-20** (провайдер-рецепты/Gemini) — вероятно ноль кода, снимает
   барьер цены с ассистента.
1. **INF-01** (CI-релизы) — час работы, навсегда закрывает класс проблемы.
2. **EXI-01 + INT-02** (ICS, ntfy) — быстрые победы, мост в мир.
3. **AGT-01 + EXI-02** (memory, стриминг tts) — агент становится ассистентом.
4. **INT-01** (telegram) — новый клиент, проверка ценности на себе.
5. **CAP-01** (automations) — клей; затем PLAY-композиции поверх него.
6. Слияния: `speech` и `connectivity` делать в момент следующей
   содержательной правки этих плагинов, не отдельным рефакторингом.
