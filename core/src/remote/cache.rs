//! `RemoteVfs`: a [`FileSystem`] over a [`RemoteBackend`] that never lets the network block
//! Finder ("zero beachball").
//!
//! * Metadata (stat + directory listings) is cached in memory and in SQLite
//!   (`<cache>/meta.db`), so a mounted remote stays browsable while offline and even across
//!   restarts.
//! * File data is cached in 64 KiB chunks on disk (`<cache>/data/`), with read-ahead.
//! * Every remote metadata call has a short deadline (default 400 ms). A timeout flips the
//!   volume to **offline**: from then on every call is answered from the cache immediately.
//! * All mutations are applied locally first and queued (structural ops in order + dirty file
//!   chunks staged on disk). A background uploader replays them when online and never holds
//!   the volume lock while talking to the network. A health thread pings the backend every
//!   2 s and measures latency.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::*;
use crate::vfs::*;

const CHUNK: u64 = 64 * 1024;
const ROOT: Ino = 1;

#[derive(Clone, Debug)]
pub struct RemoteConfig {
    /// Deadline for metadata round trips before going offline.
    pub meta_timeout: Duration,
    /// Deadline for data transfers.
    pub data_timeout: Duration,
    /// How long cached metadata is trusted while online.
    pub attr_ttl: Duration,
    pub ping_interval: Duration,
    pub cache_dir: PathBuf,
    pub uid: u32,
    pub gid: u32,
    pub label: String,
    pub read_only: bool,
    /// Where queued offline changes survive restarts (`<support>/OfflineQueue/<volume>/`).
    /// `None` keeps them next to the cache.
    pub queue_dir: Option<PathBuf>,
}

/// On-disk form of the offline write-back queue (the staged bytes live in `stage.bin`).
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct PersistedQueue {
    url: String,
    next_op: u64,
    ops: Vec<(u64, Op)>,
    dirty: Vec<(String, Dirty)>,
    stage_len: u64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
enum Op {
    Create { path: String, perm: u16 },
    Mkdir { path: String, perm: u16 },
    Symlink { path: String, target: String },
    Remove { path: String },
    Rmdir { path: String },
    Rename { from: String, to: String },
    SetPerm { path: String, perm: u16 },
}

impl Op {
    fn path(&self) -> &str {
        match self {
            Op::Create { path, .. } | Op::Mkdir { path, .. } | Op::Symlink { path, .. } | Op::Remove { path } | Op::Rmdir { path } | Op::SetPerm { path, .. } => path,
            Op::Rename { to, .. } => to,
        }
    }
}

#[derive(Clone, Debug)]
struct Meta {
    e: RemoteEntry,
    at: Instant,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Dirty {
    size: u64,
    /// Bytes below this come from the remote base (if not in `chunks`).
    base_limit: u64,
    chunks: BTreeMap<u64, u64>, // chunk index → offset in the stage file
    gen: u64,
    mtime: Option<Timestamp>,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct RemoteHealth {
    pub url: String,
    pub online: bool,
    pub latency_ms: Option<u32>,
    pub pending_ops: u64,
    pub pending_files: Vec<String>,
    pub last_sync: Option<i64>,
    pub last_error: Option<String>,
    pub cache_bytes: u64,
    pub fetched_bytes: u64,
    pub remote_size: Option<u64>,
}

struct State {
    paths: HashMap<Ino, String>,
    inos: HashMap<String, Ino>,
    next_ino: Ino,
    meta: HashMap<String, Meta>,
    listed: HashMap<String, (Vec<String>, Instant)>,
    queue: VecDeque<(u64, Op)>,
    next_op: u64,
    dirty: HashMap<Ino, Dirty>,
    stage: File,
    stage_len: u64,
    gen: u64,
    statfs: Option<(u64, u64)>,
    db_dirty: bool,
    /// Bumped whenever the uploader changes server state (op applied / data uploaded).
    epoch: u64,
    /// Per-path data version, part of the chunk-cache key.
    ver: HashMap<String, u64>,
    /// What the uploader is doing. `Op`: applying `queue.front()` on the server, which may
    /// already be past it although it is still queued.
    upload: Upload,
    /// Reads resolving a server path through queued renames: no op may start meanwhile.
    pins: u32,
}

pub struct Shared {
    backend: Arc<dyn RemoteBackend>,
    cfg: RemoteConfig,
    st: Mutex<State>,
    online: AtomicBool,
    latency_ms: AtomicU32,
    last_sync: AtomicU64,
    fetched: AtomicU64,
    last_error: Mutex<Option<String>>,
    stop: AtomicBool,
    wake: (Mutex<bool>, Condvar),
    /// Paired with `st`: notified whenever the uploader finishes a unit of work or a pinned
    /// read ends.
    settled: Condvar,
    url: String,
}

/// Handle shared with the runtime for status / sync-now.
pub type RemoteHandle = Arc<Shared>;

pub struct RemoteVfs {
    sh: Arc<Shared>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

fn now_secs() -> i64 {
    Timestamp::now().secs
}

fn map_err(e: RemoteError) -> FsError {
    match e {
        RemoteError::NotFound => FsError::NotFound,
        RemoteError::Exists => FsError::Exists,
        RemoteError::NotEmpty => FsError::NotEmpty,
        RemoteError::NotDir => FsError::NotDir,
        RemoteError::IsDir => FsError::IsDir,
        RemoteError::Perm => FsError::Perm,
        RemoteError::Unsupported(s) => FsError::Unsupported(s),
        RemoteError::Timeout => FsError::Io(crate::i18n::t("server non raggiungibile (offline)", "server unreachable (offline)")),
        RemoteError::Disconnected(s) => FsError::Io(crate::i18n::t(&format!("connessione persa: {s}"), &format!("connection lost: {s}"))),
        RemoteError::Other(s) => FsError::Io(s),
    }
}

impl Shared {
    fn set_offline(&self, why: &RemoteError) {
        if self.online.swap(false, Ordering::SeqCst) {
            eprintln!("lumendisk: {} offline ({why})", self.url);
        }
        *self.last_error.lock().unwrap() = Some(why.to_string());
    }

    fn set_online(&self, lat: Duration) {
        self.latency_ms.store(lat.as_millis().min(u32::MAX as u128) as u32, Ordering::SeqCst);
        if !self.online.swap(true, Ordering::SeqCst) {
            eprintln!("lumendisk: {} back online", self.url);
            self.kick();
        }
    }

    fn kick(&self) {
        let (m, c) = &self.wake;
        *m.lock().unwrap() = true;
        c.notify_all();
    }

    /// Run a metadata call with the short deadline; network failures flip us offline.
    fn remote<T>(&self, f: impl FnOnce(&dyn RemoteBackend) -> RResult<T>) -> RResult<T> {
        if !self.online.load(Ordering::SeqCst) {
            return Err(RemoteError::Timeout);
        }
        self.backend.set_timeout(self.cfg.meta_timeout);
        let r = f(self.backend.as_ref());
        if let Err(e) = &r {
            if e.is_network() {
                self.set_offline(e);
            }
        }
        r
    }

    fn chunk_path(&self, path: &str, e: &RemoteEntry, idx: u64, ver: u64) -> PathBuf {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update(path.as_bytes());
        h.update(ver.to_le_bytes());
        h.update(e.size.to_le_bytes());
        h.update(e.mtime.secs.to_le_bytes());
        h.update(e.mtime.nsecs.to_le_bytes());
        let d = h.finalize();
        let key: String = d[..12].iter().map(|b| format!("{b:02x}")).collect();
        self.cfg.cache_dir.join("data").join(format!("{key}.{idx}"))
    }

    pub fn health(&self) -> RemoteHealth {
        let st = self.st.lock().unwrap();
        let mut files: Vec<String> = st.queue.iter().map(|(_, o)| o.path().to_string()).collect();
        for ino in st.dirty.keys() {
            if let Some(p) = st.paths.get(ino) {
                if !files.contains(p) {
                    files.push(p.clone());
                }
            }
        }
        let pending = files.len() as u64;
        files.truncate(50);
        let cache_bytes = dir_size(&self.cfg.cache_dir);
        RemoteHealth {
            url: self.url.clone(),
            online: self.online.load(Ordering::SeqCst),
            latency_ms: Some(self.latency_ms.load(Ordering::SeqCst)).filter(|_| self.online.load(Ordering::SeqCst)),
            pending_ops: pending,
            pending_files: files,
            last_sync: Some(self.last_sync.load(Ordering::SeqCst) as i64).filter(|&t| t > 0),
            last_error: self.last_error.lock().unwrap().clone(),
            cache_bytes,
            fetched_bytes: self.fetched.load(Ordering::SeqCst),
            remote_size: None,
        }
    }

    fn queue_dir(&self) -> PathBuf {
        self.cfg.queue_dir.clone().unwrap_or_else(|| self.cfg.cache_dir.join("queue"))
    }

    /// Write the queue atomically (tmp + rename + fsync). Removes it when empty.
    pub fn persist_queue(&self) -> std::io::Result<()> {
        self.persist_queue_opt(true)
    }

    /// `durable`: also force the bytes to the device (F_FULLFSYNC). Per-operation saves skip
    /// it (an atomic rename already survives an app crash); periodic sync and unmount don't.
    fn persist_queue_opt(&self, durable: bool) -> std::io::Result<()> {
        let dir = self.queue_dir();
        let st = self.st.lock().unwrap();
        if st.queue.is_empty() && st.dirty.is_empty() {
            let _ = std::fs::remove_file(dir.join("queue.json"));
            return Ok(());
        }
        std::fs::create_dir_all(&dir)?;
        let q = PersistedQueue {
            url: self.url.clone(),
            next_op: st.next_op,
            ops: st.queue.iter().cloned().collect(),
            dirty: st.dirty.iter().filter_map(|(ino, d)| st.paths.get(ino).map(|p| (p.clone(), d.clone()))).collect(),
            stage_len: st.stage_len,
        };
        if durable {
            st.stage.sync_data()?;
        }
        drop(st);
        let tmp = dir.join("queue.json.tmp");
        let mut f = File::create(&tmp)?;
        std::io::Write::write_all(&mut f, serde_json::to_vec(&q)?.as_slice())?;
        if durable {
            f.sync_all()?;
        }
        std::fs::rename(&tmp, dir.join("queue.json"))?;
        Ok(())
    }

    pub fn pending(&self) -> u64 {
        let st = self.st.lock().unwrap();
        st.queue.len() as u64 + st.dirty.len() as u64
    }

    /// Try to upload everything now (blocks up to `max`). Returns ops synced.
    pub fn sync_now(&self, max: Duration) -> u64 {
        if !self.online.load(Ordering::SeqCst) {
            // try to come back online
            self.backend.set_timeout(self.cfg.meta_timeout * 3);
            if let Ok(l) = self.backend.ping() {
                self.set_online(l);
            }
        }
        let t0 = Instant::now();
        let mut n = 0;
        while t0.elapsed() < max {
            match self.upload_one() {
                Some(true) => n += 1,
                _ => break,
            }
        }
        n
    }

    /// Upload one unit of queued work. Some(true) = progress, Some(false) = failed (offline),
    /// None = nothing to do.
    fn upload_one(&self) -> Option<bool> {
        if !self.online.load(Ordering::SeqCst) {
            return Some(false);
        }
        // One unit of work at a time (`sync_now` runs beside the uploader thread), and no
        // structural op while a read resolves a server path through the queue (`read_chunk`).
        let mut st = self.st.lock().unwrap();
        while st.pins > 0 || st.upload != Upload::Idle {
            st = self.settled.wait(st).unwrap();
        }
        // 1) structural ops, in order
        if let Some((op_id, op)) = st.queue.front().cloned() {
            st.upload = Upload::Op;
            drop(st);
            let _busy = Busy(self);
            self.backend.set_timeout(self.cfg.data_timeout);
            let b = self.backend.as_ref();
            let r = match &op {
                Op::Create { path, perm } => match b.create(path, *perm) {
                    Err(RemoteError::Exists) => b.truncate(path, 0),
                    r => r,
                },
                Op::Mkdir { path, perm } => match b.mkdir(path, *perm) {
                    Err(RemoteError::Exists) => Ok(()),
                    r => r,
                },
                Op::Symlink { path, target } => b.symlink(path, target),
                Op::Remove { path } => b.remove(path),
                Op::Rmdir { path } => b.rmdir(path),
                Op::Rename { from, to } => b.rename(from, to),
                Op::SetPerm { path, perm } => b.set_perm(path, *perm),
            };
            let offline = matches!(&r, Err(e) if e.is_network());
            if let Err(e) = &r {
                if offline {
                    self.set_offline(e);
                } else {
                    eprintln!("lumendisk: remote op {op:?} failed permanently: {e}; dropping");
                    *self.last_error.lock().unwrap() = Some(format!("{}: {e}", op.path()));
                }
            }
            if !offline {
                // (before `_busy` ends: a reader seeing the op still queued must see it in flight)
                let mut st = self.st.lock().unwrap();
                if let Some(i) = st.queue.iter().position(|(id, _)| *id == op_id) {
                    st.queue.remove(i);
                }
                st.epoch += 1;
            }
            if offline {
                return Some(false);
            }
            self.last_sync.store(now_secs() as u64, Ordering::SeqCst);
            return Some(true);
        }
        // 2) dirty file contents, only when all structural ops are applied (checked under the
        //    same lock): remote paths then equal local paths
        let snap = {
            let (ino, d) = st.dirty.iter().next().map(|(i, d)| (*i, d.clone()))?;
            let path = st.paths.get(&ino).cloned()?;
            let mut chunks = vec![];
            for (&idx, &off) in &d.chunks {
                let start = idx * CHUNK;
                if start >= d.size {
                    continue;
                }
                let len = (d.size - start).min(CHUNK) as usize;
                let mut buf = vec![0u8; len];
                if st.stage.read_exact_at(&mut buf, off).is_err() {
                    return Some(false);
                }
                chunks.push((start, buf));
            }
            (ino, path, d, chunks)
        };
        st.upload = Upload::Data;
        drop(st);
        let _busy = Busy(self);
        let (ino, path, d, chunks) = snap;
        self.backend.set_timeout(self.cfg.data_timeout);
        let b = self.backend.as_ref();
        let res: RResult<()> = (|| {
            if b.supports_partial_write() {
                let cur = b.stat(&path)?.size;
                if d.base_limit < cur {
                    b.truncate(&path, d.base_limit)?;
                }
                for (off, data) in &chunks {
                    b.write(&path, *off, data)?;
                }
                if b.stat(&path)?.size != d.size {
                    b.truncate(&path, d.size)?;
                }
            } else {
                // Assemble the whole file: remote base + dirty chunks.
                let mut all = vec![0u8; d.size as usize];
                let base = d.base_limit.min(d.size);
                let mut pos = 0u64;
                while pos < base {
                    let n = (base - pos).min(4 << 20) as u32;
                    let part = b.read(&path, pos, n)?;
                    if part.is_empty() {
                        break;
                    }
                    all[pos as usize..pos as usize + part.len()].copy_from_slice(&part);
                    pos += part.len() as u64;
                }
                for (off, data) in &chunks {
                    all[*off as usize..*off as usize + data.len()].copy_from_slice(data);
                }
                b.put_all(&path, &all)?;
            }
            if let Some(m) = d.mtime {
                let _ = b.set_mtime(&path, m);
            }
            Ok(())
        })();
        match res {
            Err(e) if e.is_network() => {
                self.set_offline(&e);
                Some(false)
            }
            r => {
                if let Err(e) = r {
                    eprintln!("lumendisk: upload of {path} failed: {e}");
                    *self.last_error.lock().unwrap() = Some(format!("{path}: {e}"));
                }
                let mut st = self.st.lock().unwrap();
                st.epoch += 1;
                *st.ver.entry(path.clone()).or_default() += 1;
                // Keep the local copy authoritative while structural ops are still queued.
                if st.queue.is_empty() && st.dirty.get(&ino).map(|x| x.gen == d.gen).unwrap_or(false) {
                    st.dirty.remove(&ino);
                    // the remote now has new mtime/size: refresh lazily
                    if let Some(m) = st.meta.get_mut(&path) {
                        m.e.size = d.size;
                        m.at = Instant::now() - self.cfg.attr_ttl * 2;
                    }
                }
                drop(st);
                self.last_sync.store(now_secs() as u64, Ordering::SeqCst);
                Some(true)
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Upload {
    Idle,
    Op,
    Data,
}

/// A unit of upload work in progress (ends on every path, panics included).
struct Busy<'a>(&'a Shared);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.st.lock().unwrap().upload = Upload::Idle;
        self.0.settled.notify_all();
    }
}

/// A read holding the uploader off structural ops (see `RemoteVfs::read_chunk`).
struct Pin<'a>(&'a Shared);

impl Drop for Pin<'_> {
    fn drop(&mut self) {
        self.0.st.lock().unwrap().pins -= 1;
        self.0.settled.notify_all();
    }
}

fn dir_size(p: &std::path::Path) -> u64 {
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            if let Ok(m) = e.metadata() {
                n += if m.is_dir() { dir_size(&e.path()) } else { m.len() };
            }
        }
    }
    n
}

impl RemoteVfs {
    pub fn new(backend: Arc<dyn RemoteBackend>, cfg: RemoteConfig) -> FsResult<RemoteVfs> {
        std::fs::create_dir_all(cfg.cache_dir.join("data"))?;
        let qdir = cfg.queue_dir.clone().unwrap_or_else(|| cfg.cache_dir.join("queue"));
        std::fs::create_dir_all(&qdir)?;
        let url = backend.describe();
        // Restore an offline queue left by a previous session (quit / crash while offline).
        let restored: Option<PersistedQueue> = std::fs::read(qdir.join("queue.json")).ok().and_then(|b| serde_json::from_slice(&b).ok());
        let stage = OpenOptions::new().read(true).write(true).create(true).truncate(restored.is_none()).open(qdir.join("stage.bin"))?;
        let mut st = State {
            paths: HashMap::from([(ROOT, "/".to_string())]),
            inos: HashMap::from([("/".to_string(), ROOT)]),
            next_ino: 2,
            meta: HashMap::new(),
            listed: HashMap::new(),
            queue: VecDeque::new(),
            next_op: 1,
            dirty: HashMap::new(),
            stage,
            stage_len: 0,
            gen: 0,
            statfs: None,
            db_dirty: false,
            epoch: 0,
            ver: HashMap::new(),
            upload: Upload::Idle,
            pins: 0,
        };
        if let Some(q) = restored {
            eprintln!("lumendisk: restored {} queued change(s) for {url}", q.ops.len() + q.dirty.len());
            st.queue = q.ops.into_iter().collect();
            st.next_op = q.next_op.max(st.queue.iter().map(|(i, _)| i + 1).max().unwrap_or(1));
            st.stage_len = q.stage_len;
            for (path, d) in q.dirty {
                let ino = Self::ino_for(&mut st, &path);
                st.gen = st.gen.max(d.gen);
                st.dirty.insert(ino, d);
            }
        }
        // Offline-capable: preload the persisted metadata cache.
        let old = Instant::now() - cfg.attr_ttl * 10;
        if let Ok(db) = Self::db(&cfg) {
            if let Ok(mut s) = db.prepare("SELECT path, json FROM meta") {
                let rows = s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)));
                if let Ok(rows) = rows {
                    for (p, j) in rows.flatten() {
                        if let Ok(e) = serde_json::from_str::<RemoteEntry>(&j) {
                            st.meta.insert(p, Meta { e, at: old });
                        }
                    }
                }
            }
            if let Ok(mut s) = db.prepare("SELECT path, names FROM listing") {
                if let Ok(rows) = s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) {
                    for (p, j) in rows.flatten() {
                        if let Ok(names) = serde_json::from_str::<Vec<String>>(&j) {
                            st.listed.insert(p, (names, old));
                        }
                    }
                }
            }
        }
        let sh = Arc::new(Shared {
            backend,
            cfg,
            st: Mutex::new(st),
            online: AtomicBool::new(false),
            latency_ms: AtomicU32::new(0),
            last_sync: AtomicU64::new(0),
            fetched: AtomicU64::new(0),
            last_error: Mutex::new(None),
            stop: AtomicBool::new(false),
            wake: (Mutex::new(false), Condvar::new()),
            settled: Condvar::new(),
            url,
        });
        // First contact (bounded); failing is fine if we have a cache.
        sh.backend.set_timeout(sh.cfg.meta_timeout * 5);
        match sh.backend.ping() {
            Ok(l) => sh.set_online(l),
            Err(e) => {
                let have_cache = { let st = sh.st.lock().unwrap(); st.meta.contains_key("/") || !st.queue.is_empty() };
                if !have_cache {
                    return Err(map_err(e));
                }
                sh.set_offline(&e);
            }
        }
        let mut threads = vec![];
        // health
        let s1 = sh.clone();
        threads.push(std::thread::Builder::new().name("lumendisk-remote-health".into()).spawn(move || {
            while !s1.stop.load(Ordering::SeqCst) {
                s1.backend.set_timeout(s1.cfg.meta_timeout);
                match s1.backend.ping() {
                    Ok(l) => s1.set_online(l),
                    Err(e) => s1.set_offline(&e),
                }
                let (m, c) = (&s1.wake.0, &s1.wake.1);
                let g = m.lock().unwrap();
                let _ = c.wait_timeout(g, s1.cfg.ping_interval).unwrap();
            }
        })?);
        // uploader
        let s2 = sh.clone();
        threads.push(std::thread::Builder::new().name("lumendisk-remote-sync".into()).spawn(move || {
            while !s2.stop.load(Ordering::SeqCst) {
                let mut progressed = false;
                while let Some(true) = s2.upload_one() {
                    progressed = true;
                    if s2.stop.load(Ordering::SeqCst) {
                        return;
                    }
                }
                let _ = progressed;
                let (m, c) = (&s2.wake.0, &s2.wake.1);
                let mut g = m.lock().unwrap();
                if !*g {
                    g = c.wait_timeout(g, Duration::from_millis(500)).unwrap().0;
                }
                *g = false;
            }
        })?);
        Ok(RemoteVfs { sh, threads })
    }

    pub fn handle(&self) -> RemoteHandle {
        self.sh.clone()
    }

    fn db(cfg: &RemoteConfig) -> rusqlite::Result<rusqlite::Connection> {
        let c = rusqlite::Connection::open(cfg.cache_dir.join("meta.db"))?;
        c.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS meta(path TEXT PRIMARY KEY, json TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS listing(path TEXT PRIMARY KEY, names TEXT NOT NULL);",
        )?;
        Ok(c)
    }

    fn persist_meta(&self) {
        let (meta, listed) = {
            let mut st = self.sh.st.lock().unwrap();
            if !st.db_dirty {
                return;
            }
            st.db_dirty = false;
            (st.meta.iter().map(|(p, m)| (p.clone(), m.e.clone())).collect::<Vec<_>>(), st.listed.iter().map(|(p, l)| (p.clone(), l.0.clone())).collect::<Vec<_>>())
        };
        if let Ok(mut db) = Self::db(&self.sh.cfg) {
            if let Ok(tx) = db.transaction() {
                let _ = tx.execute("DELETE FROM meta", []);
                let _ = tx.execute("DELETE FROM listing", []);
                for (p, e) in meta {
                    let _ = tx.execute("INSERT OR REPLACE INTO meta(path, json) VALUES (?1, ?2)", rusqlite::params![p, serde_json::to_string(&e).unwrap_or_default()]);
                }
                for (p, n) in listed {
                    let _ = tx.execute("INSERT OR REPLACE INTO listing(path, names) VALUES (?1, ?2)", rusqlite::params![p, serde_json::to_string(&n).unwrap_or_default()]);
                }
                let _ = tx.commit();
            }
        }
    }

    fn path_of(&self, ino: Ino) -> FsResult<String> {
        self.sh.st.lock().unwrap().paths.get(&ino).cloned().ok_or(FsError::Stale)
    }

    fn ino_for(st: &mut State, path: &str) -> Ino {
        if let Some(i) = st.inos.get(path) {
            return *i;
        }
        let i = st.next_ino;
        st.next_ino += 1;
        st.inos.insert(path.to_string(), i);
        st.paths.insert(i, path.to_string());
        i
    }

    fn check_rw(&self) -> FsResult<()> {
        if self.sh.cfg.read_only {
            Err(FsError::ReadOnly)
        } else {
            Ok(())
        }
    }

    /// Metadata for a path: fresh cache, else remote (short deadline), else stale cache.
    fn meta(&self, path: &str) -> FsResult<RemoteEntry> {
        {
            let st = self.sh.st.lock().unwrap();
            if Self::locally_deleted(&st, path) {
                return Err(FsError::NotFound);
            }
            if let Some(m) = st.meta.get(path) {
                if m.at.elapsed() < self.sh.cfg.attr_ttl || !self.sh.online.load(Ordering::SeqCst) || self.locally_modified(&st, path) {
                    return Ok(m.e.clone());
                }
            } else if path != "/" {
                // Parent listed recently and the name isn't there → definitely absent.
                let parent = parent_of(path);
                let name = path.rsplit('/').next().unwrap_or("");
                if let Some((names, at)) = st.listed.get(parent) {
                    if (at.elapsed() < self.sh.cfg.attr_ttl || !self.sh.online.load(Ordering::SeqCst)) && !names.iter().any(|n| n == name) {
                        return Err(FsError::NotFound);
                    }
                }
            }
        }
        let epoch0 = self.sh.st.lock().unwrap().epoch;
        match self.sh.remote(|b| b.stat(path)) {
            Ok(mut e) => {
                if e.name.is_empty() && path != "/" {
                    e.name = path.rsplit('/').next().unwrap_or("").into();
                }
                let mut st = self.sh.st.lock().unwrap();
                if st.epoch != epoch0 || self.locally_modified(&st, path) || Self::locally_deleted(&st, path) {
                    // The server changed under us (our own uploads): local state wins.
                    if Self::locally_deleted(&st, path) {
                        return Err(FsError::NotFound);
                    }
                    if let Some(m) = st.meta.get(path) {
                        return Ok(m.e.clone());
                    }
                }
                st.meta.insert(path.to_string(), Meta { e: e.clone(), at: Instant::now() });
                st.db_dirty = true;
                Ok(e)
            }
            Err(RemoteError::NotFound) => {
                let mut st = self.sh.st.lock().unwrap();
                if st.epoch != epoch0 || self.locally_modified(&st, path) {
                    if let Some(m) = st.meta.get(path) {
                        return Ok(m.e.clone());
                    }
                }
                st.meta.remove(path);
                Err(FsError::NotFound)
            }
            Err(e) => {
                let st = self.sh.st.lock().unwrap();
                st.meta.get(path).map(|m| m.e.clone()).ok_or_else(|| map_err(e))
            }
        }
    }

    /// Where the data of local `path` lives on the server right now: undo queued renames.
    fn remote_path_of(st: &State, path: &str) -> String {
        let mut p = path.to_string();
        for (_, op) in st.queue.iter().rev() {
            if let Op::Rename { from, to } = op {
                if p == *to {
                    p = from.clone();
                } else if p.starts_with(&format!("{to}/")) {
                    p = format!("{from}{}", &p[to.len()..]);
                }
            }
        }
        p
    }

    /// Replaying the queue: is `path` deleted (or moved away) locally but maybe still remote?
    fn locally_deleted(st: &State, path: &str) -> bool {
        let under = |p: &str, root: &str| p == root || p.starts_with(&format!("{root}/"));
        let mut deleted = false;
        for (_, op) in &st.queue {
            match op {
                Op::Remove { path: q } | Op::Rmdir { path: q } if under(path, q) => deleted = true,
                Op::Rename { from, to } => {
                    if under(path, from) {
                        deleted = true;
                    }
                    if under(path, to) {
                        deleted = false;
                    }
                }
                Op::Create { path: q, .. } | Op::Mkdir { path: q, .. } | Op::Symlink { path: q, .. } if q == path => deleted = false,
                _ => {}
            }
        }
        deleted && !st.meta.contains_key(path)
    }

    /// Paths touched by queued ops must not be refreshed from the remote (it's behind).
    fn locally_modified(&self, st: &State, path: &str) -> bool {
        // Anything at, above or below a queued op's path (both ends of a rename) is ahead of
        // the server until the queue drains.
        let related = |q: &str| q == path || q.starts_with(&format!("{path}/")) || path.starts_with(&format!("{q}/"));
        st.queue.iter().any(|(_, o)| related(o.path()) || matches!(o, Op::Rename { from, .. } if related(from)))
            || st.inos.get(path).map(|i| st.dirty.contains_key(i)).unwrap_or(false)
    }

    fn list(&self, path: &str) -> FsResult<Vec<String>> {
        {
            let st = self.sh.st.lock().unwrap();
            if let Some((names, at)) = st.listed.get(path) {
                if at.elapsed() < self.sh.cfg.attr_ttl || !self.sh.online.load(Ordering::SeqCst) || self.locally_modified(&st, path) {
                    return Ok(names.clone());
                }
            }
        }
        let epoch0 = self.sh.st.lock().unwrap().epoch;
        match self.sh.remote(|b| b.list(path)) {
            Ok(entries) => {
                let mut st = self.sh.st.lock().unwrap();
                if st.epoch != epoch0 && st.listed.contains_key(path) {
                    return Ok(st.listed.get(path).map(|l| l.0.clone()).unwrap_or_default());
                }
                if self.locally_modified(&st, path) {
                    // queued local changes win
                    return Ok(st.listed.get(path).map(|l| l.0.clone()).unwrap_or_default());
                }
                let now = Instant::now();
                let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
                for e in entries {
                    let p = join(path, &e.name);
                    let keep_local = st.inos.get(&p).map(|i| st.dirty.contains_key(i)).unwrap_or(false);
                    if !keep_local {
                        st.meta.insert(p, Meta { e, at: now });
                    }
                }
                st.listed.insert(path.to_string(), (names.clone(), now));
                st.db_dirty = true;
                Ok(names)
            }
            Err(e) => {
                let st = self.sh.st.lock().unwrap();
                if let Some(l) = st.listed.get(path) {
                    return Ok(l.0.clone());
                }
                // Never listed while online: offer what we know (entries looked up or created).
                let mut known: Vec<String> = st
                    .meta
                    .keys()
                    .filter(|p| p.as_str() != "/" && parent_of(p) == path && p.as_str() != path)
                    .map(|p| p.rsplit('/').next().unwrap_or("").to_string())
                    .collect();
                known.sort();
                if known.is_empty() {
                    Err(map_err(e))
                } else {
                    Ok(known)
                }
            }
        }
    }

    fn attr(&self, ino: Ino, path: &str, e: &RemoteEntry) -> Attr {
        let st = self.sh.st.lock().unwrap();
        let (size, mtime) = match st.dirty.get(&ino) {
            Some(d) => (d.size, d.mtime.unwrap_or(e.mtime)),
            None => (e.size, e.mtime),
        };
        let _ = path;
        Attr {
            ino,
            kind: e.kind,
            size,
            perm: if e.perm == 0 { if e.kind == FileKind::Dir { 0o755 } else { 0o644 } } else { e.perm },
            nlink: if e.kind == FileKind::Dir { 2 } else { 1 },
            uid: self.sh.cfg.uid,
            gid: self.sh.cfg.gid,
            atime: mtime,
            mtime,
            ctime: mtime,
            used_bytes: size,
        }
    }

    // local mutation helpers
    fn local_add(&self, parent: &str, name: &str, e: RemoteEntry) -> Ino {
        let p = join(parent, name);
        let mut st = self.sh.st.lock().unwrap();
        st.meta.insert(p.clone(), Meta { e, at: Instant::now() });
        if let Some((names, _)) = st.listed.get_mut(parent) {
            if !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
        st.db_dirty = true;
        Self::ino_for(&mut st, &p)
    }

    fn local_remove(&self, path: &str) {
        let mut st = self.sh.st.lock().unwrap();
        st.meta.remove(path);
        let parent = parent_of(path).to_string();
        let name = path.rsplit('/').next().unwrap_or("").to_string();
        if let Some((names, _)) = st.listed.get_mut(&parent) {
            names.retain(|n| *n != name);
        }
        st.listed.remove(path);
        if let Some(ino) = st.inos.remove(path) {
            st.paths.remove(&ino);
            st.dirty.remove(&ino);
        }
        st.db_dirty = true;
    }

    fn enqueue(&self, op: Op) {
        let mut st = self.sh.st.lock().unwrap();
        Self::push_op(&mut st, op);
        drop(st);
        self.queued();
    }

    /// Queue `op` in the same critical section as the local change it mirrors: the uploader
    /// treats an empty queue as "server paths equal local paths" and must never see one half.
    fn push_op(st: &mut State, op: Op) {
        let id = st.next_op;
        st.next_op += 1;
        st.queue.push_back((id, op));
    }

    /// After [`Self::push_op`] (lock released): save the queue and wake the uploader.
    fn queued(&self) {
        let _ = self.sh.persist_queue_opt(false);
        self.sh.kick();
    }

    /// Get (or start) the dirty record of a file. A new record's remote base is the size the
    /// server holds *now* (the cached metadata, kept current by the uploader), never a size
    /// captured earlier by the caller — the uploader may have finished in between.
    fn dirty_entry<'a>(st: &'a mut State, ino: Ino, path: &str, fallback: u64) -> &'a mut Dirty {
        st.gen += 1;
        let g = st.gen;
        if !st.dirty.contains_key(&ino) {
            let base = st.meta.get(path).map(|m| m.e.size).unwrap_or(fallback);
            st.dirty.insert(ino, Dirty { size: base, base_limit: base, chunks: BTreeMap::new(), gen: g, mtime: None });
        }
        let d = st.dirty.get_mut(&ino).unwrap();
        d.gen = g;
        d
    }

    /// Read a data chunk: staged dirty data, else disk cache, else remote (online only).
    fn read_chunk(&self, ino: Ino, path: &str, e: &RemoteEntry, idx: u64) -> FsResult<Vec<u8>> {
        let start = idx * CHUNK;
        // While the file has unsynced local data the server copy is behind: read only what the
        // remote base really holds and don't populate the shared chunk cache.
        let mut limit = e.size;
        let mut has_dirty = false;
        {
            let st = self.sh.st.lock().unwrap();
            if let Some(d) = st.dirty.get(&ino) {
                has_dirty = true;
                limit = d.base_limit.min(e.size);
                if let Some(&off) = d.chunks.get(&idx) {
                    let mut buf = vec![0u8; CHUNK as usize];
                    st.stage.read_exact_at(&mut buf, off)?;
                    return Ok(buf);
                }
                if start >= d.base_limit {
                    return Ok(vec![0u8; CHUNK as usize]);
                }
            }
        }
        let mut buf = vec![0u8; CHUNK as usize];
        let base_len = limit.saturating_sub(start).min(CHUNK) as usize;
        if base_len == 0 {
            return Ok(buf);
        }
        let ver = self.sh.st.lock().unwrap().ver.get(path).copied().unwrap_or(0);
        let cp = self.sh.chunk_path(path, e, idx, ver);
        if let (false, Ok(data)) = (has_dirty, std::fs::read(&cp)) {
            if data.len() == base_len {
                buf[..base_len].copy_from_slice(&data);
                return Ok(buf);
            }
        }
        if !self.sh.online.load(Ordering::SeqCst) {
            return Err(FsError::Io(crate::i18n::t("offline: contenuto non ancora in cache", "offline: content not cached yet")));
        }
        // Fetch this chunk plus read-ahead (up to 4 chunks) in one request. Queued renames say
        // where the data still is on the server, so while we read there the uploader must not
        // start another op (pin). An op it was already applying may have moved the data before
        // we looked: then read again once that op has landed.
        let mut pin = None;
        let mut attempt = 0;
        let (renamed, data) = loop {
            attempt += 1;
            let (rpath, inflight) = {
                let mut st = self.sh.st.lock().unwrap();
                let rp = Self::remote_path_of(&st, path);
                if rp != path && pin.is_none() {
                    st.pins += 1;
                    pin = Some(Pin(&self.sh));
                }
                (rp, st.upload == Upload::Op)
            };
            let renamed = rpath != path;
            let ahead = if has_dirty || renamed { 1 } else { ((limit - start).div_ceil(CHUNK)).min(4) };
            self.sh.backend.set_timeout(self.sh.cfg.data_timeout);
            let r = self.sh.backend.read(&rpath, start, (ahead * CHUNK).min(limit - start) as u32);
            if renamed && inflight && attempt < 3 {
                let st = self.sh.st.lock().unwrap();
                let _ = self.sh.settled.wait_timeout_while(st, self.sh.cfg.data_timeout * 2, |s| s.upload == Upload::Op);
                continue;
            }
            match r {
                Ok(d) => break (renamed, d),
                Err(err) => {
                    if err.is_network() {
                        self.sh.set_offline(&err);
                    }
                    return Err(map_err(err));
                }
            }
        };
        drop(pin);
        let cacheable = !has_dirty && !renamed;
        let ahead = if cacheable { ((limit - start).div_ceil(CHUNK)).min(4) } else { 0 };
        self.sh.fetched.fetch_add(data.len() as u64, Ordering::SeqCst);
        for k in 0..ahead {
            let s = (k * CHUNK) as usize;
            if s >= data.len() {
                break;
            }
            let e2 = (s + CHUNK as usize).min(data.len());
            let _ = std::fs::write(self.sh.chunk_path(path, e, idx + k, ver), &data[s..e2]);
        }
        let n = base_len.min(data.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(buf)
    }
}

impl Drop for RemoteVfs {
    fn drop(&mut self) {
        self.persist_meta();
        self.sh.stop.store(true, Ordering::SeqCst);
        self.sh.kick();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        // Keep pending offline changes on disk; clean up only when everything is synced.
        if self.sh.persist_queue().is_ok() && self.sh.pending() == 0 {
            let q = self.sh.queue_dir();
            let _ = std::fs::remove_file(q.join("stage.bin"));
            let _ = std::fs::remove_file(q.join("queue.json"));
            let _ = std::fs::remove_dir(&q);
        }
    }
}

impl FileSystem for RemoteVfs {
    fn volume_info(&self) -> VolumeInfo {
        VolumeInfo { fs_type: FsType::Ext4, label: self.sh.cfg.label.clone(), uuid: self.sh.url.clone(), read_only: self.sh.cfg.read_only }
    }

    fn statfs(&mut self) -> FsResult<FsStat> {
        let cached = self.sh.st.lock().unwrap().statfs;
        let s = match cached {
            Some(s) if !self.sh.online.load(Ordering::SeqCst) => Some(s),
            _ => match self.sh.remote(|b| b.statfs()) {
                Ok(s) => {
                    self.sh.st.lock().unwrap().statfs = s;
                    s
                }
                Err(_) => cached,
            },
        };
        let (total, free) = s.unwrap_or((1 << 40, 1 << 40));
        Ok(FsStat { block_size: 4096, total_bytes: total, free_bytes: free, total_inodes: 1 << 32, free_inodes: 1 << 31 })
    }

    fn root_ino(&self) -> Ino {
        ROOT
    }

    fn lookup(&mut self, parent: Ino, name: &str) -> FsResult<Ino> {
        let pp = self.path_of(parent)?;
        let p = join(&pp, name);
        let e = self.meta(&p)?;
        let _ = e;
        let mut st = self.sh.st.lock().unwrap();
        Ok(Self::ino_for(&mut st, &p))
    }

    fn getattr(&mut self, ino: Ino) -> FsResult<Attr> {
        let p = self.path_of(ino)?;
        let e = self.meta(&p)?;
        Ok(self.attr(ino, &p, &e))
    }

    fn setattr(&mut self, ino: Ino, set: &SetAttr) -> FsResult<Attr> {
        self.check_rw()?;
        let p = self.path_of(ino)?;
        let mut e = self.meta(&p)?;
        if let Some(sz) = set.size {
            if e.kind == FileKind::Dir {
                return Err(FsError::IsDir);
            }
            let mut st = self.sh.st.lock().unwrap();
            let d = Self::dirty_entry(&mut st, ino, &p, e.size);
            if sz < d.base_limit {
                d.base_limit = sz;
            }
            let keep = sz.div_ceil(CHUNK);
            d.chunks.retain(|&k, _| k < keep);
            let old = d.size;
            d.size = sz;
            d.mtime = Some(Timestamp::now());
            if sz < old && sz % CHUNK != 0 {
                if let Some(&off) = d.chunks.get(&(sz / CHUNK)) {
                    let z = vec![0u8; (CHUNK - sz % CHUNK) as usize];
                    st.stage.write_all_at(&z, off + sz % CHUNK)?;
                }
            }
            if let Some(m) = st.meta.get_mut(&p) {
                m.e.size = sz;
            }
            drop(st);
            self.sh.kick();
        }
        if let Some(perm) = set.perm {
            e.perm = perm;
            let mut st = self.sh.st.lock().unwrap();
            if let Some(m) = st.meta.get_mut(&p) {
                m.e.perm = perm;
            }
            drop(st);
            self.enqueue(Op::SetPerm { path: p.clone(), perm });
        }
        if let Some(t) = set.mtime {
            let mut st = self.sh.st.lock().unwrap();
            if let Some(m) = st.meta.get_mut(&p) {
                m.e.mtime = t;
            }
            let d = Self::dirty_entry(&mut st, ino, &p, e.size);
            d.mtime = Some(t);
            drop(st);
            self.sh.kick();
        }
        self.getattr(ino)
    }

    fn read(&mut self, ino: Ino, offset: u64, len: u32) -> FsResult<Vec<u8>> {
        let p = self.path_of(ino)?;
        let e = self.meta(&p)?;
        match e.kind {
            FileKind::Dir => return Err(FsError::IsDir),
            FileKind::Symlink => return Ok(self.readlink(ino)?.into_bytes()),
            _ => {}
        }
        let size = self.sh.st.lock().unwrap().dirty.get(&ino).map(|d| d.size).unwrap_or(e.size);
        if offset >= size {
            return Ok(vec![]);
        }
        let end = (offset + len as u64).min(size);
        let mut out = Vec::with_capacity((end - offset) as usize);
        let mut pos = offset;
        while pos < end {
            let idx = pos / CHUNK;
            let c = self.read_chunk(ino, &p, &e, idx)?;
            let s = (pos % CHUNK) as usize;
            let n = ((end - pos) as usize).min(CHUNK as usize - s);
            out.extend_from_slice(&c[s..s + n]);
            pos += n as u64;
        }
        Ok(out)
    }

    fn write(&mut self, ino: Ino, offset: u64, data: &[u8]) -> FsResult<u32> {
        self.check_rw()?;
        let p = self.path_of(ino)?;
        let e = self.meta(&p)?;
        if e.kind == FileKind::Dir {
            return Err(FsError::IsDir);
        }
        if data.is_empty() {
            return Ok(0);
        }
        let end = offset + data.len() as u64;
        let first = offset / CHUNK;
        let last = (end - 1) / CHUNK;
        for _attempt in 0..8 {
            // 1) Without the lock: fetch the remote base of partially-overwritten chunks that
            //    are not staged yet (the only part that may touch the network).
            let epoch0 = self.sh.st.lock().unwrap().epoch;
            let mut base: HashMap<u64, Vec<u8>> = HashMap::new();
            for idx in [first, last] {
                if base.contains_key(&idx) {
                    continue;
                }
                let cs = idx * CHUNK;
                let full = offset <= cs && end >= cs + CHUNK;
                let staged = self.sh.st.lock().unwrap().dirty.get(&ino).map(|d| d.chunks.contains_key(&idx)).unwrap_or(false);
                if !full && !staged {
                    base.insert(idx, self.read_chunk(ino, &p, &e, idx)?);
                }
            }
            // 2) With the lock held once: apply the whole write atomically w.r.t. the uploader.
            let mut st = self.sh.st.lock().unwrap();
            if st.epoch != epoch0 {
                continue; // the server copy moved while we fetched: refetch
            }
            let fallback = e.size;
            for idx in first..=last {
                let cs = idx * CHUNK;
                let existing = st.dirty.get(&ino).and_then(|d| d.chunks.get(&idx).copied());
                let mut c = match existing {
                    Some(off) => {
                        let mut b = vec![0u8; CHUNK as usize];
                        st.stage.read_exact_at(&mut b, off)?;
                        b
                    }
                    None => base.remove(&idx).unwrap_or_else(|| vec![0u8; CHUNK as usize]),
                };
                let s0 = offset.max(cs);
                let e0 = end.min(cs + CHUNK);
                c[(s0 - cs) as usize..(e0 - cs) as usize].copy_from_slice(&data[(s0 - offset) as usize..(e0 - offset) as usize]);
                let off = match existing {
                    Some(o) => o,
                    None => {
                        let o = st.stage_len;
                        st.stage_len += CHUNK;
                        o
                    }
                };
                st.stage.write_all_at(&c, off)?;
                let d = Self::dirty_entry(&mut st, ino, &p, fallback);
                d.chunks.insert(idx, off);
            }
            let d = Self::dirty_entry(&mut st, ino, &p, fallback);
            if end > d.size {
                d.size = end;
            }
            d.mtime = Some(Timestamp::now());
            let sz = d.size;
            if let Some(m) = st.meta.get_mut(&p) {
                m.e.size = sz;
            }
            drop(st);
            self.sh.kick();
            return Ok(data.len() as u32);
        }
        Err(FsError::Io(crate::i18n::t("server troppo occupato, riprova", "server too busy, try again")))
    }

    fn readdir(&mut self, dir: Ino) -> FsResult<Vec<DirEntry>> {
        let p = self.path_of(dir)?;
        if self.meta(&p)?.kind != FileKind::Dir {
            return Err(FsError::NotDir);
        }
        let names = self.list(&p)?;
        let mut st = self.sh.st.lock().unwrap();
        let mut out = vec![];
        for n in names {
            let cp = join(&p, &n);
            let kind = st.meta.get(&cp).map(|m| m.e.kind).unwrap_or(FileKind::File);
            let ino = Self::ino_for(&mut st, &cp);
            out.push(DirEntry { name: n, ino, kind });
        }
        Ok(out)
    }

    fn create(&mut self, parent: Ino, name: &str, perm: u16, _uid: u32, _gid: u32) -> FsResult<Ino> {
        self.check_rw()?;
        check_name(name, 255)?;
        let pp = self.path_of(parent)?;
        if self.lookup(parent, name).is_ok() {
            return Err(FsError::Exists);
        }
        let e = RemoteEntry { name: name.into(), kind: FileKind::File, size: 0, mtime: Timestamp::now(), perm, symlink_target: None };
        let ino = self.local_add(&pp, name, e);
        {
            let mut st = self.sh.st.lock().unwrap();
            let d = Self::dirty_entry(&mut st, ino, &join(&pp, name), 0);
            d.base_limit = 0;
            Self::push_op(&mut st, Op::Create { path: join(&pp, name), perm });
        }
        self.queued();
        Ok(ino)
    }

    fn mkdir(&mut self, parent: Ino, name: &str, perm: u16, _uid: u32, _gid: u32) -> FsResult<Ino> {
        self.check_rw()?;
        check_name(name, 255)?;
        let pp = self.path_of(parent)?;
        if self.lookup(parent, name).is_ok() {
            return Err(FsError::Exists);
        }
        let e = RemoteEntry { name: name.into(), kind: FileKind::Dir, size: 0, mtime: Timestamp::now(), perm, symlink_target: None };
        let ino = self.local_add(&pp, name, e);
        self.sh.st.lock().unwrap().listed.insert(join(&pp, name), (vec![], Instant::now()));
        self.enqueue(Op::Mkdir { path: join(&pp, name), perm });
        Ok(ino)
    }

    fn symlink(&mut self, parent: Ino, name: &str, target: &str, _uid: u32, _gid: u32) -> FsResult<Ino> {
        self.check_rw()?;
        check_name(name, 255)?;
        let pp = self.path_of(parent)?;
        if self.lookup(parent, name).is_ok() {
            return Err(FsError::Exists);
        }
        let e = RemoteEntry {
            name: name.into(),
            kind: FileKind::Symlink,
            size: target.len() as u64,
            mtime: Timestamp::now(),
            perm: 0o777,
            symlink_target: Some(target.into()),
        };
        let ino = self.local_add(&pp, name, e);
        self.enqueue(Op::Symlink { path: join(&pp, name), target: target.into() });
        Ok(ino)
    }

    fn readlink(&mut self, ino: Ino) -> FsResult<String> {
        let p = self.path_of(ino)?;
        let e = self.meta(&p)?;
        if let Some(t) = e.symlink_target {
            return Ok(t);
        }
        if e.kind != FileKind::Symlink {
            return Err(FsError::Invalid("not a symlink".into()));
        }
        let t = self.sh.remote(|b| b.readlink(&p)).map_err(map_err)?;
        if let Some(m) = self.sh.st.lock().unwrap().meta.get_mut(&p) {
            m.e.symlink_target = Some(t.clone());
        }
        Ok(t)
    }

    fn unlink(&mut self, parent: Ino, name: &str) -> FsResult<()> {
        self.check_rw()?;
        let pp = self.path_of(parent)?;
        let p = join(&pp, name);
        let e = self.meta(&p)?;
        if e.kind == FileKind::Dir {
            return Err(FsError::IsDir);
        }
        self.local_remove(&p);
        self.enqueue(Op::Remove { path: p });
        Ok(())
    }

    fn rmdir(&mut self, parent: Ino, name: &str) -> FsResult<()> {
        self.check_rw()?;
        let pp = self.path_of(parent)?;
        let p = join(&pp, name);
        if self.meta(&p)?.kind != FileKind::Dir {
            return Err(FsError::NotDir);
        }
        if !self.list(&p)?.is_empty() {
            return Err(FsError::NotEmpty);
        }
        self.local_remove(&p);
        self.enqueue(Op::Rmdir { path: p });
        Ok(())
    }

    fn rename(&mut self, fp: Ino, fname: &str, tp: Ino, tname: &str) -> FsResult<()> {
        self.check_rw()?;
        check_name(tname, 255)?;
        let from = join(&self.path_of(fp)?, fname);
        let to_parent = self.path_of(tp)?;
        let to = join(&to_parent, tname);
        if from == to {
            return Ok(());
        }
        let e = self.meta(&from)?;
        if e.kind == FileKind::Dir && (to.starts_with(&format!("{from}/"))) {
            return Err(FsError::Invalid("cannot move a directory inside itself".into()));
        }
        let replaces = match self.meta(&to) {
            Ok(d) => {
                match (e.kind == FileKind::Dir, d.kind == FileKind::Dir) {
                    (true, false) => return Err(FsError::NotDir),
                    (false, true) => return Err(FsError::IsDir),
                    (true, true) if !self.list(&to)?.is_empty() => return Err(FsError::NotEmpty),
                    _ => {}
                }
                true
            }
            Err(_) => false,
        };
        let mut st = self.sh.st.lock().unwrap();
        if replaces {
            if let Some(ino) = st.inos.remove(&to) {
                st.paths.remove(&ino);
                st.dirty.remove(&ino);
            }
            st.meta.remove(&to);
        }
        // Move the subtree's path ↔ ino mappings and cached metadata.
        let prefix = format!("{from}/");
        let moved: Vec<(String, Ino)> = st.inos.iter().filter(|(p, _)| **p == from || p.starts_with(&prefix)).map(|(p, i)| (p.clone(), *i)).collect();
        for (old, ino) in moved {
            let new = format!("{to}{}", &old[from.len()..]);
            st.inos.remove(&old);
            st.inos.insert(new.clone(), ino);
            st.paths.insert(ino, new);
        }
        let metas: Vec<(String, Meta)> = st.meta.iter().filter(|(p, _)| **p == from || p.starts_with(&prefix)).map(|(p, m)| (p.clone(), m.clone())).collect();
        for (old, mut m) in metas {
            st.meta.remove(&old);
            let new = format!("{to}{}", &old[from.len()..]);
            if new == to {
                m.e.name = tname.into();
            }
            st.meta.insert(new, m);
        }
        let lists: Vec<(String, (Vec<String>, Instant))> = st.listed.iter().filter(|(p, _)| **p == from || p.starts_with(&prefix)).map(|(p, l)| (p.clone(), l.clone())).collect();
        for (old, l) in lists {
            st.listed.remove(&old);
            st.listed.insert(format!("{to}{}", &old[from.len()..]), l);
        }
        let fparent = parent_of(&from).to_string();
        if let Some((names, _)) = st.listed.get_mut(&fparent) {
            names.retain(|n| n != fname);
        }
        if let Some((names, _)) = st.listed.get_mut(&to_parent) {
            if !names.iter().any(|n| n == tname) {
                names.push(tname.into());
            }
        }
        st.db_dirty = true;
        Self::push_op(&mut st, Op::Rename { from, to });
        drop(st);
        self.queued();
        Ok(())
    }

    fn sync(&mut self) -> FsResult<()> {
        self.persist_meta();
        self.sh.persist_queue()?;
        self.sh.kick();
        Ok(())
    }

    fn dirty_state(&mut self) -> DirtyState {
        DirtyState::Clean
    }

    fn repair(&mut self) -> FsResult<String> {
        Err(FsError::Unsupported("repair on a remote volume".into()))
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    /// In-memory remote with a "stall" switch that simulates a dead network.
    #[derive(Default)]
    pub struct MemBackend {
        pub files: Mutex<BTreeMap<String, (FileKind, Vec<u8>, u16, Option<String>)>>,
        pub stalled: AtomicBool,
        pub timeout: Mutex<Duration>,
        pub calls: AtomicU64,
        pub mtimes: Mutex<HashMap<String, Timestamp>>,
        /// Hook: the next `rename` applies the change, reports it on `.0` and then holds the
        /// caller (the uploader, with the op still queued) until `.1` fires.
        pub park_rename: Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>,
        /// Hook: the next `read` fires this first (releasing a parked rename).
        pub release_on_read: Mutex<Option<mpsc::Sender<()>>>,
    }

    impl MemBackend {
        pub fn new() -> Arc<Self> {
            let b = MemBackend::default();
            b.files.lock().unwrap().insert("/".into(), (FileKind::Dir, vec![], 0o755, None));
            Arc::new(b)
        }
        fn gate(&self) -> RResult<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.stalled.load(Ordering::SeqCst) {
                std::thread::sleep(*self.timeout.lock().unwrap());
                return Err(RemoteError::Timeout);
            }
            Ok(())
        }
        fn ent(&self, path: &str, v: &(FileKind, Vec<u8>, u16, Option<String>)) -> RemoteEntry {
            RemoteEntry {
                name: path.rsplit('/').next().unwrap_or("").into(),
                kind: v.0,
                size: v.3.as_ref().map(|t| t.len() as u64).unwrap_or(v.1.len() as u64),
                mtime: self.mtimes.lock().unwrap().get(path).copied().unwrap_or(Timestamp { secs: 1_700_000_000, nsecs: 0 }),
                perm: v.2,
                symlink_target: v.3.clone(),
            }
        }
    }

    impl RemoteBackend for MemBackend {
        fn describe(&self) -> String {
            "mem://test".into()
        }
        fn set_timeout(&self, t: Duration) {
            *self.timeout.lock().unwrap() = t;
        }
        fn ping(&self) -> RResult<Duration> {
            self.gate()?;
            Ok(Duration::from_millis(1))
        }
        fn stat(&self, path: &str) -> RResult<RemoteEntry> {
            self.gate()?;
            self.files.lock().unwrap().get(path).map(|v| self.ent(path, v)).ok_or(RemoteError::NotFound)
        }
        fn list(&self, path: &str) -> RResult<Vec<RemoteEntry>> {
            self.gate()?;
            let f = self.files.lock().unwrap();
            if !f.contains_key(path) {
                return Err(RemoteError::NotFound);
            }
            Ok(f.iter().filter(|(p, _)| *p != "/" && parent_of(p) == path && p.as_str() != path).map(|(p, v)| self.ent(p, v)).collect())
        }
        fn read(&self, path: &str, offset: u64, len: u32) -> RResult<Vec<u8>> {
            self.gate()?;
            if let Some(go) = self.release_on_read.lock().unwrap().take() {
                let _ = go.send(());
            }
            let f = self.files.lock().unwrap();
            let v = &f.get(path).ok_or(RemoteError::NotFound)?.1;
            let s = (offset as usize).min(v.len());
            let e = (s + len as usize).min(v.len());
            Ok(v[s..e].to_vec())
        }
        fn supports_partial_write(&self) -> bool {
            true
        }
        fn write(&self, path: &str, offset: u64, data: &[u8]) -> RResult<()> {
            self.gate()?;
            let mut f = self.files.lock().unwrap();
            let v = &mut f.get_mut(path).ok_or(RemoteError::NotFound)?.1;
            let end = offset as usize + data.len();
            if v.len() < end {
                v.resize(end, 0);
            }
            v[offset as usize..end].copy_from_slice(data);
            Ok(())
        }
        fn put_all(&self, path: &str, data: &[u8]) -> RResult<()> {
            self.gate()?;
            self.files.lock().unwrap().insert(path.into(), (FileKind::File, data.to_vec(), 0o644, None));
            Ok(())
        }
        fn create(&self, path: &str, perm: u16) -> RResult<()> {
            self.gate()?;
            let mut f = self.files.lock().unwrap();
            if f.contains_key(path) {
                return Err(RemoteError::Exists);
            }
            f.insert(path.into(), (FileKind::File, vec![], perm, None));
            Ok(())
        }
        fn truncate(&self, path: &str, size: u64) -> RResult<()> {
            self.gate()?;
            self.files.lock().unwrap().get_mut(path).ok_or(RemoteError::NotFound)?.1.resize(size as usize, 0);
            Ok(())
        }
        fn mkdir(&self, path: &str, perm: u16) -> RResult<()> {
            self.gate()?;
            let mut f = self.files.lock().unwrap();
            if f.contains_key(path) {
                return Err(RemoteError::Exists);
            }
            f.insert(path.into(), (FileKind::Dir, vec![], perm, None));
            Ok(())
        }
        fn remove(&self, path: &str) -> RResult<()> {
            self.gate()?;
            self.files.lock().unwrap().remove(path).map(|_| ()).ok_or(RemoteError::NotFound)
        }
        fn rmdir(&self, path: &str) -> RResult<()> {
            self.gate()?;
            let mut f = self.files.lock().unwrap();
            if f.keys().any(|p| p.starts_with(&format!("{path}/"))) {
                return Err(RemoteError::NotEmpty);
            }
            f.remove(path).map(|_| ()).ok_or(RemoteError::NotFound)
        }
        fn rename(&self, from: &str, to: &str) -> RResult<()> {
            self.gate()?;
            let mut f = self.files.lock().unwrap();
            let moved: Vec<String> = f.keys().filter(|p| *p == from || p.starts_with(&format!("{from}/"))).cloned().collect();
            if moved.is_empty() {
                return Err(RemoteError::NotFound);
            }
            f.remove(to);
            for old in moved {
                let v = f.remove(&old).unwrap();
                f.insert(format!("{to}{}", &old[from.len()..]), v);
            }
            drop(f);
            if let Some((parked, go)) = self.park_rename.lock().unwrap().take() {
                let _ = parked.send(());
                let _ = go.recv_timeout(Duration::from_secs(20)); // (bound: never hang a failing test)
            }
            Ok(())
        }
        fn symlink(&self, path: &str, target: &str) -> RResult<()> {
            self.gate()?;
            self.files.lock().unwrap().insert(path.into(), (FileKind::Symlink, vec![], 0o777, Some(target.into())));
            Ok(())
        }
        fn readlink(&self, path: &str) -> RResult<String> {
            self.gate()?;
            self.files.lock().unwrap().get(path).and_then(|v| v.3.clone()).ok_or(RemoteError::NotFound)
        }
        fn set_perm(&self, path: &str, perm: u16) -> RResult<()> {
            self.gate()?;
            self.files.lock().unwrap().get_mut(path).ok_or(RemoteError::NotFound)?.2 = perm;
            Ok(())
        }
        fn set_mtime(&self, p: &str, t: Timestamp) -> RResult<()> {
            self.gate()?;
            self.mtimes.lock().unwrap().insert(p.into(), t);
            Ok(())
        }
        fn statfs(&self) -> RResult<Option<(u64, u64)>> {
            self.gate()?;
            Ok(Some((1 << 30, 1 << 29)))
        }
    }

    pub fn cfg(name: &str) -> RemoteConfig {
        let dir = std::env::temp_dir().join(format!("ld-remote-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        RemoteConfig {
            meta_timeout: Duration::from_millis(150),
            data_timeout: Duration::from_millis(500),
            attr_ttl: Duration::from_millis(500),
            ping_interval: Duration::from_millis(200),
            cache_dir: dir,
            uid: 501,
            gid: 20,
            label: "TEST".into(),
            read_only: false,
            queue_dir: Some(std::env::temp_dir().join(format!("ld-queue-{}-{name}", std::process::id()))),
        }
    }

    /// Until the uploader has applied everything (woken by each unit it finishes).
    pub fn wait_synced(v: &RemoteVfs) {
        let sh = v.handle();
        let st = sh.st.lock().unwrap();
        let (st, _) = sh.settled.wait_timeout_while(st, Duration::from_secs(20), |s| !s.queue.is_empty() || !s.dirty.is_empty() || s.upload != Upload::Idle).unwrap();
        assert!(st.queue.is_empty() && st.dirty.is_empty(), "queue did not drain");
    }

    /// Until the health thread has noticed the dead network.
    pub fn wait_offline(v: &RemoteVfs) {
        let t0 = Instant::now();
        while v.handle().health().online {
            assert!(t0.elapsed() < Duration::from_secs(20), "never went offline");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn remote_vfs_conformance_and_upload() {
        let be = MemBackend::new();
        let mut v = RemoteVfs::new(be.clone(), cfg("conf")).unwrap();
        crate::vfs::testutil::conformance(&mut v, true);
        wait_synced(&v);
        let f = be.files.lock().unwrap();
        let want = crate::vfs::testutil::pattern(300 * 1024, 7);
        assert!(f.get("/keep/persist.bin").map(|x| x.1 == want).unwrap_or(false), "persist.bin not uploaded correctly");
        assert_eq!(f.get("/keep/sub/hello.txt").unwrap().1, b"hello lumendisk");
        assert!(!f.contains_key("/big.bin") && !f.contains_key("/a.txt"));
    }

    /// A read of a renamed file while the uploader is applying that rename on the server: the
    /// server already has the new name, but the op is still queued (in flight).
    #[test]
    fn read_while_its_rename_is_being_applied() {
        let be = MemBackend::new();
        let mut v = RemoteVfs::new(be.clone(), cfg("inflight")).unwrap();
        let root = v.root_ino();
        let x = v.mkdir(root, "x", 0o755, 0, 0).unwrap();
        let m = v.create(x, "m.txt", 0o644, 0, 0).unwrap();
        v.write(m, 0, b"move me").unwrap();
        wait_synced(&v); // the data is on the server only now, nothing is staged locally
        let (parked_tx, parked) = mpsc::channel();
        let (go, go_rx) = mpsc::channel();
        *be.park_rename.lock().unwrap() = Some((parked_tx, go_rx));
        *be.release_on_read.lock().unwrap() = Some(go);
        v.rename(x, "m.txt", x, "n.txt").unwrap();
        parked.recv().unwrap(); // server: /x/n.txt; local queue: still Rename(/x/m.txt → /x/n.txt)
        let n = v.lookup(x, "n.txt").unwrap();
        assert_eq!(v.read(n, 0, 100).unwrap(), b"move me");
        wait_synced(&v);
        assert_eq!(be.files.lock().unwrap().get("/x/n.txt").unwrap().1, b"move me");
    }

    #[test]
    fn offline_never_blocks_and_flushes_on_reconnect() {
        let be = MemBackend::new();
        be.files.lock().unwrap().insert("/doc.txt".into(), (FileKind::File, b"remote".to_vec(), 0o644, None));
        be.files.lock().unwrap().insert("/dir".into(), (FileKind::Dir, vec![], 0o755, None));
        let mut v = RemoteVfs::new(be.clone(), cfg("offline")).unwrap();
        let root = v.root_ino();
        let d = v.lookup(root, "doc.txt").unwrap();
        assert_eq!(v.read(d, 0, 100).unwrap(), b"remote");
        let names: Vec<String> = v.readdir(root).unwrap().into_iter().map(|e| e.name).collect();
        assert!(names.contains(&"dir".to_string()));
        // network dies
        be.stalled.store(true, Ordering::SeqCst);
        wait_offline(&v); // the health ping notices
        let mut worst = Duration::ZERO;
        for _ in 0..50 {
            let t0 = Instant::now();
            v.getattr(d).unwrap();
            v.readdir(root).unwrap();
            assert_eq!(v.read(d, 0, 100).unwrap(), b"remote"); // cached chunk
            worst = worst.max(t0.elapsed());
        }
        assert!(worst < Duration::from_millis(50), "slowest offline stat+ls+read took {worst:?}");
        let mut slowest = Duration::ZERO;
        let mut timed = |f: &mut dyn FnMut()| {
            let t0 = Instant::now();
            f();
            slowest = slowest.max(t0.elapsed());
        };
        let mut n = 0;
        timed(&mut || n = v.create(root, "scritto offline.txt", 0o644, 0, 0).unwrap());
        timed(&mut || {
            v.write(n, 0, b"ciao dal treno").unwrap();
        });
        let dd = v.lookup(root, "dir").unwrap();
        timed(&mut || {
            v.mkdir(dd, "nuova", 0o755, 0, 0).unwrap();
        });
        timed(&mut || v.rename(root, "doc.txt", dd, "doc2.txt").unwrap());
        assert!(slowest < Duration::from_millis(50), "slowest offline write op took {slowest:?}");
        assert!(v.handle().pending() >= 3);
        assert!(!be.files.lock().unwrap().contains_key("/scritto offline.txt"));
        // network back
        be.stalled.store(false, Ordering::SeqCst);
        wait_synced(&v);
        let f = be.files.lock().unwrap();
        assert_eq!(f.get("/scritto offline.txt").unwrap().1, b"ciao dal treno");
        assert!(f.contains_key("/dir/nuova"));
        assert_eq!(f.get("/dir/doc2.txt").unwrap().1, b"remote");
        assert!(!f.contains_key("/doc.txt"));
    }

    /// Quit while offline with queued writes → restart → reconnect → everything syncs.
    #[test]
    fn offline_queue_survives_restart() {
        let be = MemBackend::new();
        be.files.lock().unwrap().insert("/docs".into(), (FileKind::Dir, vec![], 0o755, None));
        be.files.lock().unwrap().insert("/docs/vecchio.txt".into(), (FileKind::File, b"v1".to_vec(), 0o644, None));
        let c = cfg("restart");
        let _ = std::fs::remove_dir_all(c.queue_dir.as_ref().unwrap());
        let big = crate::vfs::testutil::pattern(700_000, 21);
        {
            let mut v = RemoteVfs::new(be.clone(), c.clone()).unwrap();
            let root = v.root_ino();
            let docs = v.lookup(root, "docs").unwrap();
            v.readdir(docs).unwrap();
            v.readdir(root).unwrap();
            let o = v.lookup(docs, "vecchio.txt").unwrap();
            assert_eq!(v.read(o, 0, 10).unwrap(), b"v1"); // opened once while online
            be.stalled.store(true, Ordering::SeqCst);
            wait_offline(&v);
            let f = v.create(docs, "nuovo offline.bin", 0o644, 0, 0).unwrap();
            v.write(f, 0, &big).unwrap();
            let o = v.lookup(docs, "vecchio.txt").unwrap();
            v.write(o, 2, b"+modificato").unwrap();
            v.mkdir(root, "Cartella", 0o755, 0, 0).unwrap();
            v.rename(docs, "vecchio.txt", docs, "rinominato.txt").unwrap();
            assert!(v.handle().pending() >= 4);
            // the app quits (engine dropped while still offline)
        }
        assert!(c.queue_dir.as_ref().unwrap().join("queue.json").exists(), "queue must be on disk");
        assert!(!be.files.lock().unwrap().contains_key("/docs/nuovo offline.bin"));
        // restart, still offline: the local view shows the pending changes
        let mut v = RemoteVfs::new(be.clone(), c.clone()).unwrap();
        assert!(v.handle().pending() >= 4);
        let root = v.root_ino();
        let docs = v.lookup(root, "docs").unwrap();
        let f = v.lookup(docs, "nuovo offline.bin").unwrap();
        assert!(v.read(f, 0, 800_000).unwrap() == big);
        // network back → automatic sync
        be.stalled.store(false, Ordering::SeqCst);
        wait_synced(&v);
        let files = be.files.lock().unwrap();
        assert!(files.get("/docs/nuovo offline.bin").unwrap().1 == big);
        assert_eq!(files.get("/docs/rinominato.txt").unwrap().1, b"v1+modificato");
        assert!(files.contains_key("/Cartella") && !files.contains_key("/docs/vecchio.txt"));
        drop(files);
        drop(v);
        assert!(!c.queue_dir.as_ref().unwrap().join("queue.json").exists(), "queue removed once synced");
    }

    #[test]
    fn mounts_offline_from_persisted_cache() {
        let be = MemBackend::new();
        be.files.lock().unwrap().insert("/a".into(), (FileKind::Dir, vec![], 0o755, None));
        let c = cfg("persist");
        {
            let mut v = RemoteVfs::new(be.clone(), c.clone()).unwrap();
            let root = v.root_ino();
            v.readdir(root).unwrap();
            v.sync().unwrap();
        }
        be.stalled.store(true, Ordering::SeqCst);
        let mut v = RemoteVfs::new(be.clone(), c).unwrap();
        let root = v.root_ino();
        let names: Vec<String> = v.readdir(root).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["a".to_string()]);
    }
}

#[cfg(test)]
mod stress {
    use super::tests::*;
    use super::*;

    /// Local changes racing the uploader: write a file and rename it at once, many times. Each
    /// rename moves the local paths and queues the op; the uploader must never see one without
    /// the other (it would upload the staged data to a name the server doesn't have yet).
    #[test]
    fn write_then_rename_while_uploading() {
        let be = MemBackend::new();
        let mut v = RemoteVfs::new(be.clone(), cfg("wrstress")).unwrap();
        // "Sync now" from the app, beside the uploader thread
        let stop = Arc::new(AtomicBool::new(false));
        let syncer = {
            let (h, stop) = (v.handle(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    h.sync_now(Duration::from_millis(5));
                }
            })
        };
        let root = v.root_ino();
        let d = v.mkdir(root, "d", 0o755, 0, 0).unwrap();
        for i in 0..600 {
            let f = v.create(d, &format!("tmp{i}"), 0o644, 0, 0).unwrap();
            v.write(f, 0, format!("data {i}").as_bytes()).unwrap();
            v.rename(d, &format!("tmp{i}"), d, &format!("f{i}")).unwrap();
            let f = v.lookup(d, &format!("f{i}")).unwrap();
            assert_eq!(v.read(f, 0, 100).unwrap(), format!("data {i}").as_bytes(), "file {i}");
        }
        wait_synced(&v);
        stop.store(true, Ordering::SeqCst);
        syncer.join().unwrap();
        let files = be.files.lock().unwrap();
        for i in 0..600 {
            assert_eq!(files.get(&format!("/d/f{i}")).map(|x| x.1.clone()), Some(format!("data {i}").into_bytes()), "file {i} on the server");
        }
        assert!(v.handle().health().last_error.is_none(), "{:?}", v.handle().health().last_error);
    }

    #[test]
    fn big_sequential_write_while_uploading() {
        for round in 0..15 {
            let be = MemBackend::new();
            let mut v = RemoteVfs::new(be.clone(), cfg(&format!("stress{round}"))).unwrap();
            let root = v.root_ino();
            let big = crate::vfs::testutil::pattern(1_234_567, 3);
            let b = v.create(root, "big.bin", 0o644, 0, 0).unwrap();
            let mut off = 0;
            for chunk in big.chunks(65536) {
                v.write(b, off as u64, chunk).unwrap();
                off += chunk.len();
                std::thread::sleep(Duration::from_micros(300));
            }
            let mut back = vec![];
            let mut o = 0u64;
            loop {
                let r = v.read(b, o, 100_000).unwrap();
                if r.is_empty() {
                    break;
                }
                o += r.len() as u64;
                back.extend(r);
            }
            if back != big {
                let first = back.iter().zip(big.iter()).position(|(a, b)| a != b);
                panic!("round {round}: len {} vs {}, first diff at {:?}", back.len(), big.len(), first);
            }
        }
    }
}
