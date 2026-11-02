def rep_in(path, pairs):
    s = open(path, encoding='utf-8').read()
    n0 = len(s)
    for old, new in pairs:
        assert old in s, f"NOT FOUND in {path}: {old[:70]!r}"
        s = s.replace(old, new, 1)
    open(path, 'w', encoding='utf-8', newline='\n').write(s)
    print(path, "patched, delta", len(s) - n0)

# ================= src/ai.rs =================
rep_in('src/ai.rs', [
    # NEW-2 (critical): separator guard on root-prefix stripping, both passes.
    # Mirrors tree.rs find_path: the remainder must be empty or start with '/'.
    ("""    let raw = p.replace('\\\\', "/").trim_end_matches('/').to_string();
    let root: String = t.root_path.replace('\\\\', "/").trim_end_matches('/').to_string();
    let raw_rel = raw.strip_prefix(&root).map(|r| r.trim_start_matches('/')).unwrap_or(&raw).to_string();""",
     """    let raw = p.replace('\\\\', "/").trim_end_matches('/').to_string();
    let root: String = t.root_path.replace('\\\\', "/").trim_end_matches('/').to_string();
    // strip a root prefix only on a path boundary: `C:/x` must not eat the
    // front of `C:/xy` (remainder must be empty or start with '/')
    let raw_rel = strip_root(&raw, &root).unwrap_or_else(|| raw.clone());"""),
    ("""    let lower_rel = raw_rel.to_lowercase();
    let root_l = root.to_lowercase();
    let lower_rel = lower_rel.strip_prefix(&root_l).map(|r| r.trim_start_matches('/')).unwrap_or(&lower_rel).to_string();
    if let Some(id) = walk(t, t.root, lower_rel.split('/').filter(|c| !c.is_empty()), true) {
        return (id, true);
    }
    (NONE, false)
}""",
     """    let lower_rel = raw_rel.to_lowercase();
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
    // volume roots can end in ':' (`C:`): `C:Users` is not `C:/Users`
    if root.ends_with(':') && !rest.starts_with('/') {
        return Some(rest.trim_start_matches('/').to_string());
    }
    None
}"""),
    # NEW-1: pass 1 already prints levels 1-3; the top-4 block only duplicated
    # them. Removed; add an explicit line budget to subtree instead.
    ("""    // levels 0-2 from the root; then level 3 under the four biggest dirs
    // (starting at depth 2 prints only their children — no duplication)
    subtree(&mut out, tree, root, 0, 8, 12, "");
    let mut top_kids = tree.sorted_children(root);
    top_kids.truncate(4);
    for c in top_kids {
        if tree.can_enter(c) && tree.get(c).size > 0 {
            let _ = writeln!(out);
            subtree(&mut out, tree, c, 2, 8, 8, "");
        }
    }""",
     """    // three levels from the root, top-N per folder with `<rest>` folding,
    // hard-capped at ~500 lines (DEC-AI-001: a dossier, never a dump)
    let mut budget: usize = 500;
    subtree(&mut out, tree, root, 0, 6, 12, "", &mut budget);"""),
    ("""fn subtree(out: &mut String, t: &Tree, id: u32, depth: usize, top: usize, root_top: usize, prefix: &str) {""",
     """fn subtree(out: &mut String, t: &Tree, id: u32, depth: usize, top: usize, root_top: usize, prefix: &str, budget: &mut usize) {"""),
    ("""        let denied = if k.denied() { "  (no access)" } else { "" };
        let _ = writeln!(out, "{pad}{} {:>3}% {}{}", fmt_size(k.size), pct, label, denied);
        if depth < 2 && t.can_enter(c) && k.size > 0 && k.kind == Kind::Dir {
            let next = format!("{prefix}{}/", k.name);
            subtree(out, t, c, depth + 1, top, root_top, &next);
        }
    }""",
     """        let denied = if k.denied() { "  (no access)" } else { "" };
        let _ = writeln!(out, "{pad}{} {:>3}% {}{}", fmt_size(k.size), pct, label, denied);
        if *budget == 0 {
            continue;
        }
        if depth < 2 && t.can_enter(c) && k.size > 0 && k.kind == Kind::Dir {
            let next = format!("{prefix}{}/", k.name);
            subtree(out, t, c, depth + 1, top, root_top, &next, budget);
        }
    }"""),
    ("""    let mut kids = t.sorted_children(id);
    let rest = if kids.len() > cap { kids.split_off(cap) } else { Vec::new() };""",
     """    let mut kids = t.sorted_children(id);
    if *budget == 0 {
        return;
    }
    kids.truncate(kids.len().min(cap).min(*budget));
    *budget -= kids.len();
    let rest: Vec<u32> = Vec::new();"""),
    # reviewer nit: expansion listing budget too
    ("""    let mut out = format!("EXPANSION of {} ({}):\\n", if rel.is_empty() { "(scan root)" } else { &rel }, fmt_size(tree.get(id).size));
    subtree(&mut out, tree, id, 0, 8, 10, "");
    Some(out)""",
     """    let mut out = format!("EXPANSION of {} ({}):\\n", if rel.is_empty() { "(scan root)" } else { &rel }, fmt_size(tree.get(id).size));
    let mut budget: usize = 200;
    subtree(&mut out, tree, id, 0, 8, 10, "", &mut budget);
    Some(out)"""),
    # max_tokens headroom for the maximal triage shape
    ('"max_tokens": 4096,', '"max_tokens": 8192,'),
])

# ================= src/scan/mft.rs =================
rep_in('src/scan/mft.rs', [
    # NEW-1: nameless extension records are legal; parse them so the ext-map
    # fold actually runs and they stop counting as errors
    ("""    let (_, name, parent, fn_mtime, fn_real) = best?;
    let mtime = if si_mtime > 0 { si_mtime } else { fn_mtime };
    Some(Parsed { name, parent, is_dir, reparse, size: data_size.unwrap_or(fn_real), mtime, base, data_owner })""",
     """    // extension records carry attribute instances, not names — they still
    // matter (their $DATA belongs to a base record), so parse them nameless
    let (name, parent, fn_mtime, fn_real) = match best {
        Some((_, n, p, m, s)) => (n, p, m, s),
        None if base != 0 => (String::new(), 0, 0, 0),
        None => return None,
    };
    let mtime = if si_mtime > 0 { si_mtime } else { fn_mtime };
    Some(Parsed { name, parent, is_dir, reparse, size: data_size.unwrap_or(fn_real), mtime, base, data_owner })"""),
    # stage(): base-branch first, name unused there; then the nameless guard
    ("""fn stage(dirs: &mut HashMap<u64, Staged>, ext: &mut HashMap<u64, u64>, frn: u64, p: Parsed, limits: &ScanLimits) {
    if p.base != 0 {""",
     """fn stage(dirs: &mut HashMap<u64, Staged>, ext: &mut HashMap<u64, u64>, frn: u64, p: Parsed, limits: &ScanLimits) {
    if p.base != 0 {
        let _ = frn;"""),
    # error counter: only base records that fail to parse are errors
    ("""                        None if live => {
                            progress.errors.fetch_add(1, Ordering::Relaxed);
                        }
                        None => {}""",
     """                        None if live => {
                            // nameless extension records parse fine now;
                            // only a live BASE record that fails is an error
                            progress.errors.fetch_add(1, Ordering::Relaxed);
                        }
                        None => {}"""),
    # NEW-3: saturating staging/assembly sums
    ("""        } else {
            self.small_size += size;
            self.small_count += 1;
            return;
        }""",
     """        } else {
            self.small_size = self.small_size.saturating_add(size);
            self.small_count += 1;
            return;
        }"""),
    ("""                while h.len() > *cap {
                    // smallest falls out into the folded aggregate
                    if let Some(Reverse((sz, ..))) = h.pop() {
                        self.small_size += sz;
                        self.small_count += 1;
                    }
                }""",
     """                while h.len() > *cap {
                    // smallest falls out into the folded aggregate
                    if let Some(Reverse((sz, ..))) = h.pop() {
                        self.small_size = self.small_size.saturating_add(sz);
                        self.small_count += 1;
                    }
                }"""),
    ("""        let mut sz = *size;
        if let Some(extra) = ext.get(ffrn) {
            sz += extra; // $ATTRIBUTE_LIST files: split $DATA runs
        }""",
     """        let mut sz = *size;
        if let Some(extra) = ext.get(ffrn) {
            sz = sz.saturating_add(*extra); // $ATTRIBUTE_LIST: split $DATA runs
        }"""),
    ("""    if size_sum > 0 || file_count > 0 {
        t.add_size(node, size_sum as i64, file_count as i64);
    }""",
     """    if size_sum > 0 || file_count > 0 {
        t.add_size(node, size_sum.min(i64::MAX as u64) as i64, file_count as i64);
    }"""),
    # NEW-4: shift overflow on a hostile BPB
    ("""        let ind = b[0x40] as i8;
        let record_bytes = if ind < 0 { 1u64 << (-ind as u32) } else { ind as u64 * cluster_bytes };""",
     """        let ind = b[0x40] as i8;
        let record_bytes = if ind < 0 {
            let shift = (-(ind as i32)) as u32;
            if shift >= 64 {
                return None;
            }
            1u64 << shift
        } else {
            ind as u64 * cluster_bytes
        };"""),
    # NEW-5: cap 0 folds everything (walker parity)
    ("""        match self.heap.as_mut() {
            None => {
                let cap = limits.max_files_per_dir.max(1);""",
     """        match self.heap.as_mut() {
            None => {
                let cap = limits.max_files_per_dir;"""),
])

# ================= local.rs: NEW-2 — panic mid-assembly must not double-add
rep_in('src/scan/local.rs', [
    ("""                let guard = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::scan::mft::run(t.clone(), &drive, p.clone(), limits)
                }));
                mft_done = guard.unwrap_or(false);""",
     """                let guard = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::scan::mft::run(t.clone(), &drive, p.clone(), limits)
                }));
                match guard {
                    Ok(done) => mft_done = done,
                    Err(_) => {
                        // the tree may be half-assembled: running the walker
                        // on top of it would double-count. Fail the scan
                        // honestly instead (DEC-SCAN-002).
                        p.expected.store(0, Ordering::Relaxed);
                        p.bytes.store(0, Ordering::Relaxed);
                        p.fail("NTFS scan crashed (corrupt volume metadata) — scan again");
                        return;
                    }
                }"""),
])

# ================= ai_panel.rs: NEW-3 gate chat on Analyze; link liveness ====
rep_in('src/ui/ai_panel.rs', [
    ("""            // input row (pinned below the scroll area)
            let can_send = !busy && !self.sessions[si].ai.input.trim().is_empty();""",
     """            // input row (pinned below the scroll area); chat needs the
            // dossier in context — gate on a completed Analyze
            let can_send = !busy && self.sessions[si].ai.analyzed && !self.sessions[si].ai.input.trim().is_empty();"""),
    ("""                egui::TextEdit::singleline(&mut self.sessions[si].ai.input)
                    .hint_text(tr("Ask about this scan…").to_owned())""",
     """                egui::TextEdit::singleline(&mut self.sessions[si].ai.input)
                    .hint_text(if self.sessions[si].ai.analyzed { tr("Ask about this scan…").to_owned() } else { tr("Analyze first, then ask…").to_owned() })"""),
    # navigate only to live nodes (incremental deletions detach nodes)
    ("""        if let Some(n) = a.navigate {
            self.sessions[si].navigate(View::node(n));
            self.sfx(crate::sound::Sfx::Blip);
        }""",
     """        if let Some(n) = a.navigate.filter(|&n| self.sessions[si].tree.read().unwrap().is_alive(n)) {
            self.sessions[si].navigate(View::node(n));
            self.sfx(crate::sound::Sfx::Blip);
        }"""),
])

# ================= DECISIONS: plaintext key note =================
d = open('DECISIONS.md', encoding='utf-8').read()
d = d.replace("DEC-AI-005: The default endpoint is z.ai's coding-plan OpenAI-compatible endpoint (`…/api/coding/paas/v4/chat/completions`), because the pay-as-you-go endpoint rejects coding-plan keys with \"insufficient balance\". Any OpenAI-compatible URL works via Settings.",
"DEC-AI-005: The default endpoint is z.ai's coding-plan OpenAI-compatible endpoint (`…/api/coding/paas/v4/chat/completions`), because the pay-as-you-go endpoint rejects coding-plan keys with \"insufficient balance\". Any OpenAI-compatible URL works via Settings. The API key is stored in cleartext in `settings.json` in the app's config folder (same as every other local tool on this machine); it is masked in the UI and never appears in toasts, errors or logs.")
open('DECISIONS.md', 'w', encoding='utf-8', newline='\n').write(d)
print("DECISIONS key note added")
