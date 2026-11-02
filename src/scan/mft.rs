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
use crate::tree::{Kind, F_DONE, F_EXPANDABLE};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// File reference number of the volume root directory.
const ROOT_FRN: u64 = 5;
/// NTFS file references: low 48 bits are the record number.
const FRN_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

// ---------------------------------------------------------------------------
// Raw volume reading (sector-aligned buffers, sequential over dataruns)

pub struct Volume {
    pub file: std::fs::File,
    sector: u64,
}

impl Volume {
    /// `C` (or `C:`) -> unbuffered handle on the raw volume. Fails without
    /// elevation. FILE_FLAG_NO_BUFFERING bypasses the cache manager (raw DMA
    /// into our aligned buffers, ~2-3x faster than buffered volume reads);
    /// it requires sector-aligned offsets, lengths and buffers - all of
    /// which this reader guarantees.
    fn open(drive: &str) -> Option<Volume> {
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::Storage::FileSystem::CreateFileW;
        use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_NO_BUFFERING, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
        let letter = drive.trim_end_matches(':').chars().next()?.to_ascii_uppercase();
        if !letter.is_ascii_alphabetic() {
            return None;
        }
        let device = format!("\\\\.\\{letter}:");
        let wide: Vec<u16> = device.encode_utf16().chain(std::iter::once(0)).collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                windows_sys::Win32::Storage::FileSystem::FILE_READ_DATA,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_NO_BUFFERING,
                std::ptr::null_mut(),
            )
        };
        if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return None;
        }
        let file = unsafe { std::fs::File::from_raw_handle(handle) };
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
        // no zero-fill: read_at writes every byte below `len` before any
        // parse reads it (saves ~30ms of memset across the 4×64MB pool)
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
        // the physical layout of the whole MFT. NO_BUFFERING reads must be
        // sector-aligned in length too: on 4Kn volumes a 1024-byte record is
        // read padded to a whole sector and the fixup runs on the prefix.
        let rb = record_bytes as usize;
        let bps = bytes_per_sector as usize;
        let read_len = rb.div_ceil(bps) * bps;
        let mut rec = AlignedBuf::new(read_len);
        vol.read_at(mft_lcn * sectors_per_cluster, &mut rec).ok()?;
        let fixed = fixed_up(&rec.bytes()[..rb], bytes_per_sector as usize)?;
        let mut attrs = [AttrView::default(); MAX_ATTRS];
        let n = walk_attributes_buf(&fixed, &mut attrs)?;
        let mut runs = Vec::new();
        for att in attrs.into_iter().take(n) {
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

/// Applies the update-sequence fixups in place. Every sector-sized block of
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
}

#[derive(Clone, Copy, Default)]
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

/// Most records carry <= 4 attributes; 16 covers hardlink-heavy base
/// records (attribute chains are type-sorted; $DATA sits late) without heap.
const MAX_ATTRS: usize = 16;

/// Walks the attribute chain of one fixed-up record into `out`; returns the
/// count. No allocation.
fn walk_attributes_buf(rec: &[u8], out: &mut [AttrView; MAX_ATTRS]) -> Option<usize> {
    let first = u16at(rec, 0x14) as usize;
    let used = u32at(rec, 0x18) as usize;
    if first < 0x30 || first >= rec.len() || used == 0 || used > rec.len() {
        return None;
    }
    let mut cnt = 0usize;
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
        if cnt == MAX_ATTRS {
            break; // pathological record: extra attributes ignored
        }
        out[cnt] = view;
        cnt += 1;
        off += len;
    }
    Some(cnt)
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

/// Parsed record borrowing the record buffer: the name stays an undecoded
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
}

/// Parses one fixed-up record. Names: a record can carry several `$FILE_NAME`
/// attributes (hard links, DOS aliases); the best-namespace name wins and the
/// record is counted exactly once. Sizes: unnamed `$DATA` (allocated for
/// non-resident); directories fall back to `$FILE_NAME` sizes (0).
fn parse_record_buf<'a>(rec: &'a [u8], attrs: &mut [AttrView; MAX_ATTRS]) -> Option<ParsedRef<'a>> {
    let flags = u16at(rec, 0x16);
    if flags & 0x0001 == 0 {
        return None; // free record
    }
    let base = u64at(rec, 0x20) & FRN_MASK;
    let is_dir = flags & 0x0002 != 0;
    let n = walk_attributes_buf(rec, attrs)?;
    let mut reparse = false;
    let mut si_mtime = 0u64;
    // namespace rank: 3 = Win32+DOS → 4, 1 = Win32 → 3, 0 = POSIX → 2, 2 = DOS-only → 1
    // (rank, name span start, name span len, parent, mtime, size)
    let mut best: Option<(u8, usize, usize, u64, u64, u64)> = None;
    let mut data_size: Option<u64> = None;
    let mut data_owner = false;
    for att in attrs.iter().take(n) {
        match att.type_ {
            0x10 => {
                if !att.nonresident && att.content_len >= 0x20 {
                    if let Some(c) = rec.get(att.content_off..att.content_off + att.content_len) {
                        si_mtime = filetime_to_unix(u64at(c, 0x08));
                    }
                }
            }
            0x30 => {
                if att.nonresident || att.name_len != 0 || att.content_len < 0x42 {
                    continue; // 0x42 fixed part; name bounds checked below
                }
                let Some(c) = rec.get(att.content_off..att.content_off + att.content_len) else { continue };
                let parent = u64at(c, 0x00) & FRN_MASK;
                let alloc = u64at(c, 0x28);
                let real = u64at(c, 0x30);
                // layout: parent@0, times@8..0x28, alloc@0x28, real@0x30,
                // flags u32@0x38, reserved@0x3C, name len@0x40, namespace@0x41,
                // UTF-16 name@0x42 (verified against a live Win11 volume dump)
                let nl = c[0x40] as usize;
                let ns = c[0x41];
                if nl == 0 || c.len() < 0x42 + nl * 2 {
                    continue;
                }
                let rank: u8 = match ns {
                    3 => 4,
                    1 => 3,
                    0 => 2,
                    _ => 1,
                };
                let better = best.map(|(r, ..)| r < rank).unwrap_or(true);
                if better {
                    let mtime = filetime_to_unix(u64at(c, 0x10));
                    // $FILE_NAME sizes are directory-entry-time values;
                    // allocated is the disk-usage truth, real the fallback
                    let fn_size = if alloc > 0 { alloc } else { real };
                    let start = att.content_off + 0x42;
                    best = Some((rank, start, nl * 2, parent, mtime, fn_size));
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
    let (name_bytes, parent, fn_mtime, fn_real) = match best {
        Some((_, start, len, p, m, sz)) => (rec.get(start..start + len)?, p, m, sz),
        None if base != 0 => (&[][..], 0, 0, 0),
        None => return None,
    };
    let mtime = if si_mtime > 0 { si_mtime } else { fn_mtime };
    Some(ParsedRef { name_bytes, parent, is_dir, reparse, size: data_size.unwrap_or(fn_real), mtime, base, data_owner })
}

/// Owned-parse adapter (tests, one-off callers): decodes the name span.
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

/// Hot-path staging on ref-parsed records: small files never decode a name.
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

/// True when the volume handle exists but elevation is missing: the scan
/// will silently use the directory walker unless the app is relaunched
/// elevated. Surfaces as a UI hint (Progress::hint).
pub fn needs_admin(drive: &str) -> bool {
    let Some(letter) = drive.trim_end_matches(':').chars().next().map(|c| c.to_ascii_uppercase()) else { return false };
    if !letter.is_ascii_alphabetic() {
        return false;
    }
    match std::fs::File::open(format!(r"\\.\{letter}:")) {
        Err(e) => e.kind() == std::io::ErrorKind::PermissionDenied,
        _ => false,
    }
}

/// Runs an MFT scan of `drive` into `tree` (root = volume root). Returns
/// `false` when this engine cannot run (no elevation / not NTFS / no MFT) —
/// the caller then uses the directory walker.
pub fn run(tree: SharedTree, drive: &str, progress: Arc<Progress>, limits: ScanLimits) -> bool {
    let prev_expected = progress.expected.load(Ordering::Relaxed);
    let restore = |progress: &Progress| {
        // hand the progress counters back to caller semantics so a walker
        // fallback is not pegged at 99% (or double-counted) by half-read MFT
        progress.expected.store(prev_expected, Ordering::Relaxed);
        progress.bytes.store(0, Ordering::Relaxed);
        progress.files.store(0, Ordering::Relaxed);
        progress.errors.store(0, Ordering::Relaxed);
    };
    let t_open = std::time::Instant::now();
    let mut vol = match Volume::open(drive) {
        Some(v) => v,
        None => {
            super::scanlog::line(&format!("mft {drive}: volume open failed (elevated={})", super::scanlog::elevated()));
            return false;
        }
    };
    super::scanlog::line(&format!("mft {drive}: volume open {:.1}ms", t_open.elapsed().as_secs_f32() * 1000.0));
    let lay = match MftLayout::parse(&mut vol) {
        Some(l) => l,
        None => {
            super::scanlog::line(&format!("mft {drive}: layout parse refused (boot/MFT record), falling back to directories"));
            restore(&progress);
            return false;
        }
    };
    super::scanlog::line(&format!(
        "mft {drive}: layout ok — record={}B clusters={} runs={} total={:.1}MB threads={}",
        lay.record_bytes,
        lay.cluster_bytes,
        lay.runs.len(),
        lay.total_bytes as f64 / 1e6,
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
    ));

    // USN-cache fast path (DECISIONS.md DEC-SCAN-005): rebuild from the last
    // scan's tree and replay journal changes. Any doubt falls through to the
    // full scan below with the tree untouched.
    if let Some(cache) = super::usncache::load(drive) {
        let t_cache = std::time::Instant::now();
        match super::usncache::apply(&tree, cache, &mut vol, drive, &progress) {
            Ok((cp, frn_of)) => {
                super::scanlog::line(&format!(
                    "usn cache: hit — tree rebuilt in {:.2}s (checkpoint usn {})",
                    t_cache.elapsed().as_secs_f32(),
                    cp.next_usn
                ));
                super::usncache::spawn_persist(drive.to_string(), tree, frn_of, vol);
                return true;
            }
            Err(reason) => {
                super::scanlog::line(&format!("usn cache: discarded ({reason}) — full scan"));
            }
        }
    }
    let t_start = std::time::Instant::now();
    progress.set_status("Reading NTFS master file table");
    progress.expected.store(lay.total_bytes, Ordering::Relaxed);

    // Per-thread staging maps: every thread parses its slice of each chunk
    // into its own maps, merged once after the read loop. Record numbers are
    // absolute MFT stream positions / record size, so threads stay consistent.
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 16);
    let est_dirs = ((lay.total_bytes / lay.record_bytes) / 16) as usize;
    let mut tdirs: Vec<HashMap<u64, Staged>> = (0..threads).map(|_| HashMap::with_capacity((est_dirs / threads).max(1024))).collect();
    let mut texts: Vec<HashMap<u64, u64>> = (0..threads).map(|_| HashMap::new()).collect();
    let mut tlive: Vec<u64> = vec![0; threads];

    // Record-aligned chunk plan; records straddling a run boundary (possible
    // only when a cluster is smaller than a record) are skipped by keeping
    // every read on the absolute record grid.
    let record = lay.record_bytes as usize;
    // I/O grid: every planned read must contain whole records (the absolute
    // record grid) AND be sector-aligned for FILE_FLAG_NO_BUFFERING — on 4Kn
    // volumes those two differ, so round to their least common multiple.
    fn gcd(a: u64, b: u64) -> u64 {
        if b == 0 { a } else { gcd(b, a % b) }
    }
    let grid: u64 = record as u64 / gcd(record as u64, lay.bytes_per_sector) * lay.bytes_per_sector;
    let chunk: u64 = 64 << 20;
    let mut plan: Vec<(u64, u64, u64)> = Vec::new(); // (lba, bytes, record_base)
    let mut stream_pos: u64 = 0;
    for &(lcn, clusters) in &lay.runs {
        let run_bytes = clusters.saturating_mul(lay.cluster_bytes);
        let run_lba = lcn * lay.cluster_bytes / lay.bytes_per_sector;
        let misalign = stream_pos % grid;
        let mut done = if misalign != 0 { grid - misalign } else { 0 };
        done = done.min(run_bytes);
        stream_pos += done;
        while done < run_bytes {
            let mut want = (run_bytes - done).min(chunk);
            want -= want % grid; // whole records, sector-aligned
            if want == 0 {
                break; // sub-record tail: skipped by the grid realignment
            }
            plan.push((run_lba + done / lay.bytes_per_sector, want, stream_pos / lay.record_bytes));
            stream_pos += want;
            done += want;
        }
        stream_pos += run_bytes - done;
    }
    let total_bytes = lay.total_bytes;

    // Reader thread streams chunks ahead of the parse threads (ping-pong
    // buffers): read of chunk N+1 overlaps the parallel parse of chunk N,
    // so the whole pass costs ~max(I/O, parse) instead of their sum.
    let read_failed: std::sync::Arc<std::sync::Mutex<Option<String>>> = std::sync::Arc::new(std::sync::Mutex::new(None));
    // two readers on separate unbuffered handles, interleaving the plan:
    // two outstanding reads give the NVMe queue depth it wants (~2x I/O).
    // Buffers travel WITH their absolute record base — two interleaved
    // readers complete chunks out of plan order, and assigning bases by
    // arrival order would corrupt every FRN in the wrong chunk.
    type Chunk = (u64, AlignedBuf);
    let (tx, rx): (std::sync::mpsc::Sender<Chunk>, std::sync::mpsc::Receiver<Chunk>) = std::sync::mpsc::channel();
    let (tx_back, rx_back): (std::sync::mpsc::Sender<AlignedBuf>, std::sync::mpsc::Receiver<AlignedBuf>) = std::sync::mpsc::channel();
    let back_shared: std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Receiver<AlignedBuf>>> =
        std::sync::Arc::new(std::sync::Mutex::new(rx_back));
    let free_pool: std::sync::Arc<std::sync::Mutex<Vec<AlignedBuf>>> =
        std::sync::Arc::new(std::sync::Mutex::new((0..4).map(|_| AlignedBuf::new(chunk as usize)).collect()));
    let mut readers = Vec::new();
    for reader_id in 0..2u8 {
        let Some(mut vol_r) = Volume::open(drive) else {
            super::scanlog::line("mft: reader handle open failed (sharing violation?)");
            break;
        };
        let plan_r: Vec<(u64, u64, u64)> = plan
            .clone()
            .into_iter()
            .enumerate()
            .filter(|(i, _)| (*i as u8 % 2) == reader_id)
            .map(|(_, p)| p)
            .collect();
        let tx = tx.clone();
        let back = back_shared.clone();
        let pool = free_pool.clone();
        let fail_flag = read_failed.clone();
        let p_r = progress.clone();
        readers.push(std::thread::spawn(move || {
            'plan: for &(lba, want, base) in &plan_r {
                if p_r.cancelled() {
                    break 'plan;
                }
                let mut buf = match pool.lock().unwrap().pop() {
                    Some(b) => b,
                    None => match back.lock().unwrap().recv() {
                        Ok(b) => b,
                        Err(_) => break 'plan,
                    },
                };
                buf.set_len(want as usize);
                if vol_r.read_at(lba, &mut buf).is_err() {
                    let mut f = fail_flag.lock().unwrap();
                    if f.is_none() {
                        *f = Some("volume read failed".into());
                    }
                    break 'plan;
                }
                if tx.send((base, buf)).is_err() {
                    break 'plan;
                }
            }
        }));
    }
    drop(tx); // readers hold their own senders; rx ends when all are done
    // `vol` stays open: the post-scan cache writer reuses this handle to
    // take the USN checkpoint (spawn_persist at the end of run)
    let mut seen_bytes = 0u64;
    let mut got_chunks = 0usize;
    while let Ok((record_base, buf)) = rx.recv() {
        if progress.cancelled() {
            break;
        }
        let want = buf.len() as u64;
        let b = buf.bytes();
        {
            let n = b.len() / record;
            let slice = (n / threads).max(1) * record;
            std::thread::scope(|sc| {
                let it = tdirs.iter_mut().zip(texts.iter_mut()).zip(tlive.iter_mut());
                for (t, ((dirs_t, ext_t), live_t)) in it.enumerate() {
                    let start = t * slice;
                    if start >= b.len() {
                        break;
                    }
                    let end = if t + 1 == threads || (t + 1) * slice >= b.len() { b.len() } else { (t + 1) * slice };
                    let base = record_base + (start / record) as u64;
                    let b_t: &[u8] = &b[start..end];
                    let sector = lay.bytes_per_sector as usize;
                    let limits = &limits;
                    let progress = &progress;
                    sc.spawn(move || {
                        let mut scratch = vec![0u8; record];
                        let mut attrs = [AttrView::default(); MAX_ATTRS];
                        for (i, off) in (0..b_t.len()).step_by(record).enumerate() {
                            let hdr = &b_t[off..off + record];
                            // fast reject before any copy: magic + in-use
                            if &hdr[..4] != b"FILE" {
                                continue;
                            }
                            if hdr[0x16] & 1 == 0 {
                                continue; // free record: no fixups, no parse
                            }
                            scratch.copy_from_slice(hdr);
                            if !apply_fixups_inplace(&mut scratch, sector) {
                                continue;
                            }
                            match parse_record_buf(&scratch, &mut attrs) {
                                Some(p) => {
                                    *live_t += 1;
                                    stage_ref(dirs_t, ext_t, base + i as u64, p, limits);
                                }
                                None => {
                                    // free records never reach here (rejected
                                    // before the copy): a live record that
                                    // fails to parse is an error
                                    progress.errors.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                    });
                }
            });
        }
        seen_bytes = (seen_bytes + want).min(total_bytes);
        got_chunks += 1;
        progress.bytes.store(seen_bytes, Ordering::Relaxed);
        progress.files.store(tlive.iter().sum::<u64>(), Ordering::Relaxed);
        if tx_back.send(buf).is_err() {
            break;
        }
    }
    drop(tx_back);
    for r in readers {
        let _ = r.join();
    }
    {
        let cancelled = progress.cancelled();
        super::scanlog::line(&format!(
            "mft: read+parse {:.2}s — {:.0}MB, {} live records, {} errors{}",
            t_start.elapsed().as_secs_f32(),
            seen_bytes as f64 / 1e6,
            tlive.iter().sum::<u64>(),
            progress.errors.load(Ordering::Relaxed),
            if cancelled { ", cancelled" } else { "" }
        ));
    }
    if let Some(err) = read_failed.lock().unwrap().take() {
        super::scanlog::line(&format!("mft: reader failed after {}/{} chunks: {err}", got_chunks, plan.len()));
        restore(&progress);
        return false;
    }

    if progress.cancelled() {
        return true;
    }

    // merge the per-thread staging maps
    let mut dirs: HashMap<u64, Staged> = tdirs.swap_remove(0);
    for other in tdirs {
        for (frn, st) in other {
            use std::collections::hash_map::Entry;
            match dirs.entry(frn) {
                Entry::Occupied(mut e) => {
                    let dst = e.get_mut();
                    if dst.name.is_empty() && !st.name.is_empty() {
                        dst.name = st.name;
                        dst.parent = st.parent;
                        dst.mtime = st.mtime;
                    }
                    for (f, n, sz, m) in st.files {
                        dst.add_file(f, n, sz, m, &limits);
                    }
                    if let Some((h, _)) = st.heap {
                        use std::cmp::Reverse;
                        for Reverse((sz, f, n, m)) in h {
                            dst.add_file(f, n, sz, m, &limits);
                        }
                    }
                    dst.small_size = dst.small_size.saturating_add(st.small_size);
                    dst.small_count = dst.small_count.saturating_add(st.small_count);
                    for m in st.mounts {
                        if dst.mounts.len() < 64 {
                            dst.mounts.push(m);
                        }
                    }
                }
                Entry::Vacant(e) => {
                    e.insert(st);
                }
            }
        }
    }
    let mut ext: HashMap<u64, u64> = texts.swap_remove(0);
    for other in texts {
        for (k, v) in other {
            let e = ext.entry(k).or_insert(0);
            *e = (*e).max(v);
        }
    }
    super::scanlog::line(&format!(
        "mft: staged {} dirs, {} ext-data entries (merge {:.2}s)",
        dirs.len(),
        ext.len(),
        t_start.elapsed().as_secs_f32()
    ));

    if !dirs.get(&ROOT_FRN).is_some_and(|d| d.named()) {
        super::scanlog::line("mft: no usable root record staged, falling back to directories");
        restore(&progress);
        return false; // no usable root: wrong parse — let the walker do it
    }

    // big-file heaps are final now: drain them into their lists
    for st in dirs.values_mut() {
        st.finish_files();
    }

    // assemble bottom-up: reverse-BFS subtree totals, then one creation
    // pass whose nodes are born with their final sizes (no ancestor walks)
    let frn_nodes = assemble(&tree, dirs, &ext, &progress);
    // post-scan cache write on its own thread (checkpoint + ~100MB
    // serialization must never sit on the scan critical path); it also
    // carries the volume handle out and drops it when done
    super::usncache::spawn_persist(drive.to_string(), tree, frn_nodes, vol);
    true
}

/// Assembles the staged directories into the arena tree.
///
/// Two passes over a BFS order from the root. The reverse order accumulates
/// every directory's final subtree total bottom-up (reverse BFS guarantees
/// each child is final before its parent sums it); the forward order then
/// creates the nodes with those totals directly, so `add_size` ancestor
/// walks disappear entirely — they cost one hop per depth level per
/// directory, ~7M hops on a real volume.
///
/// Returns the frn→node list for the USN cache (assembled dirs + kept files).
fn assemble(tree: &SharedTree, mut dirs: HashMap<u64, Staged>, ext: &HashMap<u64, u64>, progress: &Progress) -> Vec<(u64, u32)> {
    let t0 = std::time::Instant::now();
    // children index, pre-sized: rehashing ~600k entries costs ~30ms
    let mut kids: HashMap<u64, Vec<u64>> = HashMap::with_capacity(dirs.len());
    let file_est: usize = dirs.values().map(|d| d.files.len()).sum();
    for (&frn, st) in dirs.iter() {
        if frn != ROOT_FRN && st.named() && st.parent != frn {
            kids.entry(st.parent).or_default().push(frn);
        }
    }
    // BFS order from the root; each staged dir appears under exactly one
    // parent (map keys), so no visited set is needed. Dirs the BFS never
    // reaches (orphaned cycles, records for deleted parents) stay in `dirs`
    // and are dropped — hidden-space absorbs their size, as before.
    let mut order: Vec<u64> = Vec::with_capacity(dirs.len());
    let mut pos_of: HashMap<u64, u32> = HashMap::with_capacity(dirs.len());
    order.push(ROOT_FRN);
    pos_of.insert(ROOT_FRN, 0);
    let mut i = 0;
    while i < order.len() {
        let frn = order[i];
        i += 1;
        if let Some(cs) = kids.get(&frn) {
            for &c in cs {
                pos_of.insert(c, order.len() as u32);
                order.push(c);
            }
        }
    }
    // bottom-up totals: own files (+ extension-record $DATA) + small fold,
    // then each dir pushes its final total into its parent's slot
    let mut totals: Vec<(u64, u32)> = vec![(0, 0); order.len()];
    for pos in (0..order.len()).rev() {
        let Some(st) = dirs.get(&order[pos]) else { continue };
        let mut sz = st.small_size;
        let mut fc = st.small_count;
        for &(ffrn, _, size, _) in &st.files {
            sz = sz.saturating_add(size.saturating_add(ext.get(&ffrn).copied().unwrap_or(0)));
            fc += 1;
        }
        sz = sz.saturating_add(totals[pos].0);
        fc = fc.saturating_add(totals[pos].1);
        totals[pos] = (sz, fc);
        if pos != 0 {
            if let Some(&p) = pos_of.get(&st.parent) {
                let (ps, pf) = &mut totals[p as usize];
                *ps = ps.saturating_add(sz);
                *pf = pf.saturating_add(fc);
            }
        }
    }
    let pre_secs = t0.elapsed().as_secs_f32();

    // forward creation. Batches of 2048 release the write lock between them
    // so the sunburst keeps animating while the tree fills.
    progress.set_status("Building tree");
    let mut dir_nodes: Vec<u32> = vec![0; order.len()];
    let mut frn_nodes: Vec<(u64, u32)> = Vec::with_capacity(order.len().saturating_add(file_est));
    let mut t = tree.write().unwrap();
    let root = t.root;
    t.reserve(file_est.saturating_add(order.len()).saturating_add(16));
    dir_nodes[0] = root;
    if let Some(m) = dirs.get(&ROOT_FRN).map(|d| d.mtime) {
        t.set_mtime(root, m);
    }
    let mut seen_files = 0u32;
    for pos in 0..order.len() {
        let Some(st) = dirs.remove(&order[pos]) else { continue };
        let node = if pos == 0 {
            root // already exists: this position only attaches its children
        } else {
            let Some(&ppos) = pos_of.get(&st.parent) else { continue };
            let id = t.add_child_owned(dir_nodes[ppos as usize], st.name.into(), Kind::Dir, totals[pos].0, totals[pos].1);
            dir_nodes[pos] = id;
            {
                let n = t.get_mut(id);
                n.flags |= F_DONE;
                n.mtime = st.mtime;
            }
            frn_nodes.push((order[pos], id));
            id
        };
        // own children, in the order the engine always used: big files,
        // mounts, the small-files fold, then the child dirs (created when
        // their own positions come up — same sibling chain as before). Names
        // move out of the staging map: no per-node allocation on this path.
        let kept = st.files.len() as u32;
        for (ffrn, name, size, mtime) in st.files {
            let sz = size.saturating_add(ext.get(&ffrn).copied().unwrap_or(0));
            let id = t.add_child_owned(node, name.into(), Kind::File, sz, 1);
            t.get_mut(id).mtime = mtime;
            frn_nodes.push((ffrn, id));
        }
        for name in st.mounts {
            t.add_child_owned(node, name.into(), Kind::Mount, 0, 0);
        }
        if st.small_count > 0 {
            let id = t.add_child_owned(node, SMALL_FILES_NAME.into(), Kind::SmallFiles, st.small_size, st.small_count);
            t.get_mut(id).flags |= F_EXPANDABLE;
        }
        seen_files = seen_files.saturating_add(kept).saturating_add(st.small_count);
        if (pos + 1) % 2048 == 0 || pos + 1 == order.len() {
            progress.files.store(seen_files as u64, Ordering::Relaxed);
            drop(t);
            t = tree.write().unwrap();
        }
    }
    // the root carries the exact roll-up: its size must equal the sum of
    // everything reachable from it (the bench asserts this on the corpus)
    {
        let r = t.get_mut(root);
        r.size = totals[0].0;
        r.files = totals[0].1;
        r.flags |= F_DONE;
    }
    let nodes_len = t.nodes.len();
    drop(t);
    super::scanlog::line(&format!(
        "mft: assembly {:.2}s (order+totals {:.2}s, create {:.2}s) — {} nodes, {} frn entries",
        t0.elapsed().as_secs_f32(),
        pre_secs,
        t0.elapsed().as_secs_f32() - pre_secs,
        nodes_len,
        frn_nodes.len(),
    ));
    // leftover (unreachable) staged dirs hold real Strings — free them on a
    // throwaway thread so the scan tail doesn't pay for a million drops
    std::thread::spawn(move || drop(dirs));
    frn_nodes
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


    /// Perf probe (no admin needed): parse throughput on synthetic records.
    /// Run: cargo test --release --no-default-features bench_parse -- --ignored --nocapture
    #[test]
    #[ignore = "perf probe"]
    fn bench_parse_throughput() {
        let record = 1024usize;
        let n = 96 * 1024; // 96 MB of records
        let mut data = vec![0u8; n * record];
        for i in 0..n {
            // spread files over 4000 parent dirs, half small (folded), half big
            let parent = 5 + (i % 4000) as u64;
            let big = i % 2 == 0;
            let rec = synth_record(false, parent, "file-name.bin", Some(if big { 900_000 } else { 900 }));
            data[i * record..(i + 1) * record].copy_from_slice(&rec);
        }
        let limits = ScanLimits { keep_min: 32 * 1024, max_files_per_dir: 48 };
        let mut dirs: HashMap<u64, Staged> = HashMap::with_capacity(8192);
        let mut ext: HashMap<u64, u64> = HashMap::new();
        let t0 = std::time::Instant::now();
        let mut scratch = vec![0u8; record];
        let mut attrs = [AttrView::default(); MAX_ATTRS];
        let mut live = 0u64;
        for i in 0..n {
            let s = i * record;
            if &data[s..s + 4] != b"FILE" || data[s + 0x16] & 1 == 0 {
                continue;
            }
            scratch.copy_from_slice(&data[s..s + record]);
            if !apply_fixups_inplace(&mut scratch, 512) {
                continue;
            }
            if let Some(p) = parse_record_buf(&scratch, &mut attrs) {
                live += 1;
                stage_ref(&mut dirs, &mut ext, i as u64, p, &limits);
            }
        }
        let secs = t0.elapsed().as_secs_f64();
        let mb = (n * record) as f64 / 1e6;
        eprintln!(
            "MFT parse: {} records ({:.0} MB) in {:.2}s = {:.0} MB/s single-thread, {:.0}k records/s, live={}",
            n, mb, secs, mb / secs, n as f64 / secs / 1000.0, live
        );
        assert!(live > n as u64 * 9 / 10, "synthetic records must parse");
    }


    /// Offline probe against a real MFT dump: `C:\tmp\mft-dump.bin`
    /// (first 4 KB = boot sector copy, then 1 MB of MFT records).
    /// Run: cargo test --release --no-default-features mft_dump -- --ignored --nocapture
    #[test]
    #[ignore = "needs C:/tmp/mft-dump.bin"]
    fn mft_dump_probe() {
        let Ok(data) = std::fs::read("C:/tmp/mft-dump.bin") else {
            eprintln!("no dump file");
            return;
        };
        let record = 1024usize;
        let mft = &data[4096..];
        let mut ok = 0usize;
        let mut fail = 0usize;
        let mut shown = 0;
        for i in 0..mft.len() / record {
            let rec = &mft[i * record..(i + 1) * record];
            if &rec[..4] != b"FILE" || rec[0x16] & 1 == 0 {
                continue; // free / not a record: not counted
            }
            let mut scratch = rec.to_vec();
            if !apply_fixups_inplace(&mut scratch, 512) {
                if shown < 6 {
                    eprintln!("rec {i}: FIXUP REJECT usa_off={:#x} usa_count={} hdr={}", u16at(rec, 4), u16at(rec, 6), rec[..48].iter().map(|b| format!("{b:02x}")).collect::<String>());
                    shown += 1;
                }
                fail += 1;
                continue;
            }
            let mut attrs = [AttrView::default(); MAX_ATTRS];
            match parse_record_buf(&scratch, &mut attrs) {
                Some(_) => ok += 1,
                None => {
                    if shown < 6 {
                        let first = u16at(&scratch, 0x14);
                        let used = u32at(&scratch, 0x18);
                        eprintln!(
                            "rec {i}: PARSE REJECT flags={:#06x} usa_off={:#x} usa_count={} first={:#x} used={} base={} bytes[0x28..0x60]={}",
                            u16at(&scratch, 0x16),
                            u16at(&scratch, 4),
                            u16at(&scratch, 6),
                            first,
                            used,
                            u64at(&scratch, 0x20),
                            scratch[0x28..0x60].iter().map(|b| format!("{b:02x}")).collect::<String>()
                        );
                        shown += 1;
                    }
                    fail += 1;
                }
            }
        }
        eprintln!("dump probe: {ok} parsed, {fail} failed");
        // bisect: run walk_attributes_buf + the FN acceptance on record 5
        {
            let rec = &mft[5 * record..6 * record];
            let mut scratch = rec.to_vec();
            assert!(apply_fixups_inplace(&mut scratch, 512));
            let mut attrs = [AttrView::default(); MAX_ATTRS];
            let n = walk_attributes_buf(&scratch, &mut attrs);
            eprintln!("rec5 walk: {:?}", n);
            if let Some(n) = n {
                for att in attrs.iter().take(n) {
                    eprintln!("rec5 attr type={:#x} nr={} nl={} coff={:#x} clen={}", att.type_, att.nonresident as u8, att.name_len, att.content_off, att.content_len);
                }
                let mut best: Option<(u8, usize, usize, u64, u64, u64)> = None;
                for att in attrs.iter().take(n) {
                    if att.type_ != 0x30 { continue; }
                    eprintln!("rec5 FN: nr={} nl={} clen={} min_ok={}", att.nonresident, att.name_len, att.content_len, att.content_len >= 0x3C);
                    let Some(c) = scratch.get(att.content_off..att.content_off + att.content_len) else { eprintln!("rec5 FN: CONTENT GET FAILS {}..{}", att.content_off, att.content_off + att.content_len); continue; };
                    let nl = c[0x3A] as usize;
                    let ns = c[0x3B];
                    eprintln!("rec5 FN: nl={nl} ns={ns} c.len={} need={}", c.len(), 0x3C + nl * 2);
                    if nl == 0 || c.len() < 0x3C + nl * 2 { eprintln!("rec5 FN: BOUNDS REJECT"); continue; }
                    eprintln!("rec5 FN: ACCEPTED");
                    let _ = &mut best;
                }
                eprintln!("rec5 parse_record_buf: {:?}", parse_record_buf(&scratch, &mut attrs).map(|_| "ok"));
            }
        }
        // trace the attribute walk of record 5 (the volume root)
        {
            let rec = &mft[5 * record..6 * record];
            let mut scratch = rec.to_vec();
            if apply_fixups_inplace(&mut scratch, 512) {
                let first = u16at(&scratch, 0x14) as usize;
                let used = u32at(&scratch, 0x18) as usize;
                eprintln!("root trace: first={first:#x} used={used}");
                let mut off = first;
                let mut k = 0;
                while off + 0x10 <= used && k < 12 {
                    let type_ = u32at(&scratch, off);
                    let len = u32at(&scratch, off + 4) as usize;
                    let nr = scratch[off + 8];
                    eprintln!("  attr@{off:#x}: type={type_:#x} len={len} nonresident={nr} bytes={:?}", &scratch[off..(off + 24).min(used)]);
                    if type_ == 0xFFFF_FFFF || type_ == 0 || len < 8 {
                        eprintln!("  -> walk stops (end/invalid)");
                        break;
                    }
                    off += len;
                    k += 1;
                }
            }
        }
    }


    /// Full-dump bench: parse+stage throughput against E:\mft-full.bin,
    /// streamed chunk-by-chunk exactly like the engine (read and parse timed
    /// separately). Run with: --release --ignored --nocapture
    #[test]
    #[ignore = "needs E:/mft-true.bin"]
    fn bench_full_dump() {
        use std::io::{Read, Seek};
        let record = 1024usize;
        let total: usize = 5_665_194_496; // dump size minus the 4KB boot copy
        let usable = total - total % record;
        let mut f = std::fs::File::open("E:/mft-true.bin").expect("dump file");
        f.seek(std::io::SeekFrom::Start(4096)).unwrap();
        let limits = ScanLimits { keep_min: 1 << 20, max_files_per_dir: 48 };
        let threads = 12usize;
        let est = ((usable / record) / 8).max(1024);
        let mut tdirs: Vec<HashMap<u64, Staged>> = (0..threads).map(|_| HashMap::with_capacity(est / threads)).collect();
        let mut texts: Vec<HashMap<u64, u64>> = (0..threads).map(|_| HashMap::new()).collect();
        let mut tlive: Vec<u64> = vec![0; threads];
        let chunk = 64 << 20;
        let mut buf = vec![0u8; chunk];
        let t_all = std::time::Instant::now();
        let mut t_read = 0f32;
        let mut parsed = 0usize;
        let mut done = 0usize;
        while done < usable {
            let want = (usable - done).min(chunk);
            let t0 = std::time::Instant::now();
            f.read_exact(&mut buf[..want]).expect("dump read");
            t_read += t0.elapsed().as_secs_f32();
            let b = &buf[..want];
            let n = b.len() / record;
            let slice = (n / threads).max(1) * record;
            let base = (done / record) as u64;
            std::thread::scope(|sc| {
                let it = tdirs.iter_mut().zip(texts.iter_mut()).zip(tlive.iter_mut());
                for (t, ((dirs_t, ext_t), live_t)) in it.enumerate() {
                    let start = t * slice;
                    if start >= b.len() { break; }
                    let end_t = if t + 1 == threads || (t + 1) * slice >= b.len() { b.len() } else { (t + 1) * slice };
                    let end_t = end_t - end_t % record;
                    if end_t <= start { continue; }
                    let b_t: &[u8] = &b[start..end_t];
                    let base = base + (start / record) as u64;
                    let limits = &limits;
                    sc.spawn(move || {
                        let mut scratch = vec![0u8; record];
                        let mut attrs = [AttrView::default(); MAX_ATTRS];
                        for (i, o) in (0..b_t.len()).step_by(record).enumerate() {
                            let hdr = &b_t[o..o + record];
                            if &hdr[..4] != b"FILE" || hdr[0x16] & 1 == 0 { continue; }
                            scratch.copy_from_slice(hdr);
                            if !apply_fixups_inplace(&mut scratch, 512) { continue; }
                            if let Some(p) = parse_record_buf(&scratch, &mut attrs) {
                                *live_t += 1;
                                stage_ref(dirs_t, ext_t, base + i as u64, p, limits);
                            }
                        }
                    });
                }
            });
            parsed += n;
            done += want;
        }
        let dirs_total: usize = tdirs.iter().map(|d| d.len()).sum();
        let live: u64 = tlive.iter().sum();
        let parse_s = t_all.elapsed().as_secs_f32() - t_read;
        eprintln!(
            "FULLDUMP: parse+stage {:.2}s = {:.0} MB/s - {} live records, {} staged dirs",
            parse_s, (usable as f64 / 1e6) / parse_s as f64, live, dirs_total
        );
        // merge (same code shape as the engine)
        let t_m = std::time::Instant::now();
        let mut dirs: HashMap<u64, Staged> = tdirs.swap_remove(0);
        for other in tdirs {
            for (frn, st) in other {
                use std::collections::hash_map::Entry;
                match dirs.entry(frn) {
                    Entry::Occupied(mut e) => {
                        let dst = e.get_mut();
                        if dst.name.is_empty() && !st.name.is_empty() {
                            dst.name = st.name;
                            dst.parent = st.parent;
                            dst.mtime = st.mtime;
                        }
                        for (f, n, sz, m) in st.files {
                            dst.add_file(f, n, sz, m, &limits);
                        }
                        if let Some((h, _)) = st.heap {
                            use std::cmp::Reverse;
                            for Reverse((sz, f, n, m)) in h {
                                dst.add_file(f, n, sz, m, &limits);
                            }
                        }
                        dst.small_size = dst.small_size.saturating_add(st.small_size);
                        dst.small_count = dst.small_count.saturating_add(st.small_count);
                        for m in st.mounts {
                            if dst.mounts.len() < 64 {
                                dst.mounts.push(m);
                            }
                        }
                    }
                    Entry::Vacant(e) => {
                        e.insert(st);
                    }
                }
            }
        }
        let mut ext: HashMap<u64, u64> = texts.swap_remove(0);
        for other in texts {
            for (k, v) in other {
                let e = ext.entry(k).or_insert(0);
                *e = (*e).max(v);
            }
        }
        for st in dirs.values_mut() {
            st.finish_files();
        }
        eprintln!("FULLDUMP: merge+drain {:.2}s - {} dirs", t_m.elapsed().as_secs_f32(), dirs.len());
        // assembly, through the same code path the engine uses (bottom-up
        // totals + direct-size creation). The root roll-up is asserted:
        // root size/files must equal the sum over its children.
        let tree = std::sync::Arc::new(std::sync::RwLock::new(crate::tree::Tree::new("C:\\", "C:", crate::tree::Source::Local)));
        let progress = Progress::default();
        let dirs_before = dirs.len();
        let t_a = std::time::Instant::now();
        let frn_nodes = assemble(&tree, dirs, &ext, &progress);
        let asm = t_a.elapsed().as_secs_f32();
        let t = tree.read().unwrap();
        let root = t.get(t.root);
        let child_sum: u64 = t.children(t.root).map(|c| t.get(c).size).sum();
        let child_files: u64 = t.children(t.root).map(|c| t.get(c).files as u64).sum();
        eprintln!(
            "FULLDUMP: assembly {:.2}s - {} nodes from {} staged dirs, root size {} (children sum {}, exact={}), root files {} (children {}), {} frn entries",
            asm,
            t.nodes.len(),
            dirs_before,
            crate::tree::fmt_size(root.size),
            crate::tree::fmt_size(child_sum),
            root.size == child_sum,
            root.files,
            child_files,
            frn_nodes.len()
        );
        assert_eq!(root.size, child_sum, "root size must equal the sum of its children");
        assert_eq!(u64::from(root.files), child_files, "root file count must equal the sum of its children");
    }


    /// Prints the $MFT run list from the dump's record 0, as PS read commands.
    #[test]
    #[ignore = "needs E:/mft-true.bin"]
    fn mft_runs_from_dump() {
        let data = std::fs::read("E:/mft-true.bin").expect("dump");
        let rec = &data[4096..4096 + 1024];
        let mut scratch = rec.to_vec();
        assert!(apply_fixups_inplace(&mut scratch, 512));
        let mut attrs = [AttrView::default(); MAX_ATTRS];
        let n = walk_attributes_buf(&scratch, &mut attrs).unwrap();
        for att in attrs.iter().take(n) {
            if att.type_ == 0x80 && att.name_len == 0 && att.nonresident {
                let runs = parse_dataruns(&scratch, att.run_off).unwrap();
                let bps = u16at(&data[0x0B..], 0).to_le();
                let bps = bps as u64;
                let spc = data[0x0D] as u64;
                let clus = bps * spc;
                for (lcn, clusters) in runs {
                    eprintln!("RUN {} {}", lcn * clus, clusters * clus);
                }
            }
        }
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

    /// Builds one synthetic record with a valid fixup array (1024 B,
    /// 2 sectors: tag at 510/1022 replaced from the header array).
    fn synth_record(dir: bool, parent: u64, name: &str, data: Option<u64>) -> Vec<u8> {
        let mut rec = vec![0u8; 1024];
        rec[..4].copy_from_slice(b"FILE");
        rec[4..6].copy_from_slice(&0x28u16.to_le_bytes()); // usa offset
        rec[6..8].copy_from_slice(&3u16.to_le_bytes()); // tag + 2 entries
        rec[0x28..0x2A].copy_from_slice(&0xAABBu16.to_le_bytes()); // tag
        rec[0x2A..0x2C].copy_from_slice(&0x1111u16.to_le_bytes()); // sec 1 tail
        rec[0x2C..0x2E].copy_from_slice(&0x2222u16.to_le_bytes()); // sec 2 tail
        rec[510..512].copy_from_slice(&0xAABBu16.to_le_bytes());
        rec[1022..1024].copy_from_slice(&0xAABBu16.to_le_bytes());
        rec[0x14..0x16].copy_from_slice(&0x30u16.to_le_bytes()); // first attr
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
        let fn_len = 0x18 + 0x42 + nl * 2;
        rec[off..off + 4].copy_from_slice(&0x30u32.to_le_bytes());
        rec[off + 4..off + 8].copy_from_slice(&(fn_len as u32).to_le_bytes());
        rec[off + 0x10..off + 0x14].copy_from_slice(&((fn_len - 0x18) as u32).to_le_bytes());
        rec[off + 0x14..off + 0x16].copy_from_slice(&0x18u16.to_le_bytes());
        let f = off + 0x18;
        rec[f..f + 8].copy_from_slice(&parent.to_le_bytes());
        rec[f + 0x28..f + 0x30].copy_from_slice(&(data.unwrap_or(0)).to_le_bytes());
        rec[f + 0x30..f + 0x38].copy_from_slice(&(data.unwrap_or(0)).to_le_bytes());
        rec[f + 0x38..f + 0x3C].copy_from_slice(&0u32.to_le_bytes()); // flags
        rec[f + 0x40] = nl as u8;
        rec[f + 0x41] = 3; // Win32+DOS namespace
        for (k, ch) in name.encode_utf16().collect::<Vec<u16>>().iter().enumerate() {
            rec[f + 0x42 + k * 2..f + 0x44 + k * 2].copy_from_slice(&ch.to_le_bytes());
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

    /// The bottom-up assembly must produce exactly the sizes the old
    /// per-directory add_size walk produced: every directory's size/files
    /// equal the sum of its live children, and kept files/mounts/folds keep
    /// their shapes. Also checks the frn→node list handed to the USN cache.
    #[test]
    fn assemble_bottom_up_rollup() {
        // root(5) → a(100){f(200, 8KB), tiny(201, 100B → fold)} → b(101){g(300, 1MB)}
        let limits = ScanLimits { keep_min: 4096, max_files_per_dir: 8 };
        let mut dirs = HashMap::new();
        let mut ext = HashMap::new();
        stage(&mut dirs, &mut ext, 5, parse_record(&synth_record(true, 5, ".", None)).unwrap(), &limits);
        stage(&mut dirs, &mut ext, 100, parse_record(&synth_record(true, 5, "a", None)).unwrap(), &limits);
        stage(&mut dirs, &mut ext, 101, parse_record(&synth_record(true, 100, "b", None)).unwrap(), &limits);
        stage(&mut dirs, &mut ext, 200, parse_record(&synth_record(false, 100, "f", Some(8192))).unwrap(), &limits);
        stage(&mut dirs, &mut ext, 201, parse_record(&synth_record(false, 100, "tiny", Some(100))).unwrap(), &limits);
        stage(&mut dirs, &mut ext, 300, parse_record(&synth_record(false, 101, "g", Some(1 << 20))).unwrap(), &limits);
        for st in dirs.values_mut() {
            st.finish_files();
        }
        let tree = std::sync::Arc::new(std::sync::RwLock::new(crate::tree::Tree::new("X:\\", "X:", crate::tree::Source::Local)));
        let frn_nodes = assemble(&tree, dirs, &ext, &Progress::default());
        let t = tree.read().unwrap();
        // directories roll up exactly; file/fold nodes are atoms
        fn subtree(t: &crate::tree::Tree, id: u32) -> (u64, u32) {
            let n = t.get(id);
            if n.kind != Kind::Dir {
                return (n.size, n.files);
            }
            let mut s = 0u64;
            let mut f = 0u32;
            for c in t.children(id) {
                let (cs, cf) = subtree(t, c);
                s += cs;
                f += cf;
            }
            assert_eq!((n.size, n.files), (s, f), "dir node {id} roll-up");
            (s, f)
        }
        let (s, f) = subtree(&t, t.root);
        assert_eq!(s, 8192 + 100 + (1 << 20));
        assert_eq!(f, 3); // f, g, and the one folded tiny file
        // frn map: dirs a + b, kept files f + g; root and the fold have none
        assert_eq!(frn_nodes.len(), 4);
        let frns: std::collections::HashSet<u64> = frn_nodes.iter().map(|&(frn, _)| frn).collect();
        assert!(frns.contains(&100) && frns.contains(&101) && frns.contains(&200) && frns.contains(&300));
    }
}
