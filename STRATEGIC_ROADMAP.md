# Gilb Strategic Roadmap
## From local recorder to desktop Process Mining platform

**Дата:** 2026-06-19
**Статус:** Draft / стратегическое обсуждение
**Язык:** RU (для обсуждения; formal docs в репо — по конвенции EN)

---

# Часть 1. Celonis — что это и почему это релевантно

## Что такое Celonis

**Celonis** — немецкая компания (Мюнхен, основана 2011), мировой лидер в **process mining** и **Execution Management Systems (EMS)**. Один из крупнейших SaaS-единорогов Европы. Клиенты: Siemens, ABB, Uber, Vodafone, BMW, Pfizer и т.д. Оценка ~$13B (2021, раунд), сейчас частная.

## Что такое Process Mining (дисциплина)

Process mining — это аналитическая дисциплина (основатель — **Wil van der Aalst**, "отец process mining"), которая по **event logs** реконструирует, визуализирует и анализирует **как процессы работают на самом деле**, а не как их себе представляют.

Ключевое: данные берутся из реальных систем (логи), а не из опросов или описанных моделей. Видна **правда** о процессе.

### Три ветви process mining:
1. **Discovery** — из логов построить модель процесса (нет априорной модели)
2. **Conformance** — сравнить реальность с "как должно быть" (девиации)
3. **Enhancement** — улучшить модель, добавив данные (время, ресурсы)

## Основной примитив: Event Log

Минимальная структура — таблица событий, где каждое событие имеет **минимум три атрибута**:

| case_id | activity | timestamp |
|---------|----------|-----------|
| order-1 | Create PO | 10:00 |
| order-1 | Approve | 10:15 |
| order-1 | Ship | 11:30 |

- **case_id** — один экземпляр процесса (заказ, тикет, ...). У Gilb это = **сессия или workflow instance**.
- **activity** — что сделали. У Gilb = **нормализованный шаг** (therblig).
- **timestamp** — когда.

Плюс опционально: resource (кто), cost, и др.

Формат обмена — **XES (eXtensible Event Stream)**, IEEE 1849 стандарт. Поддерживается ProM, PM4Py, Celonis.

## Как работает Celonis технически

```
1. Extraction — вытягивает event logs из ERP/CRM/ITSM (SAP, Salesforce, ServiceNow, ...)
                через коннекторы + ETL ("Event Collection")
2. Data Model — складывает в activity tables (event log) + case tables + metadata
3. Process Discovery — строит граф процесса (частота переходов между активностями)
4. Visualization — "spaghetti"/"happy path" диаграммы, variant analysis
5. Analysis — bottlenecks, conformance, KPI dashboards
6. Action (EMS) — не только показывает, но и действует: триггеры, автоматизация
```

### Флагманские фичи:
- **Variant analysis** — сколько разных путей выполнения одного процесса существует; "happy path" vs редкие/проблемные варианты
- **Bottleneck detection** — где по времени застревают (wait time между активностями)
- **Root-cause** — почему отклонения (машинное обучение по контексту)
- **Multi-Event Log** — связывание нескольких процессов сквозь отделы (end-to-end)
- **Execution Apps / Action Engine** — автоматизация на основе инсайтов

## Масштаб ценности

Celonis продаёт не "аналитику", а **найденные деньги/время**. Типичные кейсы:
- "Процесс закупок идёт через 4 лишних шага у 30% заказов → убираем → экономим $X"
- "Инвойсы зависают на approval 5 дней → bottleneck на конкретном менеджере"
- "12% кейсов идут по нестандартному пути → мошенничество/ошибки"

## Стандартные алгоритмы discovery

| Алгоритм | Идея | Плюс | Минус |
|----------|------|------|-------|
| **Alpha Miner** | Реконструкция причинности (causality) из последовательностей → Petri net | Фундамент, прост | Не держит шум, короткие циклы |
| **Heuristic Miner** | Frequency-based dependency метрики между активностями | Держит шум, циклы | Эвристическая сеть, не Petri net |
| **Inductive Miner** | Рекурсивное разрезание log (seq/choice/loop/parallel) → block-structured model | Масштабируется, sound | Грубая структуризация |
| **Fuzzy Miner** | Абстрактные "нечёткие" графы | Хорош для messy логов | Менее формальный |

Библиотеки: **PM4Py** (Python, de-facto стандарт), **ProM** (Java, академический).

## Что это значит для Gilb

**Gilb = process mining, применённый к desktop activity вместо ERP/CRM.**

Celonis смотрит в логи SAP. Gilb смотрит в логи кликов/клавиатуры/focus.
Celonis находит узкие места в "procure-to-pay". Gilb находит узкие места в "как команда реально работает за компьютером".

Это **не занятая ниша** на desktop-уровне. Celonis туда не идёт — у него фокус enterprise-systems. а individual knowledge work — это белое пятно.

---

# Часть 2. Обновлённое позиционирование Gilb

## Было (текущий README)
> "A desktop app that records what you do ... into a local SQLite database"

## Должно стать
> **"Gilb is desktop Process Mining.** It continuously captures how teams actually work at their computers — via OS accessibility APIs — and discovers the repetitive workflows, bottlenecks, and expertise gaps that no one notices. Where Codex Record & Replay captures one task on demand, Gilb mines behavior continuously across an organization."

## Слоган-направление
- ❌ "Activity recorder"
- ✅ "Process mining for knowledge work"
- ✅ "Discover the workflows you didn't know you had"

## Отстройка от Codex (не конкуренция — complementary)

| | Codex Record & Replay | Gilb |
|---|---|---|
| Что | Execution (записал → навык) | Discovery (нашёл → автоматизировать) |
| Триггер | On-demand (Record button) | Continuous (always-on) |
| Горизонт | 30 минут | дни/недели/месяцы |
| Знание юзера | Знает что записать | Не знает что упускает |
| Масштаб | Один человек | Команда / компания |
| Skill | Создаёт под известную задачу | **Находит** кандидатов на автоматизацию |

**Интеграционный нарратив:** Gilb находит паттерн → генерирует Codex skill → распространяет. Gilb = discovery layer, Codex = execution layer. Не конкуренты.

## Концептуальная основа: Therbligs

Имя **Gilb = Gilbreth** (Frank & Lillian Gilbreth, пионеры motion study, 1920-е). Они разбили ручной труд на **18 базовых элементов — therbligs** (Search, Select, Grasp, Transport, ...). Gilb делает то же для **цифровой работы**: определяет "digital therbligs" (click-button, type-in-field, copy-paste, switch-app, ...) и mine-ит из них процессы.

Это и название, и метафора продукта. Заложить в叙事.

---

# Часть 3. Архитектура сервера v1 (Postgres-only)

## Допущения v1
- Нет приватности как блокера (raw data на сервер: внутри контура клиента или наш сервер)
- **Только PostgreSQL** (без Kafka, без ClickHouse, без Redis)
- Batch ingestion (не realtime стриминг)
- Ночной pattern mining job

## Scale reality check (Postgres)

```
1000 users × ~1000 events/day = 1M events/day = ~30M/month = ~360M/year
```

Postgres это держит **при условиях**:
1. **Декларативное партиционирование** по дням (или неделям)
2. **Retention** — drop старых партиций (или архив)
3. **Pre-aggregation** — тяжёлые запросы из summary-таблиц, не из raw events
4. **JSONB с GIN** для гибких полей
5. Опционально **TimescaleDB** extension (остаётся "только PG", но с hypertables) — отложенный апгрейд

1M/day на raw партициях + materialized views — комфортно. На 10M/day — уже думать про TimescaleDB/ClickHouse (v2).

## Слои

```
┌──────────────────────────────────────────────────────────┐
│ EDGE: gilb-recorder (Rust, как сейчас)                   │
│   capture → local buffer → batch (gzip) → HTTP POST      │
└──────────────────────────┬───────────────────────────────┘
                           │ POST /v1/events (batch, idempotent)
                           ▼
┌──────────────────────────────────────────────────────────┐
│ INGESTION: Rust (axum)                                   │
│   auth (org/user/machine tokens) → validate → COPY в PG  │
└──────────────────────────┬───────────────────────────────┘
                           ▼
┌──────────────────────────────────────────────────────────┐
│ STORAGE: PostgreSQL 16+                                  │
│   • events (partitioned by day)                          │
│   • sessions, users, orgs, machines                      │
│   • discovered_patterns, workflow_variants               │
│   • skills (generated)                                   │
└──────────────────────────┬───────────────────────────────┘
                           ▼
┌──────────────────────────────────────────────────────────┐
│ ANALYSIS: Rust batch workers (cron / pg_cron / systemd)  │
│   • normalize events → therbligs                         │
│   • n-gram pattern mining                                │
│   • variant clustering                                   │
│   • LLM skill generation (external)                      │
└──────────────────────────┬───────────────────────────────┘
                           ▼
┌──────────────────────────────────────────────────────────┐
│ SERVING: Rust (axum) + web dashboard                     │
│   • REST/GraphQL query API                               │
│   • "Top repetitive workflows", "bottlenecks" dashboards │
│   • skill marketplace                                    │
└──────────────────────────────────────────────────────────┘
```

## Схема данных (ключевые таблицы)

```sql
-- Орг-структура
CREATE TABLE orgs      (id UUID PK, name TEXT, created_at TIMESTAMPTZ);
CREATE TABLE users     (id UUID PK, org_id UUID, email TEXT, created_at);
CREATE TABLE machines  (id UUID PK, org_id UUID, hostname TEXT, os TEXT);

CREATE TABLE sessions (
    id          UUID PK,
    org_id      UUID NOT NULL,
    user_id     UUID NOT NULL,
    machine_id  UUID NOT NULL,
    started_at  TIMESTAMPTZ NOT NULL,
    ended_at    TIMESTAMPTZ,
    gilb_version TEXT
);

-- RAW events. Партиционируется по captured_at (ежедневно).
CREATE TABLE events (
    event_id      UUID NOT NULL,           -- client-gen, idempotency key
    org_id        UUID NOT NULL,
    user_id       UUID NOT NULL,
    machine_id    UUID NOT NULL,
    session_id    UUID NOT NULL,
    captured_at   TIMESTAMPTZ NOT NULL,
    ingested_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    kind          TEXT NOT NULL,           -- click/text/key/scroll/clipboard/focus_change
    -- app context
    app_bundle_id TEXT,
    app_name      TEXT,
    window_title  TEXT,
    browser_url   TEXT,
    -- element context
    element_role     TEXT,
    element_name     TEXT,
    element_value    TEXT,
    element_frame    JSONB,                -- {x,y,w,h}
    selected_text    TEXT,
    selection_range  JSONB,                -- {start,end}
    -- action payload
    text_content  TEXT,
    password_flag BOOLEAN DEFAULT false,
    click_count   INT,
    modifiers     JSONB,                   -- ["Command","Shift"]
    extra         JSONB,
    PRIMARY KEY (event_id, captured_at)
) PARTITION BY RANGE (captured_at);

-- Ежедневная партиция (создаётся job'ом наперёд):
CREATE TABLE events_2026_06_19 PARTITION OF events
    FOR VALUES FROM ('2026-06-19') TO ('2026-06-20');

-- Индексы на каждой партиции:
CREATE INDEX ON events_2026_06_19 (org_id, captured_at DESC);
CREATE INDEX ON events_2026_06_19 (org_id, user_id, captured_at DESC);
CREATE INDEX ON events_2026_06_19 (org_id, kind, captured_at DESC);
```

```sql
-- Нормализованные "therbligs" (canonical steps) — lookup
CREATE TABLE therbligs (
    sig           TEXT PRIMARY KEY,         -- "click:com.google.chrome:AXButton"
    kind          TEXT,
    app_bundle_id TEXT,
    element_role  TEXT,
    label         TEXT,                      -- human-readable
    first_seen    TIMESTAMPTZ
);

-- Therblig-последовательность каждой сессии (pre-materialized для mining)
CREATE TABLE session_sequences (
    session_id    UUID,
    org_id        UUID,
    seq_index     INT,                       -- 1,2,3...
    therblig_sig  TEXT,
    captured_at   TIMESTAMPTZ,
    PRIMARY KEY (session_id, seq_index)
);
CREATE INDEX ON session_sequences (org_id, therblig_sig);

-- НАЙДЕННЫЕ паттерны (output mining job)
CREATE TABLE discovered_patterns (
    id              BIGSERIAL PK,
    org_id          UUID NOT NULL,
    pattern_hash    TEXT NOT NULL,            -- hash(normalized steps)
    pattern_steps   JSONB NOT NULL,           -- [sig, sig, ...]
    pattern_length  INT,
    occurrence_count INT NOT NULL,            -- сколько раз встретился
    distinct_users   INT NOT NULL,
    distinct_sessions INT NOT NULL,
    avg_duration_ms  INT,
    total_time_ms    BIGINT,                  -- occurrence × avg = потенциал автоматизации
    first_seen       TIMESTAMPTZ,
    last_seen        TIMESTAMPTZ,
    discovered_at    TIMESTAMPTZ DEFAULT now(),
    UNIQUE (org_id, pattern_hash)
);
CREATE INDEX ON discovered_patterns (org_id, occurrence_count DESC);

-- Сгенерированные skills
CREATE TABLE skills (
    id            UUID PK,
    org_id        UUID,
    pattern_id    BIGINT REFERENCES discovered_patterns(id),
    name          TEXT,
    markdown      TEXT,                       -- Codex-совместимый skill
    status        TEXT,                       -- draft/published
    created_at    TIMESTAMPTZ
);
```

## Ingestion protocol (v1)

```
POST /v1/events/batch
Authorization: Bearer <machine_token>
Content-Encoding: gzip

{
  "org_id": "...",
  "machine_id": "...",
  "schema_version": 1,
  "events": [ { ...event... }, ... ]     // 100-2000 events per batch
}
```

Требования:
- **Idempotent** по `event_id` (UPSERT / ON CONFLICT DO NOTHING)
- **Batched** на клиенте (каждые 30с или N событий)
- **Offline buffer** на клиенте (если сервер недоступен — копим локально)
- **Gzip** (сжимает текст events ~10x)
- **Schema versioning** для совместимой эволюции

Insert через `COPY` (bulk) или multi-row `INSERT ... ON CONFLICT`.

## Retention

- Raw events: 30-90 дней в hot партициях, дальше archive (export в S3/Parquet) или drop
- Session sequences + discovered_patterns: хранить долго (это ценность, не сырьё)
- Aggregates (daily/weekly rollups): навсегда

---

# Часть 4. Pattern Mining — алгоритм

## Шаг 0: Нормализация → therbligs

Raw events слишком шумные для mining. Нужен **canonical step signature**:

```
therblig_sig = kind : app_bundle_id : element_role
             (+ опционально нормализованный element_name)
```

Пример:
```
raw: click at (123,456) on button "Submit" in Chrome
canonical: "click:com.google.chrome:AXButton"
```

Игнорируем: координаты, timestamps, конкретные значения текста, выбранный текст.
Сохраняем: тип действия + где (app) + на чём (role).

Это **digital therblig**. Один therblig = один атом работы.

## Шаг 1: Построение последовательностей

Каждая сессия → упорядоченный список therblig-ов:
```
session-abc: [chrome:click:Link, chrome:type:TextField, gmail:click:Button, ...]
```

Decision: что считать "case" (границу workflow)?
- v1: вся сессия как одна последовательность + sliding windows
- v2: автоматическая сегментация (паузы > X сек = граница)

## Шаг 2: N-gram frequency mining (ядро v1)

Алгоритм (простой, эффективный, даёт первые инсайты):

```
for session in sessions(org):
    seq = therbligs(session)
    for window_len in [3, 4, 5, 6, 7, 8]:
        for i in 0..len(seq)-window_len:
            gram = seq[i : i+window_len]
            h = hash(gram)
            counts[h] += 1
            users[h].add(session.user_id)
            durations[h].push(time(seq[i+window_len]) - time(seq[i]))

patterns = [g for g in counts if counts[g] >= MIN_SUPPORT]
            сортируем по (distinct_users desc, occurrence_count desc)
```

**MIN_SUPPORT**: например, ≥5 вхождений И ≥2 разным юзерам (или 1 юзеру ≥20 раз).

Результат — таблица `discovered_patterns`:
```
pattern: [jira:click:Link, jira:click:Button, chrome:cmd:C, github:cmd:V, github:click:Button]
occurrences: 847
users: 23
avg_duration: 92s
total_time: 847 × 92s = ~21.7 часа/мес  ← потенциал автоматизации
```

## Шаг 3: Variant analysis (для каждого "похожего" процесса)

Группируем сессии, которые **начинаются и заканчиваются** одинаковыми therblig-ами (общий "case definition"), и смотрим внутреннюю вариативность:
```
"deploy workflow" variants:
  V1 (8 users,  18 steps, 4 min)  — happy path
  V2 (12 users, 23 steps, 7 min)  — лишние шаги
  V3 (3 users,  4 steps, 2 min)   — power-user shortcut
→ документируем V3, обучаем V2 → V1/V3
```

## Шаг 4: Bottleneck detection

Для каждой пары (therblig_i → therblig_i+1) считаем wait time. Топ медленных переходов = узкие места.

## Шаг 5 (v2+): Process mining proper

Когда данных много — заменить n-gram на настоящий discovery:
- **Heuristic Miner**: dependency matrix `D(a,b)` = насколько часто `a` предшествует `b`. Граф переходов с весами. Держит шум.
- **Inductive Miner**: рекурсивное разрезание → sound model. Масштабируется.
- Реализация: **PM4Py** (можно дёргать из Rust через PyO3/subprocess) или портировать heuristic miner на Rust.

## Шаг 6: Skill generation (замыкаем цикл)

Для top-N patterns (по `total_time`):
1. Грузим N representative sessions (raw events с контекстом)
2. Формируем промт в стиле Codex `create-replay-skill` (он у нас уже есть!)
3. LLM генерирует Markdown skill
4. Сохраняем в `skills`, публикуем в marketplace

**Красота:** промт Codex `create-replay-skill` (см. `codex-record-replay-prompts-original.md`) reusable на сервере 1-в-1 — только вместо одной записи подаём агрегированный pattern.

---

# Часть 5. Roadmap (фазы)

## Phase 0 — Capture quality (параллельно, ~2-4 нед)
Уже описано в `EVENT_EXPANSION_PLAN.md`:
- Selection (selected_text, selection_range)
- Click detail (click_count, modifiers)
- Drag & drop
- Focus element (полный context)

**Зачем:** качественнее therbligs → качественнее mining. Без click_count double-click и два single-click неразличимы.

## Phase 1 — Server MVP, Postgres-only (~3-4 нед)
1. **Клиент:** upload pipeline (batch + gzip + offline buffer + retry)
2. **Ingestion:** Rust/axum сервер, auth, idempotent batch insert
3. **Schema:** events (partitioned) + org/user/session таблицы
4. **Деплой:** docker-compose (postgres + ingestion + serving)

**Done =** события с машины появляются в Postgres, есть простой дашборд "events today".

## Phase 2 — Therbligs + первый mining (~2-3 нед)
1. **Normalization job:** events → therbligs → session_sequences
2. **N-gram mining job:** nightly, пишет в discovered_patterns
3. **API:** "Top 20 repetitive workflows" по org
4. **Дашборд:** список паттернов с частотой, пользователями, временем

**Done =** менеджер видит "ваши люди 847 раз делают X вручную, это 21ч/мес".

## Phase 3 — Variant & bottleneck analysis (~2-3 нед)
1. Case segmentation (границы workflow по паузам)
2. Variant clustering внутри case
3. Bottleneck detection (wait times)
4. Дашборд: "happy path" vs variants, heatmap bottleneck

## Phase 4 — Skill generation (~2-3 нед)
1. Промт pipeline по top patterns (используем Codex create-replay-skill формат)
2. LLM job → Markdown skills → skills table
3. Skill marketplace UI
4. Export в Codex (нативно совместимый формат)

## Phase 5 — Process mining proper (v2, опционально)
1. Heuristic Miner (PM4Py или порт на Rust)
2. Process graph visualization (spaghetti / happy path)
3. Conformance checking
4. При необходимости — миграция hot storage на ClickHouse/TimescaleDB (если scale >5M/day)

---

# Часть 6. Риски и решения

| Риск | Решение |
|------|---------|
| Postgres не тянет scale | Партиционирование + pre-aggregation тащат до ~5M/day; дальше TimescaleDB (всё ещё PG) или ClickHouse для hot layer |
| Pattern noise (много мусорных n-grams) | MIN_SUPPORT фильтры + пост-фильтрация (длина, entropy, длительность) + ручной approve patterns |
| Сегментация case (где workflow начинается/заканчивается) | Начать с whole-session + sliding window; v2 — паузы и семантические границы (focus change на key app) |
| Раздувание raw events | Retention на raw (30-90д), therbligs/patterns хранить вечно, archive в Parquet |
| Identity mapping (user → org) | Machine token при установке; привязка user/machine к org на provisioning |
| LLM стоимость skill generation | Только top-N patterns; батчинг; кеширование по pattern_hash |

---

# Часть 7. Следующие шаги (concrete)

1. **Согласовать позиционирование** — обновить README/CLAUDE.md под "desktop process mining" (могу перевести этот док на EN для commit).
2. **Финализировать Phase 0** (capture events) — без этого mining шумный.
3. **Спроектировать Phase 1 ingestion protocol** детально (контракт клиент↔сервер) — следующий документ.
4. **Прототип therblig normalization** на существующих данных в SQLite — проверить, что n-grams дают осмысленные паттерны до строительства сервера.

Готов перейти в любой из этих пунктов.

---

## Источники (по Celonis / process mining)

- [Celonis — Taking Process Mining to the Next Level](https://www.celonis.com/blog/process-mining-next-level)
- [Celonis Academy — What is an Event Log?](https://academy.celonis.com/learn/video/what-is-an-event-log)
- [Celonis — Execution Management System / Multi-Event Log](https://www.celonis.com/news/press/celonis-raises-the-bar-for-execution-management-as-only-solution-to-enable-optimization-of-multiple-interconnected-processes)
- [Alpha algorithm — Wikipedia](https://en.wikipedia.org/wiki/Alpha_algorithm)
- [Process mining algorithms simply explained — Decisions](https://decisions.com/blog/process-mining-algorithms-simply-explained)
- [Event logs & algorithms — ProcessMining.org](https://processmining.org/old-version/event-book.html)
- [Process Mining course (PDF) — Univ. La Rochelle](https://pageperso.univ-lr.fr/ahmed.hamdi/cours/UE_Process_Mining-cours.pdf)
