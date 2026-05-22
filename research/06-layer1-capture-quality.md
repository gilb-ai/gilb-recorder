# Слой 1: качество сбора сырых данных через a11y

Это **главный документ для текущей итерации Gilb**. Слои 2 (анализ) и 3
(agent skill) пока вне scope.

## Контракт Слоя 1

Слой 1 должен поставлять Слою 2 **полный, дедуплицированный, ordered поток
действий пользователя**, не убивая машину и переживая всё что может пойти
не так.

Четыре свойства, по которым оцениваем качество:

| Свойство | Что значит |
|----------|------------|
| **Идеально** (completeness) | ни один значимый input event и focus change не теряется в нормальной работе |
| **Масштабируемо** (scalability) | месяцы непрерывной записи без деградации write throughput и поиска |
| **Устойчиво** (robustness) | переживает sleep/wake, потерю permission, зависание AX, краши приложений, переполнение каналов |
| **Не нагружая** (lightweight) | < 5% CPU avg, < 500 MB RAM, < N GB/месяц диска при типичной работе |

Screenpipe реализует все четыре. Ниже — конкретные механики, которые надо
взять и адаптировать.

---

## 1. Completeness: что считать "значимым event'ом"

Из `screenpipe-a11y/src/events.rs:47-124` — полный список того, что
обязательно ловим на Слое 1:

| Канал | Источник macOS | Источник Windows | Зачем для Слоя 2 |
|-------|----------------|------------------|------------------|
| Click | CGEventTap LEFT/RIGHT/MOUSE_DOWN+UP | WH_MOUSE_LL | atomic "user clicked X" |
| Move | CGEventTap MOUSE_MOVED, threshold 5px | WH_MOUSE_LL | drag/drop, hover контекст |
| Scroll | CGEventTap SCROLL_WHEEL | WH_MOUSE_LL | "reading vs acting" сигнал |
| Key | CGEventTap KEY_DOWN/UP | WH_KEYBOARD_LL | shortcuts, control keys |
| Text | aggregated chars, timeout 300ms | aggregated chars | ввод данных как строка, не keycodes |
| AppSwitch | NSWorkspace `did_activate_app` | UIA focus change | граница "контекстного" therblig'а |
| WindowFocus | AX observer `focused_window_changed` | UIA focus change | смена окна в одном app |
| FocusedUIElement | AX `focused_ui_element_changed` | UIA focus | "user сейчас работает с полем X" |
| Clipboard | NSPasteboard.changeCount() poll | clipboard hook | copy/paste — частые therblig'и |
| ElementContext @ click | `AXUIElementCopyElementAtPosition` | UIA `ElementFromPoint` | role/name/value/bounds кликнутого элемента |
| TreeSnapshot (periodic) | full AX walk focused window | UIA tree walk | "состояние UI до/после действия" |

**Не теряем** на macOS:
- если **Input Monitoring не дан** → CGEventTap не работает → теряем
  click/key/scroll. Screenpipe в этом режиме всё ещё пишет clipboard и app
  switches (см. `01-a11y-capture.md` §9). Мы делаем так же — graceful
  degradation, а не падение.
- если **Accessibility не дан** → AX queries не работают → теряем
  ElementContext и tree. Сохраняем чистый input stream.

**Не теряем** на Windows: hooks работают без явного permission, UIA — тоже
(нет TCC).

---

## 2. Lightweight: бюджет CPU / RAM / диска

Из VISION.md screenpipe: `< 20% CPU, < 3 GB RAM`. Для Gilb v0 поставим
**жёстче**, т.к. мы не делаем OCR/audio/video:

- **CPU avg: < 5%** на типичной машине (idle 1%, активная работа 5-10% peak).
- **RAM steady-state: < 500 MB**.
- **Диск: < 1 GB / месяц** для actions+tree без snapshots (~оценка
  500 events/мин × 8 ч/день × 22 дня).

### Механики, дающие этот бюджет

#### 2.1 Adaptive FPS (activity_feed.rs:92-134)

Частота AX walk **подстраивается** под активность пользователя:

```text
keyboard burst (3+ kb in 500ms)     → 200 ms   (5 Hz)
active typing (kb idle < 300ms)     → 150 ms   (~7 Hz)
general activity                    → 200 ms   (5 Hz)
cooling                             → 500 ms   (2 Hz)
idle                                → 1 000 ms (1 Hz)
deep idle                           → 2 000 ms (0.5 Hz)
```

Это **главный лимитер CPU**. Без него — постоянный 10-20% CPU 24/7.

#### 2.2 Per-app `WalkBudget` (budget.rs:14-194)

Бюджет персональный для каждого `(app_name)`:

```text
avg walk >= 250 ms OR truncated >= 3 times
    → Critical:  60 s interval, 500 nodes cap, 250 ms hard timeout
else avg walk >= 150 ms
    → Heavy:     5 s interval,  1 000 nodes cap
else avg walk >= 50 ms
    → Moderate:  2 s interval,  2 000 nodes cap
else
    → Light:     200 ms interval, 5 000 nodes cap
```

Discord / Slack / Teams / VS Code с гигантским tree автоматически уходят в
Critical вместо того чтобы съесть машину. **Это must-have** — без него один
Electron app убивает performance целиком.

#### 2.3 SimHash dedup tree snapshots (tree/cache.rs)

```text
should_store(snapshot) =
    hamming_distance(prev_hash, new_hash) > 10 bits
    OR ttl_expired (60 s)
```

Скролл = ~10-20% контента меняется = ~5-10 bit diff = **не пишем дубль**. На
типичной "читаю reddit / scrolling docs" сессии — сокращает write volume в
~10×.

#### 2.4 OCR semaphore (capacity=1)

Для Gilb v0 OCR **выключен** (полагаемся на a11y текст). Но если включим —
строго semaphore=1, иначе Apple Vision / Tesseract пожрут все CPU.

#### 2.5 mmap + WAL tuning (см. `02-storage.md` §4)

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA cache_size = -65536;     -- 64 MB
PRAGMA mmap_size = 268435456;   -- 256 MB
PRAGMA temp_store = MEMORY;
PRAGMA wal_autocheckpoint = 4000;
```

mmap = 256 MB позволяет читать БД без copy в user space. WAL = одновременные
reads + 1 writer без блокировок.

#### 2.6 Write queue с батчингом ≤500 ops/TX (write_queue.rs)

500 clicks/keys/text в одной транзакции вместо 500 commit'ов = ~50× меньше
fsync'ов. На типичной нагрузке writer держит семафор ~5 мс на батч.

---

## 3. Robustness: что может пойти не так и как screenpipe это переживает

Это **самая недооценённая часть**. v0 без этих защит будет крашиться через
неделю.

### 3.1 Hot path **никогда** не блокируется

- CGEventTap callback **не дёргает AX напрямую** — кидает запрос в bounded
  channel (size=4) → worker thread читает AXUIElementCopyElementAtPosition
  (`platform/macos.rs:568-595`). Если worker занят — request **дропается**,
  но event tap не тормозит → нет потери input events.
- **`AX_QUERY_LOCK` mutex с try-lock** (`platform/macos.rs:28`): AX queries
  нельзя гонять параллельно (портят AppKit caches). Если занят — context
  capture **пропускает текущий click** (только в ElementContext, сам click
  всё равно пишется). Лучше пропустить контекст, чем повесить tap.
- На Windows: hook callback кладёт в bounded channel → UIA worker (отдельный
  COM-apartment thread) обрабатывает.

### 3.2 Lock-free shared state

`arc_swap::ArcSwap<Option<String>>` для `current_app`, `current_window`
(`platform/macos.rs:237-238`). Event tap callback читает `Relaxed` load
**без mutex'а**. Observer thread обновляет через CAS. Zero contention на
самом горячем пути.

### 3.3 Bounded channels везде

| Канал | Size |
|-------|------|
| context capture requests | 4 |
| clipboard capture requests | 4 |
| LinkerMessage (если у нас будет snapshot pairing) | 1024 |
| EventBatch → DB | 100 / 1 s timeout |

Никаких unbounded channels. Если consumer медленный — producer **дропает с
warn**, а не растёт в RAM.

### 3.4 Sleep / wake / permission revoke

- **Sleep monitor** (CFNotification на macOS) → atomic invalidation flag →
  при следующем capture stream handle пересоздаётся ленивый.
- **Permission revoke detection** → publish `PermissionEvent::Lost`
  (`screenpipe-events`), capture session переходит в reduced mode без
  падения. Когда permission вернулся → `Restored` → reattach.
- **AX observer detach** при app switch → reattach к новому pid
  (`platform/macos.rs:1100-1143`).

### 3.5 БД: WAL deadlock + checksum drift

- **`BEGIN IMMEDIATE`** (не DEFERRED) — захватывает write lock сразу, не
  даёт двум TX'ам эскалироваться и зайти в deadlock.
- **`ImmediateTx::Drop`** — async ROLLBACK + return conn в пул. На ошибке
  ROLLBACK — detach connection (лучше leak один slot, чем отравить пул).
- **Migration checksum self-heal** (`db.rs:477-495`): если случайно изменили
  старую миграцию — патчим checksum и продолжаем, а не падаем.
- **`busy_timeout = 5 s`** per connection.

### 3.6 БД emergency recovery (`db.rs:6970-7053`)

При corruption: VACUUM → REINDEX → ANALYZE → `integrity_check` +
`foreign_key_check` → ещё VACUUM. CLI команда `gilb db recover` обязательна
с v0.

### 3.7 Process supervision

Tauri app должен:
- автоматически перезапускать capture session при panic в `gilb-a11y` thread;
- логировать причину (sentry-style, опционально);
- показать пользователю badge "recording paused" а не молча умереть.

### 3.8 Schedule / pause

`schedule_monitor.rs` style — пользователь хочет иметь возможность
**временно отключать запись** (chai с боссом, банковский интерфейс).
Минимум: глобальная пауза + per-app exclusion list (`excluded_apps` —
1Password, Bitwarden, ...) с самого начала.

---

## 4. Scalability: запись месяцами без деградации

### 4.1 Партиционирование по времени через индексы

```sql
CREATE INDEX idx_actions_ts ON actions(ts);
CREATE INDEX idx_actions_session ON actions(session_id, ts);
CREATE INDEX idx_actions_app ON actions(app_name, ts);
```

SQLite не поддерживает реального partitioning, но composite индекс
`(app, ts)` и `(session, ts)` даёт partition pruning по факту.

### 4.2 Soft-evict snapshots, keep actions forever

Snapshots / большие tree JSON'ы — удаляем по retention (например, 30 дней
полные tree, 90 дней snapshots, потом только actions). Actions = малые
строки = храним вечно. Это даёт Слою 2 длинную историю для pattern mining,
не съедая диск.

### 4.3 FTS5 sync via triggers

```sql
CREATE VIRTUAL TABLE actions_fts USING fts5(
    text_content, element_name, window_title, app_name,
    id UNINDEXED
);
-- + AFTER INSERT/UPDATE/DELETE triggers
```

Триггеры синхронные. Screenpipe пробовал deferred indexer — откатил из-за
latency поиска (см. `02-storage.md` §8).

### 4.4 Partial indexes

```sql
CREATE INDEX idx_actions_unsynced ON actions(id) WHERE synced_at IS NULL;
```

Если/когда добавим sync — partial индексы экономят место и ускоряют pickup
queries.

### 4.5 WAL checkpoint discipline

`wal_autocheckpoint = 4000` (~16 MB) + при старте `wal_checkpoint(TRUNCATE)`
рекулирует stale WAL. Без этого WAL разрастается в десятки GB.

---

## 5. Чек-лист "Слой 1 готов"

Когда мы можем сказать "Слой 1 production-ready" и начать делать Слой 2:

- [ ] Полное покрытие event'ов из таблицы §1 на macOS.
- [ ] Скелет per-platform (`platform/macos.rs`, `platform/windows.rs` stub)
      с общим trait'ом — Windows будет реализован сразу после.
- [ ] Adaptive FPS реализован и наблюдаем (метрики "events/min vs
      tree_walks/min vs cpu%").
- [ ] Per-app WalkBudget с тестом на Discord/Slack/VS Code.
- [ ] SimHash dedup tree snapshots (метрика "% snapshots stored vs walked").
- [ ] Worker isolation: tap callback **не делает блокирующих AX queries**.
- [ ] Bounded channels везде, drop-with-warn вместо unbounded.
- [ ] ArcSwap для current_app/window.
- [ ] WAL + write queue + ImmediateTx + checkpoint discipline.
- [ ] Graceful degradation на потерю Input Monitoring и Accessibility.
- [ ] Sleep/wake handler через CFNotification → stream re-create.
- [ ] Excluded apps list по умолчанию (1Password, Bitwarden, KeePassXC,
      Keychain Access, ...).
- [ ] Password field detection (AXSecureTextField + heuristic).
- [ ] Глобальная пауза в UI и tray icon "recording / paused".
- [ ] `gilb db recover` CLI.
- [ ] Стресс-тест: 8 часов реальной работы — не падает, CPU avg < 5%,
      RAM steady-state < 500 MB, БД растёт линейно.

---

## 6. Что **не делать** на Слое 1 (чтобы не размывать фокус)

Эти вещи относятся к Слоям 2-3, и попытка их сделать "заодно" размоет
качество Слоя 1:

- Therblig detection / pattern mining.
- Sliding-window analytics.
- LLM-классификация действий.
- Agent skill generation / replay.
- Snapshot pairing с экранными кадрами (можно отложить — pure a11y stream
  для Слоя 1 достаточен на старте).
- Cloud sync.
- Sharing/export паттернов.
- Любые "smart" эвристики над actions ("это похоже на отправку отчёта").

Когда чек-лист §5 закрыт и Gilb пишет недели — переходим к Слою 2.
