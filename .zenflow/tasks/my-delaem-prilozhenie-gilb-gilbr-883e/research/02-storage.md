# prior-art-db: хранение длительных данных

Crate: `/Users/leonid/src/gilb/prior-art/crates/prior-art-db/`

Для Gilb важно: как **хранить терабайты сырого input/UI потока за месяцы**,
чтобы поиск и анализ паттернов оставались быстрыми.

## 1. Стек

- **SQLite** (libsqlite3-sys, `bundled`, v0.26) — основная БД.
- **sqlx@0.7** с feature `migrate` — миграции из SQL-файлов.
- **sqlite-vec@0.1.3** — векторный поиск (speaker embeddings).
- **FTS5** (встроен в SQLite) — для полнотекстового поиска.

Почему SQLite: embedded, не требует сервиса, FTS5 + extensions из коробки,
WAL обеспечивает реальный concurrency reads + один writer.

## 2. Ключевые таблицы

### Media + frames

| Таблица | Что хранит |
|---------|-----------|
| `video_chunks` | id, file_path, device_name, fps, sync_id, cloud_blob_id, **evicted_at**, synced_at |
| `frames` | id, video_chunk_id FK, offset_index, timestamp, app_name, window_name, browser_url, device_name, **snapshot_path**, accessibility_text, full_text, elements_ref_frame_id, **content_hash**, **simhash**, sync_id |
| `audio_chunks` | id, file_path, timestamp, device_name, **transcription_status** (pending/transcribed/silent/failed), transcription_attempts |
| `audio_transcriptions` | id, audio_chunk_id FK, timestamp, transcription, engine, device_name, speaker_id, start_time, end_time |

Индексы: `idx_frames_timestamp`, `idx_frames_timestamp_device`,
`idx_frames_snapshot_path` (partial WHERE NOT NULL), `idx_video_chunks_device_name`.

### Unified elements (OCR + a11y)

`elements` (`migrations/20260301000000_create_elements_table.sql:1-61`):

```
id, frame_id FK, source ('ocr' | 'accessibility'),
role, text, parent_id (hierarchy!), depth,
left_bound, top_bound, width_bound, height_bound  -- 0..1 normalized
confidence, sort_order
```

`elements_fts` — FTS5 virtual table в external-content mode
(`20260301100000_fts_external_content.sql`) — индексирует `text + role`,
не дублируя данные.

### UI events

`ui_events` (`20250202000000_add_accessibility_and_input_tables.sql:69-149`):

```
id, timestamp, event_type ('click'|'key'|'text'|'scroll'|...),
x, y, button, key_code, modifiers,
text_content, text_length,
app_name, app_pid, window_title,
frame_id FK,  -- linked асинхронно через Frame Linker
sync_id
```

`ui_events_fts` (FTS5, content=ui_events) — поиск по text_content / app_name /
window_title / element_name.

### Diarization (audio speakers)

`speakers`, `speaker_embeddings` (`FLOAT[512]` через sqlite-vec),
`diarization_runs`, `diarization_segments`, `speaker_identity_evidence`.

### Memories (high-level)

`memories` (`20260310000000_create_memories.sql`): id, content, source, tags,
importance, frame_id, created_at + `memories_fts`.

### Meetings, tags, pipes

`meetings`, `meeting_transcript_segments`, `tags`, `vision_tags`, `audio_tags`,
плюс таблицы для pipe executions.

## 3. Миграции

- ~86 файлов, формат `YYYYMMDDHHMMSS_description.sql`.
- `sqlx::migrate!` macro подключает все.
- **Self-healing** (`db.rs:477-495`): если изменили checksum старой миграции —
  чинит mismatch и продолжает.
- **Column drift fix** (`db.rs:502-573`): `pragma_table_info` проверяет
  наличие event-driven и memories sync колонок, добавляет недостающие.

## 4. PRAGMA-настройки (db.rs:284-304)

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;       -- не fsync каждый commit в WAL
PRAGMA cache_size = -{cache_kb};   -- KiB
PRAGMA mmap_size = {bytes};        -- 256 MB default
PRAGMA temp_store = MEMORY;
PRAGMA wal_autocheckpoint = 4000;  -- ~16 MB
PRAGMA busy_timeout = 5000;        -- per-connection
```

**Tier-based config** (`prior-art-config/src/defaults.rs:93-140`):

| Tier | mmap | cache | read_pool | write_pool |
|------|------|-------|-----------|------------|
| High | 256 MB | 64 MB | 27 | 8 |
| Mid | 128 MB | 32 MB | 12 | 6 |
| Low | 32 MB | 8 MB | 5 | 4 |

WAL pre-conversion перед открытием пула (`db.rs:313-319`) — избегает race на
fresh DB. На старте — `wal_checkpoint(TRUNCATE)` для очистки stale WAL.

## 5. Конкурентность: два пула + семафоры

```rust
pub struct DatabaseManager {
    pub pool: SqlitePool,                 // read-only pool (27 conns)
    write_pool: SqlitePool,               // dedicated writer pool (8)
    write_semaphore: Arc<Semaphore>,      // capacity = 1 (serialize writers)
    heavy_read_semaphore: Arc<Semaphore>, // capacity = 2 (cap heavy queries)
    write_queue: WriteQueue,              // batching
}
```

- **READs** не ждут семафор, идут прямо в read_pool.
- **WRITEs** проходят через write_queue → один writer держит permit ~5 мс
  на батч ~200–500 строк.
- **Heavy reads** (минутные сканы типа `find_video_chunks`) ограничены до 2,
  чтобы не starve мелкие запросы.

## 6. Write queue — критичная оптимизация (write_queue.rs:1-350)

```
const MAX_BATCH_SIZE: usize = 500;
const CHANNEL_CAPACITY: usize = 4096;
```

Hot-path writes (insert frame, insert ui_event, insert ocr) подаются как
`WriteOp` enum в channel. Drain loop собирает до 500 операций → **один**
`BEGIN IMMEDIATE` → все queries → один `COMMIT`. Вместо 500 commit'ов
становится 1.

Поддерживаемые операции (выборочно):
`InsertSnapshotFrameWithOcr`, `InsertFramesBatch`, `InsertVideoChunkWithFps`,
`InsertAudioChunk`, `InsertAudioTranscription`, `InsertUiEvent`,
`InsertUiEventsBatch`, `CompactSnapshots`, `MarkSynced`, `SyncInsertFrame`, ...

**ImmediateTx wrapper** (`db.rs:104-184`):
- Использует `BEGIN IMMEDIATE` (не DEFERRED) — захватывает write lock сразу,
  избегает WAL deadlock когда два транзакции эскалируются.
- Owned semaphore permit живёт пока tx жива.
- На Drop без commit — async ROLLBACK + возврат conn в пул. Если ROLLBACK
  падает — detach connection (лучше потерять один слот, чем отравить пул).

## 7. Retention / эviction

Гениальное решение: **soft-evict media, keep DB rows**.

- `evict_media_in_range()` (`db.rs:6180-6299`) — file_path → '', evicted_at = now.
  Видео/JPEG удаляются с диска, но строки `frames` / `audio_chunks` остаются.
  Timeline и поиск работают (текст-то остался).
- `delete_time_range()` / `delete_time_range_batch()` — полное удаление с
  cleanup file paths.
- `estimate_evictable_bytes()` — preview для UI.
- Emergency recovery (`db.rs:6970-7053`): VACUUM → REINDEX → ANALYZE →
  `integrity_check` + `foreign_key_check` → ещё один VACUUM.

## 8. FTS5

Унифицировано в одну таблицу `frames_fts`
(`20260312000000_consolidate_search_to_frames_full_text.sql`):

```sql
CREATE VIRTUAL TABLE frames_fts USING fts5(
    full_text,           -- merged accessibility + OCR
    app_name,
    window_name,
    browser_url,
    id UNINDEXED
);
```

Синхронные триггеры `frames_ai/au/ad` (insert/update/delete) держат индекс
актуальным. До этого был эксперимент с deferred indexer'ом раз в 30 с
(`20260209000001_deferred_fts_indexing.sql`) — откатили: latency поиска важнее.

Параллельно: `audio_transcriptions_fts`, `elements_fts` (external content),
`memories_fts`, `ui_events_fts`.

## 9. Embeddings

- `ocr_text_embeddings` — BLOB-embeddings для OCR-текста (для семантического
  поиска по содержимому экрана).
- `speaker_embeddings` — `FLOAT[512]` через sqlite-vec с CHECK constraint
  `vec_length(embedding) == 512` — для diarization.

## 10. Где медиа

**Не в БД, а на диске** — в `~/.prior-art/` (или `$prior-art_DATA_DIR`):

```
~/.prior-art/
├── db.sqlite
├── data/
│   ├── YYYY-MM-DD/{timestamp_ms}_m{monitor_id}.jpg   -- snapshots
│   ├── device-<name>-<timestamp>.mp4                  -- video chunks
│   └── device-<name>-chunk-<id>.wav                   -- audio chunks
├── pipes/
├── logs/
└── secrets/
```

Cloud sync: `cloud_blob_id` колонка + URL `cloud://...` вместо локального пути.

## 11. Partial / composite indexes

- `idx_audio_chunks_pending_timestamp` (WHERE status='pending') — быстрый
  pickup для transcription worker'а.
- `idx_frames_snapshot_path` (WHERE snapshot_path IS NOT NULL).
- `idx_ui_events_unsynced` (WHERE synced_at IS NULL).
- `idx_diarization_segments_chunk_time` (audio_chunk_id, start_time, end_time).
- `idx_frames_video_chunk_id_timestamp`.
- `idx_pipe_exec_name_time` (pipe_name, id DESC).

## 12. Что забрать в Gilb

1. **SQLite + WAL + tier-based PRAGMA tuning** — готовый рецепт.
2. **Split read/write pools + single-permit write semaphore** — стандарт для
   write-heavy SQLite приложений.
3. **Write queue с батчингом 500 ops / TX** — это даёт в десятки раз больший
   throughput, чем "1 commit на insert". Для нашего потока кликов / keystrokes
   обязательно.
4. **BEGIN IMMEDIATE + ImmediateTx wrapper** — избегает WAL deadlock.
5. **Soft-evict pattern**: храним метаданные событий **вечно**, удаляем только
   тяжёлые media. Для therblig-анализа нам важна история действий, а не
   видеоархив.
6. **Unified `elements` таблица** с (source, role, text, bounds, parent_id) —
   ровно та форма, в которой мы хотим хранить UI-элементы для матчинга
   паттернов.
7. **`content_hash` + `simhash` колонки на уровне БД** — позволят дедуплицировать
   повторяющиеся "состояния экрана" при поиске therblig.
8. **FTS5 sync via triggers** — поиск "когда я последний раз отправлял отчёт"
   работает мгновенно.
9. **sqlx migrations + self-heal checksum** — production-ready.

## 13. Что упростить для Gilb v0

- Embeddings (sqlite-vec) и diarization — позже.
- `video_chunks` нам, скорее всего, вообще не нужны (мы про действия, а не
  про скрин-таймлапс). Хватит snapshot JPEG + a11y tree.
- Cloud sync — позже.
- 86 миграций нам не нужны: одна стартовая схема + дальше инкрементальные.
