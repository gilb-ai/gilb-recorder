# Захват экрана и оркестрация (prior-art-screen / -engine / -core)

Crates:
- `/Users/leonid/src/gilb/prior-art/crates/prior-art-screen/`
- `/Users/leonid/src/gilb/prior-art/crates/prior-art-engine/`
- `/Users/leonid/src/gilb/prior-art/crates/prior-art-core/`

Для Gilb: даже если мы не делаем видео-таймлапс, нам нужно **периодически
снимать snapshot экрана при значимых событиях** (для contex'а therblig'а).
Этот pipeline показывает как это делать **дёшево и без drop-frame**.

## 1. Технологии захвата по платформам

| OS | Primary | Fallback | Файл |
|----|---------|----------|------|
| macOS 12.3+ | ScreenCaptureKit (`sck_rs`, persistent SCStream) | xcap | `prior-art-screen/src/monitor.rs:142` |
| macOS <12.3 | xcap | — | — |
| Windows | Windows Graphics Capture (WGC, persistent session) | per-frame init | `wgc_capture.rs` |
| Linux | xcap (X11/Wayland) | — | — |

Ключевая идея: **persistent stream handles** (SCK / WGC) — переинициализация
оранжевого indicator'а на macOS не дёргается, нет border flash на Windows.

**Stream invalidation на sleep/unlock** (`lib.rs:38-66`) через atomic flag,
recreate ленивый на следующем capture.

## 2. Кадры: FPS, разрешение, качество

**Event-driven capture** (`event_driven_capture.rs`):

| Параметр | Default | Что значит |
|----------|---------|------------|
| `idle_capture_interval_ms` | 10 000 | fallback если нет активности |
| `min_capture_interval_ms` | 500 | debounce между триггерами |
| `visual_check_interval_ms` | 3 000 | периодическая проверка визуальных изменений |
| `max_skip_duration` | 10 s | safety valve — даже если кадры идентичны |
| `MAX_FPS` | 30 | в `prior-art-core/src/video.rs:11` |

**Quality presets** (`video.rs:64-86`):

| Preset | Max width | JPEG q |
|--------|-----------|--------|
| low | 1280 | 60 |
| balanced (default) | 1920 | 80 |
| high | 3840 | 85 |
| max | native | 92 |

JPEG writes — async `BufWriter` + `sync_all()`, файлы
`data/YYYY-MM-DD/{ts_ms}_m{monitor_id}.jpg`, downscale `FilterType::Triangle`.

**Видео** (если включено): `ffmpeg -f image2pipe -vcodec png -i - -vcodec
libx265 -preset {ultrafast|fast|medium} -crf {14..32} -bf 0 -movflags
frag_keyframe+empty_moov+default_base_moof`. Fragmented MP4 читается во время
записи (для live preview).

## 3. OCR

| OS | Primary | Fallback | Файл |
|----|---------|----------|------|
| macOS | Apple Vision (prior-art VN) | Tesseract | `apple.rs:106-200` |
| Windows | Windows.Graphics.Imaging OCRS (async) | Tesseract | `microsoft.rs` |
| Linux | Tesseract | Cloud (Unstructured) | `tesseract.rs` |

**Concurrency**: `OCR_SEMAPHORE` (`paired_capture.rs:48-52`) — capacity **1**.
Vision и Tesseract — sync C FFI, гнать параллельно бессмысленно (только
пожрёт RAM). Не-Windows запускают через `spawn_blocking`.

**Hybrid mode** (`paired_capture.rs:150-200`) — самая важная идея для нас:

```
tree_snapshot.text_content
    ├─ если "тонко" (<X символов) или пусто
    │    → запустить OCR + merge
    └─ если полно
         → пропустить OCR (экономия CPU)

Terminal apps     → всегда OCR (текст в буфере, не в a11y)
Canvas apps       → всегда OCR (Docs, Figma)
Regular apps      → a11y text если есть, иначе OCR
```

Это даёт **~80% экономию OCR** при сохранении полноты текста.

`strip_gutter_noise()` — regex убирает 30+ digit sequences (line-number
gutters в IDE).

## 4. Оркестратор (prior-art-engine, VisionManager)

```
main
└─ VisionManager (singleton per process)
   ├─ Frame Linker Actor (tokio::spawn)
   │    • mpsc::Receiver<LinkerMessage>
   │    • periodic 5s tick — evict stale
   │
   ├─ Monitor Watcher (tokio::spawn)
   │    • polls list_monitors() ~5s
   │    • hotplug detect → start/stop tasks
   │
   └─ Per-monitor tasks: DashMap<u32, JoinHandle>
        └─ event_driven_capture_loop
             • broadcast::Receiver<CaptureTriggerMsg> (shared)
             • ActivityFeed poll 250ms
             • visual-change detection
             • paired_capture() on trigger
```

## 5. Каналы и их роли

| Канал | Тип | Capacity | Назначение |
|-------|-----|----------|------------|
| CaptureTriggerMsg | `broadcast::Sender` | 64 | UI event triggers → все monitors |
| PowerProfile | `watch::Sender` | — | adaptive FPS tuning |
| HotFrame | `broadcast::Sender` | 256 | live frames → WS клиенты |
| LinkerMessage | `mpsc::Sender` | 1024 | recorder + capture → linker actor |
| EventBatch | timer | 100 rows / 1s | recorder → DB batched insert |

Broadcast drop-old при заполнении (триггер можно потерять, capture не
коалесцируется). MPSC к linker'у — настоящий backpressure (если linker tormoznul —
recorder flush ждёт).

## 6. Threading model

- **Pure tokio async** (multi-threaded runtime).
- `spawn_blocking` для OCR и любого sync C FFI.
- **Без rayon** — capture pipeline I/O-bound.
- Native OS threads — только в `prior-art` (event tap, Cocoa observer).

## 7. Frame Linker — связка a11y events ↔ frames

Это **ключевая абстракция** для нашей задачи. Решает проблему: UI event пришёл
в момент T, а snapshot экрана сделался в T+200мс — как их связать?

**Решение**: **correlation_id**, а не timestamp matching.

```
UI Event:
  1. a11y detects click → recorder assigns correlation_id (monotonic)
  2. CaptureTriggerMsg(correlation_id) → broadcast
  3. batch-flush ui_events row → EventPersisted(corr_id, row_id) → linker

Capture:
  1. accumulates CaptureTriggerMsg в debounce-окне (500мс)
  2. takes screenshot
  3. INSERT frames → frame_id
  4. FrameCaptured(frame_id, vec![correlation_ids]) → linker

Linker actor (pure state machine):
  • pending_events: HashMap<corr_id → row_id>
  • pending_frames: Vec<(frame_id, unmatched_corr_ids)>
  • EventPersisted → check pending_frames, emit LinkUpdate
  • FrameCaptured  → check pending_events, emit LinkUpdate
  • TTL 60s, capacity 4096 — защита от OOM на unmatched bursts

LinkUpdate → UPDATE ui_events SET frame_id = ? WHERE id = ? AND frame_id IS NULL
```

**Свойство**: order-independent. Не важно, кто пришёл первым — event или frame.

## 8. Multi-monitor

`recording_tasks: DashMap<u32, JoinHandle>` — независимые задачи на монитор.
Linker один на всю систему. `is_monitor_allowed()` фильтрует по
`monitor_ids` / `use_all_monitors`. Focus-aware FPS (Active / Warm / Cold).

## 9. Window / app filtering

`recording_config.rs:64-75`:
- `ignored_windows: Vec<String>` (substring match)
- `included_windows: Vec<String>` (allowlist)
- `ignore_incognito_windows: bool`
- `pause_on_drm_content: bool` — Netflix/Prime Video паузят capture
  (`drm_detector.rs` — bundle ID / URL match), но **audio/a11y продолжают**.

На macOS — `get_excluded_sck_window_ids()` отдаёт IDs прямо в
`SCStreamConfiguration` (OS-level exclusion, не наш фильтр).

`WindowOcrCache` (per-window) — не OCR'ить одно и то же окно дважды.

## 10. Privacy (детали в 04)

- Text PII через **Tinfoil-hosted enclave** (AMD SEV-SNP attestation +
  Sigstore + TLS pinning) — fails closed.
- SHA256 LRU cache (2000 / 1h TTL) для дедупликации.
- Image PII через **RFDETR v8 ONNX** (~108 MB) — закрашивает чёрным.
- `pii_backend: "local" | "tinfoil"`.

## 11. Power awareness

PowerProfile watch channel — все capture'ы переподписываются и тюнят:
- `idle_capture_interval_ms`
- `visual_change_threshold`
- JPEG quality + max width

Source: battery / thermal state. Это позволяет работать "ambient" на ноутбуке
от батареи.

## 12. Lifecycle

`VisionManager::start()` — lock status, spawn per-monitor tasks, monitor
watcher. `.stop()` — set stop flags (`Arc<AtomicBool>`), join all
`JoinHandle`'ы.

**Sleep monitor** (`sleep_monitor.rs`) — CFNotification callback на macOS →
`request_invalidation()` → stream handles invalidated → recreate ленивый.

**Schedule monitor** — pause/resume по work hours.

## 13. Data flow целиком

```
USER ACTION
  └→ prior-art: click + AX tree walk + UiEvent
       └→ correlation_id assignment
            ├→ CaptureTriggerMsg → broadcast → все monitors
            └→ EventBatch → DB (write_queue) → EventPersisted → linker

prior-art-engine: event_driven_capture_loop per monitor
  └→ debounce (500ms..10s) + visual-change check
       └→ paired_capture():
            • SCK/WGC/xcap capture_image()
            • SnapshotWriter JPEG (async + sync_all)
            • hybrid OCR (a11y + Vision/Tesseract, semaphore=1)
            • PII removal (optional, async)
            • INSERT frames → frame_id
            • HotFrameCache push + broadcast HotFrame
            • FrameCaptured → linker

Linker actor (singleton):
  • match correlation_id event ↔ frame
  • emit LinkUpdate → UPDATE ui_events SET frame_id

DB write queue:
  • coalesce ≤500 ops в одну BEGIN IMMEDIATE TX
  • commit, FTS5 triggers индексируют
```

## 14. Что забрать в Gilb

1. **Event-driven capture** (а не fixed FPS) — снимаем экран при значимом UI
   event'е, fallback на idle interval. Это идеально для therblig'ов:
   "состояние экрана до/после действия".
2. **Hybrid OCR (a11y first, OCR fallback)** + per-app dedup cache.
3. **Frame Linker actor pattern с correlation_id** — order-independent,
   простой, расширяемый.
4. **OCR semaphore (capacity 1)** — не сжигать CPU параллельным OCR.
5. **Per-monitor `DashMap<u32, JoinHandle>` + monitor watcher** — hotplug
   handling.
6. **PowerProfile watch channel** — capture tuning от состояния батареи.
7. **Atomic stop flags + lazy stream re-creation** — clean lifecycle.

## 15. Что упростить для Gilb v0

- Gilb должен работать на **macOS + Windows** (обе обязательны). MVP начинаем
  с macOS (**ScreenCaptureKit**), сразу после этого добавляем Windows
  (**Windows Graphics Capture** через тот же интерфейс). Linux пропускаем.
- **Без видео** — только JPEG snapshots при триггерах (на обеих платформах).
- OCR можно сначала **выключить** (полагаемся на a11y текст).
- PowerProfile тоже опционально — фиксированные дефолты вначале.
- Multi-monitor — позже; стартуем с primary.
- HotFrameCache + WebSocket broadcast — позже.
