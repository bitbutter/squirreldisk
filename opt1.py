import re
p = 'src/scan/mft.rs'
s = open(p, encoding='utf-8').read()

def rep(old, new):
    global s
    assert old in s, "NOT FOUND: " + old[:90]
    s = s.replace(old, new, 1)

# --- 1) in-place fixups (hot path) + keep the allocating wrapper for tests --
rep("""/// Copies a record out and applies the update-sequence fixups. Every
/// sector-sized block of a record ends in the USN tag; the original tail
/// bytes live in the header array. `None` = not a record / corrupt.
fn fixed_up(rec: &[u8], sector: usize) -> Option<Vec<u8>> {
    if rec.len() < 0x30 || rec.len() % sector != 0 || sector < 512 {
        return None;
    }
    let mut out = rec.to_vec();
    if &out[0..4] != b"FILE" {
        return None;
    }
    let usa_off = u16at(&out, 4) as usize;
    let usa_count = u16at(&out, 6) as usize;
    if usa_count == 0 || usa_off + usa_count * 2 > out.len() {
        return None;
    }
    let tag = u16at(&out, usa_off);
    for i in 1..usa_count {
        let end = i * sector;
        if end > out.len() {
            break;
        }
        let at = end - 2;
        if u16at(&out, at) != tag {
            return None; // wrong USN tag: record is corrupt
        }
        let fix = u16at(&out, usa_off + i * 2);
        out[at] = (fix & 0xFF) as u8;
        out[at + 1] = (fix >> 8) as u8;
    }
    Some(out)
}""",
"""/// Applies the update-sequence fixups in place. Every sector-sized block of
/// a record ends in the USN tag; the original tail bytes live in the header
/// array. `false` = not a record / corrupt. Hot path: no allocation.
fn apply_fixups_inplace(rec: &mut [u8], sector: usize) -> bool {
    if rec.len() < 0x30 || rec.len() % sector != 0 || sector < 512 {
        return false;
    }
    if &rec[0..4] != b"FILE" {
        return false;
    }
    let usa_off = u16at(rec, 4) as usize;
    let usa_count = u16at(rec, 6) as usize;
    if usa_count == 0 || usa_off + usa_count * 2 > rec.len() {
        return false;
    }
    let tag = u16at(rec, usa_off);
    for i in 1..usa_count {
        let end = i * sector;
        if end > rec.len() {
            break;
        }
        let at = end - 2;
        if u16at(rec, at) != tag {
            return false; // wrong USN tag: record is corrupt
        }
        let fix = u16at(rec, usa_off + i * 2);
        rec[at] = (fix & 0xFF) as u8;
        rec[at + 1] = (fix >> 8) as u8;
    }
    true
}

/// Test/boot-sector helper: fixed-up copy of one record.
fn fixed_up(rec: &[u8], sector: usize) -> Option<Vec<u8>> {
    if rec.len() < 0x30 || rec.len() % sector != 0 || sector < 512 {
        return None;
    }
    let mut out = rec.to_vec();
    if apply_fixups_inplace(&mut out, sector) {
        Some(out)
    } else {
        None
    }
}""")

# --- 2) attribute walk without the per-record Vec --------------------------
rep("struct AttrView {", "#[derive(Clone, Copy, Default)]\nstruct AttrView {")

rep("""/// Walks the attribute chain of one fixed-up record.
fn walk_attributes(rec: &[u8]) -> Option<Vec<AttrView>> {""",
"""/// Most records carry <= 4 attributes; 8 covers the rest without heap.
const MAX_ATTRS: usize = 8;

/// Walks the attribute chain of one fixed-up record into `out`; returns the
/// count. No allocation.
fn walk_attributes_buf(rec: &[u8], out: &mut [AttrView; MAX_ATTRS]) -> Option<usize> {""")

rep("""        out.push(view);
        off += len;
    }
    Some(out)
}""",
"""        if cnt == MAX_ATTRS {
            break; // pathological record: extra attributes ignored
        }
        out[cnt] = view;
        cnt += 1;
        off += len;
    }
    Some(cnt)
}""")

# walk body needs a counter declared and `let mut cnt = 0;`
rep("""    let mut out = Vec::new();
    let mut off = first;
    loop {
        if off + 0x10 > used {
            break;
        }""",
"""    let mut cnt = 0usize;
    let mut off = first;
    loop {
        if off + 0x10 > used {
            break;
        }""")

# allocate-free adapter for the boot-sector record (once per volume)
rep("""        let fixed = fixed_up(rec.bytes(), bytes_per_sector as usize)?;
        let attrs = walk_attributes(&fixed)?;
        let mut runs = Vec::new();
        for att in attrs {""",
"""        let fixed = fixed_up(rec.bytes(), bytes_per_sector as usize)?;
        let mut attrs = [AttrView::default(); MAX_ATTRS];
        let n = walk_attributes_buf(&fixed, &mut attrs)?;
        let mut runs = Vec::new();
        for att in attrs.into_iter().take(n) {""")

open(p, 'w', encoding='utf-8', newline='\n').write(s)
print("stage 1 done")
