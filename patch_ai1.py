import re

# ============ src/ai.rs ============
p = 'src/ai.rs'
s = open(p, encoding='utf-8').read()

# A2: no byte-slice panic on non-UTF-8 boundary; cap the error snippet
old = """            let text = resp.body_mut().read_to_string().map_err(|e| format!("{e}"))?;
            let v: serde_json::Value = serde_json::from_str(&text).map_err(|_| format!("not JSON: {}", &text[..text.len().min(300)]))?;"""
new = """            // cap the body: a misconfigured endpoint must not OOM the app
            let mut limited = resp.body_mut().take(1 << 20);
            let text = limited.read_to_string().map_err(|e| format!("{e}"))?;
            let snippet: String = text.chars().take(300).collect();
            let v: serde_json::Value = serde_json::from_str(&text).map_err(|_| format!("not JSON: {snippet}"))?;"""
assert old in s; s = s.replace(old, new)

# use io::Read for take()
old = "use std::time::Duration;"
new = "use std::io::Read as _;\nuse std::time::Duration;"
assert old in s; s = s.replace(old, new, 1)

# A5: the top-4 loop re-renders levels 1-2 that the root pass already
# printed — start it at depth 3 (only adds the level the root pass lacks)
old = """    subtree(&mut out, tree, root, 0, 8, 12, "");
    let mut top_kids = tree.sorted_children(root);
    top_kids.truncate(4);
    for c in top_kids {
        if tree.can_enter(c) && tree.get(c).size > 0 {
            let _ = writeln!(out);
            subtree(&mut out, tree, c, 0, 8, 8, "");
        }
    }"""
new = """    // levels 0-2 from the root; then level 3 under the four biggest dirs
    // (starting at depth 2 prints only their children — no duplication)
    subtree(&mut out, tree, root, 0, 8, 12, "");
    let mut top_kids = tree.sorted_children(root);
    top_kids.truncate(4);
    for c in top_kids {
        if tree.can_enter(c) && tree.get(c).size > 0 {
            let _ = writeln!(out);
            subtree(&mut out, tree, c, 2, 8, 8, "");
        }
    }"""
assert old in s; s = s.replace(old, new)

# A7: drop the bare-name false-positive junk entries
old = '''const JUNK_NAMES: &[&str] = &[
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
];'''
new = '''const JUNK_NAMES: &[&str] = &[
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
];'''
assert old in s; s = s.replace(old, new)

# A7: nested junk must not double-count — DFS with an in-junk flag
old = """pub fn teaser(tree: &Tree) -> Option<Teaser> {
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
    }"""
new = """pub fn teaser(tree: &Tree) -> Option<Teaser> {
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
    }"""
assert old in s; s = s.replace(old, new)

# A9: exact-case walk first; case-insensitive only as fallback, ambiguity fails
old = """    let norm = p.replace('\\\\', "/").trim_end_matches('/').to_lowercase();
    // drop a scan-root prefix if present
    let root = t.root_path.replace('\\\\', "/").trim_end_matches('/').to_lowercase();
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
}"""
new = """    let raw = p.replace('\\\\', "/").trim_end_matches('/').to_string();
    let root = t.root_path.replace('\\\\', "/").trim_end_matches('/');
    let raw_rel = raw.strip_prefix(&root).map(|r| r.trim_start_matches('/')).unwrap_or(&raw).to_string();
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
    let lower_rel = lower_rel.strip_prefix(&root_l).map(|r| r.trim_start_matches('/')).unwrap_or(&lower_rel).to_string();
    if let Some(id) = walk(t, t.root, lower_rel.split('/').filter(|c| !c.is_empty()), true) {
        return (id, true);
    }
    (NONE, false)
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
}"""
assert old in s; s = s.replace(old, new)

open(p, 'w', encoding='utf-8', newline='\n').write(s)
print("ai.rs patched")

# ============ src/i18n.rs ============
p = 'src/i18n.rs'
s = open(p, encoding='utf-8').read()
removed = 0
for line in ['                include_str!("sponsor/mod.rs"),\n',
             '                include_str!("sponsor/interests.rs"),\n',
             '                include_str!("ui/ads.rs"),\n']:
    if line in s:
        s = s.replace(line, '')
        removed += 1
open(p, 'w', encoding='utf-8', newline='\n').write(s)
print("i18n.rs: removed", removed, "dangling include_strs")

# ============ src/ui/app.rs ============
p = 'src/ui/app.rs'
s = open(p, encoding='utf-8').read()

# A10: a rescan of the same session must not keep stale AI context
old = """                    {
                        let t = s.tree.read().unwrap();
                        s.ai.teaser = crate::ai::teaser(&t);
                    }
                    s.ai.teaser_seen = false;"""
new = """                    {
                        let t = s.tree.read().unwrap();
                        s.ai.teaser = crate::ai::teaser(&t);
                    }
                    s.ai.teaser_seen = false;
                    // a rescan replaced the tree: the old dossier, triage and
                    // chat describe files that may not exist anymore — start
                    // the conversation over (the tab stays open)
                    if s.ai.analyzed || s.ai.phase != crate::ai::Phase::Idle {
                        s.ai.dossier.clear();
                        s.ai.history.clear();
                        s.ai.triage = None;
                        s.ai.analyzed = false;
                        s.ai.follow_ups.clear();
                        s.ai.error = None;
                        s.ai.phase = crate::ai::Phase::Idle;
                    }"""
assert old in s; s = s.replace(old, new)
open(p, 'w', encoding='utf-8', newline='\n').write(s)
print("app.rs patched")

# ============ src/ui/modals.rs ============
p = 'src/ui/modals.rs'
s = open(p, encoding='utf-8').read()
old = """        let upd = self.updater.state();
        let _ = &upd;
"""
assert old in s; s = s.replace(old, "")
open(p, 'w', encoding='utf-8', newline='\n').write(s)
print("modals.rs patched")
