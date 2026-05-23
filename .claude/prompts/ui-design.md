# UI design guards (Tauri frontend)

Conditional prompt block referenced by `card-eval.md` and worker
prompts when a card touches the Tauri frontend (HTML, CSS, TS, JSX, or
`apps/gilb-app-tauri/src/`). When referenced, the PLAN's `## Approach`
must cite this file and the worker must read it before generating UI.

The goal is to prevent the default "AI-generic" look that the model
gravitates to when nothing constrains it — generic sans-serif on a
flat background with one purple accent.

## Anti-defaults (do NOT pick these without a reason)

- **Fonts.** No `Inter`, `Roboto`, `system-ui` fallback chain
  by default. Use the font stack already present in the codebase
  (`grep -r font-family apps/gilb-app-tauri/src/` first). If none
  exists, pick one with intent and write a one-line rationale in the
  commit body.
- **Colors.** No `#7c3aed` / generic purple as accent. No pure white
  (`#fff`) on pure black (`#000`) — both are visually harsh on
  macOS displays. Use the palette already present, or derive from the
  app's existing accent if one exists.
- **Layouts.** No floating top navbar with `backdrop-filter: blur()`.
  No 12-column grid scaffolds for screens with one or two regions.
  No rounded corners on every element (`border-radius` is a signal,
  not a default).
- **Empty states.** No purple cartoon illustration. No SVG of a robot,
  rocket, or magnifying glass. Use a one-line copy + maybe a small
  monochrome glyph from the existing icon set.
- **Hotlinked images.** No `unsplash.com`, `picsum.photos`,
  `via.placeholder.com`, or any external image URL. Local assets only.

## States to cover (not optional)

For every interactive element introduced (button, input, list row,
menu item), the PR diff must show all of:

- default
- hover
- focused (keyboard)
- active / pressed
- disabled (if the action can be unavailable)

A button with only a default and hover state is incomplete. If a state
genuinely doesn't apply (e.g., the element is never disabled in this
flow), note it in the PR body under `## What`.

## Tauri-specific

- The webview is macOS-first (`LSUIElement=1`, native window chrome).
  Respect `prefers-color-scheme` — both light and dark must look
  intentional. Don't ship dark-only or light-only without a reason.
- Modal dialogs: prefer native `<dialog>` over a hand-rolled
  `position: fixed` overlay unless there's a concrete behavior the
  native element can't provide.
- Don't reach for animation libraries (Framer Motion, GSAP) for
  effects that one CSS transition handles. The current bundle is
  small; keep it that way unless the card explicitly asks for motion.

## When a card requires substantial visual work

If the plan involves more than minor copy / state tweaks (i.e., real
visual design — new screen, new component family, redesign of an
existing flow), the triage agent should treat that as a research /
design step before code:

- Add to the PLAN's `## Approach`: a one-paragraph description of the
  intended look (texture, density, hierarchy, motion style) in words,
  not just file names.
- Add to `## Out of scope`: an explicit list of design directions
  considered and rejected, so the worker doesn't drift back to them.

## Self-check before the worker outputs `PR_URL=`

- Did I grep the existing CSS / theme files before adding new tokens?
- Are all interactive states covered?
- Did I pick fonts / colors / spacing with intent, or did I let the
  defaults pick them?
- Would a reviewer look at this and say "feels like the rest of the
  app" or "feels like a Tailwind starter"?

If any answer is "no" / "the second one", fix it before pushing.
