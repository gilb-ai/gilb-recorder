# Redaction spec — Shannon's «выжимка»

Defines exactly how the analyzer (**Shannon**, `apps/gilb-analyzer`) turns raw
recorded activity (`~/.gilb/db.sqlite`) into the abstracted, de-identified
**slice** that is the *only* thing that leaves the machine. It is built
**deterministically in code** (never by sending raw data to an LLM) and is the
**auditable contract**: anyone can read this + the public crate and verify what
is collected and what is dropped.

It builds on what the recorder already masks at capture time: password fields →
`[masked]`, PII (cards/SSN/tokens) → `[redacted]`, and `password_flag` on rows.

## Inputs (from `gilb-db`, the `actions` / `tree_snapshots` rows)

Per action: `kind`, `captured_at`, `app_name`, `app_bundle_id`, `window_title`,
`browser_url`, `element_role`, `element_name`, `element_value`, `element_help`,
`element_identifier`, `element_frame`, `text_content`, `password_flag`,
`extra_json` (button/x/y/delta/key/…), `tree_snapshot_id`. Tree snapshots carry
a `root_json` role tree.

## Output: the slice

A **structured step log** grouped into **focus segments**
`(app, window/url, contiguous time)`:

```jsonc
segment {
  app:            string,    // app identity (e.g. "Google Chrome" / "chrome.exe")
  window_title?:  string,    // a LABEL only (rules W1–W3)
  url_host?:      string,    // browsers: host only
  url_path_shape?:string,    // path with dynamic ids generalized; NO query/fragment
  started_at:     string,    // ISO8601
  duration_s:     number,
  roles?:         { [role: string]: number },  // from tree snapshot: role histogram, no values
  steps:          Step[]
}
// Step = one of:
{ kind:"click",     role, name? }
{ kind:"text",      role, name?, len, value_type? }  // NEVER the text; value_type = coarse local category (see below); secure ⇒ {len, secure:true}
{ kind:"key",       key }                    // only nav/edit keys: Enter/Tab/Esc/Arrows/Home/End/PageUp/Down
{ kind:"scroll",    dir:"up"|"down" }
{ kind:"nav",       url_host, url_path_shape? }
{ kind:"clipboard", len?, value_type? }      // content dropped; value_type kept
{ kind:"focus",     app, window_title? }     // app/window switch
// Identical consecutive steps collapse to { …, repeat:k }.
```

## Field rules (keep / strip / transform)

| Source | Disposition |
|---|---|
| `kind` | **keep** → `Step.kind` |
| `captured_at` | **keep** at segment level (start + duration); per-step offset optional |
| `app_name` / `app_bundle_id` | **keep** (app identity) |
| `window_title` | **keep as label**, transforms W1–W3 |
| `browser_url` | **host + path-shape only**; DROP query, fragment, userinfo |
| `element_role` | **keep** |
| `element_name` | **keep with guards** N1–N3 |
| `element_value` | **DROP** (content) |
| `element_help` | **DROP** (free text, can carry content) |
| `element_identifier` | **keep** (stable a11y id) unless it trips N3 |
| `element_frame` (coords) | **DROP** |
| `text_content` | **DROP value; keep `len` + `value_type`** (see Content-type tags) |
| `password_flag=true` | content already `[masked]`; emit `{kind:"text", role, len, secure:true}` with **no name** |
| `extra_json` | keep nav `key`, scroll `dir`; **DROP** `x`/`y`/coords |
| tree `root_json` | reduce to **role histogram**; DROP names/values |

## Edge-case rules (the resolved opens)

- **N1 — `name == typed value`:** if `element_name` equals or is a substring of
  this row's `text_content`, **drop the name** (it's content, not a UI label).
- **N2 — length cap:** cap `element_name` at 64 chars; longer ⇒ likely content
  ⇒ **drop**.
- **N3 — looks like input/PII:** if `element_name`/`element_identifier` matches
  the PII regexes (email, card, long token) or is mostly digits ⇒ **drop**.
- **W (window_title):** apply the PII regexes (→ `[redacted]`) and the N2 length
  cap to the title; keep the rest as a label.
- **URL:** keep `scheme+host`; path segments that are numeric/UUID/long-hex →
  `:id`; drop query, fragment, userinfo entirely.
- **Excluded apps** (`password_masking::is_excluded_app` — password managers,
  LogonUI, UAC, etc.): the **whole segment is dropped**.

## Content-type tags (the intent lever)

To recover **intent** without leaking content, the redactor classifies each
typed value / clipboard payload into a **coarse, deterministic category** and
emits only that label (`value_type`) — never the value. The LLM then sees
"typed a **currency** into Amount" / "pasted a **card**" instead of the data.

- Computed **locally** by ordered regex/heuristics on `text_content` (and, if
  needed, `element_value`); **first match wins**; default `freeform`.
- **Never emit the value** — only `value_type` + `len`. The classifier is part
  of the public crate (auditable).
- Taxonomy (coarse on purpose): `email`, `url`, `phone`, `card`, `ssn`,
  `currency`, `number`, `date`, `uuid`, `token` (high-entropy), `path`,
  `freeform`, `empty`. PII categories (`email`/`card`/`ssn`/…) still set the tag
  and **drop the value** — consistent with the invariants.
- If the recorder already redacted the value (`[masked]`/`[redacted]`), the type
  is unknown → `value_type:"redacted"` (or omit); `len` from the placeholder.
- Secure fields (`password_flag`): no `value_type` (already `secure:true`).

This is the main dial for **intent fidelity vs privacy**: categories add a lot
of "what kind of thing" signal at ~zero content risk. Layer-2 eval
(`eval-slice`) measures whether it's enough.

## Budget / volume

Per run: a time window (since the last run) and/or top-N segments by activity;
collapse repeated consecutive steps (`repeat:k`); per-segment step cap and
segment cap, mirroring the snapshotter's existing limits.

## Invariants — what must NEVER appear in a slice

1. No `text_content` value, `element_value`, or clipboard content.
2. No URL query strings / tokens / userinfo.
3. No coordinates.
4. Nothing matching the PII regexes (card / SSN / long token / email).
5. Nothing from `is_excluded_app` segments.

These are exactly the assertions the verification enforces.

## Verification mechanism

Two layers — **safety** (Layer 1) and **utility** (Layer 2). Tightening Layer 1
is always re-validated against Layer 2 so we don't over-strip.

### Layer 1 — Redaction correctness (deterministic, in-repo, no LLM)

- **Golden fixtures** under `docs/redaction-fixtures/`:
  - `<case>.input.json` — synthetic raw action rows, **with planted secrets/PII**;
  - `<case>.slice.json` — the expected slice;
  - `<case>.secrets.json` — the list of planted secret strings.
- **Snapshot test** (becomes Shannon's `cargo test`): redactor(input) == slice.
- **Leak test:** assert none of `secrets.json` appears anywhere in the serialized
  slice (substring scan) — catches passthrough even if a snapshot is updated.
- Fixtures are **synthetic** → safe to keep in the public repo.

### Layer 2 — Prompting eval (is the slice still rich enough for Therbligs?)

The redaction is only useful if the **therblig-finder** prompt still produces
good Therbligs from the reduced slice. Mechanism (lives with the private prompt,
in `gilb-analyzer`):

- **Eval harness `bin/eval-slice`:** feed a slice (a Layer-1 `*.slice.json`, or a
  real `--dry-run` slice from Shannon) to the therblig-finder prompt via
  `claude -p` (reusing `bin/_lib.sh`), print the resulting Therbligs.
- **Human review:** are the Therbligs sensible — did over-redaction lose them?
- **Auto-grade (optional):** a judge prompt scores each run
  (coverage / specificity / hallucination) for regression tracking as the spec
  evolves.
- **A/B:** run the prompt on a richer slice vs the redacted slice to quantify
  what the redaction costs in Therblig quality.

This separates the two failure modes: Layer 1 proves *nothing sensitive leaks*;
Layer 2 proves *the LLM still works on the safe slice*.
