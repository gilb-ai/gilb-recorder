# Assist: findings and TODO

Follow-ups from building the real-time suggestions stack on the
`assist/gilb-assist` branch (July 2026). The full architecture doc lives in
the Rodnik repo — `rodnik-app-tauri/docs/REALTIME_ASSIST.md` — and stays the
source of truth for decisions (D1–D12); this file tracks what *gilb* still
owes, found while Rodnik adopted the stack first.

## Blockers before gilb wires the assist pipeline (stage 2)

- [ ] **One whisper model per process.** `transcribe_worker` (post-meeting
  transcription) and the realtime STT worker (`gilb-assist-audio`) each own a
  private `LocalTranscriber` with a private idle-unload timer. The moment a
  meeting ends, post-processing loads its copy while the realtime copy is
  still warm → 2 × ~570 MB, guaranteed, every meeting. Introduce a single
  model owner (one `Arc<LocalTranscriber>` handed to both workers, one shared
  idle accounting) when wiring assist into gilb-app. Verified 2026-07-29:
  Rodnik is unaffected (no client-side post-processing there).
- [ ] **Local `AssistConfig` + backend.** Rodnik implements the traits over
  `/api/v1/config` and an OpenAI-compatible proxy; gilb needs the file-based
  prompt (`~/.gilb/`, prompts deliberately not persisted server-side — see
  REALTIME_ASSIST §12.1) and the local-agent backend. ACP is the recommended
  transport over `claude -p` (persistent session holds context, streaming);
  reuse `gilb-analyzer`'s `resolve_claude_bin()` regardless of transport.
  Budget for: binary-missing gating, a text-only no-tool-loop mode (a
  permission prompt mid-meeting hangs the suggestion), latency measurement
  before committing to the UX, and the `[NO_RESP]` discipline — coding agents
  hate staying silent.
- [ ] **Overlay window on the gilb side.** The frontend (`assist.html` behind
  `VITE_FEATURE_ASSIST`) is shared and done; the window creation, capability
  file and events forwarding live in the shell — port Rodnik's
  `src-tauri/src/assist.rs` pattern (hidden-at-init window, content-protected,
  never focused).

## Release / packaging debt

- [ ] **onnxruntime in release builds.** The `silero` feature of
  `gilb-assist-audio` pulls `ort`, whose build script downloads a native
  onnxruntime for the target. macOS signing/notarization and the Windows
  installer must pick that library up — verify on the first release build of
  a shell that enables the feature. Fallback exists: without the feature (or
  if the model fails to load) segmentation uses the energy VAD.
- [ ] **whisper portability already noted** (`GGML_NATIVE=OFF` comment in
  `gilb-transcribe/Cargo.toml`) — same check applies to any shell that ships
  the realtime worker.

## Hardware verification debt (needs a Mac / Windows box)

- [ ] **macOS tap + finalization edits were never compiled.** The
  `gilb-record` changes (AudioTap in the SCK/cpal callbacks, shared
  `finalize_meeting_audio` in `stop()`) are cross-checked on Windows
  (`x86_64-pc-windows-gnu`) but macOS is `cfg`-gated off the Linux dev box.
  First `cargo check` on a Mac may surface trivial breakage.
- [ ] **Recording must not degrade with a live tap subscriber** — compare
  size/duration/dropped frames against a baseline recording.
- [ ] **Offline echo cancellation on real hardware**: convergence on a real
  speaker→mic delay, `mic.wav` audibly cleaned, `mic-raw.wav` intact; Windows
  additionally cleans the 48 kHz mp4 track.
- [ ] **Segment boundaries on a real conversation**: pause threshold (700 ms),
  Silero vs energy behaviour, whether short adjacent segments need stitching
  (`short_segment_merge_ms` idea from huggingface/speech-to-speech).

## Optional / later

- [ ] **Batch VAD unification.** The file-based post-processing path still
  uses the batch-adaptive energy `voiced_mask` (it has no streaming mask to
  reuse; realtime carries its detector's mask with each segment —
  `VoicedMask`, single VAD pass per audio). Switching that path to Silero
  would unify the algorithm too, at the cost of `ort` inside
  `gilb-transcribe`. Decide when transcription quality data exists.
- [ ] **Parakeet TDT 0.6B v3** as the STT plan-B if whisper large-v3-turbo
  misses the latency budget under recording load (faster RTF, Russian
  supported; the default in huggingface/speech-to-speech). Integration cost:
  ONNX/NeMo instead of the in-tree whisper.cpp.
- [ ] **Segmenter knobs in one place** for field tuning: pause, min/max
  segment, overlap, Silero threshold, queue depth (REALTIME_ASSIST §6.8).
