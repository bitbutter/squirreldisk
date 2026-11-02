# SquirrelDisk-AI — Design Decisions

Binding rules for this fork. Change an entry by editing it in place (keep its
ID); retire one by deleting it. Unconfirmed ideas live in the Open section,
never as entries.

## AI

DEC-AI-001: The app computes, the model judges. Every number the model sees is pre-aggregated locally (tree skeleton with top-N and `<rest>` folding, same-name folder rollups, extension rollups, pattern-matched caches); the model never receives a raw tree dump.

Context: LLM judgment degrades in noise; local code does counting/sorting better and for free. The dossier is capped at a few hundred lines (~2–4k tokens).

DEC-AI-002: The model proposes, the collector disposes. Model output is data, never actions: every suggested path is validated against the live tree (`ai::resolve_path`), becomes a collector candidate via the existing `Session::collect`, and every deletion still passes the app's safety rules and confirm dialog. Suggested shell commands are copy-only; the app never executes anything the model said.

DEC-AI-003: No network call without a click. Scan completion runs a local pattern pass only (the teaser) and opens the AI tab (unless the user deliberately switched back to FILES); the LLM is contacted solely on explicit Ask/Analyze/Send clicks. Auto-call settings do not exist.

Context: Until the first click, a scan is entirely local; outbound traffic carrying folder names must stay a deliberate act.

DEC-AI-004: The fork never self-updates. `auto_update`/`auto_install` are forced off at load, the periodic check is disabled, and Settings shows no update controls. Upstream release binaries would overwrite the fork executable and silently remove the AI integration.

DEC-AI-005: The default endpoint is z.ai's coding-plan OpenAI-compatible endpoint (`…/api/coding/paas/v4/chat/completions`), because the pay-as-you-go endpoint rejects coding-plan keys with "insufficient balance". Any OpenAI-compatible URL works via Settings. The API key is stored in cleartext in `settings.json` in the app's config folder (same as every other local tool on this machine); it is masked in the UI and never appears in toasts, errors or logs.

DEC-AI-006: Tier affordance tracks provenance. SAFE items (pattern-matched or clearly regenerable) get a bulk "Collect all" into the staging basket; VERIFY items (model judgment) get per-item review; YOUR CALL items get information only, no collect button.

Context: Staging in the collector is reversible; the irreversible delete click always remains the app's own confirmed flow.

DEC-AI-007: The first reply is parsed as structured JSON (triage with tiers, commands, expand requests); chat follow-ups are plain prose. If the JSON parse fails, the raw reply is shown as prose rather than discarded. Expand requests travel as a trailing `EXPAND: <path>` line or the triage `expand` array, answered by a locally generated depth-2 listing — never a rescan.
DEC-AI-008: Every path the AI panel shows is a navigation link. Path text in triage rows and expansion requests, and any chat line that names exactly one resolvable path, zooms the sunburst to that item on click (`Session::navigate`) — the same view change as clicking a slice. Navigation never selects for deletion; staging stays a separate explicit action ([+] or bulk buttons).

DEC-AI-009: The fork carries no advertising or sponsor surface: the sponsor module, banner widget, on-device interest matching and its daily view-count reporting are removed, and Settings has no toggles for them. After this removal the only outbound request the app ever makes on its own is none — the AI endpoint is contacted solely on an explicit click (see DEC-AI-003).

## SCAN

DEC-SCAN-001: Whole-volume NTFS scans on Windows go through a Master File Table reader (`src/scan/mft.rs`): boot sector → `$MFT` dataruns → sequential record stream. It engages only for volume-root scans of NTFS drives; everything else (subfolders, remote, non-elevated, unreadable MFT, any parse refusal) uses the upstream directory walker. The MFT engine must leave the tree untouched and restore progress semantics on every fallback path.

Context: WizTree-style speed — one sequential read of the MFT replaces per-file enumeration and stat calls. Reading `\.\C:` needs an elevated process; the fallback makes elevation optional, not required.

DEC-SCAN-002: The MFT engine treats disk contents as untrusted input. No offset, length or size read from a record may index memory without a bounds check. Pre-assembly failures (open, parse, unreadable MFT run) fall back to the directory walker with the tree untouched and progress semantics restored; a caught panic during assembly fails the scan with an error message instead — the tree may be half-assembled and the walker would double-count on top of it.

DEC-SCAN-003: File sizes are allocated sizes (non-resident `$DATA` allocated, `$FILE_NAME` allocated as fallback), matching the directory walker; hardlinked files count once per MFT record (one name, the best-namespace one), unlike the walker which keeps every directory entry but zeroes repeats' sizes; reparse points are mounts when directories, skipped when files; cancelling during the MFT read yields an empty tree (the user cancelled; nothing is assembled mid-stream).

Context: the per-record hardlink rule is deliberate — it is the file's real occupancy attributed once.

DEC-SCAN-004: The fork targets Windows only. Cross-platform code from upstream is left in place where harmless; no effort is spent keeping other targets building.

DEC-SCAN-005: Repeat whole-volume scans rebuild from the USN cache: after a successful MFT scan the engine writes the assembled arena plus a USN journal checkpoint to `<config_dir>/mft-cache-<drive>.bin` on a background thread, and the next scan of the same volume replays the journal's close-records since that checkpoint onto the cached tree instead of re-reading the MFT. Any doubt discards the cache and runs the full scan without asking: journal ID changed, checkpoint below the lowest valid USN (wrapped/reset), record parse hiccup, replay backlog over the cap, unknown parent for a created entry, magic/version/checksum mismatch. The replay is approximate where the tree is aggregated — files folded into a "small files" node carry no file reference number, so their changes land as aggregate staleness until the next full scan or a watcher refresh, and re-added files are sized by the walker's stat rules (a cloud placeholder counts 0 where an MFT read would show its on-disk allocation).

Context: repeat scans drop to ~1s (cache load + journal replay). The cache is an integrity-checked local artifact written by the app itself; the replay validates the journal identity before any tree mutation, the patch runs on a detached node vector first so a refusal leaves the live tree untouched for the full-scan fallback (DEC-SCAN-002), and re-serialization after every scan keeps staleness from compounding.

## Open

- Scan snapshots + diff ("where did my space go since last month") — computed locally, LLM narrates.
- Duplicate-finder results could feed the dossier when a dupes run exists for the same tree.
- Streaming replies (SSE) instead of blocking wait with a spinner.
- The update window is untouched upstream code and can no longer open (updater state stays Idle, reachable only through the `SQD_UPDATE` debug env hook).
