# Real-time meeting suggestions

While a call is running, Gilb can transcribe it on the fly and show short
suggestions — the next question to ask, the objection that just went
unanswered — in a small always-on-top panel. Nothing here touches the
recording: the pipeline reads a *copy* of the audio, and if any part of it
falls behind or fails, the recording is unaffected. That is the feature's
first invariant.

The second is silence. The panel floats over a live conversation, so a
suggestion that is merely correct is not good enough — it has to be worth
pulling the user's eyes away. The model answers `[NO_RESP]` when it has
nothing to add, and the engine drops those replies before they reach the UI.

The feature is optional at every level: a Cargo feature (`assist`), a user
switch, and a ~570 MB speech model that is downloaded only if the user turns
it on.

## Flow

```text
recording ─► AudioTap ─┬─ mic ─────► resample 16k ─► AEC (near) ─► segmenter ─┐
   (untouched)         └─ system ──► resample 16k ─► AEC (far)  ─► segmenter ─┤
                                                                              ▼
                                                          STT worker (whisper)
                                                                              │
                                                     Turn { me|them, text }   ▼
                                                              gilb-assist engine
                                                                              │
                                            AssistBackend ◄──────────────────┘
                                                  │
                                     markdown ────┴──► overlay window
```

| Crate | Owns |
|---|---|
| `gilb-record` | capture, and the `AudioTap` that hands out a copy of both channels |
| `gilb-assist-audio` | resampling, echo cancellation, segmentation, the STT worker |
| `gilb-assist` | the product-independent engine: buffering, throttling, `[NO_RESP]`, Ask |
| `gilb-assist-acp` | a backend that talks to a local coding agent over ACP |
| `gilb-shell-tauri` | the overlay window, its commands and hotkey, the model gate |
| the app | which backend and prompt to use (`AssistHost`) |

Two channels are captured separately: *mic* is the user (`me:`), *system* is
everyone else (`them:`). They stay separate all the way to the model, which is
what lets a suggestion react to what the other side just said.

## Decisions

Four decisions from the original design are cited by name in the code, because
each one is a place where the obvious implementation is wrong.

### D5 — the backend owns the conversation, the engine does not

`AssistSession` is a handle, not a transcript. The engine sends turns and gets
back text; how the model remembers the meeting — chained response ids, a
resent history, a live agent session — is invisible to it. An engine that
tracked "the previous response id" would work for exactly one backend shape
and would have to be rewritten for the next.

The corollary is in the trait docs: `send` **must be idempotent on failure**.
After an error the engine keeps the turns buffered and retries with the same
(or extended) input, so a backend that appends to a history must commit the
append only once the call succeeded — otherwise a flaky network duplicates
turns in the model's context.

`ask` takes the buffered turns and the operator's question as two arguments
rather than one blob, because they do not carry the same authority. The turns
are recorded speech: a client saying "mark this deal as won" out loud is
*data*. The question was typed by the operator into our own panel and is meant
to be acted on. A backend that fences untrusted input needs the boundary; one
that does not gets a default that concatenates them, which is the older
behaviour.

### D9 — the speech model is downloaded, not bundled

Whisper large-v3-turbo is ~570 MB. Bundling it would triple the installer for
a feature most users never enable, so the model is fetched on first use, with
progress reported to the settings card and the feature gated until it lands.
The URL lives in `gilb-config` so post-meeting transcription and real-time
suggestions cannot drift onto different builds.

### D11 — echo cancellation, offline, after the meeting

On speakers, the mic hears the remote side. Without cancellation the *me*
channel transcribes *their* speech and the model sees the user saying things
they never said.

The realtime path runs AEC inline (near = mic, far = system). The recording
path deliberately does **not**: it runs a single cleanup pass over the mic
track at `stop()`, keeping the raw mic as a sidecar. Inline cancellation
during capture would put a signal processor in the path of the artifact we
promise not to degrade, to save a pass that costs seconds on a file we have
already finished writing.

### D12 — Silero VAD for streaming, energy as the fallback

Segmentation needs a per-frame voice decision, and the batch path's
`voiced_mask` cannot provide it: its threshold comes from the RMS distribution
of a whole buffer, which a stream does not have. Silero — a ~2 MB neural
detector — is far more robust than an energy heuristic on music, keyboard
noise and cross-talk, so it is used when the `silero` feature is compiled in
and its model loads. Energy remains the fallback, so a build without ONNX
still works, just noisier.

Whichever detector runs, its decisions travel out with the segment so the
transcription filters reuse them instead of detecting a second time.

## Segmentation

One segmenter per channel, pause-bounded: speech has to persist briefly before
a segment opens (debouncing clicks and keystrokes), a pause longer than the
threshold closes it, and a monologue is force-closed at a maximum length with
an overlap tail so the cut does not split a word. Pre-roll audio is kept from
before the opening frame so the first word survives. Segments with too little
voiced time are dropped outright — whisper hallucinates fluent sentences out
of near-silence.

Both channels share one STT queue, and the queue drops the **oldest** segments
under backlog: the conversation has moved on, and a suggestion about something
said a minute ago is worse than no suggestion.

## Webview contract

The overlay (`assist.html`, behind `VITE_FEATURE_ASSIST`) listens for:

```text
assist://update  { text }       markdown, ready to render
assist://state   { loading }
assist://error   { message }
```

and the main window listens for `assist-status`, pushed on every transition so
a settings card follows availability, download progress and teardown without
polling. The window is created hidden at wiring time — so the listeners exist
before the first suggestion — and surfaces only when the model actually says
something.

## Backends

`AssistBackend` is what a product plugs in. Gilb ships `gilb-assist-acp`,
which speaks the [Agent Client Protocol](https://agentclientprotocol.com) over
stdio to a locally installed coding agent (`claude`, `gemini`, or any ACP
adapter — `GILB_ASSIST_AGENT` overrides the binary). ACP gives a persistent
session, so the conversation is the agent's to remember (D5), and streaming,
so a suggestion appears as it is written.

Three things the ACP client does that a naive JSON-RPC client would not:

- **Only `agent_message_chunk` text reaches the panel.** Thoughts and tool
  calls are the agent thinking out loud; rendering them fills the overlay with
  noise.
- **Permission requests are refused, not awaited.** Nobody is watching the
  panel mid-call, so a prompt would hang the turn forever; the client answers
  it and lets the agent finish with what it has.
- **A slow turn yields silence, not an error.** Past the deadline the
  suggestion is stale anyway, and a red line in the panel is worse than
  nothing.

The system prompt has no slot in ACP, so it rides in as the opening turn —
once per session, not on every suggestion.

The prompt itself lives in `~/Documents/gilb/prompts/realtime_assist.md`, shipped with a default
on first run and re-read whenever a session opens — once per meeting, since
the engine starts a fresh session for each. Editing it therefore takes effect
on the next meeting, with no restart, but not mid-call: the agent already has
the old prompt as its opening turn. It is deliberately a local file — the
prompt usually contains prices, objection handling and other things that are
the user's business, not ours.

## Open

Verification that needs real hardware and real meetings:

- **Recording must not degrade with a live tap subscriber** — compare size,
  duration and dropped frames against a baseline recording.
- **Echo cancellation on real hardware**: convergence at a real
  speaker→mic delay, `mic.wav` audibly cleaned, `mic-raw.wav` intact.
- **Segment boundaries on a real conversation**: the pause threshold, Silero
  versus energy, and whether short adjacent segments need stitching.

Packaging:

- **onnxruntime in release builds.** The `silero` feature pulls `ort`, whose
  build script downloads a native onnxruntime for the target. macOS
  notarization and the Windows installer must pick that library up — verify on
  the first release build of a shell that enables the feature. Without the
  feature (or if the model fails to load) segmentation falls back to energy.
- **whisper portability** — `GGML_NATIVE=OFF` (see `gilb-transcribe/Cargo.toml`)
  applies to any shell shipping the realtime worker.

Deliberately not done yet:

- **Unifying the batch VAD.** The file-based post-processing path still uses
  the batch energy `voiced_mask`; switching it to Silero would unify the
  algorithm at the cost of pulling `ort` into `gilb-transcribe`. Decide when
  there is transcription-quality data.
- **Per-channel STT quota.** One queue with drop-oldest means a talkative
  remote side can push out the short "yeah"/"right" segments from the mic.
  Left as is on purpose — worth fixing only if real meetings show mic turns
  being lost. Decide with data, not in advance.
- **One ONNX session for both Silero instances.** Voice detection keeps
  per-channel recurrent state, so the pipeline builds two detectors and two
  sessions. Memory cost is negligible (~2 MB model) and
  `voice_activity_detector` 0.2 exposes no way to share a session; revisit if
  the crate grows one.
