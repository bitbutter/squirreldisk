# SquirrelDisk-AI — Design Decisions

Binding rules for this fork. Change an entry by editing it in place (keep its
ID); retire one by deleting it. Unconfirmed ideas live in the Open section,
never as entries.

## AI

DEC-AI-001: The app computes, the model judges. Every number the model sees is pre-aggregated locally (tree skeleton with top-N and `<rest>` folding, same-name folder rollups, extension rollups, pattern-matched caches); the model never receives a raw tree dump.

Context: LLM judgment degrades in noise; local code does counting/sorting better and for free. The dossier is capped at a few hundred lines (~2–4k tokens).

DEC-AI-002: The model proposes, the collector disposes. Model output is data, never actions: every suggested path is validated against the live tree (`ai::resolve_path`), becomes a collector candidate via the existing `Session::collect`, and every deletion still passes the app's safety rules and confirm dialog. Suggested shell commands are copy-only; the app never executes anything the model said.

DEC-AI-003: No network call without a click. Scan completion runs a local pattern pass only (the teaser); the LLM is contacted solely on explicit Ask/Analyze/Send clicks. Auto-call settings do not exist.

Context: Until the first click, a scan is entirely local; outbound traffic carrying folder names must stay a deliberate act.

DEC-AI-004: The fork never self-updates. `auto_update`/`auto_install` are forced off at load, the periodic check is disabled, and Settings shows no update controls. Upstream release binaries would overwrite the fork executable and silently remove the AI integration.

DEC-AI-005: The default endpoint is z.ai's coding-plan OpenAI-compatible endpoint (`…/api/coding/paas/v4/chat/completions`), because the pay-as-you-go endpoint rejects coding-plan keys with "insufficient balance". Any OpenAI-compatible URL works via Settings.

DEC-AI-006: Tier affordance tracks provenance. SAFE items (pattern-matched or clearly regenerable) get a bulk "Collect all" into the staging basket; VERIFY items (model judgment) get per-item review; YOUR CALL items get information only, no collect button.

Context: Staging in the collector is reversible; the irreversible delete click always remains the app's own confirmed flow.

DEC-AI-007: The first reply is parsed as structured JSON (triage with tiers, commands, expand requests); chat follow-ups are plain prose. If the JSON parse fails, the raw reply is shown as prose rather than discarded. Expand requests travel as a trailing `EXPAND: <path>` line or the triage `expand` array, answered by a locally generated depth-2 listing — never a rescan.

## Open

- File age (mtime) is not in the tree; the dossier has no age signal. Adding `mtime` to `Node` would make "old and big" triage much stronger.
- Scan snapshots + diff ("where did my space go since last month") — computed locally, LLM narrates.
- Duplicate-finder results could feed the dossier when a dupes run exists for the same tree.
- Streaming replies (SSE) instead of blocking wait with a spinner.
- The sponsor banner and update window are untouched upstream code; the update window can no longer open (state stays Idle).
