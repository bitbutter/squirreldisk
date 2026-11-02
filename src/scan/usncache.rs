//! USN-journal cache for repeat MFT scans (Windows-only, DECISIONS.md
//! DEC-SCAN-005).
//!
//! After a successful whole-volume scan the engine serializes the assembled
//! arena plus a USN journal checkpoint to `<config_dir>/mft-cache-<drive>.bin`
//! (background thread — never on the scan critical path). The next scan of
//! the same volume loads that file, replays the USN close-records written
//! since the checkpoint and patches the tree: creates, deletes, renames and
//! size changes by file reference number.
//!
//! Safety rule (binding): ANY doubt discards the cache and a full scan runs
//! instead — journal ID changed, checkpoint below the journal's lowest valid
//! USN (wrapped/reset), record parse hiccup, backlog over the cap, unknown
//! parent for a created entry, magic/version/checksum mismatch. The patch is
//! applied to the detached node vector first, so a mid-patch refusal leaves
//! the live tree untouched for the full-scan fallback (DEC-SCAN-002).
//!
//! Trust model: the cache is a local artifact written by this app (integrity
//! checked with xxh3), but the USN stream is disk data and is bounds-checked
//! like every other input; nothing here trusts offsets from either.

use super::platform;
use crate::tree::{Kind, Node, Tree, F_DETACHED, NONE};
use std::collections::HashMap;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"SQDMFTC1";
const VERSION: u32 = 1;
/// Refuse replays whose record backlog exceeds this (wrapped-ish journal).
const MAX_REPLAY_BYTES: u64 = 512 << 20;

// USN reason bits (Win32, verified against MS documentation).
const R_DATA_OVERWRITE: u32 = 0x0000_0001;
const R_DATA_EXTEND: u32 = 0x0000_0002;
const R_DATA_TRUNCATION: u32 = 0x0000_0004;
const R_FILE_CREATE: u32 = 0x0000_0100;
const R_FILE_DELETE: u32 = 0x0000_0200;
const R_RENAME_OLD: u32 = 0x0000_1000;
const R_RENAME_NEW: u32 = 0x0000_2000;
const R_CLOSE: u32 = 0x8000_0000;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// Journal position this cache is valid at.
#[derive(Clone, Copy, Debug)]
pub struct Checkpoint {
    pub journal_id: u64,
    pub next_usn: u64,
}

/// Everything needed to rebuild the tree without reading the MFT.
pub struct Cache {
    pub nodes: Vec<Node>,
    pub detached: usize,
    /// (frn, node index) for every node that has a file reference number.
    pub frn_of: Vec<(u64, u32)>,
    pub cp: Checkpoint,
}

fn cache_path(drive: &str) -> Option<PathBuf> {
    let letter = drive.trim_end_matches(':').chars().next()?.to_ascii_uppercase();
    if !letter.is_ascii_alphabetic() {
        return None;
    }
    crate::settings::config_dir().map(|d| d.join(format!("mft-cache-{letter}.bin")))
}

// ---------------------------------------------------------------------------
// Serialization (hand-rolled, xxh3-checked; no new dependencies)

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

struct Rd<'a> {
    b: &'a [u8],
    at: usize,
}
impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        if end > self.b.len() {
            return None;
        }
        let s = &self.b[self.at..end];
        self.at = end;
        Some(s)
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn str(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        let s = self.take(n)?;
        Some(String::from_utf8_lossy(s).into_owned())
    }
}

fn serialize(nodes: &[Node], detached: usize, frn_of: &[(u64, u32)], cp: &Checkpoint) -> Vec<u8> {
    let mut out = Vec::with_capacity(nodes.len() * 56 + frn_of.len() * 12 + 64);
    out.extend_from_slice(MAGIC);
    put_u32(&mut out, VERSION);
    put_u64(&mut out, cp.journal_id);
    put_u64(&mut out, cp.next_usn);
    put_u64(&mut out, detached as u64);
    put_u64(&mut out, nodes.len() as u64);
    for n in nodes {
        put_str(&mut out, &n.name);
        put_u64(&mut out, n.size);
        put_u32(&mut out, n.files);
        put_u32(&mut out, n.parent);
        put_u32(&mut out, n.first_child);
        put_u32(&mut out, n.next_sibling);
        put_u16(&mut out, n.kind as u16);
        put_u16(&mut out, n.flags as u16);
        put_u64(&mut out, n.mtime);
    }
    put_u64(&mut out, frn_of.len() as u64);
    for &(frn, id) in frn_of {
        put_u64(&mut out, frn);
        put_u32(&mut out, id);
    }
    let h = xxhash_rust::xxh3::xxh3_64(&out);
    put_u64(&mut out, h);
    out
}

fn deserialize(data: &[u8]) -> Option<Cache> {
    if data.len() < 8 + 8 + 8 {
        return None;
    }
    let body = &data[..data.len() - 8];
    let want = u64::from_le_bytes(data[data.len() - 8..].try_into().ok()?);
    if xxhash_rust::xxh3::xxh3_64(body) != want {
        return None;
    }
    let mut r = Rd { b: body, at: 0 };
    if r.take(8)? != MAGIC {
        return None;
    }
    if r.u32()? != VERSION {
        return None;
    }
    let cp = Checkpoint { journal_id: r.u64()?, next_usn: r.u64()? };
    let detached = r.u64()? as usize;
    let n_nodes = r.u64()? as usize;
    if n_nodes > 1 << 30 || n_nodes == 0 {
        return None;
    }
    let mut nodes = Vec::with_capacity(n_nodes.min(1 << 22));
    let mut max_link: u32 = 0;
    for _ in 0..n_nodes {
        let name = r.str()?;
        let size = r.u64()?;
        let files = r.u32()?;
        let parent = r.u32()?;
        let first_child = r.u32()?;
        let next_sibling = r.u32()?;
        let kind = r.u16()?;
        let flags = r.u16()? as u8;
        let mtime = r.u64()?;
        let kind = match kind {
            0 => Kind::Dir,
            1 => Kind::File,
            2 => Kind::SmallFiles,
            3 => Kind::Link,
            4 => Kind::Mount,
            5 => Kind::Hidden,
            _ => return None,
        };
        // forward links (parent -> later child) are legal: validate after
        max_link = max_link.max(parent).max(first_child).max(next_sibling);
        nodes.push(Node { name: name.into(), size, files, parent, first_child, next_sibling, kind, flags, mtime });
    }
    let n_nodes_final = nodes.len() as u32;
    if max_link != crate::tree::NONE && max_link >= n_nodes_final {
        return None; // corrupt link: refuse the whole cache (DEC-SCAN-005)
    }
    let n_frn = r.u64()? as usize;
    if n_frn > n_nodes {
        return None;
    }
    let mut frn_of = Vec::with_capacity(n_frn);
    for _ in 0..n_frn {
        let frn = r.u64()?;
        let id = r.u32()?;
        if id as usize >= n_nodes {
            return None;
        }
        frn_of.push((frn, id));
    }
    if r.at != r.b.len() {
        return None;
    }
    Some(Cache { nodes, detached, frn_of, cp })
}

/// Writes the cache atomically (temp file + rename). Never call on the scan
/// critical path — this serializes ~100MB on a healthy volume.
pub fn save(drive: &str, nodes: &[Node], detached: usize, frn_of: &[(u64, u32)], cp: &Checkpoint) -> std::io::Result<()> {
    // Tombstoned nodes can never come back: drop their frn entries.
    let live: Vec<(u64, u32)> =
        frn_of.iter().copied().filter(|&(_, id)| nodes.get(id as usize).is_none_or(|n| n.flags & F_DETACHED == 0)).collect();
    let data = serialize(nodes, detached, &live, cp);
    let path = cache_path(drive).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no config dir"))?;
    // hygiene: remove orphans from crashed writers, then write+rename ours
    if let Some(dir) = path.parent() {
        let prefix = format!("{}.tmp-", path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default());
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with(&prefix) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, &data)?;
    match std::fs::rename(&tmp, &path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Loads and integrity-checks the cache. Any doubt returns None (caller does
/// a full scan).
pub fn load(drive: &str) -> Option<Cache> {
    let path = cache_path(drive)?;
    let data = std::fs::read(path).ok()?;
    deserialize(&data)
}

// ---------------------------------------------------------------------------
// Raw USN journal access

/// `journal_now` result: (journal id, lowest valid USN, next USN).
pub type JournalInfo = (u64, u64, u64);

#[repr(C)]
struct ReadUsnJournalDataV2 {
    start_usn: u64,
    reason_mask: u32,
    return_only_on_close: u32,
    timeout: u64,
    bytes_to_wait: u64,
    journal_id: u64,
    min_major: u32,
    max_major: u32,
}

const FSCTL_READ_USN_JOURNAL: u32 = 0x0009_00BB;

/// Queries the volume's current USN journal identity. `vol` is a handle on
/// `\\.\C:` opened with read access.
pub fn journal_now(vol: &mut super::mft::Volume) -> Option<JournalInfo> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    let inb = ReadUsnJournalDataV2 {
        start_usn: 0,
        reason_mask: 0xFFFF_FFFF,
        return_only_on_close: 0,
        timeout: 0,
        bytes_to_wait: 0,
        journal_id: 0,
        min_major: 2,
        max_major: 2,
    };
    let mut out = [0u8; 64];
    let mut got = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            vol.file.as_raw_handle(),
            FSCTL_READ_USN_JOURNAL,
            &inb as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<ReadUsnJournalDataV2>() as u32,
            out.as_mut_ptr().cast(),
            out.len() as u32,
            &mut got,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || got < 24 {
        return None;
    }
    // USN_JOURNAL_DATA_V1/V2 share the first three fields.
    Some((u64::from_le_bytes(out[0..8].try_into().unwrap()), u64::from_le_bytes(out[8..16].try_into().unwrap()), u64::from_le_bytes(out[16..24].try_into().unwrap())))
}

/// One closed change-set from the journal.
pub struct Rec {
    frn: u64,
    parent: u64,
    reason: u32,
    attrs: u32,
    name: String,
}

/// Reads every close-record from `start_usn` to the journal's live edge.
/// Any hiccup is an Err (caller discards the cache — safety rule).
pub fn read_records(vol: &mut super::mft::Volume, journal_id: u64, start_usn: u64) -> Result<Vec<Rec>, String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::IO::DeviceIoControl;
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut usn = start_usn;
    let mut total = 0u64;
    loop {
        let inb = ReadUsnJournalDataV2 {
            start_usn: usn,
            reason_mask: 0xFFFF_FFFF,
            return_only_on_close: 1,
            timeout: 0,
            bytes_to_wait: 0,
            journal_id,
            min_major: 2,
            max_major: 2,
        };
        let mut got = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                vol.file.as_raw_handle(),
                FSCTL_READ_USN_JOURNAL,
                &inb as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<ReadUsnJournalDataV2>() as u32,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                &mut got,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(format!("read_usn_journal failed: {}", std::io::Error::last_os_error()));
        }
        if (got as usize) < 8 {
            return Err("short USN buffer".into());
        }
        let next = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        if next < usn {
            return Err("USN went backwards".into());
        }
        let mut off = 8usize;
        while off + 8 <= got as usize {
            let len = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
            if len == 0 {
                return Err("zero-length USN record".into());
            }
            if len < 60 || off + len > got as usize {
                return Err("bad USN record length".into());
            }
            let major = u16::from_le_bytes(buf[off + 4..off + 6].try_into().unwrap());
            if major != 2 {
                return Err(format!("unexpected USN record v{major}"));
            }
            let g = |o: usize, n: usize| u64::from_le_bytes(buf[off + o..off + o + n].try_into().unwrap());
            let frn = g(8, 8);
            let parent = g(16, 8);
            let reason = u32::from_le_bytes(buf[off + 40..off + 44].try_into().unwrap());
            let attrs = u32::from_le_bytes(buf[off + 52..off + 56].try_into().unwrap());
            let nl = u16::from_le_bytes(buf[off + 56..off + 58].try_into().unwrap()) as usize;
            let no = u16::from_le_bytes(buf[off + 58..off + 60].try_into().unwrap()) as usize;
            // name span is disk data: bounds-check against the record, then decode
            if no + nl > len || nl == 0 {
                return Err("bad USN name span".into());
            }
            let span = &buf[off + no..off + no + nl];
            let units: Vec<u16> = span.chunks_exact(2).map(|p| u16::from_le_bytes([p[0], p[1]])).collect();
            out.push(Rec { frn, parent, reason, attrs, name: String::from_utf16_lossy(&units) });
            off += len;
        }
        total += got as u64;
        if total > MAX_REPLAY_BYTES {
            return Err("USN backlog over cap".into());
        }
        if next == usn {
            break; // caught up
        }
        usn = next;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Bare Vec<Node> tree surgery (mirrors Tree:: semantics exactly)

fn is_alive(nodes: &[Node], id: u32) -> bool {
    if id == NONE || id as usize >= nodes.len() {
        return false;
    }
    let mut cur = id;
    while cur != NONE {
        let n = &nodes[cur as usize];
        if n.flags & F_DETACHED != 0 {
            return false;
        }
        cur = n.parent;
    }
    true
}

fn add_size_walk(nodes: &mut [Node], from: u32, size: i64, files: i64) {
    let mut cur = from;
    while cur != NONE {
        let n = &mut nodes[cur as usize];
        n.size = (n.size as i64 + size).max(0) as u64;
        n.files = (n.files as i64 + files).max(0) as u32;
        cur = n.parent;
    }
}

/// Tree::remove on a bare vector (no version counter).
fn remove_node(nodes: &mut [Node], id: u32, detached: &mut usize) {
    if id == 0 || !is_alive(nodes, id) {
        return; // root is never removed; tombstones stay tombstones
    }
    let (parent, size, files) = {
        let n = &nodes[id as usize];
        (n.parent, n.size, n.files)
    };
    if parent != NONE {
        add_size_walk(nodes, parent, -(size as i64), -(files as i64));
        let first = nodes[parent as usize].first_child;
        if first == id {
            nodes[parent as usize].first_child = nodes[id as usize].next_sibling;
        } else {
            let mut cur = first;
            while cur != NONE {
                let next = nodes[cur as usize].next_sibling;
                if next == id {
                    nodes[cur as usize].next_sibling = nodes[id as usize].next_sibling;
                    break;
                }
                cur = next;
            }
        }
    }
    let n = &mut nodes[id as usize];
    n.flags |= F_DETACHED;
    n.next_sibling = NONE;
    *detached += 1;
}

/// Tree::add_child on a bare vector.
fn push_child(nodes: &mut Vec<Node>, parent: u32, name: &str, kind: Kind, size: u64, files: u32, mtime: u64) -> u32 {
    let id = nodes.len() as u32;
    let next = nodes[parent as usize].first_child;
    nodes.push(Node { name: name.into(), size, files, parent, first_child: NONE, next_sibling: next, kind, flags: 0, mtime });
    nodes[parent as usize].first_child = id;
    id
}

/// First live child with this exact name.
fn child_by_name(nodes: &[Node], parent: u32, name: &str) -> Option<u32> {
    let mut cur = nodes[parent as usize].first_child;
    while cur != NONE {
        let n = &nodes[cur as usize];
        if n.flags & F_DETACHED == 0 && &*n.name == name {
            return Some(cur);
        }
        cur = n.next_sibling;
    }
    None
}

/// Path of a node: drive root prefix + ancestor names.
fn path_of(nodes: &[Node], id: u32, root_path: &Path) -> PathBuf {
    let mut parts = Vec::new();
    let mut cur = id;
    while cur != NONE && cur != 0 {
        parts.push(nodes[cur as usize].name.to_string());
        cur = nodes[cur as usize].parent;
    }
    let mut p = root_path.to_path_buf();
    for seg in parts.into_iter().rev() {
        p.push(seg);
    }
    p
}

/// Copies `other`'s root subtree under `parent` (Tree::graft on a bare vector).
fn graft(nodes: &mut Vec<Node>, parent: u32, other: &Tree, name: &str) -> u32 {
    let mut map: HashMap<u32, u32> = HashMap::new();
    let o_root = other.get(other.root);
    let new_root = push_child(nodes, parent, name, o_root.kind, o_root.size, o_root.files, o_root.mtime);
    nodes[new_root as usize].flags = o_root.flags;
    map.insert(other.root, new_root);
    let mut stack = vec![other.root];
    while let Some(o) = stack.pop() {
        let dst = map[&o];
        for c in other.children(o) {
            let cn = other.get(c);
            let id = push_child(nodes, dst, &cn.name, cn.kind, cn.size, cn.files, cn.mtime);
            nodes[id as usize].flags = cn.flags;
            map.insert(c, id);
            stack.push(c);
        }
    }
    add_size_walk(nodes, parent, o_root.size as i64, o_root.files as i64);
    new_root
}

/// Walker-rule allocated size for an existing path.
fn stat_alloc(path: &Path) -> (u64, u32, u64) {
    match std::fs::metadata(path) {
        Ok(md) => {
            let info = platform::entry_info_path(&md, path);
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            (info.alloc, 1, mtime)
        }
        Err(_) => (0, 0, 0),
    }
}

/// Collects the node and all its descendants (live or tombstoned — the
/// links stay intact under tombstones).
fn subtree_ids(nodes: &[Node], id: u32, out: &mut Vec<u32>) {
    out.push(id);
    let mut cur = nodes[id as usize].first_child;
    while cur != NONE {
        subtree_ids(nodes, cur, out);
        cur = nodes[cur as usize].next_sibling;
    }
}

/// Removes a node AND scrubs every descendant's frn mapping (a deleted or
/// replaced directory must not leave children mapped to dead nodes).
fn scrub_subtree(nodes: &mut [Node], frn_of: &mut HashMap<u64, u32>, detached: &mut usize, id: u32) {
    let mut ids = Vec::new();
    subtree_ids(nodes, id, &mut ids);
    if ids.len() > 1 {
        let gone: std::collections::HashSet<u32> = ids.into_iter().collect();
        frn_of.retain(|_, v| !gone.contains(v));
    }
    remove_node(nodes, id, detached);
}

/// Replays `recs` onto the detached node vector. Returns Err (with no tree
/// mutation yet) on the safety rule's "any doubt" conditions.
fn patch_nodes(nodes: &mut Vec<Node>, frn_of: &mut HashMap<u64, u32>, detached: &mut usize, recs: &[Rec], root_path: &Path, cancelled: &dyn Fn() -> bool) -> Result<(), String> {
    for r in recs {
        if cancelled() {
            return Err("cancelled during replay".into());
        }
        if r.reason & R_CLOSE == 0 {
            continue;
        }
        let create = r.reason & R_FILE_CREATE != 0;
        let delete = r.reason & R_FILE_DELETE != 0 || (r.reason & R_RENAME_OLD != 0 && r.reason & R_RENAME_NEW == 0);
        let rename_new = r.reason & R_RENAME_NEW != 0;
        let data_change = r.reason & (R_DATA_OVERWRITE | R_DATA_EXTEND | R_DATA_TRUNCATION) != 0;
        if create && delete {
            return Err("create+delete in one record".into());
        }
        if delete {
            if let Some(id) = frn_of.remove(&r.frn) {
                scrub_subtree(nodes, frn_of, detached, id);
            }
            continue;
        }
        if create || rename_new {
            let Some(&pnode) = frn_of.get(&r.parent) else {
                return Err(format!("parent {:x} of frn {:x} not in cache", r.parent, r.frn));
            };
            // a node with this name may already exist at the new parent
            // (watcher raced ahead, or the record is a replay): keep it and
            // take over the frn mapping — never duplicate, never churn
            if let Some(existing) = child_by_name(nodes, pnode, &r.name) {
                if let Some(old) = frn_of.remove(&r.frn) {
                    if old != existing {
                        scrub_subtree(nodes, frn_of, detached, old);
                    }
                }
                frn_of.insert(r.frn, existing);
                continue;
            }
            // otherwise a rename drops the old location before re-adding
            if let Some(old) = frn_of.remove(&r.frn) {
                scrub_subtree(nodes, frn_of, detached, old);
            }
            let path = path_of(nodes, pnode, root_path).join(&r.name);
            let is_dir = r.attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
            // symlink_metadata does NOT follow the link: reparse points are
            // junction-mounts when directories and skipped when files (the
            // same rule the walker applies); a stat through the link would
            // pull in the target's size
            let Ok(lm) = std::fs::symlink_metadata(&path) else {
                continue; // vanished between journal and stat
            };
            if lm.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                if is_dir {
                    let id = push_child(nodes, pnode, &r.name, Kind::Mount, 0, 0, 0);
                    frn_of.insert(r.frn, id);
                }
                continue;
            }
            let id = if is_dir {
                // new directory: sub-scan it with the walker (bounded to the
                // new subtree — the same thing a watcher refresh would do)
                let progress = std::sync::Arc::new(super::Progress::default());
                let sub = super::local::scan_blocking(&path, super::ScanLimits::default(), progress);
                graft(nodes, pnode, &sub, &r.name)
            } else {
                let (size, files, mtime) = stat_alloc(&path);
                if files == 0 {
                    // vanished between journal and stat: stage it gone
                    continue;
                }
                let id = push_child(nodes, pnode, &r.name, Kind::File, size, files, mtime);
                // the node carries its own size; propagate into the ancestors
                add_size_walk(nodes, pnode, size as i64, files as i64);
                id
            };
            frn_of.insert(r.frn, id);
            continue;
        }
        if data_change {
            let Some(&id) = frn_of.get(&r.frn) else {
                continue; // folded small file / untracked: aggregate staleness (DEC-SCAN-005)
            };
            if !is_alive(nodes, id) {
                continue;
            }
            if nodes[id as usize].kind != Kind::File {
                continue; // dirs carry subtree totals: an entry-size delta on one
                // would corrupt every ancestor (review finding; any doubt ->
                // leave stale for the watcher, never mutate wrong)
            }
            let path = path_of(nodes, id, root_path);
            match std::fs::metadata(&path) {
                Ok(md) => {
                    let info = platform::entry_info_path(&md, &path);
                    let old = nodes[id as usize].size;
                    let delta = info.alloc as i64 - old as i64;
                    if delta != 0 {
                        add_size_walk(nodes, id, delta, 0);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if let Some(id) = frn_of.remove(&r.frn) {
                        scrub_subtree(nodes, frn_of, detached, id);
                    }
                }
                Err(_) => {} // transient lock: keep the old size, watcher will catch it
            }
        }
    }
    Ok(())
}

/// Fast path: load the cache, replay the journal, install the tree.
/// Returns the fresh checkpoint plus the frn→node list for re-persisting.
/// On any doubt: Err and the tree untouched (caller runs the full MFT scan).
pub fn apply(
    tree: &super::SharedTree,
    cache: Cache,
    vol: &mut super::mft::Volume,
    drive: &str,
    progress: &super::Progress,
) -> Result<(Checkpoint, Vec<(u64, u32)>), String> {
    let Some((jid, lowest, next)) = journal_now(vol) else {
        return Err("journal query failed".into());
    };
    if jid != cache.cp.journal_id {
        return Err("journal id changed".into());
    }
    if cache.cp.next_usn < lowest || next < cache.cp.next_usn {
        return Err("journal wrapped or reset".into());
    }
    let t_read = std::time::Instant::now();
    let recs = read_records(vol, jid, cache.cp.next_usn)?;
    let root_path = {
        let t = tree.read().unwrap();
        t.root_path.clone()
    };
    let root_fs = PathBuf::from(format!("{}\\", drive.trim_end_matches(':').to_ascii_uppercase()));
    let t_patch = std::time::Instant::now();
    let mut nodes = cache.nodes;
    let mut detached = cache.detached;
    let mut frn_of: HashMap<u64, u32> = HashMap::with_capacity(cache.frn_of.len());
    for (frn, id) in cache.frn_of {
        frn_of.insert(frn, id);
    }
    let p = progress.clone();
    patch_nodes(&mut nodes, &mut frn_of, &mut detached, &recs, &root_fs, &|| p.cancelled())?;
    // install: patch validated — swap the arena in one step
    {
        let mut t = tree.write().unwrap();
        let root_name = t.get(t.root).name.clone();
        if let Some(r) = nodes.get_mut(0) {
            r.name = root_name; // keep the current display name (volume label)
        }
        t.install_nodes(nodes, detached);
    }
    let root_files = {
        let t = tree.read().unwrap();
        t.get(t.root).files
    };
    progress.set_status(format!("Rebuilt from cache · {} changes since last scan", recs.len()));
    let exp = progress.expected.load(Relaxed);
    if exp > 0 {
        progress.bytes.store(exp, Relaxed);
    }
    progress.files.store(root_files as u64, Relaxed);
    super::scanlog::line(&format!(
        "usn cache: {} records replayed (read {:.2}s, patch {:.2}s) — scan skipped",
        recs.len(),
        t_read.elapsed().as_secs_f32(),
        t_patch.elapsed().as_secs_f32()
    ));
    let mut frn_list: Vec<(u64, u32)> = frn_of.into_iter().collect();
    frn_list.sort_unstable();
    Ok((Checkpoint { journal_id: jid, next_usn: next }, frn_list))
}

use std::sync::atomic::Ordering::Relaxed;

/// Spawns the post-scan cache writer (background; never blocks the scan).
/// Takes the volume handle: the checkpoint is read BEFORE the tree snapshot,
/// so the replay window starts at the checkpoint and every later change is
/// caught by the next replay (the patch ops are idempotent on overlap — see
/// DEC-SCAN-005).
pub fn spawn_persist(drive: String, tree: super::SharedTree, frn_of: Vec<(u64, u32)>, mut vol: super::mft::Volume) {
    let _ = std::thread::Builder::new().name("mft-cache".into()).spawn(move || {
        let t0 = std::time::Instant::now();
        let Some((jid, _lowest, next)) = journal_now(&mut vol) else {
            super::scanlog::line("usn cache: journal query failed, cache not written");
            return;
        };
        let cp = Checkpoint { journal_id: jid, next_usn: next };
        let (nodes, detached) = {
            let t = tree.read().unwrap();
            (t.nodes.clone(), t.detached_count())
        };
        let n_nodes = nodes.len();
        match save(&drive, &nodes, detached, &frn_of, &cp) {
            Ok(()) => super::scanlog::line(&format!(
                "usn cache: written ({} nodes, {} frn) in {:.2}s",
                n_nodes,
                frn_of.len(),
                t0.elapsed().as_secs_f32()
            )),
            Err(e) => super::scanlog::line(&format!("usn cache: write failed: {e}")),
        }
    });
}

// ---------------------------------------------------------------------------
// Tests: serialization round-trip and replay semantics on synthetic trees
// (patch ops that touch the filesystem use real temp files)

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, size: u64, files: u32, parent: u32, first: u32, next: u32, kind: Kind) -> Node {
        Node { name: name.into(), size, files, parent, first_child: first, next_sibling: next, kind, flags: 0, mtime: 0 }
    }

    /// 0 root → 1 dir "a" (frn 100) → 2 file "f" (frn 200), 3 fold
    fn sample() -> (Vec<Node>, Vec<(u64, u32)>) {
        let nodes = vec![
            node("", 8292, 2, NONE, 1, NONE, Kind::Dir),
            node("a", 8292, 2, 0, 2, NONE, Kind::Dir),
            node("f", 8192, 1, 1, NONE, 3, Kind::File),
            node("\u{2026}", 100, 1, 1, NONE, NONE, Kind::SmallFiles),
        ];
        (nodes, vec![(100, 1), (200, 2)])
    }

    fn rec(frn: u64, parent: u64, reason: u32, name: &str, attrs: u32) -> Rec {
        Rec { frn, parent, reason, attrs, name: name.into() }
    }

    #[test]
    fn serialize_round_trip() {
        let (nodes, frn) = sample();
        let cp = Checkpoint { journal_id: 0x1234_5678_9abc_def0, next_usn: 0xdead_beef_cafe };
        let data = serialize(&nodes, 3, &frn, &cp);
        let c = deserialize(&data).expect("round trip");
        assert_eq!(c.nodes.len(), nodes.len());
        assert_eq!(c.detached, 3);
        assert_eq!(c.cp.journal_id, cp.journal_id);
        assert_eq!(c.cp.next_usn, cp.next_usn);
        assert_eq!(c.frn_of, frn);
        for (a, b) in c.nodes.iter().zip(nodes.iter()) {
            assert_eq!(&*a.name, &*b.name);
            assert_eq!((a.size, a.files, a.parent, a.first_child, a.next_sibling, a.flags, a.mtime), (b.size, b.files, b.parent, b.first_child, b.next_sibling, b.flags, b.mtime));
            assert_eq!(a.kind, b.kind);
        }
        // corruption in the body must be refused (checksum)
        let mut bad = data.clone();
        let mid = bad.len() / 2;
        bad[mid] ^= 0x40;
        assert!(deserialize(&bad).is_none());
        // truncated and empty too
        assert!(deserialize(&data[..data.len() - 12]).is_none());
        assert!(deserialize(&[]).is_none());
    }

    #[test]
    fn replay_delete_updates_rollup() {
        let (mut nodes, frn) = sample();
        let mut map: HashMap<u64, u32> = frn.into_iter().collect();
        let mut detached = 0usize;
        let recs = [rec(200, 1, R_FILE_DELETE | R_CLOSE, "f", 0)];
        patch_nodes(&mut nodes, &mut map, &mut detached, &recs, Path::new("Z:\\"), &|| false).unwrap();
        assert_eq!((nodes[1].size, nodes[1].files), (100, 1)); // fold remains
        assert_eq!((nodes[0].size, nodes[0].files), (100, 1));
        assert!(!map.contains_key(&200));
        assert_eq!(detached, 1);
        assert_ne!(nodes[2].flags & F_DETACHED, 0);
        // deleting an unknown frn is a safe no-op (idempotent replay)
        let recs = [rec(200, 1, R_FILE_DELETE | R_CLOSE, "f", 0)];
        patch_nodes(&mut nodes, &mut map, &mut detached, &recs, Path::new("Z:\\"), &|| false).unwrap();
        assert_eq!(detached, 1);
    }

    #[test]
    fn replay_dir_delete_drops_subtree() {
        let (mut nodes, frn) = sample();
        let mut map: HashMap<u64, u32> = frn.into_iter().collect();
        let mut detached = 0usize;
        let recs = [rec(100, 0, R_FILE_DELETE | R_CLOSE, "a", FILE_ATTRIBUTE_DIRECTORY)];
        patch_nodes(&mut nodes, &mut map, &mut detached, &recs, Path::new("Z:\\"), &|| false).unwrap();
        assert_eq!((nodes[0].size, nodes[0].files), (0, 0));
        assert_eq!(nodes[0].first_child, NONE);
        assert!(!map.contains_key(&100) && !map.contains_key(&200));
        assert_eq!(detached, 1); // the dir only; its children stay under it
    }

    #[test]
    fn replay_create_is_idempotent_by_name() {
        let base = std::env::temp_dir().join(format!("sqd-usn-create-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("a")).unwrap();
        std::fs::write(base.join("a/new.bin"), vec![7u8; 1 << 20]).unwrap();
        // root 0 → dir "a" (frn 100); the disk path of node 1 is base/a
        let mut nodes = vec![
            node("", 0, 0, NONE, 1, NONE, Kind::Dir),
            node("a", 0, 0, 0, NONE, NONE, Kind::Dir),
        ];
        let mut map: HashMap<u64, u32> = HashMap::from([(100, 1)]);
        let mut detached = 0usize;
        let recs = [
            rec(200, 100, R_FILE_CREATE | R_CLOSE, "new.bin", 0x20),
            // same change replayed (watcher raced ahead): must not duplicate
            rec(200, 100, R_FILE_CREATE | R_CLOSE, "new.bin", 0x20),
        ];
        patch_nodes(&mut nodes, &mut map, &mut detached, &recs, &base, &|| false).unwrap();
        let got = child_by_name(&nodes, 1, "new.bin").expect("created");
        assert_eq!(nodes[got as usize].size, 1 << 20); // 1 MiB is 4K-cluster exact
        assert_eq!((nodes[1].size, nodes[1].files), (1 << 20, 1));
        assert_eq!((nodes[0].size, nodes[0].files), (1 << 20, 1));
        assert_eq!(map.get(&200), Some(&got));
        assert_eq!(nodes.iter().filter(|n| n.kind == Kind::File).count(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn replay_rename_moves_between_parents() {
        let base = std::env::temp_dir().join(format!("sqd-usn-rename-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("b")).unwrap();
        std::fs::write(base.join("b/moved.bin"), vec![0u8; 8192]).unwrap();
        // root 0 → a(1, frn 100), b(2, frn 101); file f (frn 200) under a
        let mut nodes = vec![
            node("", 8192, 1, NONE, 1, NONE, Kind::Dir),
            node("a", 8192, 1, 0, 3, 2, Kind::Dir),
            node("b", 0, 0, 0, NONE, NONE, Kind::Dir),
            node("f", 8192, 1, 1, NONE, NONE, Kind::File),
        ];
        let mut map: HashMap<u64, u32> = HashMap::from([(100, 1), (101, 2), (200, 3)]);
        let mut detached = 0usize;
        let recs = [rec(200, 101, R_RENAME_OLD | R_RENAME_NEW | R_CLOSE, "moved.bin", 0x20)];
        patch_nodes(&mut nodes, &mut map, &mut detached, &recs, &base, &|| false).unwrap();
        assert_ne!(nodes[3].flags & F_DETACHED, 0);
        assert_eq!((nodes[1].size, nodes[1].files), (0, 0));
        assert_eq!((nodes[2].size, nodes[2].files), (8192, 1));
        assert_eq!((nodes[0].size, nodes[0].files), (8192, 1));
        assert_eq!(child_by_name(&nodes, 2, "moved.bin"), Some(4));
        assert_eq!(map.get(&200), Some(&4));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn replay_size_change_and_vanish() {
        let base = std::env::temp_dir().join(format!("sqd-usn-size-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let f = base.join("grow.bin");
        std::fs::write(&f, vec![0u8; 4096]).unwrap();
        let mut nodes = vec![node("", 4096, 1, NONE, 1, NONE, Kind::Dir), node("grow.bin", 4096, 1, 0, NONE, NONE, Kind::File)];
        let mut map: HashMap<u64, u32> = HashMap::from([(200, 1)]);
        let mut detached = 0usize;
        std::fs::write(&f, vec![0u8; 16384]).unwrap(); // 4 pages
        let recs = [rec(200, 0, R_DATA_EXTEND | R_CLOSE, "grow.bin", 0x20)];
        patch_nodes(&mut nodes, &mut map, &mut detached, &recs, &base, &|| false).unwrap();
        assert_eq!((nodes[1].size, nodes[0].size), (16384, 16384));
        // file gone on disk → removed from the tree (NotFound branch)
        std::fs::remove_file(&f).unwrap();
        let recs = [rec(200, 0, R_DATA_TRUNCATION | R_CLOSE, "grow.bin", 0x20)];
        patch_nodes(&mut nodes, &mut map, &mut detached, &recs, &base, &|| false).unwrap();
        assert_eq!((nodes[0].size, nodes[0].files), (0, 0));
        assert!(!map.contains_key(&200));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn replay_doubt_refuses_without_mutation() {
        let (mut nodes, frn) = sample();
        let (snap_nodes, snap_frn) = (nodes.clone(), frn.clone());
        let mut map: HashMap<u64, u32> = frn.into_iter().collect();
        let mut detached = 0usize;
        // create whose parent is not in the cache: refuse, change nothing
        let recs = [rec(999, 555, R_FILE_CREATE | R_CLOSE, "x", 0)];
        assert!(patch_nodes(&mut nodes, &mut map, &mut detached, &recs, Path::new("Z:\\"), &|| false).is_err());
        assert_eq!(nodes.len(), snap_nodes.len());
        assert_eq!(nodes[0].first_child, snap_nodes[0].first_child);
        assert_eq!(map.len(), snap_frn.len());
        // create + delete in one record: refuse
        let recs = [rec(200, 1, R_FILE_CREATE | R_FILE_DELETE | R_CLOSE, "f", 0)];
        assert!(patch_nodes(&mut nodes, &mut map, &mut detached, &recs, Path::new("Z:\\"), &|| false).is_err());
    }

    #[test]
    fn tombstones_are_never_revived() {
        let (mut nodes, frn) = sample();
        let mut map: HashMap<u64, u32> = frn.into_iter().collect();
        let mut detached = 0usize;
        let recs = [rec(200, 1, R_FILE_DELETE | R_CLOSE, "f", 0)];
        patch_nodes(&mut nodes, &mut map, &mut detached, &recs, Path::new("Z:\\"), &|| false).unwrap();
        // a create reusing the same frn must NOT revive the tombstone
        let base = std::env::temp_dir().join(format!("sqd-usn-tomb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("a")).unwrap();
        std::fs::write(base.join("a/f"), vec![0u8; 4096]).unwrap();
        let recs = [rec(200, 100, R_FILE_CREATE | R_CLOSE, "f", 0x20)];
        patch_nodes(&mut nodes, &mut map, &mut detached, &recs, &base, &|| false).unwrap();
        let id = map.get(&200).copied().unwrap();
        assert_ne!(id, 2, "must be a fresh node, not the tombstone");
        assert_eq!(nodes[id as usize].size, 4096);
        assert_eq!(nodes.iter().filter(|n| n.flags & F_DETACHED == 0 && n.kind == Kind::File).count(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_nodes_replaces_arena() {
        let mut t = Tree::new("C:\\", "C:", crate::tree::Source::Local);
        t.add_child(0, "live", Kind::Dir, 5, 1);
        let (nodes, _frn) = sample();
        let d = nodes.iter().filter(|n| n.flags & F_DETACHED != 0).count();
        t.install_nodes(nodes, d);
        assert_eq!(t.get(t.root).first_child, 1);
        assert_eq!(t.get(t.root).size, 8292);
        assert_eq!(t.detached_count(), d);
        assert!(t.is_alive(1));
        assert_eq!(t.path(2), "C:\\a\\f");
    }
}
