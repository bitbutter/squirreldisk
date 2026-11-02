p = 'src/scan/mft.rs'
s = open(p, encoding='utf-8').read()

def rep(old, new):
    global s
    assert old in s, "NOT FOUND: " + old[:90]
    s = s.replace(old, new, 1)

# --- 3) ref-based parse: names stay undecoded UTF-16 spans -----------------
rep("""struct Parsed {
    name: String,
    parent: u64,
    is_dir: bool,
    reparse: bool,
    /// allocated size: compressed and sparse files report what they occupy,
    /// not their logical length (matches the directory walker)
    size: u64,
    mtime: u64,
    /// extension record: FRN of its base record; 0 for a base record
    base: u64,
    /// this record carried an unnamed $DATA instance starting at VCN 0 —
    /// only those may contribute size (split instances repeat stream sizes)
    data_owner: bool,
}""",
"""/// Parsed record borrowing the record buffer: the name stays an undecoded
/// UTF-16 span, so small files (folded away, name never stored) never pay
/// for a String allocation.
struct ParsedRef<'a> {
    name_bytes: &'a [u8],
    parent: u64,
    is_dir: bool,
    reparse: bool,
    /// allocated size: compressed and sparse files report what they occupy,
    /// not their logical length (matches the directory walker)
    size: u64,
    mtime: u64,
    /// extension record: FRN of its base record; 0 for a base record
    base: u64,
    /// this record carried an unnamed $DATA instance starting at VCN 0 —
    /// only those may contribute size (split instances repeat stream sizes)
    data_owner: bool,
}

/// Owned form for tests and one-off callers.
struct Parsed {
    name: String,
    parent: u64,
    is_dir: bool,
    reparse: bool,
    size: u64,
    mtime: u64,
    base: u64,
    data_owner: bool,
}

/// Decodes a UTF-16LE name span.
fn decode_name(b: &[u8]) -> String {
    let units: Vec<u16> = b.chunks_exact(2).map(|p| u16::from_le_bytes([p[0], p[1]])).collect();
    String::from_utf16_lossy(&units)
}""")

# --- parse_record -> parse_record_buf (fills attr array, returns refs) -----
rep("""/// Parses one fixed-up record. Names: a record can carry several `$FILE_NAME`
/// attributes (hard links, DOS aliases); the best-namespace name wins and the
/// record is counted exactly once. Sizes: unnamed `$DATA` (resident or real
/// size of non-resident); directories fall back to `$FILE_NAME` sizes (0).
fn parse_record(rec: &[u8]) -> Option<Parsed> {""",
"""/// Parses one fixed-up record. Names: a record can carry several `$FILE_NAME`
/// attributes (hard links, DOS aliases); the best-namespace name wins and the
/// record is counted exactly once. Sizes: unnamed `$DATA` (allocated for
/// non-resident); directories fall back to `$FILE_NAME` sizes (0).
fn parse_record_buf<'a>(rec: &'a [u8], attrs: &mut [AttrView; MAX_ATTRS]) -> Option<ParsedRef<'a>> {""")

rep("""    let base = u64at(rec, 0x20) & FRN_MASK;
    let is_dir = flags & 0x0002 != 0;
    let attrs = walk_attributes(rec)?;
    let mut reparse = false;""",
"""    let base = u64at(rec, 0x20) & FRN_MASK;
    let is_dir = flags & 0x0002 != 0;
    let n = walk_attributes_buf(rec, attrs)?;
    let mut reparse = false;""")

rep("""    let mut best: Option<(u8, String, u64, u64, u64)> = None;
    let mut data_size: Option<u64> = None;
    let mut data_owner = false;
    for att in &attrs {""",
"""    // (rank, name span start, name span len, parent, mtime, size)
    let mut best: Option<(u8, usize, usize, u64, u64, u64)> = None;
    let mut data_size: Option<u64> = None;
    let mut data_owner = false;
    for att in attrs.iter().take(n) {""")

rep("""                let better = best.as_ref().map(|(r, ..)| *r < rank).unwrap_or(true);
                if better {
                    let units: Vec<u16> = (0..nl).map(|k| u16::from_le_bytes([c[0x3C + k * 2], c[0x3C + k * 2 + 1]])).collect();
                    let name = String::from_utf16_lossy(&units);
                    let mtime = filetime_to_unix(u64at(c, 0x10));
                    // $FILE_NAME sizes are directory-entry-time values;
                    // allocated is the disk-usage truth, real the fallback
                    let fn_size = if alloc > 0 { alloc } else { real };
                    best = Some((rank, name, parent, mtime, fn_size));
                }""",
"""                let better = best.map(|(r, ..)| r < rank).unwrap_or(true);
                if better {
                    let mtime = filetime_to_unix(u64at(c, 0x10));
                    // $FILE_NAME sizes are directory-entry-time values;
                    // allocated is the disk-usage truth, real the fallback
                    let fn_size = if alloc > 0 { alloc } else { real };
                    let start = att.content_off + 0x3C;
                    best = Some((rank, start, nl * 2, parent, mtime, fn_size));
                }""")

rep("""    // extension records carry attribute instances, not names — they still
    // matter (their $DATA belongs to a base record), so parse them nameless
    let (name, parent, fn_mtime, fn_real) = match best {
        Some((_, n, p, m, s)) => (n, p, m, s),
        None if base != 0 => (String::new(), 0, 0, 0),
        None => return None,
    };
    let mtime = if si_mtime > 0 { si_mtime } else { fn_mtime };
    Some(Parsed { name, parent, is_dir, reparse, size: data_size.unwrap_or(fn_real), mtime, base, data_owner })""",
"""    // extension records carry attribute instances, not names — they still
    // matter (their $DATA belongs to a base record), so parse them nameless
    let (name_bytes, parent, fn_mtime, fn_real) = match best {
        Some((_, start, len, p, m, sz)) => (rec.get(start..start + len)?, p, m, sz),
        None if base != 0 => (&[][..], 0, 0, 0),
        None => return None,
    };
    let mtime = if si_mtime > 0 { si_mtime } else { fn_mtime };
    Some(ParsedRef { name_bytes, parent, is_dir, reparse, size: data_size.unwrap_or(fn_real), mtime, base, data_owner })""")

# owned wrapper for tests / non-hot callers
rep("""// ---------------------------------------------------------------------------
// Staging""",
"""/// Owned-parse adapter (tests, one-off callers): decodes the name span.
fn parse_record(rec: &[u8]) -> Option<Parsed> {
    let mut attrs = [AttrView::default(); MAX_ATTRS];
    parse_record_buf(rec, &mut attrs).map(|p| Parsed {
        name: decode_name(p.name_bytes),
        parent: p.parent,
        is_dir: p.is_dir,
        reparse: p.reparse,
        size: p.size,
        mtime: p.mtime,
        base: p.base,
        data_owner: p.data_owner,
    })
}

// ---------------------------------------------------------------------------
// Staging""")

# --- 4) stage_ref: decode names only when they get stored ------------------
rep("""fn stage(dirs: &mut HashMap<u64, Staged>, ext: &mut HashMap<u64, u64>, frn: u64, p: Parsed, limits: &ScanLimits) {""",
"""/// Hot-path staging on ref-parsed records: small files never decode a name.
fn stage_ref(dirs: &mut HashMap<u64, Staged>, ext: &mut HashMap<u64, u64>, frn: u64, p: ParsedRef, limits: &ScanLimits) {
    if p.base != 0 {
        // extension record: only a VCN-0 unnamed $DATA instance may carry
        // size home, and never more than once per base record
        if p.data_owner && p.size > 0 {
            let e = ext.entry(p.base).or_insert(0);
            *e = (*e).max(p.size);
        }
        return;
    }
    if p.reparse {
        if p.is_dir {
            let d = dirs.entry(p.parent).or_insert_with(|| Staged::new(p.parent));
            if d.mounts.len() < 64 {
                d.mounts.push(decode_name(p.name_bytes));
            }
        }
        // reparse FILES (symlinks) are skipped, as the walker skips links
        return;
    }
    if p.is_dir {
        let e = dirs.entry(frn).or_insert_with(|| Staged::new(p.parent));
        if e.name.is_empty() {
            e.name = decode_name(p.name_bytes);
            e.parent = p.parent;
            e.mtime = p.mtime;
        }
        return;
    }
    if p.size >= limits.keep_min {
        dirs.entry(p.parent).or_insert_with(|| Staged::new(p.parent)).add_file(frn, decode_name(p.name_bytes), p.size, p.mtime, limits);
    } else {
        // small file: folded into the aggregate — the name is never stored,
        // so it is never decoded
        let d = dirs.entry(p.parent).or_insert_with(|| Staged::new(p.parent));
        d.small_size = d.small_size.saturating_add(p.size);
        d.small_count += 1;
    }
}

/// Owned staging (tests).
fn stage(dirs: &mut HashMap<u64, Staged>, ext: &mut HashMap<u64, u64>, frn: u64, p: Parsed, limits: &ScanLimits) {""")

open(p, 'w', encoding='utf-8', newline='\n').write(s)
print("stage 2 done")
