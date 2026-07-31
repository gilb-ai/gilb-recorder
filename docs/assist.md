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

### Is it working?

One line per recognized utterance, at info:

```
assist: utterance speaker=them chars=64 at_secs=12.4
```

That single line says the whole chain is alive — audio reached the tap,
survived resampling and echo cancellation, closed a segment, and came back
from whisper as words. Without it a silent panel is indistinguishable from a
broken pipeline, and the two have very different fixes. Length rather than
text: this is a recording of someone's meeting, and it does not belong in a log
that gets pasted into bug reports. The words are in the meeting's transcript.

`tests/realtime_whisper.rs` runs the same path with the real model and asserts
words come back, skipping itself when no model is downloaded. The stub-based
test next to it proves the plumbing; only this one proves that whisper, fed by
*this* segmenter at *this* rate, returns speech rather than empty strings.

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
stdio to an agent already on the user's machine. ACP gives a persistent
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

### Silence, and its one exception

`[NO_RESP]` is the model's licence to stay out of a conversation it is only
watching. It does **not** apply to a question the user typed: they are sitting
in front of the panel having pressed Enter, and silence there is
indistinguishable from a broken feature. The engine strips the marker from an
answer to a direct question, and says so plainly if that is all there was.

### Where it is written down

Each meeting's folder gets an `assist.md` next to `video.mp4` and `audio.wav`:
the questions asked and the suggestions given, stamped with wall-clock time. A
meeting is a thing people open as a folder, so what the assistant said during
it belongs there rather than in a separate archive they have to learn about.
The path comes off the meeting row rather than being derived a second time —
two places computing the same path is how they end up disagreeing. Writing is
best-effort: the panel is the product, the file is a record of it, and a failed
write must never cost a suggestion.

### Choosing an agent

The switch turns on before anything is installed — that is the point. Flipping
it with no agent chosen shows the agents gilb knows, marking the ones this
machine has; picking one saves the choice and installs its adapter (a first
`npx` run downloads it), and the feature comes up when that lands. No step
where the user is told to go and run an npm command: that is a feature that
stays off forever.

While nothing is chosen the feature is simply off: no pipeline, no
transcription, no overlay — and the switch still shows *on* if that is what the
user asked for, because a switch that springs back to off reads as the app
refusing rather than as a step remaining. Turning it on with nothing chosen is
not an error; it is how someone says "set this up". The only genuine refusal is
when there is nothing they could do from here — a product gated on something
else, like a sign-in.

With no agent installed at all, the row says which ones to install rather than
"choose an agent": there is nothing to choose from, and asking anyway is how a
setup screen becomes a riddle.

The picker stays after the choice is made, with the current agent marked. A
first-run wizard that disappears leaves the user with a decision they cannot
revisit — and this one is "whose model hears my meetings", which is exactly the
decision people change their mind about. Switching re-installs and re-wires on
the spot.

Nothing is chosen by default, even when only one agent is installed. Which CLI
runs the suggestions decides whose model hears the meeting, and picking that on
someone's behalf because it happened to be first in a list is not a decision to
make for them. An agent that is not installed stays in the list, disabled —
knowing Codex is an option you do not have beats not knowing Codex is an
option.

The install shows an *indeterminate* bar, not a percentage. The whisper model
is our own download with a content-length, so that one reports a real
fraction; an adapter is fetched by `npx`, which tells us nothing we could turn
into a number. A bar that sweeps says "still working" honestly, where a fake
percentage jumping to 100% and then waiting is worse than none.

"Install" is a handshake, not a download: `AssistHost::prepare` opens a real
ACP session and throws the result away. A package that fetched but does not
answer `initialize` is not installed in any sense the user cares about, and
finding that out at setup beats finding it out mid-meeting.

### Finding an agent

The CLI a user has is usually *not* the thing that speaks ACP. `claude` is an
interactive REPL: pipe an `initialize` into it and nothing comes back, so the
session dies at the handshake timeout — a failure that looks like a hang and
says nothing about the cause. Claude Code reaches ACP through an adapter
package; Gemini speaks it itself behind `--experimental-acp`.

So gilb looks for the *harness* and works out what to run for it:

1. `GILB_ASSIST_AGENT`, if set — a wrapper script, an in-house adapter.
2. An adapter already installed (`claude-agent-acp`, or the older
   `claude-code-acp`).
3. The CLI itself, when it speaks ACP with a flag.
4. Otherwise `npx -y @agentclientprotocol/claude-agent-acp`, which fetches the
   adapter on first use and serves it from the npx cache afterwards.

Nobody is asked to install a second thing. That fourth step is why: an error
telling a user to run an npm command is a feature that does not work, and the
tools that pioneered this (Zed, block/buzz) fetch the adapter the same way. The
first cold start pays a download, so the handshake deadline is raised to three
minutes when the npx path is taken and left at thirty seconds otherwise.

### Which model it uses

By default, whatever the agent itself is configured with: Codex reads
`~/.codex/config.toml` (`model = …`), Claude Code reads
`~/.claude/settings.json` (`"model": …`) or `ANTHROPIC_MODEL`.

That default is often the wrong shape for this feature. A suggestion is worth
having for about fifteen seconds, and the model someone picked for interactive
coding — a heavyweight with high reasoning effort — spends longer than that
thinking. If the panel is mostly silent while the agent is clearly working,
this is the first place to look.

So the session's model is chosen in **Settings → Suggestions model**, without
touching the coding setup. The dropdown is populated by the agent itself —
gilb opens a throwaway session and reads `configOptions`, so the list can
never drift from what the agent actually offers — and "Agent default" shows
what not choosing means. The choice persists in prefs and applies from the
next session (a meeting already running keeps its model).

`GILB_ASSIST_MODEL` / `GILB_ASSIST_EFFORT` remain as dev overrides and win
over the saved choice:

```sh
GILB_ASSIST_MODEL=haiku GILB_ASSIST_EFFORT=low npm run tauri dev
```

This rides on ACP itself: `session/new` advertises the agent's knobs in
`configOptions` (the Claude Code adapter lists `model`, `effort` and the
permission `mode`; run `initialize` + `session/new` by hand to see an
adapter's set), and gilb applies the values via `session/set_config_option`
right after the handshake, before the first suggestion. Best-effort by design:
an adapter without the method — or without that particular knob — gets a
warning in the log, never a dead feature.

`GILB_ASSIST_AGENT_ARGS` replaces the arguments gilb passes, for an agent that
takes the model on its command line instead.

Whichever agent won is **named in the UI** — a chip under the switch, "Runs on
Claude Code". With more than one CLI installed the choice is ours to make but
not ours to hide: it decides which vendor sees the conversation, and a user
who did not know which one was picked cannot object to it. `AssistHost::
backend_label` is where a product answers.

The prompt itself lives in `~/Documents/Gilb/prompts/realtime_assist.md`, shipped with a default
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
