p = 'src/scan/mft.rs'
s = open(p, encoding='utf-8').read()

def rep(old, new):
    global s
    assert old in s, "NOT FOUND: " + old[:90]
    s = s.replace(old, new, 1)

# --- 5) parallel record-aligned chunk loop --------------------------------
rep("""    let mut dirs: HashMap<u64, Staged> = HashMap::new();
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
            let mut want = (run_bytes - done).min(chunk as u64);
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
    }""",
"""    // Per-thread staging maps: every thread parses its slice of each chunk
    // into its own maps, merged once after the read loop. Record numbers are
    // absolute MFT stream positions / record size, so threads stay consistent.
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 8);
    let est_dirs = ((lay.total_bytes / lay.record_bytes) / 16) as usize;
    let mut tdirs: Vec<HashMap<u64, Staged>> = (0..threads).map(|_| HashMap::with_capacity((est_dirs / threads).max(1024))).collect();
    let mut texts: Vec<HashMap<u64, u64>> = (0..threads).map(|_| HashMap::new()).collect();
    let mut tlive: Vec<u64> = vec![0; threads];

    // Chunks are record-aligned reads; records straddling a run boundary
    // (possible only when a cluster is smaller than a record) are skipped:
    // each run re-aligns to the absolute record grid first.
    let record = lay.record_bytes as usize;
    let chunk: u64 = 64 << 20;
    let mut buf = AlignedBuf::new(chunk as usize);
    let mut stream_pos: u64 = 0; // absolute byte offset in the MFT stream
    let mut seen_bytes = 0u64;

    'runs: for &(lcn, clusters) in &lay.runs {
        let run_bytes = clusters.saturating_mul(lay.cluster_bytes);
        let run_lba = lcn * lay.cluster_bytes / lay.bytes_per_sector;
        // skip forward to the record grid (records never straddle runs here)
        let misalign = stream_pos % lay.record_bytes;
        let mut done = if misalign != 0 { lay.record_bytes - misalign } else { 0 };
        done = done.min(run_bytes);
        stream_pos += done;
        while done < run_bytes {
            if progress.cancelled() {
                return true; // partial tree stays; the caller handles cancel
            }
            let mut want = (run_bytes - done).min(chunk);
            want -= want % lay.record_bytes; // whole records only
            if want == 0 {
                break; // sub-record tail: skipped with the alignment above
            }
            buf.set_len(want as usize);
            if vol.read_at(run_lba + done / lay.bytes_per_sector, &mut buf).is_err() {
                restore(&progress);
                return false;
            }
            let b = buf.bytes();
            let record_base = stream_pos / lay.record_bytes;
            {
                let tdirs = &mut tdirs;
                let texts = &mut texts;
                let tlive = &mut tlive;
                std::thread::scope(|sc| {
                    let n = b.len() / record;
                    let slice = (n / threads).max(1) * record;
                    for t in 0..threads {
                        let start = t * slice;
                        if start >= b.len() {
                            break;
                        }
                        let end = if t + 1 == threads || (t + 1) * slice >= b.len() { b.len() } else { (t + 1) * slice };
                        let dirs_t = &mut tdirs[t];
                        let ext_t = &mut texts[t];
                        let live_t = &mut tlive[t];
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
                                    None => progress.errors.fetch_add(1, Ordering::Relaxed),
                                }
                            }
                        });
                    }
                });
            }
            stream_pos += want;
            done += want;
            seen_bytes = (seen_bytes + want).min(lay.total_bytes);
            progress.bytes.store(seen_bytes, Ordering::Relaxed);
            progress.files.store(tlive.iter().sum::<u64>(), Ordering::Relaxed);
        }
        // tail of this run below the record grid: accounted in stream_pos
        let tail = run_bytes - done;
        stream_pos += tail;
        let _ = tail;
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
    }""")

open(p, 'w', encoding='utf-8', newline='\n').write(s)
print("stage 3 done")
