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
    "build",
    "dist",
    "out",
    "coverage",
    ".parcel-cache",
    ".turbo",
    "tmp",
    "temp",
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
    let mut stack = vec![tree.root];
    while let Some(id) = stack.pop() {
        for c in tree.children(id) {
            let n = tree.get(c);
            stack.push(c);
            if n.kind == Kind::Dir && junk_match(tree, c) {
                let e = hits.entry(n.name.to_string()).or_insert((0, 0));
                e.0 += 1;
                e.1 += n.size;
                total += n.size;
                count += 1;
            }
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
/// skeleton is depth-2 top-8 with `<rest>` folding, plus rollups the model
/// could not compute itself (same-name dirs, file types, cache matches).
pub fn dossier(tree: &Tree, free: Option<u64>) -> String {
    let mut out = String::new();
    let root = tree.root;
    let n = tree.get(root);
    let _ = writeln!(out, "SCAN: {} · {} total · {} files · free: {}", tree.root_path, fmt_size(n.size), fmt_count(n.files as u64), free.map(fmt_size).unwrap_or_else(|| "unknown".into()));

    let _ = writeln!(out, "\nTREE (2 levels, top 8 per folder, … = rest):");
    subtree(&mut out, tree, root, 0, 8, 12, "");
    let mut top_kids = tree.sorted_children(root);
    top_kids.truncate(4);
    for c in top_kids {
        if tree.can_enter(c) && tree.get(c).size > 0 {
            let _ = writeln!(out);
            subtree(&mut out, tree, c, 0, 8, 8, "");
        }
    }

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

fn subtree(out: &mut String, t: &Tree, id: u32, depth: usize, top: usize, root_top: usize, prefix: &str) {
    let n = t.get(id);
    let cap = if depth == 0 { root_top } else { top };
    let mut kids = t.sorted_children(id);
    let rest = if kids.len() > cap { kids.split_off(cap) } else { Vec::new() };
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
            subtree(out, t, c, depth + 1, top, root_top, &next);
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
    let norm = p.replace('\\', "/").trim_end_matches('/').to_lowercase();
    // drop a scan-root prefix if present
    let root = t.root_path.replace('\\', "/").trim_end_matches('/').to_lowercase();
    let rel = norm.strip_prefix(&root).map(|r| r.trim_start_matches('/')).unwrap_or(&norm);
    if rel.is_empty() {
        return (NONE, false); // the scan root itself is never a suggestion
    }
    // walk from the root, component by component, case-insensitively
    let mut cur = t.root;
    for comp in rel.split('/').filter(|c| !c.is_empty()) {
        match t.children(cur).find(|&c| t.get(c).name.as_ref() == comp) {
            Some(c) => cur = c,
            None => match t.children(cur).find(|&c| t.get(c).name.to_lowercase() == comp) {
                Some(c) => cur = c,
                None => return (cur, false),
            },
        }
    }
    (cur, true)
}

/// A depth-2 top-10 listing of one folder, for follow-up "expand" requests.
pub fn expansion_of(tree: &Tree, path: &str) -> Option<String> {
    let (id, exact) = resolve_path(tree, path);
    if !exact || !tree.is_alive(id) || !tree.can_enter(id) {
        return None;
    }
    let rel = tree.rel_components(id).join("/");
    let mut out = format!("EXPANSION of {} ({}):\n", if rel.is_empty() { "(scan root)" } else { &rel }, fmt_size(tree.get(id).size));
    subtree(&mut out, tree, id, 0, 8, 10, "");
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
        "max_tokens": 4096,
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
            let text = resp.body_mut().read_to_string().map_err(|e| format!("{e}"))?;
            let v: serde_json::Value = serde_json::from_str(&text).map_err(|_| format!("not JSON: {}", &text[..text.len().min(300)]))?;
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
    pub scroll: f32,
    pub content_h: f32,
    pub verify_open: bool,
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
            scroll: 0.0,
            content_h: 0.0,
            verify_open: false,
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
