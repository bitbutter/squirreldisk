//! AI advice layer (SquirrelDisk-AI fork).
//!
//! Split of work: this machine does all counting and aggregation, the model
//! only judges names and structure (DEC-AI-001). The model's suggestions are
//! data, never actions: paths are validated against the tree and land in the
//! collector, which keeps running the app's safety checks (DEC-AI-002).
//! No network call happens without a click on Ask/Analyze/Send (DEC-AI-003).

use crate::tree::{fmt_count, fmt_size, Kind, Tree};
use serde_json::json;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;


// ---------------------------------------------------------------------------
// Local pattern matching (the teaser + the dossier's cache section)

/// Directory names that are almost always regenerable caches, build output or
/// virtualenvs. This list only *finds* them cheaply and locally; the model
/// still adjudicates what actually lands in the tiers.
const JUNK_NAMES: &[&str] = &[
    "node_modules",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    ".cache",
    ".npm",
    ".gradle",
    ".m2",
    ".nuget",
    ".terraform",
    "DerivedData",
    "coverage",
    ".parcel-cache",
    ".turbo",
    ".venv",
    "venv",
    "Code Cache",
    "GPUCache",
    "Cache_Data",
];

/// A rust build dir is called `target`, but that name is too generic to match
/// blindly: require a `debug`/`release` child (or CARGO marker) as well.
fn is_rust_target(t: &Tree, id: u32) -> bool {
    t.children(id).any(|c| {
        let n = t.get(c).name.as_ref();
        n == "debug" || n == "release" || n == "CACHEDIR.TAG"
    })
}

fn junk_match(t: &Tree, id: u32) -> bool {
    let name = t.get(id).name.as_ref();
    if JUNK_NAMES.contains(&name) {
        return true;
    }
    name == "target" && is_rust_target(t, id)
}

/// Result of the local pass over a finished scan: no network, no judgment.
#[derive(Clone, Debug)]
pub struct Teaser {
    pub total: u64,
    pub count: usize,
    /// (name, occurrences, bytes), biggest first.
    pub top: Vec<(String, usize, u64)>,
}

/// One pass over the tree, matching [`JUNK_NAMES`]. Cheap: the arena walk of
/// a full-disk tree (dirs + big files only, small files are folded) is well
/// under a hundred milliseconds.
pub fn teaser(tree: &Tree) -> Option<Teaser> {
    let mut hits: HashMap<String, (usize, u64)> = HashMap::new();
    let mut total = 0u64;
    let mut count = 0usize;
    // (node, inside_junk): a junk dir inside a junk dir is already counted
    // by its parent — do not descend into matched subtrees
    let mut stack = vec![(tree.root, false)];
    while let Some((id, in_junk)) = stack.pop() {
        for c in tree.children(id) {
            let n = tree.get(c);
            let hit = !in_junk && n.kind == Kind::Dir && junk_match(tree, c);
            if hit {
                let e = hits.entry(n.name.to_string()).or_insert((0, 0));
                e.0 += 1;
                e.1 += n.size;
                total += n.size;
                count += 1;
            }
            stack.push((c, in_junk || hit));
        }
    }
    if hits.is_empty() {
        return None;
    }
    let mut top: Vec<(String, usize, u64)> = hits.into_iter().map(|(k, (n, b))| (k, n, b)).collect();
    top.sort_by(|a, b| b.2.cmp(&a.2));
    top.truncate(4);
    Some(Teaser { total, count, top })
}

// ---------------------------------------------------------------------------
// Dossier: what the model is allowed to see

/// Compact, pre-aggregated view of a scan. Never a raw tree dump: the tree
/// skeleton is 3 levels, top-6 (12 at the root) with `<rest>` folding and a
/// hard 500-line budget, plus rollups the model
/// could not compute itself (same-name dirs, file types, cache matches).
pub fn dossier(tree: &Tree, free: Option<u64>) -> String {
    let mut out = String::new();
    let root = tree.root;
    let n = tree.get(root);
    let _ = writeln!(out, "SCAN: {} · {} total · {} files · free: {}", tree.root_path, fmt_size(n.size), fmt_count(n.files as u64), free.map(fmt_size).unwrap_or_else(|| "unknown".into()));

    let _ = writeln!(out, "\nTREE (3 levels, top 6 per folder / 12 at the root, … = rest of each folder):");
    // three levels from the root, top-N per folder with `<rest>` folding,
    // hard-capped at ~500 lines (DEC-AI-001: a dossier, never a dump)
    let mut budget: usize = 500;
    subtree(&mut out, tree, root, 0, 6, 12, "", &mut budget);

    // caches
    if let Some(t) = teaser(tree) {
        let _ = writeln!(out, "\nKNOWN CACHE/BUILD DIRS (matched locally, always verify before deleting):");
        for (name, cnt, bytes) in t.top {
            let _ = writeln!(out, "  {}  {} × {}", fmt_size(bytes), cnt, name);
        }
    }

    // same-name dirs
    let _ = writeln!(out, "\nSAME-NAME FOLDERS (count ≥ 3, biggest first):");
    let mut by_name: HashMap<String, (usize, u64)> = HashMap::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        for c in tree.children(id) {
            stack.push(c);
            let n = tree.get(c);
            if n.kind == Kind::Dir {
                let e = by_name.entry(n.name.to_lowercase()).or_insert((0, 0));
                e.0 += 1;
                e.1 += n.size;
            }
        }
    }
    let mut names: Vec<(String, usize, u64)> = by_name.into_iter().map(|(k, (n, b))| (k, n, b)).collect();
    names.sort_by(|a, b| b.2.cmp(&a.2));
    let mut shown = 0;
    for (name, cnt, bytes) in names {
        if cnt >= 3 && shown < 10 {
            let _ = writeln!(out, "  {}  {} × {}", fmt_size(bytes), cnt, name);
            shown += 1;
        }
    }
    if shown == 0 {
        let _ = writeln!(out, "  (none)");
    }

    // file types
    let _ = writeln!(out, "\nFILE TYPES (top 12 by size):");
    let mut exts: HashMap<String, (u64, usize)> = HashMap::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        for c in tree.children(id) {
            stack.push(c);
            let n = tree.get(c);
            if n.kind == Kind::File {
                let ext = n.name.rsplit_once('.').map(|( _, e)| e.to_lowercase()).filter(|e| !e.is_empty() && e.len() <= 8).unwrap_or_default();
                let e = exts.entry(ext).or_insert((0, 0));
                e.0 += n.size;
                e.1 += 1;
            }
        }
    }
    let mut exts: Vec<(String, u64, usize)> = exts.into_iter().map(|(k, (b, c))| (k, b, c)).collect();
    exts.sort_by(|a, b| b.1.cmp(&a.1));
    for (ext, bytes, cnt) in exts.iter().take(12) {
        let _ = writeln!(out, "  {}  {} × {}", fmt_size(*bytes), cnt, if ext.is_empty() { "(no ext)" } else { ext });
    }

    out
}

fn subtree(out: &mut String, t: &Tree, id: u32, depth: usize, top: usize, root_top: usize, prefix: &str, budget: &mut usize) {
    let n = t.get(id);
    let cap = if depth == 0 { root_top } else { top };
    let pad_here = "  ".repeat(depth + 1);
    if *budget == 0 {
        let _ = writeln!(out, "{pad_here}… (listing truncated)");
        return;
    }
    let mut kids = t.sorted_children(id);
    let shown_cap = cap.min(*budget);
    let rest: Vec<u32> = if kids.len() > shown_cap { kids.split_off(shown_cap) } else { Vec::new() };
    *budget -= kids.len();
    let total = n.size.max(1);
    let pad = "  ".repeat(depth + 1);
    for c in kids {
        let k = t.get(c);
        let pct = (k.size as f64 * 100.0 / total as f64).round() as u64;
        let label = match k.kind {
            Kind::Dir => format!("{prefix}{}/", k.name),
            Kind::SmallFiles => format!("{prefix}({} smaller files)", fmt_count(k.files as u64)),
            Kind::Mount => format!("{prefix}{} (other volume)", k.name),
            _ => format!("{prefix}{}", k.name),
        };
        let denied = if k.denied() { "  (no access)" } else { "" };
        let _ = writeln!(out, "{pad}{} {:>3}% {}{}", fmt_size(k.size), pct, label, denied);
        if depth < 2 && t.can_enter(c) && k.size > 0 && k.kind == Kind::Dir {
            let next = format!("{prefix}{}/", k.name);
            // recursion enters even at budget 0: the callee then prints one
            // '(listing truncated)' marker and returns — bounded, honest
            subtree(out, t, c, depth + 1, top, root_top, &next, budget);
        }
    }
    if !rest.is_empty() {
        let bytes: u64 = rest.iter().map(|&c| t.get(c).size).sum();
        let pct = (bytes as f64 * 100.0 / total as f64).round() as u64;
        let _ = writeln!(out, "{pad}… {} {:>3}% ({} more)", fmt_size(bytes), pct, rest.len());
    }
}

/// Resolve a path the model gave to a tree node. The dossier shows
/// root-relative paths with `/`; the model may echo them like that, join them
/// to the scan root, recase them, or use the native separator. All resolve.
pub fn resolve_path(t: &Tree, p: &str) -> (u32, bool) {
    use crate::tree::NONE;
    let p = p.trim().trim_matches('"').trim_matches('`').trim();
    if p.is_empty() || p == "/" || p == "\\" {
        return (NONE, false);
    }
    let raw = p.replace('\\', "/").trim_end_matches('/').to_string();
    let root: String = t.root_path.replace('\\', "/").trim_end_matches('/').to_string();
    // strip a root prefix only on a path boundary: `C:/x` must not eat the
    // front of `C:/xy` (remainder must be empty or start with '/')
    let raw_rel = strip_root(&raw, &root).unwrap_or_else(|| raw.clone());
    if raw_rel.is_empty() {
        return (NONE, false); // the scan root itself is never a suggestion
    }
    // pass 1: exact case (the model echoes dossier paths verbatim)
    if let Some(id) = walk(t, t.root, raw_rel.split('/').filter(|c| !c.is_empty()), false) {
        return (id, true);
    }
    // pass 2: recased input — the first case-insensitive hit wins
    let lower_rel = raw_rel.to_lowercase();
    let root_l = root.to_lowercase();
    let lower_rel = strip_root(&lower_rel, &root_l).unwrap_or(lower_rel.clone());
    // case-insensitive fallback: the first sibling hit wins (fork targets
    // Windows/NTFS, where names are case-insensitive anyway)
    if let Some(id) = walk(t, t.root, lower_rel.split('/').filter(|c| !c.is_empty()), true) {
        return (id, true);
    }
    (NONE, false)
}

/// Strips `root` from `p` when what follows is a path boundary.
fn strip_root(p: &str, root: &str) -> Option<String> {
    let rest = p.strip_prefix(root)?;
    if rest.is_empty() {
        return Some(String::new());
    }
    if let Some(tail) = rest.strip_prefix('/') {
        return Some(tail.trim_start_matches('/').to_string());
    }
    // volume roots end in ':' (`C:`): `C:Users` means `C:/Users` there
    if root.ends_with(':') && !rest.starts_with('/') {
        return Some(rest.trim_start_matches('/').to_string());
    }
    None
}

/// Walks components from `at`; `ci` = case-insensitive comparison.
fn walk<'a, I: Iterator<Item = &'a str>>(t: &Tree, mut at: u32, comps: I, ci: bool) -> Option<u32> {
    for comp in comps {
        let hit = t.children(at).find(|&c| {
            let n = t.get(c).name.as_ref();
            if ci { n.to_lowercase() == comp } else { n == comp }
        });
        match hit {
            Some(c) => at = c,
            None => return None,
        }
    }
    Some(at)
}

/// A depth-2 top-10 listing of one folder, for follow-up "expand" requests.
pub fn expansion_of(tree: &Tree, path: &str) -> Option<String> {
    let (id, exact) = resolve_path(tree, path);
    if !exact || !tree.is_alive(id) || !tree.can_enter(id) {
        return None;
    }
    let rel = tree.rel_components(id).join("/");
    let mut out = format!("EXPANSION of {} ({}):\n", if rel.is_empty() { "(scan root)" } else { &rel }, fmt_size(tree.get(id).size));
    let mut budget: usize = 200;
    subtree(&mut out, tree, id, 0, 8, 10, "", &mut budget);
    Some(out)
}

// ---------------------------------------------------------------------------
// Model output contract

#[derive(serde::Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Triage {
    pub summary: String,
    pub reclaimable: String,
    pub safe: Vec<Item>,
    pub verify: Vec<Item>,
    pub your_call: Vec<Item>,
    pub commands: Vec<Cmd>,
    pub expand: Vec<String>,
}

#[derive(serde::Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Item {
    pub path: String,
    pub reason: String,
    #[serde(default)]
    pub guess: String,
    #[serde(default)]
    #[allow(dead_code)] // part of the model contract; kept for command-type suggestions
    pub action: String,
}

#[derive(serde::Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Cmd {
    pub command: String,
    #[serde(default)]
    pub why: String,
}

/// Parse the analyze reply. Tolerates surrounding prose and markdown fences.
pub fn parse_triage(reply: &str) -> Option<Triage> {
    let t = reply.trim();
    let candidates: Vec<&str> = if t.starts_with('{') {
        vec![t]
    } else {
        let mut v = Vec::new();
        if let Some(start) = t.find("```") {
            if let Some(body) = t[start..].strip_prefix("```json").or_else(|| t[start..].strip_prefix("```")) {
                if let Some(end) = body.find("```") {
                    v.push(&body[..end]);
                }
            }
        }
        if let (Some(a), Some(b)) = (t.find('{'), t.rfind('}')) {
            if a < b {
                v.push(&t[a..=b]);
            }
        }
        v
    };
    for c in candidates {
        if let Ok(tri) = serde_json::from_str::<Triage>(c) {
            if !tri.summary.is_empty() || !tri.safe.is_empty() || !tri.verify.is_empty() {
                return Some(tri);
            }
        }
    }
    None
}

/// Strip a trailing `EXPAND: <path>` request from a chat reply.
pub fn take_expand_request(reply: &str) -> (String, Option<String>) {
    for line in reply.lines().rev() {
        let l = line.trim();
        if let Some(p) = l.strip_prefix("EXPAND:") {
            let path = p.trim().trim_matches('`').to_string();
            if !path.is_empty() {
                let cut = reply.rfind(line).unwrap_or(reply.len());
                return (reply[..cut].trim_end().to_string(), Some(path));
            }
        }
        if !l.is_empty() {
            break; // only a trailing block counts
        }
    }
    (reply.trim().to_string(), None)
}

// ---------------------------------------------------------------------------
// Prompts

pub fn analyze_system_prompt() -> String {
    "You are a disk-cleanup advisor inside SquirrelDisk, a disk usage analyzer. \
The user is looking at a sunburst chart of the scanned folder. You get a compact dossier: folder names, sizes, file counts and local rollups. You never see file contents.
Rules:
- Judge only from names, sizes and structure. Paths in the TREE section are relative to the scan root. Use EXACTLY those relative paths in your reply; never invent or guess a path.
- Never suggest deleting system locations (Windows, Program Files, /System, /usr, /bin, boot files), anything under the scan root's parents, or anything marked (no access).
- When a cache has a manager (npm, docker, pip, cargo, gradle...), prefer the manager's clean/prune command over deleting folders by hand.
- safe = regenerable caches, build output, virtualenvs, temps. verify = you recognize what it is, the user should confirm. your_call = only the user can judge (personal media, documents, projects, backups).
Reply with ONLY a JSON object, no markdown, this exact shape:
{\"summary\":\"2-4 sentences\",\"reclaimable\":\"e.g. 41 GB easily, 60+ GB total\",\"safe\":[{\"path\":\"...\",\"reason\":\"...\",\"action\":\"delete|command\"}],\"verify\":[{\"path\":\"...\",\"reason\":\"...\"}],\"your_call\":[{\"path\":\"...\",\"guess\":\"...\"}],\"commands\":[{\"command\":\"...\",\"why\":\"...\"}],\"expand\":[\"path you want listed deeper, at most 3, only if genuinely unclear\"]}
Every path must appear verbatim in the dossier (the relative paths of the TREE section, or expansions you received). At most 12 items per list, biggest wins first. If nothing fits a list, use an empty list."
        .to_string()
}

pub fn chat_system_prompt() -> String {
    "You are a disk-cleanup advisor inside SquirrelDisk, a disk usage analyzer. \
You are having a follow-up conversation about the scan dossier you analyzed before (it is included below). Rules:
- Answer in plain text, short and concrete. No JSON.
- Use EXACTLY the relative paths from the dossier or earlier expansions; never invent paths.
- Prefer a cache manager's clean command over manual deletion.
- If you need to see inside a folder to answer, end your reply with a single line: EXPAND: <exact path from the dossier>. Otherwise do not use that line."
        .to_string()
}

// ---------------------------------------------------------------------------
// OpenAI-compatible chat client (ureq, one background thread per call)

#[derive(Clone, Debug)]
pub struct Msg {
    pub role: &'static str,
    pub content: String,
}

pub fn user(content: String) -> Msg {
    Msg { role: "user", content }
}
pub fn assistant(content: String) -> Msg {
    Msg { role: "assistant", content }
}

pub type ChatResult = Arc<Mutex<Option<Result<String, String>>>>;

/// POST a chat completion in the background; the result lands in `slot`,
/// then the egui context is asked to repaint. Never blocks the UI thread.
#[cfg(feature = "gui")]
pub fn spawn_chat(endpoint: &str, key: &str, model: &str, messages: &[Msg], temperature: f32, slot: ChatResult, ctx: eframe::egui::Context) {
    let body = json!({
        "model": model,
        "messages": messages.iter().map(|m| json!({"role": m.role, "content": m.content})).collect::<Vec<_>>(),
        "temperature": temperature,
        "max_tokens": 8192,
    });
    let url = endpoint.to_string();
    let key = key.to_string();
    let ctx2 = ctx.clone();
    std::thread::spawn(move || {
        let r = (|| -> Result<String, String> {
            // an agent with a global timeout: a hung request must not leave the
            // panel in Busy forever
            let agent: ureq::Agent = ureq::config::Config::builder()
                .timeout_global(Some(Duration::from_secs(180)))
                .build()
                .into();
            let mut resp = agent
                .post(&url)
                .header("Authorization", &format!("Bearer {key}"))
                .header("Content-Type", "application/json")
                .send_json(&body)
                .map_err(|e| format!("{e}"))?;
            // ureq's read_to_string caps the body at 10 MB by default: a
            // misconfigured endpoint cannot OOM the app
            let text = resp.body_mut().read_to_string().map_err(|e| format!("{e}"))?;
            let snippet: String = text.chars().take(300).collect();
            let v: serde_json::Value = serde_json::from_str(&text).map_err(|_| format!("not JSON: {snippet}"))?;
            if let Some(e) = v.get("error") {
                let msg = e.get("message").and_then(|m| m.as_str()).unwrap_or("API error");
                return Err(msg.to_string());
            }
            v["choices"][0]["message"]["content"]
                .as_str()
                .map(|s| s.to_string())
                .ok_or_else(|| "no content in reply".to_string())
        })();
        *slot.lock().unwrap() = Some(r);
        ctx2.request_repaint();
    });
}

// ---------------------------------------------------------------------------
// Per-session panel state

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Idle,
    Busy,
    Done,
    Error,
}

pub struct AiPanel {
    /// AI tab of the side panel open (vs. the file list).
    pub open: bool,
    pub teaser: Option<Teaser>,
    pub teaser_seen: bool,
    pub phase: Phase,
    pub slot: ChatResult,
    pub error: Option<String>,
    /// Dossier snapshot of the moment of the first Analyze click.
    pub dossier: String,
    /// What was sent: [user(dossier), assistant(reply), user(q), ...].
    pub history: Vec<Msg>,
    pub triage: Option<Triage>,
    /// Paths the model asked to see, not yet answered.
    pub follow_ups: Vec<String>,
    /// True while the first reply was a parseable triage.
    pub analyzed: bool,
    pub input: String,
    pub verify_open: bool,
    /// Scroll to the bottom of the conversation at the next frame (a chat
    /// message was sent or arrived). The triage reply never sets it: its
    /// summary is at the top and that is where reading starts.
    pub jump: bool,
    /// The user deliberately switched back to the FILES tab: stop
    /// auto-opening the AI tab when scans finish.
    pub manual_files: bool,
    /// Focus the chat input at the next frame (after a send or reply).
    pub refocus: bool,
    /// (message index, line index) → resolved node for chat path links.
    /// History is append-only, so indices are stable; invalidated on rescan.
    pub link_cache: std::collections::HashMap<(usize, usize), Option<u32>>,
}

impl Default for AiPanel {
    fn default() -> Self {
        AiPanel {
            open: false,
            teaser: None,
            teaser_seen: false,
            phase: Phase::Idle,
            slot: Arc::new(Mutex::new(None)),
            error: None,
            dossier: String::new(),
            history: Vec::new(),
            triage: None,
            follow_ups: Vec::new(),
            analyzed: false,
            input: String::new(),
            verify_open: false,
            jump: false,
            manual_files: false,
            refocus: false,
            link_cache: std::collections::HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Kind, Source};

    fn tree() -> Tree {
        let mut t = Tree::new("C:/x", "x", Source::Local);
        let a = t.add_child(0, "a", Kind::Dir, 0, 0);
        t.add_child_sized(a, "node_modules", Kind::Dir, 500, 10);
        let b = t.add_child(0, "b", Kind::Dir, 0, 0);
        t.add_child_sized(b, "node_modules", Kind::Dir, 700, 10);
        t.add_child_sized(b, "data.bin", Kind::File, 900, 1);
        t.add_child_sized(0, "small.bin", Kind::File, 50, 1);
        t
    }

    #[test]
    fn teaser_aggregates_junk() {
        let t = tree();
        let te = teaser(&t).expect("matches");
        assert_eq!(te.count, 2);
        assert_eq!(te.total, 1200);
        assert_eq!(te.top[0], ("node_modules".to_string(), 2, 1200));
    }

    #[test]
    fn dossier_mentions_rollups() {
        let d = dossier(&tree(), Some(42));
        assert!(d.contains("node_modules"), "{d}");
        assert!(d.contains(".bin"), "extension rollup missing: {d}");
        assert!(d.contains("free"), "{d}");
    }

    #[test]
    fn resolve_path_boundary_and_regressions() {
        let t = tree();
        // round-2 critical: a root prefix must not eat a longer sibling name.
        // `C:/xa` discriminates: the buggy strip turned it into `a` (a real
        // child) and resolved; the guard must refuse and resolve nothing.
        assert_eq!(resolve_path(&t, "C:/xa").1, false, "C:/xa must not resolve to child a");
        assert_eq!(resolve_path(&t, "C:/xy").1, false);
        assert_eq!(resolve_path(&t, "C:/x/a").1, true);
        // recased paths resolve through the lowercase pass
        let (id, ok) = resolve_path(&t, "A/NODE_MODULES");
        assert!(ok, "recased path resolves");
        assert_eq!(t.get(id).name.as_ref(), "node_modules");
        // the scan root itself is never a suggestion
        assert_eq!(resolve_path(&t, "C:/x").1, false);
        assert_eq!(resolve_path(&t, "").1, false);
    }

    #[test]
    fn dossier_is_budget_capped_and_folds_rest() {
        // 3-level tree: 30 root dirs (root cap 12 -> 18 folded), each with 6
        // subdirs, each with 6 leaves. Uncapped TREE output would be
        // 12 + 12*6 + 12*6*6 = 516 leaf+mid lines > the 500 budget, so the
        // budget must bite and the truncation marker must appear.
        let mut t = Tree::new("C:/w", "w", Source::Local);
        for d in 0..30 {
            let dir = t.add_child(0, &format!("d{d:02}"), Kind::Dir, 0, 0);
            for s1 in 0..6 {
                let sub = t.add_child(dir, &format!("s{s1}"), Kind::Dir, 0, 0);
                for f in 0..6 {
                    t.add_child_sized(sub, &format!("f{f:02}.bin"), Kind::File, 1000 + f as u64, 1);
                }
            }
        }
        let d = dossier(&t, None);
        let lines = d.lines().count();
        assert!(lines <= 620, "dossier too long: {lines}");
        assert!(d.contains("(18 more)"), "root rest-fold missing (30 dirs vs cap 12)");
        assert!(d.contains("listing truncated"), "budget exhaustion must be visible to the model");
    }

    #[test]
    fn triage_parses_with_prose_around_it() {
        let raw = "Here you go:
```json
{\"summary\":\"ok\",\"safe\":[{\"path\":\"C:/x/a\",\"reason\":\"cache\"}]}
```
Hope that helps";
        let t = parse_triage(raw).expect("parses");
        assert_eq!(t.summary, "ok");
        assert_eq!(t.safe.len(), 1);
        assert_eq!(t.safe[0].path, "C:/x/a");
        assert!(parse_triage("no json at all").is_none());
    }

    #[test]
    fn expand_request_is_stripped_from_tail() {
        let (text, path) = take_expand_request("look at this.

EXPAND: C:/x/b
");
        assert_eq!(text, "look at this.");
        assert_eq!(path.as_deref(), Some("C:/x/b"));
        let (text, path) = take_expand_request("I would like to EXPAND: something here, but the line is not about expanding");
        assert!(path.is_none());
        assert_eq!(text, "I would like to EXPAND: something here, but the line is not about expanding");
    }

    #[test]
    fn expansion_lists_children() {
        let t = tree();
        let sep = t.sep;
        // root-relative (as the dossier shows it), native separator, and
        // absolute-with-other-separator must all resolve
        for p in ["b", &format!("b{sep}"), "C:/x/b"] {
            let x = expansion_of(&t, p).unwrap_or_else(|| panic!("no expansion for {p}"));
            assert!(x.contains("node_modules"), "{x}");
            assert!(x.contains("data.bin"), "{x}");
        }
        assert!(expansion_of(&t, "missing").is_none());
        let (id, exact) = resolve_path(&t, "a/node_modules");
        assert!(exact, "relative path resolves");
        assert_eq!(t.get(id).name.as_ref(), "node_modules");
    }
}
