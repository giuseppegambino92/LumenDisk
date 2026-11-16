//! Crash-safe block-level write-ahead redo log — **ordered metadata mode**.
//!
//! Like ext4 `data=ordered`: file payload (`BlockDev::write_data_at`) goes straight to its
//! target blocks and is fsync'ed before the metadata that references it is committed; only
//! metadata (superblocks, descriptors, bitmaps, inode tables, directories, FAT tables, MFT
//! records…) passes through the redo log. Large copies are therefore written once.
//!
//! Every [`FileSystem`] operation becomes a transaction (see [`Journaled`]). Its writes are
//! collected in an in-memory overlay of 4 KiB blocks (reads see them immediately). At
//! transaction boundaries — batched every ~100 ms / 8 MiB and on every `sync()` — all
//! complete transactions are appended to `<support>/WAL/<volume>.wal` as one CRC-protected
//! record and fsync'ed, and only **then** written in place. If LumenDisk dies at any point,
//! the next open replays every intact record (a torn tail is ignored), so the on-disk
//! structures are never half-updated: no fsck needed for FAT, exFAT, NTFS or ext4.
//!
//! Record layout (little endian):
//! `"LDWAL\x01\0\0" | seq u64 | nblocks u32 | payload_len u64 | crc32c(payload) u32 | payload`
//! with `payload = nblocks × (block_index u64 | 4096 bytes)`.
//! The file header (first record slot) is `"LDWALHDR" | device_size u64 | block_size u32 | id(64 bytes)`.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::block::BlockDev;
use crate::vfs::*;

const BS: u64 = 4096;
const REC_MAGIC: &[u8; 8] = b"LDWAL\x01\0\0";
const HDR_MAGIC: &[u8; 8] = b"LDWALHDR";
const HDR_LEN: u64 = 8 + 8 + 4 + 64;
const REC_HDR: usize = 8 + 8 + 4 + 8 + 4;
const BATCH_INTERVAL: Duration = Duration::from_millis(100);
const BATCH_BYTES: usize = 8 << 20;
const CHECKPOINT_BYTES: u64 = 64 << 20;

/// Make the log durable (`F_FULLFSYNC` on macOS). Unit tests simulate crashes in-process,
/// where the page cache survives: they skip the cache flush to stay fast.
fn log_sync(f: &File) -> io::Result<()> {
    if cfg!(test) {
        return Ok(());
    }
    f.sync_data()
}

fn crc32c(data: &[u8]) -> u32 {
    !crate::vfs::ext4_fs::crc32c_raw(!0, data)
}

/// Fault injection for crash tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// On the next flush: make the WAL record durable, apply only the first `n` blocks in
    /// place, then behave like a dead process (ignore everything afterwards).
    CrashAfterWal { apply_blocks: usize },
    /// On the next flush: die before anything reaches the WAL.
    CrashBeforeWal,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct WalStats {
    pub enabled: bool,
    pub mode: &'static str,
    pub transactions: u64,
    pub replayed_at_mount: u64,
    pub flushes: u64,
    /// Records written to the log ahead of their sync (`make_written`).
    pub records_written: u64,
    /// `F_FULLFSYNC`s of the log by group commits.
    pub log_syncs: u64,
    /// Bytes appended to the redo log (metadata records).
    pub wal_bytes: u64,
    /// File payload written in place (not journaled).
    pub data_bytes: u64,
}

struct State {
    /// End of the log, including a record being written by the flush in progress.
    log_len: u64,
    seq: u64,
    /// Committed (complete-transaction) dirty blocks not yet in the WAL.
    committed: BTreeMap<u64, Box<[u8]>>,
    /// Blocks written by the transaction in progress.
    txn: BTreeMap<u64, Box<[u8]>>,
    in_txn: u32,
    /// When `committed` last went from empty to non-empty.
    committed_since: Instant,
    stats: WalStats,
    fault: Option<Fault>,
    dead: bool,
    /// Payload written in place since the last durability barrier.
    data_dirty: bool,
    /// Blocks present in WAL records since the last checkpoint (replay would rewrite them).
    wal_blocks: std::collections::HashSet<u64>,
    /// Blocks freed by the transaction in progress / by committed-but-unflushed transactions:
    /// the last durable metadata may still reference them, so no in-place payload yet.
    freed_txn: std::collections::HashSet<u64>,
    freed_committed: std::collections::HashSet<u64>,
    /// Blocks of the record being flushed (written to the log, then in place, without this
    /// lock): reads and new transactions see them until they are in place.
    flushing: BTreeMap<u64, Box<[u8]>>,
    freed_flushing: std::collections::HashSet<u64>,
    /// Blocks of records written to the log but not yet synced (`make_written`): they stay
    /// out of place until a group commit syncs the log.
    written: BTreeMap<u64, Box<[u8]>>,
    freed_written: std::collections::HashSet<u64>,
}

pub struct WalDev {
    inner: Arc<dyn BlockDev>,
    path: PathBuf,
    log: File,
    st: Mutex<State>,
    /// One flush at a time: records reach the log, the device and a checkpoint in order.
    /// Lock order: the volume lock (if any) → `flush_lock` → `st`.
    flush_lock: Mutex<()>,
}

impl WalDev {
    /// Open (or create) the WAL for `inner`, replaying any intact records first.
    /// `id` identifies the volume (stored in the header and checked on replay).
    pub fn open(inner: Arc<dyn BlockDev>, path: &Path, id: &str) -> io::Result<Arc<WalDev>> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let log = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        let mut replayed = 0;
        let len = log.metadata()?.len();
        if len >= HDR_LEN {
            let mut h = vec![0u8; HDR_LEN as usize];
            log.read_exact_at(&mut h, 0)?;
            let size = u64::from_le_bytes(h[8..16].try_into().unwrap());
            if &h[..8] == HDR_MAGIC && size == inner.size() {
                replayed = Self::replay(&log, len, inner.as_ref())?;
            }
        }
        // fresh header
        log.set_len(0)?;
        let mut h = Vec::with_capacity(HDR_LEN as usize);
        h.extend_from_slice(HDR_MAGIC);
        h.extend_from_slice(&inner.size().to_le_bytes());
        h.extend_from_slice(&(BS as u32).to_le_bytes());
        let mut idb = [0u8; 64];
        let n = id.len().min(64);
        idb[..n].copy_from_slice(&id.as_bytes()[..n]);
        h.extend_from_slice(&idb);
        log.write_all_at(&h, 0)?;
        log_sync(&log)?;
        Ok(Arc::new(WalDev {
            inner,
            path: path.to_path_buf(),
            log,
            flush_lock: Mutex::new(()),
            st: Mutex::new(State {
                log_len: HDR_LEN,
                seq: 1,
                committed: BTreeMap::new(),
                txn: BTreeMap::new(),
                in_txn: 0,
                committed_since: Instant::now(),
                stats: WalStats { enabled: true, mode: "ordered", replayed_at_mount: replayed, ..Default::default() },
                fault: None,
                dead: false,
                data_dirty: false,
                wal_blocks: Default::default(),
                freed_txn: Default::default(),
                freed_committed: Default::default(),
                flushing: BTreeMap::new(),
                freed_flushing: Default::default(),
                written: BTreeMap::new(),
                freed_written: Default::default(),
            }),
        }))
    }

    /// Apply every intact record of a WAL file; returns how many records were replayed.
    fn replay(log: &File, len: u64, dev: &dyn BlockDev) -> io::Result<u64> {
        let mut pos = HDR_LEN;
        let mut n = 0u64;
        while pos + REC_HDR as u64 <= len {
            let mut h = [0u8; REC_HDR];
            log.read_exact_at(&mut h, pos)?;
            if &h[..8] != REC_MAGIC {
                break;
            }
            let nblocks = u32::from_le_bytes(h[16..20].try_into().unwrap()) as u64;
            let plen = u64::from_le_bytes(h[20..28].try_into().unwrap());
            let crc = u32::from_le_bytes(h[28..32].try_into().unwrap());
            if plen != nblocks * (8 + BS) || pos + REC_HDR as u64 + plen > len {
                break; // torn tail
            }
            let mut payload = vec![0u8; plen as usize];
            log.read_exact_at(&mut payload, pos + REC_HDR as u64)?;
            if crc32c(&payload) != crc {
                break;
            }
            for k in 0..nblocks as usize {
                let o = k * (8 + BS as usize);
                let blk = u64::from_le_bytes(payload[o..o + 8].try_into().unwrap());
                let data = &payload[o + 8..o + 8 + BS as usize];
                Self::write_block_clipped(dev, blk, data)?;
            }
            n += 1;
            pos += REC_HDR as u64 + plen;
        }
        if n > 0 {
            dev.flush()?;
        }
        Ok(n)
    }

    fn write_block_clipped(dev: &dyn BlockDev, blk: u64, data: &[u8]) -> io::Result<()> {
        let off = blk * BS;
        let size = dev.size();
        if off >= size {
            return Ok(());
        }
        let n = ((size - off) as usize).min(data.len());
        dev.write_at(off, &data[..n])
    }

    pub fn stats(&self) -> WalStats {
        self.st.lock().unwrap().stats
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Test hook.
    pub fn inject_fault(&self, f: Fault) {
        self.st.lock().unwrap().fault = Some(f);
    }

    pub fn begin(&self) {
        let mut s = self.st.lock().unwrap();
        s.in_txn += 1;
    }

    /// End a transaction. Group-commits when the batch is old/large enough or `force`.
    pub fn commit(&self, force: bool) -> io::Result<()> {
        let due = {
            let mut s = self.st.lock().unwrap();
            if s.in_txn > 0 {
                s.in_txn -= 1;
            }
            if s.in_txn > 0 {
                return Ok(());
            }
            let txn = std::mem::take(&mut s.txn);
            if !txn.is_empty() {
                s.stats.transactions += 1;
            }
            let ft = std::mem::take(&mut s.freed_txn);
            s.freed_committed.extend(ft);
            if s.committed.is_empty() && !txn.is_empty() {
                s.committed_since = Instant::now();
            }
            for (k, v) in txn {
                s.committed.insert(k, v);
            }
            // The volume's timer group-commits every BATCH_INTERVAL without the volume lock
            // (`make_durable`); flushing here, inside an operation, is the safety net for a big
            // batch or a timer that didn't run.
            force || s.committed_since.elapsed() >= BATCH_INTERVAL * 10 || s.committed.len() * BS as usize >= BATCH_BYTES
        };
        if due {
            // A batch flush that finds another flush running leaves its blocks to the next one.
            self.flush_round(force, force)?;
        }
        Ok(())
    }

    /// Make every complete transaction durable (payload first, then the redo record), with
    /// the volume lock *not* held: the cache flushes run without `st`, so the volume keeps
    /// serving I/O meanwhile. Transactions completed while it runs are left to the next call.
    pub fn make_durable(&self) -> io::Result<()> {
        self.flush_round(false, true)
    }

    /// Write every complete transaction to the log, *without* syncing it: from the moment
    /// this returns the transactions survive a crash of this process (the page cache does),
    /// and the next [`WalDev::make_durable`] (the volume's timer, or a COMMIT) makes them
    /// survive a power loss. Nothing is written in place before then — the blocks stay in the
    /// overlay — so a power loss can only lose whole transactions, never tear one. Payload
    /// the record refers to is ordered before it (barrier, or a flush on another device).
    pub fn make_written(&self) -> io::Result<()> {
        let _round = self.flush_lock.lock().unwrap_or_else(|e| e.into_inner());
        let (rec, keys, data_dirty, at) = {
            let mut s = self.st.lock().unwrap();
            if s.dead || s.committed.is_empty() {
                return Ok(());
            }
            if s.fault == Some(Fault::CrashBeforeWal) {
                s.dead = true;
                return Ok(());
            }
            let blocks = std::mem::take(&mut s.committed);
            let rec = Self::record(s.seq, &blocks);
            let keys: Vec<u64> = blocks.keys().copied().collect();
            for k in &keys {
                s.wal_blocks.insert(*k);
            }
            let freed = std::mem::take(&mut s.freed_committed);
            s.freed_written.extend(freed);
            s.written.extend(blocks); // newer versions replace older written ones
            let at = s.log_len;
            s.log_len += rec.len() as u64;
            s.seq += 1;
            (rec, keys, std::mem::take(&mut s.data_dirty), at)
        };
        let r = (|| {
            if data_dirty && !self.inner.barrier_before(&self.log)? {
                self.inner.flush()?;
            }
            self.log.write_all_at(&rec, at)
        })();
        let mut s = self.st.lock().unwrap();
        match r {
            Ok(()) => {
                s.stats.wal_bytes += rec.len() as u64;
                s.stats.records_written += 1;
                Ok(())
            }
            Err(e) => {
                // Not in the log: journal these blocks again with the next record.
                for k in keys {
                    if let Some(v) = s.written.remove(&k) {
                        s.committed.entry(k).or_insert(v);
                    }
                }
                s.data_dirty |= data_dirty;
                s.log_len = at;
                s.seq -= 1;
                let _ = self.log.set_len(at);
                Err(e)
            }
        }
    }

    fn record(seq: u64, blocks: &BTreeMap<u64, Box<[u8]>>) -> Vec<u8> {
        let mut payload = Vec::with_capacity(blocks.len() * (8 + BS as usize));
        for (k, v) in blocks {
            payload.extend_from_slice(&k.to_le_bytes());
            payload.extend_from_slice(v);
        }
        let mut rec = Vec::with_capacity(REC_HDR + payload.len());
        rec.extend_from_slice(REC_MAGIC);
        rec.extend_from_slice(&seq.to_le_bytes());
        rec.extend_from_slice(&(blocks.len() as u32).to_le_bytes());
        rec.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        rec.extend_from_slice(&crc32c(&payload).to_le_bytes());
        rec.extend_from_slice(&payload);
        rec
    }

    /// One group commit: (1) under `st`, take the complete transactions as one record, plus
    /// the records already written by [`WalDev::make_written`]; (2) without it, flush the
    /// payload written in place (ordered mode), append the record and `F_FULLFSYNC` the log,
    /// write the blocks in place; (3) checkpoint if asked or due.
    /// `wait`: wait for a flush already running instead of leaving the work to it.
    fn flush_round(&self, checkpoint: bool, wait: bool) -> io::Result<()> {
        let _round = if wait {
            self.flush_lock.lock().unwrap_or_else(|e| e.into_inner())
        } else {
            match self.flush_lock.try_lock() {
                Ok(g) => g,
                Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => return Ok(()),
            }
        };
        let (rec, apply, unsynced, data_dirty, at) = {
            let mut s = self.st.lock().unwrap();
            if s.dead {
                return Ok(());
            }
            if !s.committed.is_empty() && s.fault == Some(Fault::CrashBeforeWal) {
                s.dead = true;
                return Ok(());
            }
            let data_dirty = std::mem::take(&mut s.data_dirty);
            // Written but not yet synced records: this round's sync covers them too.
            let mut flushing = std::mem::take(&mut s.written);
            let unsynced = !flushing.is_empty();
            let freed = std::mem::take(&mut s.freed_written);
            s.freed_flushing.extend(freed);
            let (rec, at) = if s.committed.is_empty() {
                (None, 0)
            } else {
                let blocks = std::mem::take(&mut s.committed);
                let rec = Self::record(s.seq, &blocks);
                for k in blocks.keys() {
                    s.wal_blocks.insert(*k);
                }
                let freed = std::mem::take(&mut s.freed_committed);
                s.freed_flushing.extend(freed);
                flushing.extend(blocks); // the newest version of each block wins
                let at = s.log_len;
                s.log_len += rec.len() as u64;
                s.seq += 1;
                (Some(rec), at)
            };
            let apply: Vec<(u64, Box<[u8]>)> = flushing.iter().map(|(k, v)| (*k, v.clone())).collect();
            s.flushing = flushing;
            (rec, apply, unsynced, data_dirty, at)
        };
        let sync_log = rec.is_some() || unsynced;
        // Ordered mode: payload referenced by this metadata is durable before the record. When
        // the image lives on the same device as the log, a barrier suffices: the log's full
        // sync below flushes the drive cache for both.
        let r = (|| {
            if data_dirty && (!sync_log || !self.inner.barrier_before(&self.log)?) {
                self.inner.flush()?;
            }
            if let Some(rec) = &rec {
                self.log.write_all_at(rec, at)?;
            }
            if sync_log {
                log_sync(&self.log)?; // the records are durable before anything is written in place
            }
            Ok(())
        })();
        if let Err(e) = r {
            self.undo_round(rec.is_some(), data_dirty, at);
            return Err(e);
        }
        if sync_log {
            let limit = {
                let mut s = self.st.lock().unwrap();
                if let Some(rec) = &rec {
                    s.stats.wal_bytes += rec.len() as u64;
                }
                s.stats.flushes += 1;
                s.stats.log_syncs += 1;
                match s.fault {
                    Some(Fault::CrashAfterWal { apply_blocks }) => Some(apply_blocks),
                    _ => None,
                }
            };
            for (i, (k, v)) in apply.iter().enumerate() {
                if limit.map(|l| i >= l).unwrap_or(false) {
                    break;
                }
                if let Err(e) = Self::write_block_clipped(self.inner.as_ref(), *k, v) {
                    // The records are durable (a replay would apply them): keep the blocks in
                    // the overlay so that the next flush journals them again.
                    let mut s = self.st.lock().unwrap();
                    Self::requeue(&mut s);
                    return Err(e);
                }
            }
            let mut s = self.st.lock().unwrap();
            if limit.is_some() {
                s.dead = true;
                return Ok(());
            }
            s.flushing.clear();
            s.freed_flushing.clear();
        }
        let mut s = self.st.lock().unwrap();
        if checkpoint || s.log_len > CHECKPOINT_BYTES {
            // Under `st` (and the flush lock: no record is written but unsynced): nothing is
            // written in place while the device is flushed.
            self.inner.flush()?;
            if s.log_len > HDR_LEN {
                self.log.set_len(HDR_LEN)?;
                log_sync(&self.log)?;
                s.log_len = HDR_LEN;
            }
            s.wal_blocks.clear();
            s.data_dirty = false;
        }
        Ok(())
    }

    /// A flush failed before its records were durable: put everything back for the next one.
    fn undo_round(&self, had_record: bool, data_dirty: bool, at: u64) {
        let mut s = self.st.lock().unwrap();
        s.data_dirty |= data_dirty;
        Self::requeue(&mut s);
        if had_record {
            s.log_len = at;
            s.seq -= 1;
            let _ = self.log.set_len(at);
        }
    }

    fn requeue(s: &mut State) {
        let blocks = std::mem::take(&mut s.flushing);
        for (k, v) in blocks {
            s.committed.entry(k).or_insert(v); // a newer committed version wins
        }
        let freed = std::mem::take(&mut s.freed_flushing);
        s.freed_committed.extend(freed);
    }

    /// Flush everything and checkpoint (unmount).
    pub fn flush_all(&self) -> io::Result<()> {
        {
            let mut s = self.st.lock().unwrap();
            let txn = std::mem::take(&mut s.txn);
            for (k, v) in txn {
                s.committed.insert(k, v);
            }
        }
        self.flush_round(true, true)
    }

    /// Clean unmount: flush everything and delete the journal now, instead of when the last
    /// handle goes (a request or a status call can still hold one for a moment). Nothing may
    /// write through this device afterwards. Returns false (journal kept, as after a crash)
    /// when the flush fails or something was written meanwhile.
    pub fn close(&self) -> bool {
        if self.flush_all().is_err() {
            return false;
        }
        let mut s = self.st.lock().unwrap();
        if s.dead || !s.txn.is_empty() || !s.committed.is_empty() || !s.written.is_empty() || !s.flushing.is_empty() {
            return false;
        }
        s.dead = true; // done: Drop must neither flush nor delete again
        drop(s);
        let _ = std::fs::remove_file(&self.path);
        true
    }

    fn overlay_block(s: &State, blk: u64) -> Option<&[u8]> {
        s.txn.get(&blk).or_else(|| s.committed.get(&blk)).or_else(|| s.written.get(&blk)).or_else(|| s.flushing.get(&blk)).map(|b| &b[..])
    }
}

impl Drop for WalDev {
    fn drop(&mut self) {
        let dead = self.st.lock().map(|s| s.dead).unwrap_or(true);
        if !dead {
            if self.flush_all().is_ok() {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

impl BlockDev for WalDev {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let s = self.st.lock().unwrap();
        let first = offset / BS;
        let last = (offset + buf.len() as u64).saturating_sub(1) / BS;
        let any = buf.is_empty() || (first..=last).any(|b| Self::overlay_block(&s, b).is_some());
        if !any || s.dead {
            drop(s);
            return self.inner.read_at(offset, buf);
        }
        self.inner.read_at(offset, buf)?;
        for b in first..=last {
            if let Some(data) = Self::overlay_block(&s, b) {
                let bstart = b * BS;
                let s0 = offset.max(bstart);
                let e0 = (offset + buf.len() as u64).min(bstart + BS);
                buf[(s0 - offset) as usize..(e0 - offset) as usize].copy_from_slice(&data[(s0 - bstart) as usize..(e0 - bstart) as usize]);
            }
        }
        Ok(())
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        if offset + data.len() as u64 > self.inner.size() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "write beyond end of device"));
        }
        let mut s = self.st.lock().unwrap();
        if s.dead {
            return Ok(()); // a crashed process writes nothing
        }
        let auto = s.in_txn == 0;
        let first = offset / BS;
        let last = (offset + data.len() as u64).saturating_sub(1) / BS;
        for b in first..=last {
            let bstart = b * BS;
            if !s.txn.contains_key(&b) {
                let base: Box<[u8]> = match s.committed.get(&b).or_else(|| s.written.get(&b)).or_else(|| s.flushing.get(&b)) {
                    Some(c) => c.clone(),
                    None => {
                        let mut v = vec![0u8; BS as usize];
                        let n = ((self.inner.size() - bstart).min(BS)) as usize;
                        self.inner.read_at(bstart, &mut v[..n])?;
                        v.into_boxed_slice()
                    }
                };
                s.txn.insert(b, base);
            }
            let blk = s.txn.get_mut(&b).unwrap();
            let s0 = offset.max(bstart);
            let e0 = (offset + data.len() as u64).min(bstart + BS);
            blk[(s0 - bstart) as usize..(e0 - bstart) as usize].copy_from_slice(&data[(s0 - offset) as usize..(e0 - offset) as usize]);
        }
        if auto {
            // Writes outside an explicit transaction (driver Drop paths) form their own.
            let txn = std::mem::take(&mut s.txn);
            for (k, v) in txn {
                s.committed.insert(k, v);
            }
        }
        Ok(())
    }

    /// Ordered mode: payload goes straight to its blocks, unless a block is currently held as
    /// metadata (in the overlay, or in an un-checkpointed WAL record that a replay would
    /// re-apply over it) — then it takes the journaled path so ordering stays correct.
    fn write_data_at(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        if offset + data.len() as u64 > self.inner.size() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "write beyond end of device"));
        }
        if data.is_empty() {
            return Ok(());
        }
        // Split the range per 4 KiB block: blocks shared with journaled metadata go through
        // the log (merged into the overlay), everything else is written in place.
        let end = offset + data.len() as u64;
        let mut segs: Vec<(u64, u64, bool)> = vec![]; // (start, end, journaled)
        {
            let mut s = self.st.lock().unwrap();
            if s.dead {
                return Ok(());
            }
            let first = offset / BS;
            let last = (end - 1) / BS;
            for b in first..=last {
                let j = s.txn.contains_key(&b)
                    || s.committed.contains_key(&b)
                    || s.flushing.contains_key(&b)
                    || s.written.contains_key(&b)
                    || s.freed_written.contains(&b)
                    || s.wal_blocks.contains(&b)
                    || s.freed_txn.contains(&b)
                    || s.freed_committed.contains(&b)
                    || s.freed_flushing.contains(&b);
                let a = offset.max(b * BS);
                let z = end.min((b + 1) * BS);
                match segs.last_mut() {
                    Some(l) if l.2 == j && l.1 == a => l.1 = z,
                    _ => segs.push((a, z, j)),
                }
            }
            for &(a, z, j) in &segs {
                if !j {
                    self.inner.write_at(a, &data[(a - offset) as usize..(z - offset) as usize])?;
                    s.data_dirty = true;
                    s.stats.data_bytes += z - a;
                }
            }
        }
        for (a, z, j) in segs {
            if j {
                self.write_at(a, &data[(a - offset) as usize..(z - offset) as usize])?;
            }
        }
        Ok(())
    }

    fn note_freed(&self, offset: u64, len: u64) {
        if len == 0 {
            return;
        }
        let mut s = self.st.lock().unwrap();
        let first = offset / BS;
        let last = (offset + len - 1) / BS;
        if (last - first) > 1 << 22 {
            return; // absurd range (whole-volume reformat paths): nothing sensible to track
        }
        for b in first..=last {
            s.freed_txn.insert(b);
        }
    }

    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn flush(&self) -> io::Result<()> {
        // Driver-level flush = durable point: push complete transactions through the WAL.
        if self.st.lock().unwrap().in_txn > 0 {
            return Ok(()); // the enclosing commit flushes
        }
        self.flush_round(true, true)
    }

    fn read_only(&self) -> bool {
        self.inner.read_only()
    }
}

/// FileSystem wrapper that turns every call into a WAL transaction.
pub struct Journaled {
    pub inner: Box<dyn FileSystem>,
    pub wal: Arc<WalDev>,
}

impl Journaled {
    fn tx<T>(&mut self, force: bool, f: impl FnOnce(&mut dyn FileSystem) -> FsResult<T>) -> FsResult<T> {
        self.wal.begin();
        let r = f(self.inner.as_mut());
        // Commit even on error: the driver's in-memory state already reflects partial work.
        self.wal.commit(force).map_err(|e| FsError::Io(format!("journal: {e}")))?;
        r
    }
}

impl FileSystem for Journaled {
    fn volume_info(&self) -> VolumeInfo {
        self.inner.volume_info()
    }
    fn statfs(&mut self) -> FsResult<FsStat> {
        self.inner.statfs()
    }
    fn root_ino(&self) -> Ino {
        self.inner.root_ino()
    }
    fn lookup(&mut self, p: Ino, n: &str) -> FsResult<Ino> {
        self.tx(false, |f| f.lookup(p, n))
    }
    fn getattr(&mut self, i: Ino) -> FsResult<Attr> {
        self.tx(false, |f| f.getattr(i))
    }
    fn setattr(&mut self, i: Ino, s: &SetAttr) -> FsResult<Attr> {
        self.tx(false, |f| f.setattr(i, s))
    }
    fn read(&mut self, i: Ino, o: u64, l: u32) -> FsResult<Vec<u8>> {
        self.tx(false, |f| f.read(i, o, l))
    }
    fn write(&mut self, i: Ino, o: u64, d: &[u8]) -> FsResult<u32> {
        self.tx(false, |f| f.write(i, o, d))
    }
    fn readdir(&mut self, d: Ino) -> FsResult<Vec<DirEntry>> {
        self.tx(false, |f| f.readdir(d))
    }
    fn create(&mut self, p: Ino, n: &str, m: u16, u: u32, g: u32) -> FsResult<Ino> {
        self.tx(false, |f| f.create(p, n, m, u, g))
    }
    fn mkdir(&mut self, p: Ino, n: &str, m: u16, u: u32, g: u32) -> FsResult<Ino> {
        self.tx(false, |f| f.mkdir(p, n, m, u, g))
    }
    fn symlink(&mut self, p: Ino, n: &str, t: &str, u: u32, g: u32) -> FsResult<Ino> {
        self.tx(false, |f| f.symlink(p, n, t, u, g))
    }
    fn readlink(&mut self, i: Ino) -> FsResult<String> {
        self.inner.readlink(i)
    }
    fn unlink(&mut self, p: Ino, n: &str) -> FsResult<()> {
        self.tx(false, |f| f.unlink(p, n))
    }
    fn rmdir(&mut self, p: Ino, n: &str) -> FsResult<()> {
        self.tx(false, |f| f.rmdir(p, n))
    }
    fn rename(&mut self, a: Ino, b: &str, c: Ino, d: &str) -> FsResult<()> {
        self.tx(false, |f| f.rename(a, b, c, d))
    }
    fn sync(&mut self) -> FsResult<()> {
        self.tx(true, |f| f.sync())
    }
    fn dirty_state(&mut self) -> DirtyState {
        self.inner.dirty_state()
    }
    fn repair(&mut self) -> FsResult<String> {
        self.tx(true, |f| f.repair())
    }
    fn read_only_reason(&self) -> Option<String> {
        self.inner.read_only_reason()
    }
    fn fingerprint(&mut self) -> Option<String> {
        self.inner.fingerprint()
    }
}

/// Stable WAL file name for a volume.
pub fn wal_path(support: &Path, key: &str) -> PathBuf {
    let clean: String = key.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    support.join("WAL").join(format!("{clean}.wal"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::MemDev;
    use crate::vfs::{ext4_fs, fat_fs, testutil};

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ld-wal-{}-{name}.wal", std::process::id()))
    }

    #[test]
    fn journaled_conformance_ext4_and_fat() {
        for t in [FsType::Ext4, FsType::Fat32] {
            let mem: Arc<dyn BlockDev> = Arc::new(MemDev::new(64 << 20));
            format(mem.clone(), t, "WAL").unwrap();
            let p = tmp(&format!("conf{t:?}"));
            let wal = WalDev::open(mem.clone(), &p, "x").unwrap();
            let drv = open_fs(wal.clone(), false, MountDefaults::default()).unwrap();
            let mut j = Journaled { inner: drv, wal: wal.clone() };
            testutil::conformance(&mut j, t == FsType::Ext4);
            assert!(wal.stats().transactions > 100);
            drop(j);
            drop(wal);
            assert!(!p.exists(), "clean shutdown removes the WAL");
            let mut fs = open_fs(mem, false, MountDefaults::default()).unwrap();
            testutil::check_persisted(fs.as_mut());
        }
    }

    /// The crash scenario: the WAL record is durable, but only part of it reached the disk.
    #[test]
    fn crash_mid_apply_is_repaired_by_replay() {
        for t in [FsType::Ext4, FsType::Fat32] {
            let mem = Arc::new(MemDev::new(64 << 20));
            let dev: Arc<dyn BlockDev> = mem.clone();
            format(dev.clone(), t, "CRASH").unwrap();
            let p = tmp(&format!("crash{t:?}"));
            {
                let wal = WalDev::open(dev.clone(), &p, "x").unwrap();
                let drv = open_fs(wal.clone(), false, MountDefaults::default()).unwrap();
                let mut j = Journaled { inner: drv, wal: wal.clone() };
                let root = j.root_ino();
                let d = j.mkdir(root, "dati", 0o755, 0, 0).unwrap();
                for i in 0..200 {
                    let f = j.create(d, &format!("documento {i}.txt"), 0o644, 0, 0).unwrap();
                    j.write(f, 0, format!("contenuto {i}").as_bytes()).unwrap();
                }
                wal.inject_fault(Fault::CrashAfterWal { apply_blocks: 3 });
                let _ = j.sync();
                std::mem::forget(j); // the process "dies": no destructors, no cleanup
                std::mem::forget(wal);
            }
            assert!(p.exists());
            // Replay on next open.
            let wal = WalDev::open(dev.clone(), &p, "x").unwrap();
            assert!(wal.stats().replayed_at_mount >= 1);
            drop(wal);
            let mut fs = open_fs(dev.clone(), false, MountDefaults::default()).unwrap();
            let root = fs.root_ino();
            let d = fs.lookup(root, "dati").unwrap();
            assert_eq!(fs.readdir(d).unwrap().len(), 200);
            let f = fs.lookup(d, "documento 199.txt").unwrap();
            assert_eq!(fs.read(f, 0, 100).unwrap(), b"contenuto 199");
            if t == FsType::Ext4 {
                let mut e = ext4_fs::Ext4Fs::open(dev.clone(), false).unwrap();
                assert_eq!(e.dirty_state(), DirtyState::Clean);
            } else {
                let _ = fat_fs::probe(dev.as_ref()).unwrap();
            }
            let _ = std::fs::remove_file(&p);
        }
    }

    /// Ordered mode: a 64 MiB file costs < 5 % extra journal writes.
    #[test]
    fn ordered_mode_overhead_under_5_percent() {
        for t in [FsType::Ext4, FsType::Fat32, FsType::ExFat] {
            let mem: Arc<dyn BlockDev> = Arc::new(MemDev::new(256 << 20));
            format(mem.clone(), t, "ORD").unwrap();
            let p = tmp(&format!("ord{t:?}"));
            let wal = WalDev::open(mem.clone(), &p, "x").unwrap();
            let drv = open_fs(wal.clone(), false, MountDefaults::default()).unwrap();
            let mut j = Journaled { inner: drv, wal: wal.clone() };
            let root = j.root_ino();
            let f = j.create(root, "video.mov", 0o644, 0, 0).unwrap();
            let chunk = testutil::pattern(1 << 20, 5);
            for k in 0..64u64 {
                j.write(f, k << 20, &chunk).unwrap();
            }
            j.sync().unwrap();
            let st = wal.stats();
            let overhead = st.wal_bytes as f64 / (64u64 << 20) as f64;
            println!("{t:?}: data {} MiB in place, WAL {} KiB → {:.2} % overhead", st.data_bytes >> 20, st.wal_bytes >> 10, overhead * 100.0);
            assert!(st.data_bytes >= 63 << 20, "{st:?}");
            assert!(overhead < 0.05, "{t:?}: WAL overhead {:.2} %", overhead * 100.0);
            drop(j);
            drop(wal);
            let mut fs = open_fs(mem, false, MountDefaults::default()).unwrap();
            let f = fs.lookup(fs.root_ino(), "video.mov").unwrap();
            assert_eq!(fs.read(f, 63 << 20, 1 << 20).unwrap(), chunk);
        }
    }

    /// Blocks freed in an uncommitted batch and reused for payload must not be overwritten in
    /// place: after a crash the last committed metadata (which still references them) wins.
    #[test]
    fn freed_metadata_blocks_are_not_overwritten_before_commit() {
        for t in [FsType::Ext4, FsType::Fat32, FsType::ExFat] {
            let mem: Arc<dyn BlockDev> = Arc::new(MemDev::new(48 << 20));
            format(mem.clone(), t, "REUSE").unwrap();
            let p = tmp(&format!("reuse{t:?}"));
            {
                let wal = WalDev::open(mem.clone(), &p, "x").unwrap();
                let drv = open_fs(wal.clone(), false, MountDefaults::default()).unwrap();
                let mut j = Journaled { inner: drv, wal: wal.clone() };
                let root = j.root_ino();
                let d = j.mkdir(root, "vecchia cartella", 0o755, 0, 0).unwrap();
                for i in 0..400 {
                    j.create(d, &format!("voce con un nome piuttosto lungo {i:04}"), 0o644, 0, 0).unwrap();
                }
                j.sync().unwrap(); // durable: the directory with 400 entries
                // Not yet committed: delete everything and reuse the space for a big file.
                for i in 0..400 {
                    j.unlink(d, &format!("voce con un nome piuttosto lungo {i:04}")).unwrap();
                }
                j.rmdir(root, "vecchia cartella").unwrap();
                let f = j.create(root, "riempitivo.bin", 0o644, 0, 0).unwrap();
                let junk = vec![0xEEu8; 1 << 20];
                for k in 0..16u64 {
                    j.write(f, k << 20, &junk).unwrap();
                }
                wal.inject_fault(Fault::CrashBeforeWal);
                let _ = j.sync();
                std::mem::forget(j);
                std::mem::forget(wal);
            }
            drop(WalDev::open(mem.clone(), &p, "x").unwrap());
            let mut fs = open_fs(mem, false, MountDefaults::default()).unwrap();
            let root = fs.root_ino();
            let d = fs.lookup(root, "vecchia cartella").expect("the committed directory must survive");
            let n = fs.readdir(d).unwrap().len();
            assert_eq!(n, 400, "{t:?}: directory blocks were clobbered by uncommitted payload");
            let _ = std::fs::remove_file(&p);
        }
    }

    #[test]
    fn crash_before_wal_loses_only_the_last_batch() {
        let mem: Arc<dyn BlockDev> = Arc::new(MemDev::new(32 << 20));
        format(mem.clone(), FsType::Ext4, "C2").unwrap();
        let p = tmp("before");
        {
            let wal = WalDev::open(mem.clone(), &p, "x").unwrap();
            let drv = open_fs(wal.clone(), false, MountDefaults::default()).unwrap();
            let mut j = Journaled { inner: drv, wal: wal.clone() };
            let root = j.root_ino();
            j.mkdir(root, "salvato", 0o755, 0, 0).unwrap();
            j.sync().unwrap();
            j.mkdir(root, "perso", 0o755, 0, 0).unwrap();
            wal.inject_fault(Fault::CrashBeforeWal);
            let _ = j.sync();
            std::mem::forget(j);
            std::mem::forget(wal);
        }
        drop(WalDev::open(mem.clone(), &p, "x").unwrap());
        let mut fs = ext4_fs::Ext4Fs::open(mem, false).unwrap();
        let root = fs.root_ino();
        assert!(fs.lookup(root, "salvato").is_ok());
        assert_eq!(fs.lookup(root, "perso"), Err(FsError::NotFound));
        let _ = std::fs::remove_file(&p);
    }

    /// The NFS server's group commit (`make_durable`) runs on another thread, without the
    /// volume lock, while operations keep changing the same blocks: whatever it made durable
    /// survives a crash, and reads never see a block between the log and its place.
    #[test]
    fn make_durable_beside_running_operations() {
        for t in [FsType::Ext4, FsType::Fat32] {
            let mem: Arc<dyn BlockDev> = Arc::new(MemDev::new(64 << 20));
            format(mem.clone(), t, "LIVE").unwrap();
            let p = tmp(&format!("live{t:?}"));
            let mut n = 0;
            {
                let wal = WalDev::open(mem.clone(), &p, "x").unwrap();
                let drv = open_fs(wal.clone(), false, MountDefaults::default()).unwrap();
                let mut j = Journaled { inner: drv, wal: wal.clone() };
                use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
                let stop = Arc::new(AtomicBool::new(false));
                let rounds = Arc::new(AtomicU64::new(0));
                let (w2, s2, r2) = (wal.clone(), stop.clone(), rounds.clone());
                let flusher = std::thread::spawn(move || {
                    while !s2.load(SeqCst) {
                        w2.make_durable().unwrap();
                        r2.fetch_add(1, SeqCst);
                    }
                });
                let root = j.root_ino();
                let d = j.mkdir(root, "dati", 0o755, 0, 0).unwrap();
                while n < 300 || rounds.load(SeqCst) < 100 && n < 5000 {
                    let i = n;
                    let f = j.create(d, &format!("file {i}.txt"), 0o644, 0, 0).unwrap();
                    j.write(f, 0, &testutil::pattern(3000 + i * 7, i as u32)).unwrap();
                    n += 1;
                    if i % 50 == 0 {
                        // reads see every block, wherever it is in its flush
                        assert_eq!(j.readdir(d).unwrap().len(), n);
                    }
                }
                stop.store(true, SeqCst);
                flusher.join().unwrap();
                assert!(rounds.load(SeqCst) >= 100, "{} rounds", rounds.load(SeqCst));
                wal.make_durable().unwrap(); // what a stable reply waits for
                wal.inject_fault(Fault::CrashBeforeWal); // then the process dies
                std::mem::forget(j);
                std::mem::forget(wal);
            }
            drop(WalDev::open(mem.clone(), &p, "x").unwrap());
            let mut fs = open_fs(mem, false, MountDefaults::default()).unwrap();
            let root = fs.root_ino();
            let d = fs.lookup(root, "dati").unwrap();
            assert_eq!(fs.readdir(d).unwrap().len(), n);
            for i in 0..n {
                let f = fs.lookup(d, &format!("file {i}.txt")).unwrap();
                assert_eq!(fs.read(f, 0, 1 << 20).unwrap(), testutil::pattern(3000 + i * 7, i as u32), "{t:?} file {i}");
            }
            let _ = std::fs::remove_file(&p);
        }
    }

    /// `make_written` (behind metadata replies) puts the record in the log without syncing it
    /// and without writing anything in place; a crash of the process keeps it, and the next
    /// `make_durable` syncs the log once.
    #[test]
    fn make_written_logs_without_syncing_or_writing_in_place() {
        let mem = Arc::new(MemDev::new(32 << 20));
        let dev: Arc<dyn BlockDev> = mem.clone();
        format(dev.clone(), FsType::Ext4, "WT").unwrap();
        let p = tmp("written");
        {
            let wal = WalDev::open(dev.clone(), &p, "x").unwrap();
            let drv = open_fs(wal.clone(), false, MountDefaults::default()).unwrap();
            let mut j = Journaled { inner: drv, wal: wal.clone() };
            let root = j.root_ino();
            let d = j.mkdir(root, "scritto", 0o755, 0, 0).unwrap();
            let f = j.create(d, "f.txt", 0o644, 0, 0).unwrap();
            j.write(f, 0, b"written, not synced").unwrap();
            let before = wal.stats();
            wal.make_written().unwrap();
            let after = wal.stats();
            assert_eq!(after.log_syncs, before.log_syncs, "no F_FULLFSYNC");
            assert_eq!(after.records_written, before.records_written + 1);
            // nothing in place yet: the device alone doesn't know the directory
            let mut plain = ext4_fs::Ext4Fs::open(Arc::new(MemDev::from_vec(mem.snapshot())), true).unwrap();
            assert_eq!(plain.lookup(plain.root_ino(), "scritto"), Err(FsError::NotFound));
            // but the volume does
            assert_eq!(j.read(f, 0, 100).unwrap(), b"written, not synced");
            wal.inject_fault(Fault::CrashBeforeWal);
            std::mem::forget(j);
            std::mem::forget(wal);
        }
        let wal = WalDev::open(dev.clone(), &p, "x").unwrap();
        assert!(wal.stats().replayed_at_mount >= 1);
        drop(wal);
        let mut fs = ext4_fs::Ext4Fs::open(dev.clone(), false).unwrap();
        let d = fs.lookup(fs.root_ino(), "scritto").unwrap();
        let f = fs.lookup(d, "f.txt").unwrap();
        assert_eq!(fs.read(f, 0, 100).unwrap(), b"written, not synced");
        assert_eq!(fs.dirty_state(), DirtyState::Clean);
        let _ = std::fs::remove_file(&p);

        // A group commit after write-throughs syncs once and writes everything in place.
        let p = tmp("written2");
        let wal = WalDev::open(dev.clone(), &p, "x").unwrap();
        let drv = open_fs(wal.clone(), false, MountDefaults::default()).unwrap();
        let mut j = Journaled { inner: drv, wal: wal.clone() };
        let root = j.root_ino();
        for i in 0..5 {
            j.mkdir(root, &format!("m{i}"), 0o755, 0, 0).unwrap();
            wal.make_written().unwrap();
        }
        let syncs = wal.stats().log_syncs;
        wal.make_durable().unwrap();
        assert_eq!(wal.stats().log_syncs, syncs + 1);
        let mut plain = ext4_fs::Ext4Fs::open(Arc::new(MemDev::from_vec(mem.snapshot())), true).unwrap();
        for i in 0..5 {
            assert!(plain.lookup(plain.root_ino(), &format!("m{i}")).is_ok(), "m{i} in place after the group commit");
        }
        drop(j);
        drop(wal);
        let _ = std::fs::remove_file(&p);
    }
}
