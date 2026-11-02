//! NTFS Master File Table scanner (Windows-only, SquirrelDisk-AI fork).
//!
//! WizTree's trick, in safe Rust: open the volume, parse the boot sector,
//! read `$MFT`'s dataruns and walk every file record sequentially — one pass
//! over raw metadata gives name, size, modification time and parent for every
//! file on the volume. Directory enumeration (and its per-file stat calls) is
//! what makes ordinary scanners slow; this replaces it.
//!
//! Pipeline: stream the MFT in chunks → fix up + parse each record → stage
//! per-directory (big files kept, small folded, reparse points as mounts) →
//! assemble the arena tree from the root down via a children index.
//!
//! Trust model (DECISIONS.md, DEC-SCAN-002): the MFT is read-only input;
//! nothing here writes or opens user files. Reading the volume handle needs
//! an elevated process; callers fall back to the directory walker otherwise.

use super::local::SMALL_FILES_NAME;
use super::{Progress, ScanLimits, SharedTree};
use crate::tree::{Kind, Tree, F_DONE, F_EXPANDABLE};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// File reference number of the volume root directory.
const ROOT_FRN: u64 = 5;
/// NTFS file references: low 48 bits are the record number.
const FRN_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

// ---------------------------------------------------------------------------
// Raw volume reading (sector-aligned buffers, sequential over dataruns)

struct Volume {
    file: std::fs::File,
    sector: u64,
}

impl Volume {
    /// `C` (or `C:`) → handle on `\\.\C:`. Fails without elevation.
    fn open(drive: &str) -> Option<Volume> {
        let letter = drive.trim_end_matches(':').chars().next()?.to_ascii_uppercase();
        if !letter.is_ascii_alphabetic() {
            return None;
        }
        let path = format!(r"\\.\{letter}:");
        let file = std::fs::File::open(&path).ok()?;
        // 4096 satisfies both 512-byte and 4Kn volumes for the boot read
        Some(Volume { file, sector: 4096 })
    }

    /// Reads exactly `buf.len()` bytes starting at `lba`.
    fn read_at(&mut self, lba: u64, buf: &mut AlignedBuf) -> std::io::Result<()> {
        use std::io::{Seek, SeekFrom};
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::Storage::FileSystem::ReadFile;
        self.file.seek(SeekFrom::Start(lba * self.sector))?;
        let mut done = 0usize;
        while done < buf.len() {
            let mut got = 0u32;
            let ok = unsafe {
                ReadFile(
                    self.file.as_raw_handle() as HANDLE,
                    buf.as_mut_ptr().add(done).cast(),
                    (buf.len() - done) as u32,
                    &mut got,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            if got == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "volume EOF"));
            }
            done += got as usize;
        }
        Ok(())
    }
}

/// 4096-aligned buffer: raw volume reads on Windows require alignment.
struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
    capacity: usize,
}

// Owned, used from one thread at a time, moved into the scan thread.
unsafe impl Send for AlignedBuf {}

impl AlignedBuf {
    fn new(len: usize) -> AlignedBuf {
        let capacity = (len + 4095) & !4095;
        let layout = std::alloc::Layout::from_size_align(capacity, 4096).expect("layout");
        let ptr = unsafe { std::alloc::alloc(layout) };
        assert!(!ptr.is_null(), "volume buffer alloc failed");
        unsafe { std::ptr::write_bytes(ptr, 0, capacity) };
        AlignedBuf { ptr, len, capacity }
    }
    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr
    }
    fn len(&self) -> usize {
        self.len
    }
    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
    /// Shrinks the logical length; the allocation stays.
    fn set_len(&mut self, len: usize) {
        assert!(len <= self.capacity, "set_len beyond capacity");
        self.len = len;
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        let layout = std::alloc::Layout::from_size_align(self.capacity, 4096).expect("layout");
        unsafe { std::alloc::dealloc(self.ptr, layout) };
    }
}

// ---------------------------------------------------------------------------
// Little-endian readers

fn u16at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64at(b: &[u8], o: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(v)
}

/// FILETIME (100ns since 1601-01-01) → unix seconds.
fn filetime_to_unix(ft: u64) -> u64 {
    if ft == 0 {
        return 0;
    }
    ft.saturating_sub(116_444_736_000_000_000) / 10_000_000
}

// ---------------------------------------------------------------------------
// MFT layout from the boot sector

struct MftLayout {
    bytes_per_sector: u64,
    cluster_bytes: u64,
    record_bytes: u64,
    /// Physical runs of `$MFT`: (lcn, clusters).
    runs: Vec<(u64, u64)>,
    total_bytes: u64,
}

impl MftLayout {
    fn parse(vol: &mut Volume) -> Option<MftLayout> {
        let mut boot = AlignedBuf::new(vol.sector as usize);
        vol.read_at(0, &mut boot).ok()?;
        let b = boot.bytes();
        if b.len() < 0x44 || &b[3..7] != b"NTFS" {
            return None; // not an NTFS volume
        }
        let bytes_per_sector = u16at(b, 0x0B) as u64;
        let sectors_per_cluster = b[0x0D] as u64;
        let mft_lcn = u64at(b, 0x30);
        let cluster_bytes = bytes_per_sector.saturating_mul(sectors_per_cluster);
        if bytes_per_sector == 0 || sectors_per_cluster == 0 || mft_lcn == 0 {
            return None;
        }
        vol.sector = bytes_per_sector;
        // bytes-per-record indicator: >=0 → clusters per record, <0 → 2^-v bytes
        let ind = b[0x40] as i8;
        let record_bytes = if ind < 0 {
            let shift = (-(ind as i32)) as u32;
            if shift >= 64 {
                return None;
            }
            1u64 << shift
        } else {
            ind as u64 * cluster_bytes
        };
        if !(256..=65536).contains(&record_bytes) {
            return None;
        }
        // record 0 is `$MFT` itself; its non-resident `$DATA` runs describe
        // the physical layout of the whole MFT
        let mut rec = AlignedBuf::new(record_bytes as usize);
        vol.read_at(mft_lcn * sectors_per_cluster, &mut rec).ok()?;
        let fixed = fixed_up(rec.bytes(), bytes_per_sector as usize)?;
        let attrs = walk_attributes(&fixed)?;
        let mut runs = Vec::new();
        for att in attrs {
            if att.type_ == 0x80 && att.name_len == 0 && att.nonresident {
                if let Some(r) = parse_dataruns(&fixed, att.run_off) {
                    runs = r;
                    break;
                }
            }
        }
        if runs.is_empty() {
            return None;
        }
        let total_bytes: u64 = runs.iter().fold(0u64, |a, &(_, n)| a.saturating_add(n)).saturating_mul(cluster_bytes);
        Some(MftLayout { bytes_per_sector, cluster_bytes, record_bytes, runs, total_bytes })
    }
}

/// Copies a record out and applies the update-sequence fixups. Every
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
}

struct AttrView {
    type_: u32,
    nonresident: bool,
    name_len: u8,
    /// resident: absolute offset of the content in the record
    content_off: usize,
    content_len: usize,
    /// non-resident: absolute offset of the dataruns in the record
    run_off: usize,
    /// non-resident: allocated size (what the volume actually gives it)
    alloc_size: u64,
    /// non-resident: real (logical) size
    real_size: u64,
    /// non-resident: lowest VCN of this attribute instance
    lowest_vcn: u64,
}

/// Walks the attribute chain of one fixed-up record.
fn walk_attributes(rec: &[u8]) -> Option<Vec<AttrView>> {
    let first = u16at(rec, 0x14) as usize;
    let used = u32at(rec, 0x18) as usize;
    if first < 0x30 || first >= rec.len() || used == 0 || used > rec.len() {
        return None;
    }
    let mut out = Vec::new();
    let mut off = first;
    loop {
        if off + 0x10 > used {
            break;
        }
        let type_ = u32at(rec, off);
        if type_ == 0xFFFF_FFFF || type_ == 0 {
            break; // end marker; type 0 is never valid
        }
        let len = u32at(rec, off + 4) as usize;
        if len < 8 || off + len > used {
            break;
        }
        let nonresident = rec[off + 8] != 0;
        let name_len = rec[off + 9];
        // disk-controlled lengths must cover the header fields before any
        // read, or a corrupt record panics the scan thread
        if nonresident && off + 0x40 > rec.len() {
            break;
        }
        if !nonresident && off + 0x18 > rec.len() {
            break;
        }
        let view = if nonresident {
            let ro = u16at(rec, off + 0x20) as usize + off;
            AttrView { type_, nonresident, name_len, content_off: ro, content_len: 0, run_off: ro, alloc_size: u64at(rec, off + 0x28), real_size: u64at(rec, off + 0x30), lowest_vcn: u64at(rec, off + 0x10) }
        } else {
            let cl = u32at(rec, off + 0x10) as usize;
            let co = u16at(rec, off + 0x14) as usize + off;
            AttrView { type_, nonresident, name_len, content_off: co, content_len: cl, run_off: 0, alloc_size: cl as u64, real_size: cl as u64, lowest_vcn: 0 }
        };
        out.push(view);
        off += len;
    }
    Some(out)
}

/// MFT dataruns at `start`: (lcn, clusters). LCNs are signed-relative.
/// Sparse runs (no offset bytes) are dropped — they carry no location.
fn parse_dataruns(rec: &[u8], start: usize) -> Option<Vec<(u64, u64)>> {
    let mut runs = Vec::new();
    let mut off = start;
    let mut lcn: i128 = 0;
    loop {
        if off >= rec.len() {
            break;
        }
        let hdr = rec[off];
        if hdr == 0 {
            break;
        }
        let len_bytes = (hdr & 0x0F) as usize;
        let off_bytes = (hdr >> 4) as usize;
        if len_bytes == 0 || len_bytes > 8 || off_bytes > 8 || off + 1 + len_bytes + off_bytes > rec.len() {
            return None;
        }
        let mut nb = [0u8; 8];
        nb[..len_bytes].copy_from_slice(&rec[off + 1..off + 1 + len_bytes]);
        let clusters = u64::from_le_bytes(nb);
        off += 1 + len_bytes;
        if off_bytes == 0 {
            continue; // sparse hole: no location, nothing to read
        }
        let mut ob = [0u8; 8];
        ob[..off_bytes].copy_from_slice(&rec[off..off + off_bytes]);
        if off_bytes < 8 && ob[off_bytes - 1] & 0x80 != 0 {
            ob[off_bytes..].fill(0xFF); // sign-extend negative deltas
        }
        lcn += i64::from_le_bytes(ob) as i128;
        off += off_bytes;
        if lcn < 0 {
            return None;
        }
        if clusters > 0 {
            runs.push((lcn as u64, clusters));
        }
    }
    if runs.is_empty() {
        None
    } else {
        Some(runs)
    }
}

// ---------------------------------------------------------------------------
// Parsed record

struct Parsed {
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
}

/// Parses one fixed-up record. Names: a record can carry several `$FILE_NAME`
/// attributes (hard links, DOS aliases); the best-namespace name wins and the
/// record is counted exactly once. Sizes: unnamed `$DATA` (resident or real
/// size of non-resident); directories fall back to `$FILE_NAME` sizes (0).
fn parse_record(rec: &[u8]) -> Option<Parsed> {
    let flags = u16at(rec, 0x16);
    if flags & 0x0001 == 0 {
        return None; // free record
    }
    let base = u64at(rec, 0x20) & FRN_MASK;
    let is_dir = flags & 0x0002 != 0;
    let attrs = walk_attributes(rec)?;
    let mut reparse = false;
    let mut si_mtime = 0u64;
    // namespace rank: 3 = Win32+DOS → 4, 1 = Win32 → 3, 0 = POSIX → 2, 2 = DOS-only → 1
    let mut best: Option<(u8, String, u64, u64, u64)> = None;
    let mut data_size: Option<u64> = None;
    let mut data_owner = false;
    for att in &attrs {
        match att.type_ {
            0x10 => {
                if !att.nonresident && att.content_len >= 0x20 {
                    if let Some(c) = rec.get(att.content_off..att.content_off + att.content_len) {
                        si_mtime = filetime_to_unix(u64at(c, 0x08));
                    }
                }
            }
            0x30 => {
                if att.nonresident || att.name_len != 0 || att.content_len < 0x3C {
                    continue; // 0x3C fixed part; name bounds checked below
                }
                let Some(c) = rec.get(att.content_off..att.content_off + att.content_len) else { continue };
                let parent = u64at(c, 0x00) & FRN_MASK;
                let alloc = u64at(c, 0x28);
                let real = u64at(c, 0x30);
                let nl = c[0x3A] as usize;
                let ns = c[0x3B];
                if nl == 0 || c.len() < 0x3C + nl * 2 {
                    continue;
                }
                let rank: u8 = match ns {
                    3 => 4,
                    1 => 3,
                    0 => 2,
                    _ => 1,
                };
                let better = best.as_ref().map(|(r, ..)| *r < rank).unwrap_or(true);
                if better {
                    let units: Vec<u16> = (0..nl).map(|k| u16::from_le_bytes([c[0x3C + k * 2], c[0x3C + k * 2 + 1]])).collect();
                    let name = String::from_utf16_lossy(&units);
                    let mtime = filetime_to_unix(u64at(c, 0x10));
                    // $FILE_NAME sizes are directory-entry-time values;
                    // allocated is the disk-usage truth, real the fallback
                    let fn_size = if alloc > 0 { alloc } else { real };
                    best = Some((rank, name, parent, mtime, fn_size));
                }
            }
            0x80 => {
                if att.name_len != 0 {
                    continue; // named streams are not the file's own usage
                }
                // allocated, not logical: compressed/sparse report occupancy
                let sz = if att.nonresident { att.alloc_size } else { att.content_len as u64 };
                if att.nonresident && att.lowest_vcn == 0 {
                    data_owner = true;
                }
                data_size = Some(match data_size {
                    Some(d) => d.max(sz),
                    None => sz,
                });
            }
            0xC0 => reparse = true, // $REPARSE_POINT (0x90 is $INDEX_ROOT: every dir has it)
            _ => {}
        }
    }
    // extension records carry attribute instances, not names — they still
    // matter (their $DATA belongs to a base record), so parse them nameless
    let (name, parent, fn_mtime, fn_real) = match best {
        Some((_, n, p, m, s)) => (n, p, m, s),
        None if base != 0 => (String::new(), 0, 0, 0),
        None => return None,
    };
    let mtime = if si_mtime > 0 { si_mtime } else { fn_mtime };
    Some(Parsed { name, parent, is_dir, reparse, size: data_size.unwrap_or(fn_real), mtime, base, data_owner })
}

// ---------------------------------------------------------------------------
// Staging

/// One staged directory. Key = its own FRN in `dirs`.
struct Staged {
    parent: u64,
    name: String,
    mtime: u64,
    /// big files kept as nodes: (frn, name, size, mtime)
    files: Vec<(u64, String, u64, u64)>,
    /// bounded min-heap of staged big files: (size, frn, name, mtime), cap
    heap: Option<(std::collections::BinaryHeap<std::cmp::Reverse<(u64, u64, String, u64)>>, usize)>,
    small_size: u64,
    small_count: u32,
    /// reparse points (junctions, symlinks): shown, never followed
    mounts: Vec<String>,
}

impl Staged {
    fn new(parent: u64) -> Staged {
        Staged { parent, name: String::new(), mtime: 0, files: Vec::new(), heap: None, small_size: 0, small_count: 0, mounts: Vec::new() }
    }

    fn named(&self) -> bool {
        !self.name.is_empty()
    }

    /// Stages one file. Big files (>= keep_min) go into a bounded min-heap:
    /// the biggest `max_files_per_dir` are kept, a huge late arrival still
    /// displaces the current minimum — everything else folds into small.
    fn add_file(&mut self, frn: u64, name: String, size: u64, mtime: u64, limits: &ScanLimits) {
        if size < limits.keep_min {
            self.small_size = self.small_size.saturating_add(size);
            self.small_count += 1;
            return;
        }
        use std::cmp::Reverse;
        match self.heap.as_mut() {
            None => {
                let cap = limits.max_files_per_dir;
                let mut h = std::collections::BinaryHeap::new();
                h.push(Reverse((size, frn, name, mtime)));
                while h.len() > cap {
                    if let Some(Reverse((sz, ..))) = h.pop() {
                        self.small_size = self.small_size.saturating_add(sz);
                        self.small_count += 1;
                    }
                }
                self.heap = Some((h, cap));
            }
            Some((h, cap)) => {
                h.push(Reverse((size, frn, name, mtime)));
                while h.len() > *cap {
                    // smallest falls out into the folded aggregate
                    if let Some(Reverse((sz, ..))) = h.pop() {
                        self.small_size = self.small_size.saturating_add(sz);
                        self.small_count += 1;
                    }
                }
            }
        }
    }

    /// Drains the heap into the node-ready file list, biggest first.
    fn finish_files(&mut self) {
        use std::cmp::Reverse;
        if let Some((h, _)) = self.heap.take() {
            let mut v: Vec<(u64, String, u64, u64)> = h.into_iter().map(|Reverse((size, frn, name, mtime))| (frn, name, size, mtime)).collect();
            v.sort_unstable_by(|a, b| b.2.cmp(&a.2));
            self.files = v;
        }
    }
}

fn stage(dirs: &mut HashMap<u64, Staged>, ext: &mut HashMap<u64, u64>, frn: u64, p: Parsed, limits: &ScanLimits) {
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
                d.mounts.push(p.name);
            }
        }
        // reparse FILES (symlinks) are skipped, as the walker skips links
        return;
    }
    if p.is_dir {
        // a directory is staged under its own FRN; its record tells the parent
        let e = dirs.entry(frn).or_insert_with(|| Staged::new(p.parent));
        if e.name.is_empty() {
            e.name = p.name;
            e.parent = p.parent;
            e.mtime = p.mtime;
        }
    } else {
        dirs.entry(p.parent).or_insert_with(|| Staged::new(p.parent)).add_file(frn, p.name, p.size, p.mtime, limits);
    }
}

// ---------------------------------------------------------------------------
// The scan

/// Runs an MFT scan of `drive` into `tree` (root = volume root). Returns
/// `false` when this engine cannot run (no elevation / not NTFS / no MFT) —
/// the caller then uses the directory walker.
pub fn run(tree: SharedTree, drive: &str, progress: Arc<Progress>, limits: ScanLimits) -> bool {
    let prev_expected = progress.expected.load(Ordering::Relaxed);
    let restore = |progress: &Progress| {
        // hand the progress bar back to caller semantics so a walker
        // fallback is not pegged at 99% by half-read MFT bytes
        progress.expected.store(prev_expected, Ordering::Relaxed);
        progress.bytes.store(0, Ordering::Relaxed);
    };
    let mut vol = match Volume::open(drive) {
        Some(v) => v,
        None => return false,
    };
    let lay = match MftLayout::parse(&mut vol) {
        Some(l) => l,
        None => {
            restore(&progress);
            return false;
        }
    };
    progress.set_status("Reading NTFS master file table");
    progress.expected.store(lay.total_bytes, Ordering::Relaxed);

    let mut dirs: HashMap<u64, Staged> = HashMap::new();
    // $DATA found in extension records, folded back into base records
    let mut ext: HashMap<u64, u64> = HashMap::new();

    // Stream the MFT in ~64 MB chunks. Records can straddle chunk reads when
    // cluster_bytes is not a multiple of record_bytes (512 B clusters), so a
    // carry of the trailing partial record moves to the next chunk.
    let record = lay.record_bytes as usize;
    let chunk = 64 << 20;
    let mut buf = AlignedBuf::new(chunk + record);
    let mut carry: Vec<u8> = Vec::new();
    let mut seen_bytes = 0u64;
    let mut records = 0u64;
    let mut staged_files_total = 0u64;

    'runs: for &(lcn, clusters) in &lay.runs {
        let run_bytes = clusters.saturating_mul(lay.cluster_bytes);
        let run_lba = lcn * lay.cluster_bytes / lay.bytes_per_sector;
        let mut done: u64 = 0;
        while done < run_bytes {
            if progress.cancelled() {
                return true; // partial tree stays; the caller handles cancel
            }
            let want = (run_bytes - done).min(chunk as u64);
            let sectors = (want + lay.bytes_per_sector - 1) / lay.bytes_per_sector;
            let read_bytes = sectors * lay.bytes_per_sector;
            buf.set_len(read_bytes as usize);
            if vol.read_at(run_lba + done / lay.bytes_per_sector, &mut buf).is_err() {
                restore(&progress);
                return false;
            }
            let b = buf.bytes();
            let mut stream: Vec<u8> = std::mem::take(&mut carry);
            stream.extend_from_slice(b);
            let usable = stream.len() - stream.len() % record;
            let whole = usable / record;
            let mut in_use = 0u64;
            for i in 0..whole {
                let s = i * record;
                if let Some(fixed) = fixed_up(&stream[s..s + record], lay.bytes_per_sector as usize) {
                    let live = u16at(&fixed, 0x16) & 1 != 0;
                    match parse_record(&fixed) {
                        Some(p) => {
                            in_use += 1;
                            stage(&mut dirs, &mut ext, records + i as u64, p, &limits);
                        }
                        None if live => {
                            progress.errors.fetch_add(1, Ordering::Relaxed);
                        }
                        None => {}
                    }
                }
            }
            records += whole as u64;
            staged_files_total += in_use;
            carry = stream.split_off(usable);
            carry.shrink_to_fit();
            done += read_bytes;
            seen_bytes = (seen_bytes + read_bytes).min(lay.total_bytes);
            progress.bytes.store(seen_bytes, Ordering::Relaxed);
            progress.files.store(staged_files_total, Ordering::Relaxed);
        }
    }

    if progress.cancelled() {
        return true;
    }
    if !dirs.get(&ROOT_FRN).is_some_and(|d| d.named()) {
        restore(&progress);
        return false; // no usable root: wrong parse — let the walker do it
    }

    // big-file heaps are final now: drain them into their lists
    for st in dirs.values_mut() {
        st.finish_files();
    }

    // assemble: children index, then BFS from the root
    progress.set_status("Building tree");
    let mut kids: HashMap<u64, Vec<u64>> = HashMap::new();
    let keys: Vec<u64> = dirs.keys().copied().collect();
    for frn in keys {
        if frn == ROOT_FRN {
            continue;
        }
        if let Some(st) = dirs.get(&frn) {
            if st.named() && st.parent != frn {
                kids.entry(st.parent).or_default().push(frn);
            }
        }
    }

    {
        let mut t = tree.write().unwrap();
        let root = t.root;
        if let Some(m) = dirs.get(&ROOT_FRN).map(|d| d.mtime) {
            t.set_mtime(root, m);
        }
        let mut node_of: HashMap<u64, u32> = HashMap::new();
        node_of.insert(ROOT_FRN, root);
        let mut queue: VecDeque<u64> = VecDeque::new();
        queue.push_back(ROOT_FRN);
        let mut total_files = 0u32;
        // assemble in frontier batches, releasing the tree write lock between
        // batches so the sunburst keeps animating while the tree fills
        while !queue.is_empty() {
            let batch: Vec<u64> = queue.drain(..512.min(queue.len())).collect();
            for frn in batch {
                assemble_one(&mut t, &mut dirs, &mut node_of, &mut queue, &mut total_files, frn, &ext, &kids, &limits, &progress);
            }
            drop(t);
            t = tree.write().unwrap();
        }
        t.get_mut(root).flags |= F_DONE;
        // staged dirs never reached from the root (orphaned cycles, records
        // for deleted parents) are dropped; hidden-space absorbs their size
    }
    true
}

/// Assembles one staged directory into the tree: file/mount/small children,
/// one roll-up `add_size`, and child-dir nodes queued for the next batch.
#[allow(clippy::too_many_arguments)]
fn assemble_one(
    t: &mut Tree,
    dirs: &mut HashMap<u64, Staged>,
    node_of: &mut HashMap<u64, u32>,
    queue: &mut VecDeque<u64>,
    total_files: &mut u32,
    frn: u64,
    ext: &HashMap<u64, u64>,
    kids: &HashMap<u64, Vec<u64>>,
    _limits: &ScanLimits,
    progress: &Progress,
) {
    let Some(st) = dirs.remove(&frn) else { return };
    let node = node_of[&frn];
    if frn != ROOT_FRN {
        t.get_mut(node).flags |= F_DONE;
    }
    let mut size_sum = 0u64;
    let mut file_count = 0u32;
    for (ffrn, name, size, mtime) in st.files.iter() {
        let mut sz = *size;
        if let Some(extra) = ext.get(ffrn) {
            sz = sz.saturating_add(*extra); // $ATTRIBUTE_LIST: split $DATA runs
        }
        let id = t.add_child(node, name, Kind::File, sz, 1);
        t.set_mtime(id, *mtime);
        size_sum = size_sum.saturating_add(sz);
        file_count += 1;
        *total_files += 1;
    }
    for name in &st.mounts {
        t.add_child(node, name, Kind::Mount, 0, 0);
    }
    if st.small_count > 0 {
        let id = t.add_child(node, SMALL_FILES_NAME, Kind::SmallFiles, st.small_size, st.small_count);
        t.get_mut(id).flags |= F_EXPANDABLE;
        size_sum = size_sum.saturating_add(st.small_size);
        file_count = file_count.saturating_add(st.small_count);
        *total_files += st.small_count;
    }
    if size_sum > 0 || file_count > 0 {
        t.add_size(node, size_sum.min(i64::MAX as u64) as i64, file_count as i64);
    }
    for cfrn in kids.get(&frn).map(|v| v.as_slice()).unwrap_or(&[]) {
        if let Some(cst) = dirs.get(cfrn) {
            let id = t.add_child(node, &cst.name, Kind::Dir, 0, 0);
            t.set_mtime(id, cst.mtime);
            node_of.insert(*cfrn, id);
            queue.push_back(*cfrn);
        }
    }
    progress.files.store(*total_files as u64, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Tests: synthetic records exercise the parser without a volume

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixups_round_trip() {
        let mut rec = vec![0u8; 1024];
        rec[..4].copy_from_slice(b"FILE");
        rec[4..6].copy_from_slice(&48u16.to_le_bytes());
        rec[6..8].copy_from_slice(&3u16.to_le_bytes());
        rec[48..50].copy_from_slice(&0xAABBu16.to_le_bytes());
        rec[510..512].copy_from_slice(&0xAABBu16.to_le_bytes());
        rec[1022..1024].copy_from_slice(&0xAABBu16.to_le_bytes());
        rec[50..52].copy_from_slice(&0x1111u16.to_le_bytes());
        rec[52..54].copy_from_slice(&0x2222u16.to_le_bytes());
        let fixed = fixed_up(&rec, 512).unwrap();
        assert_eq!(u16at(&fixed, 510), 0x1111);
        assert_eq!(u16at(&fixed, 1022), 0x2222);
        let mut bad = rec.clone();
        bad[510..512].copy_from_slice(&2u16.to_le_bytes());
        assert!(fixed_up(&bad, 512).is_none());
    }

    #[test]
    fn filetime() {
        assert_eq!(filetime_to_unix(0), 0);
        assert_eq!(filetime_to_unix(133_485_408_000_000_000), 1_704_067_200);
    }

    #[test]
    fn dataruns_parse() {
        let mut rec = vec![0u8; 64];
        rec[10] = 0x11;
        rec[11] = 8;
        rec[12] = 100;
        rec[13] = 0x11;
        rec[14] = 4;
        rec[15] = 10;
        rec[16] = 0;
        let runs = parse_dataruns(&rec, 10).unwrap();
        assert_eq!(runs, vec![(100, 8), (110, 4)]);
    }

    /// Builds one synthetic fixed-up-free record (1024 B, no fixups).
    fn synth_record(dir: bool, parent: u64, name: &str, data: Option<u64>) -> Vec<u8> {
        let mut rec = vec![0u8; 1024];
        rec[..4].copy_from_slice(b"FILE");
        rec[0x14..0x16].copy_from_slice(&0x30u16.to_le_bytes());
        rec[0x18..0x1C].copy_from_slice(&1024u32.to_le_bytes());
        rec[0x16..0x18].copy_from_slice(&if dir { 0x0003u16 } else { 0x0001u16 }.to_le_bytes());
        // $STANDARD_INFORMATION @0x30 (48+0x18 content), mtime 2024-01-01
        let mut off = 0x30;
        rec[off..off + 4].copy_from_slice(&0x10u32.to_le_bytes());
        rec[off + 4..off + 8].copy_from_slice(&0x60u32.to_le_bytes());
        rec[off + 0x10..off + 0x14].copy_from_slice(&0x48u32.to_le_bytes());
        rec[off + 0x14..off + 0x16].copy_from_slice(&0x18u16.to_le_bytes());
        let mtime_ft = 133_485_408_000_000_000u64;
        rec[off + 0x18 + 0x08..off + 0x18 + 0x10].copy_from_slice(&mtime_ft.to_le_bytes());
        // $FILE_NAME @0x90
        off = 0x90;
        let nl = name.len();
        let fn_len = 0x18 + 0x3C + nl * 2;
        rec[off..off + 4].copy_from_slice(&0x30u32.to_le_bytes());
        rec[off + 4..off + 8].copy_from_slice(&(fn_len as u32).to_le_bytes());
        rec[off + 0x10..off + 0x14].copy_from_slice(&((fn_len - 0x18) as u32).to_le_bytes());
        rec[off + 0x14..off + 0x16].copy_from_slice(&0x18u16.to_le_bytes());
        let f = off + 0x18;
        rec[f..f + 8].copy_from_slice(&parent.to_le_bytes());
        rec[f + 0x28..f + 0x30].copy_from_slice(&(data.unwrap_or(0)).to_le_bytes());
        rec[f + 0x30..f + 0x38].copy_from_slice(&(data.unwrap_or(0)).to_le_bytes());
        rec[f + 0x3A] = nl as u8;
        rec[f + 0x3B] = 3; // Win32+DOS namespace
        for (k, ch) in name.encode_utf16().collect::<Vec<u16>>().iter().enumerate() {
            rec[f + 0x3C + k * 2..f + 0x3E + k * 2].copy_from_slice(&ch.to_le_bytes());
        }
        // resident $DATA after $FILE_NAME
        if let Some(sz) = data.filter(|_| !dir) {
            off = 0x90 + fn_len;
            off = (off + 7) & !7; // 8-aligned
            rec[off..off + 4].copy_from_slice(&0x80u32.to_le_bytes());
            rec[off + 4..off + 8].copy_from_slice(&0x30u32.to_le_bytes());
            rec[off + 0x10..off + 0x14].copy_from_slice(&(sz as u32).to_le_bytes());
            rec[off + 0x14..off + 0x16].copy_from_slice(&0x18u16.to_le_bytes());
            rec[off + 0x18..off + 0x1C].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        } else {
            let e = 0x90 + fn_len;
            rec[e..e + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        }
        rec
    }

    #[test]
    fn record_parse_names_and_sizes() {
        let rec = synth_record(false, 5, "bin!", Some(800));
        let p = parse_record(&rec).unwrap();
        assert_eq!(p.name, "bin!");
        assert_eq!(p.parent, 5);
        assert_eq!(p.size, 800);
        assert_eq!(p.mtime, 1_704_067_200);
        assert!(!p.is_dir);
        // a free record parses to None
        let mut free = synth_record(true, 5, "x", None);
        free[0x16..0x18].copy_from_slice(&0u16.to_le_bytes());
        assert!(parse_record(&free).is_none());
    }



    #[test]
    fn stage_and_assemble() {
        // root (frn 5) → dir "a" (frn 100) → file "f" (frn 200, 5 KB)
        // file kept only when ≥ keep_min
        let limits = ScanLimits { keep_min: 4096, max_files_per_dir: 8 };
        let mut dirs = HashMap::new();
        let mut ext = HashMap::new();
        stage(&mut dirs, &mut ext, 5, parse_record(&synth_record(true, 5, ".", None)).unwrap(), &limits);
        stage(&mut dirs, &mut ext, 100, parse_record(&synth_record(true, 5, "a", None)).unwrap(), &limits);
        stage(&mut dirs, &mut ext, 200, parse_record(&synth_record(false, 100, "f", Some(8192))).unwrap(), &limits);
        stage(&mut dirs, &mut ext, 201, parse_record(&synth_record(false, 100, "tiny", Some(100))).unwrap(), &limits);
        assert!(dirs.get(&5).unwrap().named());
        for st in dirs.values_mut() {
            st.finish_files();
        }
        let a = dirs.get(&100).unwrap();
        assert_eq!(a.name, "a");
        assert_eq!(a.files.len(), 1);
        assert_eq!(a.small_count, 1);
        assert_eq!(a.small_size, 100);
    }
}
