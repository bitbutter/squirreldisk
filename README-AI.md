# SquirrelDisk-AI

A fork of [SquirrelDisk](https://github.com/adileo/squirreldisk) (v2.6.1, AGPL-3.0) with an integrated AI advice panel: ask a language model (default GLM-5.3 via z.ai) what should go, without giving up the app's safety model.

The fork lives on the `ai-integration` branch. Upstream code is left intact wherever possible; every fork change is marked with a "SquirrelDisk-AI fork" comment. Binding design rules are in `DECISIONS.md`.

## What it adds

- **AI tab in the side panel** (FILES | AI tabs). After a scan: a local pattern pass totals the known cache/build folders (node_modules, `.cache`, `target` with a `debug`/`release` child, virtualenvs, browser caches, …) — that line costs nothing and sends nothing.
- **Analyze this scan** sends a compact dossier — not a tree dump: a depth-2 top-N skeleton with `<rest>` folding, same-name folder rollups, file-type rollups, and the local cache matches — to any OpenAI-compatible chat endpoint. The model answers with a triage: SAFE / VERIFY / YOUR CALL items with reasons, plus copy-only cleanup commands (`docker system prune`-style) when a cache has a manager.
- **Chips, not actions.** Every suggested path is validated against the live tree; valid ones can be staged into the collector individually or (SAFE only) in bulk. Deletion still goes through the app's own protected-location rules and confirm dialog. Nothing the model outputs is ever executed.
- **Chat + drill-down.** Follow-up questions in the same panel. If the model needs to see inside a folder, it asks via `EXPAND: <path>`; the app answers with a locally generated listing — a lookup, not a rescan.
- **Self-update is disabled** (DECISIONS.md, DEC-AI-004): an upstream release binary would overwrite the fork. Settings shows a static version line instead.
- **No advertising** (DEC-AI-009): the sponsor system (banner, on-device interest matching, daily view-count reporting) is removed entirely. The side panel and home page keep the freed space; the app's only outbound request is the AI endpoint, and only on an explicit click.
- **Clickable paths** (DEC-AI-008): path text in triage rows and expansion requests, and chat lines that name a resolvable path, zoom the sunburst to that item.

## Setup

Settings (gear icon → AI ADVICE):

- **API key** — stored in `settings.json` in the app's config folder (`%APPDATA%\SquirrelDisk` on Windows). On this machine it is preconfigured with the user-wide z.ai key.
- **Endpoint** — default `https://api.z.ai/api/coding/paas/v4/chat/completions`, the z.ai coding-plan endpoint; it accepts a coding-plan key, which the pay-as-you-go endpoint (`…/api/paas/v4/…`) rejects with "insufficient balance". Any OpenAI-compatible URL works.
- **Model** — default `glm-5.3`.

Privacy: nothing leaves the machine until you click Ask/Analyze/Send. Then folder names and sizes of the scanned folder go to the configured endpoint. File contents are never read; the dossier carries paths and numbers only.

## Build

```
cargo build --release
```

`~/.cargo/config`-level target dir redirection is fine; this checkout ships `.cargo/config.toml` pointing `target-dir` at `E:/rust-target` (edit or delete to taste). No new dependencies were added beyond what upstream already used (`ureq`, `serde`, `serde_json`).

Tests (headless, no GUI stack): `cargo test --no-default-features` — 33 tests including the AI module's dossier/triage/path-resolution units.

## Fork map

- `src/ai.rs` — dossier, pattern pass, prompts, triage parsing, path resolution, chat client
- `src/ui/ai_panel.rs` — the AI tab UI (tabs, teaser, triage tiers, chips, chat)
- `src/ui/app.rs` — session AI state, scan-completion teaser hook, self-update disabled
- `src/ui/view.rs` — side panel routed between the file list and the AI tab
- `src/ui/modals.rs` — AI ADVICE settings section; update controls removed
- `src/settings.rs` — `ai_api_key` / `ai_endpoint` / `ai_model`

## License

AGPL-3.0, as upstream. This is a personal fork; if you distribute it, ship the source.
