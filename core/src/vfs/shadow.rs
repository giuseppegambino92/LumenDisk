//! Feature 2 — Shadow Mount: Copy-on-Write write-back overlay (dry-run mode).
//!
//! Every mutation is kept on the Mac: file contents in a chunked staging file, metadata and
//! the directory tree in memory, plus an ordered log of structural operations. Nothing touches
//! the physical volume until `commit()`, which replays the structural log in order and then
//! writes the final content of every modified file. `rollback()` discards everything.
//!
//! Inode identity: objects that exist on the lower volume keep their inode number; objects
//! created in the overlay get ids ≥ `SHADOW_BASE`. After a commit those ids keep resolving
//! (alias table) so open Finder windows and NFS file handles stay valid.
//!
//! ## Persistent sessions (local volumes and disk images)
//!
//! With [`ShadowOverlay::open_session`] the overlay survives crashes, force-quit and power loss
//! (on-disk layout in `shadow_store.rs`):
//!
//! * **Event sourcing.** Every mutating call appends a record to an operation journal: its
//!   arguments (lower objects by their path on the untouched lower volume, overlay objects by
//!   id), the clock value it used and the stage-file chunks it filled. Recovery reopens the
//!   lower volume and re-executes the records through the same code paths in replay mode
//!   (no re-logging, no data copies: chunk assignments point at the existing stage bytes).
//!   Overlay ids are allocated by a deterministic counter, so they come out identical.
//! * **Group commit.** Records are buffered and written as one CRC-protected frame: stage file
//!   barrier (`F_BARRIERFSYNC`) first, then the frame, then the journal `F_FULLFSYNC` (which
//!   flushes the drive cache for the stage too). The NFS server runs one before every stable
//!   reply — metadata operations, FILE_SYNC / DATA_SYNC writes, COMMIT — with the cache
//!   flushes outside the volume lock ([`ShadowOverlay::durable_begin`]); requests arriving
//!   meanwhile share the next one. UNSTABLE writes are covered by the next COMMIT or by the
//!   100 ms timer. Chunks referenced by a written frame are never overwritten in place
//!   (copy-on-write), so after a crash the recovered state is always a consistent prefix of
//!   the operations, and it includes every operation that was answered as stable.
//! * **Drift detection.** The session stores a fingerprint of the lower volume
//!   ([`FileSystem::fingerprint`]) and every record notes the attributes of the lower objects
//!   it read. If the disk was modified elsewhere, recovery refuses and only Discard remains.
//! * **Resumable commit (roll-forward).** A commit first writes its plan, then applies it in
//!   windows; after each window the lower volume is synced and a checkpoint appended. Each
//!   window is built so that re-applying it after a crash is unambiguous (no two operations on
//!   the same name or object, renames alone), and every step tolerates having already been
//!   applied. After an error or a crash, `commit()` continues from the last checkpoint —
//!   re-finding objects by path, so it also works on filesystems whose inode numbers are not
//!   stable across mounts (FAT, exFAT).
//!
//! ## Remote servers (SFTP, WebDAV, LAN shares)
//!
//! The lower volume is a [`crate::remote::cache::RemoteVfs`]; the session is keyed by the
//! server's canonical endpoint (`shadow_store::session_key`). Differences from local volumes:
//!
//! * **Paths only.** Remote inode numbers are handed out per process, so journal records and
//!   the commit plan name lower objects by path (as on FAT / exFAT), never by number.
//! * **Base fingerprints.** There is no volume-wide fingerprint. Every lower object a record
//!   reads is noted as a `Seen` fact: kind, size and mtime, plus the server's content version
//!   when it has one ([`FileSystem::version_tag`], a WebDAV ETag; SFTP v3 has none, LAN shares
//!   report mtime in nanoseconds). Directories also note a digest of their entries.
//! * **Checked against the server.** Recovery re-checks those facts against the server
//!   itself ([`FileSystem::revalidate`] drops the metadata cache); if the server can't be
//!   reached — or stops answering during the check — the session is left untouched and the
//!   mount fails, without calling it a drift. Because the server can also change while the
//!   volume is mounted, a commit re-checks every fact first; a difference marks the session as
//!   drifted (only Discard is left), exactly as recovery does.
//! * **Commit = applied by the server.** The remote layer applies changes locally and uploads
//!   them from an offline queue. Each commit checkpoint first waits for that queue to drain
//!   ([`FileSystem::commit_barrier`]); if the server is unreachable or refuses a change the
//!   commit stops with an error and stays resumable, so it never reports success while data
//!   only sits in the queue. A resumed commit drains whatever an interrupted window left in the
//!   (persistent) queue before re-applying that window.
//!
//! ## Redo semantics of a resumed commit
//!
//! A checkpoint is appended (and `F_FULLFSYNC`ed) only after the lower volume was synced, and
//! the next window starts only after it, so after a crash exactly one window can be partly or
//! wholly applied without a checkpoint: the one right after the last intact checkpoint frame
//! (a torn frame at the end of `progress.ldj` is ignored, as if never written). The resume
//! re-applies that window in *redo* mode, then continues normally. Within a window no two
//! steps touch the same name or object, renames are alone, and steps are applied in order —
//! so the state of each step's own name tells whether it ran:
//!
//! * **Create / Mkdir / Symlink**: the name exists → done, if it is what the step creates
//!   (same kind; an empty file; the same symlink target), else the commit stops with an error
//!   rather than adopt a foreign object. The object is mapped to what was found.
//! * **Unlink / Rmdir**: the name is missing → done. The parent itself missing (a directory
//!   emptied and removed in the same window) → done too: a later step of the window ran.
//! * **Rename** (also over an existing target): source missing *and* target present → done;
//!   source present → apply; both missing → error (the disk changed under the commit).
//!   Only "not found" counts as missing: an I/O error stops the resume.
//! * **SetAttr** (mode / owner): absolute values, applied again.
//! * **File content**: truncate to the part of the old content that is kept, extend, write
//!   every staged chunk, set the times — the same result however often it runs.
//! * **Object map**: rebuilt by path from the plan's starting tree advanced to the window
//!   start; an object whose path is known but missing is recorded as gone and never guessed
//!   by its old inode number (freed, maybe reused, or renumbered on FAT/exFAT).
//!
//! Hard links are not part of plans (no LINK support); pre-existing ones are addressed by
//! name like everything else.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::shadow_store::{self as store, Counts, Identity, Manifest, RecordLog};
use super::*;

pub const SHADOW_BASE: Ino = 1 << 61;
const CHUNK: u64 = 64 * 1024;
/// Group-commit interval of the operation journal.
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(100);
const FLUSH_RECORDS: usize = 2048;
const MANIFEST_INTERVAL: Duration = Duration::from_secs(1);
/// Max structural operations between two commit checkpoints.
const WINDOW_OPS: usize = 512;
const CONTENT_FILES_PER_CKPT: usize = 64;
const CONTENT_BYTES_PER_CKPT: u64 = 64 << 20;

#[derive(Clone, Debug)]
struct FileData {
    size: u64,
    /// Bytes [0, base_limit) not present in `chunks` come from the lower file.
    base_limit: u64,
    /// chunk index → offset in the staging file
    chunks: BTreeMap<u64, u64>,
}

#[derive(Clone, Debug)]
struct SNode {
    attr: Attr,
    lower: Option<Ino>,
    data: Option<FileData>,
    children: Option<BTreeMap<String, (Ino, FileKind)>>,
    target: Option<String>,
    parent: Ino,
    name: String,
    alive: bool,
    attr_dirty: bool,
}

/// Structural operation, in the order the commit has to apply it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
enum Op {
    Create { parent: Ino, name: String, ino: Ino, perm: u16, uid: u32, gid: u32 },
    Mkdir { parent: Ino, name: String, ino: Ino, perm: u16, uid: u32, gid: u32 },
    Symlink { parent: Ino, name: String, target: String, ino: Ino, uid: u32, gid: u32 },
    SetAttr { ino: Ino, set: SetAttr },
    Unlink { parent: Ino, name: String, ino: Ino },
    Rmdir { parent: Ino, name: String, ino: Ino },
    Rename { fp: Ino, fname: String, tp: Ino, tname: String, ino: Ino, over: Option<Ino> },
}

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
pub struct ShadowChange {
    pub path: String,
    pub kind: String,
    pub is_dir: bool,
    pub old_size: u64,
    pub new_size: u64,
    pub delta: i64,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct ShadowSummary {
    pub added: u64,
    pub modified: u64,
    pub deleted: u64,
    pub pending_bytes: i64,
    /// Changes are journaled on disk and survive a crash / restart.
    pub persistent: bool,
    pub session_key: Option<String>,
    /// The changes were restored from a previous run when the volume was mounted.
    pub restored: bool,
    /// "pending" or "committing" (a commit started and has not finished).
    pub state: &'static str,
    pub commit_applied: u64,
    pub commit_total: u64,
    pub commit_error: Option<String>,
    /// The source was changed elsewhere since the session started (found while mounted, by
    /// the check a commit makes on remote servers): only Discard is possible.
    pub drift: Option<String>,
}

// ───────────────────────────── journal records ─────────────────────────────

/// Reference to an object in a journal record.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
enum JRef {
    /// Overlay object (id ≥ SHADOW_BASE, deterministic across replays).
    O(Ino),
    /// Lower object, by its path on the (unchanged) lower volume.
    P(String),
    /// Lower object by inode number (filesystems with stable inode numbers only).
    I(Ino),
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
enum JOp {
    Create { p: JRef, n: String, perm: u16, uid: u32, gid: u32 },
    Mkdir { p: JRef, n: String, perm: u16, uid: u32, gid: u32 },
    Symlink { p: JRef, n: String, target: String, uid: u32, gid: u32 },
    Unlink { p: JRef, n: String },
    Rmdir { p: JRef, n: String },
    Rename { fp: JRef, fname: String, tp: JRef, tname: String },
    SetAttr { i: JRef, set: SetAttr },
    Write { i: JRef, off: u64, len: u64 },
}

/// What a record read from the lower volume (drift detection on replay). On remote servers
/// this is the base fingerprint of every object the session touched.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
enum Seen {
    Attr {
        r: JRef,
        kind: FileKind,
        size: u64,
        mtime: Timestamp,
        /// Content version from the source ([`FileSystem::version_tag`], a WebDAV ETag).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tag: Option<String>,
    },
    Dir { r: JRef, digest: String },
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct JRec {
    op: JOp,
    now: Timestamp,
    ok: bool,
    /// Stage chunks assigned by the operation, in order: (chunk index, stage offset).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    a: Vec<(u64, u64)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    seen: Vec<Seen>,
}

#[derive(serde::Serialize, serde::Deserialize)]
enum Frame {
    Header { v: u32, next: u64 },
    Batch(Vec<JRec>),
}

#[derive(Default)]
struct RecCtx {
    assign: Vec<(u64, u64)>,
    seen: Vec<Seen>,
}

// ───────────────────────────── commit plan ─────────────────────────────

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct FileItem {
    ino: Ino,
    size: u64,
    base_limit: u64,
    chunks: Vec<(u64, u64)>,
    atime: Timestamp,
    mtime: Timestamp,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Plan {
    format: u32,
    root: Ino,
    ops: Vec<Op>,
    files: Vec<FileItem>,
    /// Number of user-visible changes (for the "applied" count).
    changes: u64,
    /// Lower parent / name of every lower object the plan references (and their ancestors)
    /// when the commit started: a resume re-finds objects by path after a remount.
    tree: Vec<(Ino, Ino, String)>,
}

impl Plan {
    fn total(&self) -> usize {
        self.ops.len() + self.files.len()
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Checkpoint {
    step: u64,
}

struct CommitRun {
    plan: Plan,
    /// Steps durably applied (structural operations first, then files).
    done: usize,
    /// plan object → lower inode, valid for the current mount.
    map: HashMap<Ino, Ino>,
    /// Plan objects a resume looked for by path and didn't find (see `resolve_at`).
    gone: HashSet<Ino>,
    /// Re-find objects and re-apply the last window idempotently before continuing.
    stale: bool,
    error: Option<String>,
    progress: Option<RecordLog>,
    /// The in-memory overlay describes this plan (false when resumed from disk).
    overlay_valid: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    N(Ino, String),
    I(Ino),
}

fn op_keys(op: &Op) -> Vec<Key> {
    let n = |p: &Ino, s: &str| Key::N(*p, s.to_lowercase());
    match op {
        Op::Create { parent, name, ino, .. } | Op::Mkdir { parent, name, ino, .. } | Op::Symlink { parent, name, ino, .. } => {
            vec![n(parent, name), Key::I(*ino)]
        }
        Op::Unlink { parent, name, ino } | Op::Rmdir { parent, name, ino } => vec![n(parent, name), Key::I(*ino)],
        Op::Rename { fp, fname, tp, tname, ino, over } => {
            let mut v = vec![n(fp, fname), n(tp, tname), Key::I(*ino)];
            if let Some(o) = over {
                v.push(Key::I(*o));
            }
            v
        }
        Op::SetAttr { ino, .. } => vec![Key::I(*ino)],
    }
}

/// End (exclusive) of the commit window starting at `s`: no two operations of a window touch
/// the same name or object, renames are alone, at most WINDOW_OPS operations. Deterministic.
fn window_end(ops: &[Op], s: usize) -> usize {
    let mut keys: HashSet<Key> = HashSet::new();
    let mut k = s;
    while k < ops.len() && k - s < WINDOW_OPS {
        if matches!(ops[k], Op::Rename { .. }) {
            return if k == s { s + 1 } else { k };
        }
        let ok = op_keys(&ops[k]);
        if ok.iter().any(|x| keys.contains(x)) {
            return k;
        }
        keys.extend(ok);
        k += 1;
    }
    k
}

/// Parent/name of every object after applying `ops[..upto]` to the starting tree.
fn sim_tree(plan: &Plan, upto: usize) -> HashMap<Ino, (Ino, String)> {
    let mut t: HashMap<Ino, (Ino, String)> = plan.tree.iter().map(|(i, p, n)| (*i, (*p, n.clone()))).collect();
    for op in &plan.ops[..upto] {
        match op {
            Op::Create { parent, name, ino, .. } | Op::Mkdir { parent, name, ino, .. } | Op::Symlink { parent, name, ino, .. } => {
                t.insert(*ino, (*parent, name.clone()));
            }
            Op::Rename { tp, tname, ino, over, .. } => {
                if let Some(o) = over {
                    t.remove(o);
                }
                t.insert(*ino, (*tp, tname.clone()));
            }
            Op::Unlink { ino, .. } | Op::Rmdir { ino, .. } => {
                t.remove(ino);
            }
            Op::SetAttr { .. } => {}
        }
    }
    t
}

fn tree_path(t: &HashMap<Ino, (Ino, String)>, root: Ino, obj: Ino) -> Option<Vec<String>> {
    let mut parts = vec![];
    let mut cur = obj;
    for _ in 0..4096 {
        if cur == root {
            parts.reverse();
            return Some(parts);
        }
        let (p, n) = t.get(&cur)?;
        parts.push(n.clone());
        cur = *p;
    }
    None
}

// ───────────────────────────── session ─────────────────────────────

struct Session {
    dir: PathBuf,
    manifest: Manifest,
    /// Created at the first flush (after the manifest).
    journal: Option<RecordLog>,
    pending: Vec<JRec>,
    /// Sequence number of `pending[0]` (records are numbered from 0 in each session). Records
    /// below it are written to the journal: handed to the kernel, so they survive a crash of
    /// this process, but not necessarily a power loss.
    pending_base: u64,
    /// Records below this are durable (journal `F_FULLFSYNC`ed): they survive a power loss.
    synced: u64,
    /// Distinguishes sessions (a background flush must not complete into a newer one).
    epoch: u64,
    next_start: u64,
    last_flush: Instant,
    /// When the oldest record of `pending` was buffered.
    pending_since: Instant,
    stage_dirty: bool,
    counts_dirty: bool,
    last_manifest: Instant,
    /// Drop may delete / update the session only when armed (false while recovering).
    armed: bool,
}

impl Session {
    fn fresh(dir: PathBuf, identity: Identity, fingerprint: Option<String>, next: u64) -> Session {
        let now = Timestamp::now().secs;
        let key = dir.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Session {
            dir,
            manifest: Manifest {
                format: store::FORMAT_VERSION,
                key,
                identity,
                created: now,
                updated: now,
                state: "pending".into(),
                fingerprint,
                ..Default::default()
            },
            journal: None,
            pending: vec![],
            pending_base: 0,
            synced: 0,
            epoch: SESSION_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            next_start: next,
            last_flush: Instant::now(),
            pending_since: Instant::now(),
            stage_dirty: false,
            counts_dirty: false,
            last_manifest: Instant::now(),
            armed: true,
        }
    }
}

static SESSION_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// First half of a group commit whose expensive parts run without the volume lock (see
/// [`ShadowOverlay::durable_begin`]): order the stage chunks of the sealed records before
/// their journal frame.
pub struct FlushJob {
    stage: File,
    target: u64,
    epoch: u64,
    dirty: bool,
}

impl FlushJob {
    /// Barrier-sync the stage file (call it without holding the volume lock).
    pub fn sync_stage(&self) -> std::io::Result<()> {
        if self.dirty {
            store::barrier_sync(&self.stage)
        } else {
            Ok(())
        }
    }
}

/// Second half of a group commit: the frame is written, make it durable.
pub struct JournalJob {
    journal: File,
    target: u64,
    epoch: u64,
}

impl JournalJob {
    /// `F_FULLFSYNC` the journal (call it without holding the volume lock). This also flushes
    /// the drive cache holding the stage chunks ordered before it by [`FlushJob::sync_stage`].
    pub fn sync(&self) -> std::io::Result<()> {
        store::full_sync(&self.journal)
    }
}

/// Next step of a group commit (see [`ShadowOverlay::durable_begin`]).
pub enum Durable {
    /// Every operation answered so far is durable.
    Done,
    /// Run [`FlushJob::sync_stage`] without the lock, then [`ShadowOverlay::durable_after_stage`].
    Stage(FlushJob),
    /// Run [`JournalJob::sync`] without the lock, then [`ShadowOverlay::durable_after_journal`].
    Journal(JournalJob),
}

fn drift_message(detail: &str, remote: bool) -> String {
    if remote {
        return crate::i18n::t(
            &format!("Sul server è stato modificato qualcosa che la sessione Shadow aveva già letto: le modifiche salvate non possono più essere applicate in sicurezza e si possono solo scartare. ({detail})"),
            &format!("Something the Shadow session had already read was changed on the server: the saved changes can no longer be applied safely and can only be discarded. ({detail})"),
        );
    }
    crate::i18n::t(
        &format!("Il disco è stato modificato altrove (per esempio su Linux o Windows) dopo l'inizio della sessione Shadow: le modifiche salvate non possono più essere applicate in sicurezza e si possono solo scartare. ({detail})"),
        &format!("The disk was modified elsewhere (for example on Linux or Windows) after the Shadow session started: the saved changes can no longer be applied safely and can only be discarded. ({detail})"),
    )
}

/// The saved changes of a remote session can't be checked right now (not a drift).
fn unverifiable_message(why: &str) -> String {
    crate::i18n::t(
        &format!("Le modifiche Shadow salvate non si possono verificare sul server in questo momento ({why}): riprova quando il server è raggiungibile."),
        &format!("The saved Shadow changes can't be checked against the server right now ({why}): try again when the server is reachable."),
    )
}

fn io(e: std::io::Error) -> FsError {
    FsError::Io(e.to_string())
}

fn json_err(e: serde_json::Error) -> FsError {
    FsError::Io(format!("shadow journal: {e}"))
}

pub struct ShadowOverlay {
    pub lower: Box<dyn FileSystem>,
    nodes: HashMap<Ino, SNode>,
    deleted: HashMap<Ino, (String, u64, bool)>,
    original_path: HashMap<Ino, String>,
    known_parent: HashMap<Ino, (Ino, String)>,
    log: Vec<Op>,
    next: u64,
    stage: File,
    stage_path: PathBuf,
    stage_len: u64,
    alias: HashMap<Ino, Ino>,
    /// Lower inode numbers survive a remount (not on FAT / exFAT).
    stable_inos: bool,
    // chunk allocation
    free: Vec<u64>,
    /// (sequence number of the releasing record, stage offset)
    pending_free: Vec<(u64, u64)>,
    /// Chunks not yet referenced by a durable journal frame: may be overwritten in place.
    unsealed: HashSet<u64>,
    // recording / replay
    sess: Option<Session>,
    rec: Option<RecCtx>,
    op_now: Option<Timestamp>,
    replay_assign: Option<VecDeque<(u64, u64)>>,
    path_cache: HashMap<String, Ino>,
    restored: bool,
    commit: Option<CommitRun>,
    /// Group-commit interval (tests raise it to control when frames are written).
    flush_interval: Duration,
    /// Set when a commit found the source changed elsewhere (remote servers): the session can
    /// only be inspected and discarded.
    drift: Option<String>,
}

impl ShadowOverlay {
    /// In-memory overlay (changes are lost when it is dropped). Used for volumes without a
    /// session identity.
    pub fn new(lower: Box<dyn FileSystem>, stage_path: PathBuf) -> FsResult<Self> {
        if let Some(d) = stage_path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let stage = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&stage_path)?;
        Ok(Self::build(lower, stage, stage_path, None))
    }

    fn build(lower: Box<dyn FileSystem>, stage: File, stage_path: PathBuf, sess: Option<Session>) -> Self {
        let stable_inos = !matches!(lower.volume_info().fs_type, FsType::Fat12 | FsType::Fat16 | FsType::Fat32 | FsType::ExFat);
        ShadowOverlay {
            lower,
            nodes: HashMap::new(),
            deleted: HashMap::new(),
            original_path: HashMap::new(),
            known_parent: HashMap::new(),
            log: vec![],
            next: 1,
            stage,
            stage_path,
            stage_len: 0,
            alias: HashMap::new(),
            stable_inos,
            free: vec![],
            pending_free: vec![],
            unsealed: HashSet::new(),
            sess,
            rec: None,
            op_now: None,
            replay_assign: None,
            path_cache: HashMap::new(),
            restored: false,
            commit: None,
            flush_interval: FLUSH_INTERVAL,
            drift: None,
        }
    }

    /// Persistent overlay for the volume whose session lives in `dir`: restores the pending
    /// changes found there (or loads an interrupted commit, which the caller resumes with
    /// [`ShadowOverlay::commit`]). Fails — leaving the session untouched — if the lower volume
    /// was modified since the session started.
    pub fn open_session(lower: Box<dyn FileSystem>, dir: PathBuf, identity: Identity) -> Result<Self, String> {
        let e = |x: &dyn std::fmt::Display| crate::i18n::t(&format!("sessione Shadow: {x}"), &format!("Shadow session: {x}"));
        std::fs::create_dir_all(&dir).map_err(|x| e(&x))?;
        let stage_path = dir.join(store::STAGE);
        let manifest = Manifest::load(&dir);
        let mut lower = lower;
        let fingerprint = lower.fingerprint();
        let remote = identity.is_remote();
        let Some(mut manifest) = manifest else {
            // New session. Anything left here without a valid manifest never held a durable
            // change (the manifest is written before the first journal frame): set it aside.
            // A stage file alone is just a session that never recorded anything; a journal
            // without a manifest shouldn't exist, but is kept for inspection, not deleted.
            let journal = dir.join(store::JOURNAL);
            if journal.metadata().map(|m| m.len() > 0).unwrap_or(false) {
                let aside = dir.with_file_name(format!(".orphan-{}-{}", dir.file_name().unwrap_or_default().to_string_lossy(), Timestamp::now().secs));
                std::fs::rename(&dir, &aside).map_err(|x| e(&x))?;
                std::fs::create_dir_all(&dir).map_err(|x| e(&x))?;
            }
            let stage = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&stage_path).map_err(|x| e(&x))?;
            let sess = Session::fresh(dir, identity, fingerprint, 1);
            let mut ov = Self::build(lower, stage, stage_path, Some(sess));
            ov.stable_inos &= !remote;
            return Ok(ov);
        };
        if let Some(d) = &manifest.drift {
            return Err(drift_message(d, remote));
        }
        // Keep what the app knows about the device up to date.
        if identity.media_uuid.is_some() {
            manifest.identity.media_uuid = identity.media_uuid.clone();
        }
        manifest.identity.source_path = identity.source_path.clone();
        if remote {
            // Same endpoint (that's the key), maybe a new address / name (LAN shares).
            manifest.identity.label = identity.label.clone();
            manifest.identity.display = identity.display.clone();
        }
        let stage = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&stage_path).map_err(|x| e(&x))?;
        let mut sess = Session::fresh(dir.clone(), manifest.identity.clone(), manifest.fingerprint.clone(), 1);
        sess.manifest = manifest.clone();
        sess.armed = false;
        let mut ov = Self::build(lower, stage, stage_path, Some(sess));
        // Remote inode numbers are handed out per process: the journal names lower objects by
        // path only (as on FAT / exFAT).
        ov.stable_inos &= !remote;

        if manifest.state == "committing" {
            let plan: Plan = std::fs::read(dir.join(store::PLAN))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .ok_or_else(|| e(&crate::i18n::t("piano di applicazione illeggibile", "unreadable commit plan")))?;
            let (progress, frames) = RecordLog::open(&dir.join(store::PROGRESS)).map_err(|x| e(&x))?;
            let done = frames.iter().filter_map(|f| serde_json::from_slice::<Checkpoint>(f).ok()).map(|c| c.step as usize).max().unwrap_or(0);
            ov.commit = Some(CommitRun {
                done: done.min(plan.total()),
                plan,
                map: HashMap::new(),
                gone: HashSet::new(),
                stale: true,
                error: manifest.commit_error.clone(),
                progress: Some(progress),
                overlay_valid: false,
            });
            ov.restored = true;
            if let Some(s) = &mut ov.sess {
                s.armed = true;
            }
            return Ok(ov);
        }

        // A plan without "committing" never reached the disk: forget it.
        let _ = std::fs::remove_file(dir.join(store::PLAN));
        let _ = std::fs::remove_file(dir.join(store::PROGRESS));
        if manifest.fingerprint.is_some() && fingerprint != manifest.fingerprint {
            return Err(ov.mark_drift(crate::i18n::t("impronta del volume diversa", "volume fingerprint changed")));
        }
        // Remote servers: the recorded bases are checked against the server itself, never
        // against the local cache; if it can't be reached the session is left as it is.
        let gen = match ov.lower.revalidate() {
            None => None,
            Some(Ok(g)) => Some(g),
            Some(Err(x)) => return Err(unverifiable_message(&x.to_string())),
        };
        let (journal, frames) = RecordLog::open(&dir.join(store::JOURNAL)).map_err(|x| e(&x))?;
        let mut records = 0usize;
        for (i, f) in frames.iter().enumerate() {
            let frame: Frame = serde_json::from_slice(f).map_err(|x| e(&x))?;
            match frame {
                Frame::Header { next, .. } if i == 0 => {
                    ov.next = next;
                    if let Some(s) = &mut ov.sess {
                        s.next_start = next;
                    }
                }
                Frame::Header { .. } => return Err(e(&"unexpected header")),
                Frame::Batch(recs) => {
                    for r in recs {
                        if let Err(d) = ov.replay_record(r) {
                            if let Some(why) = ov.connection_lost(gen) {
                                return Err(unverifiable_message(&why));
                            }
                            return Err(ov.mark_drift(d));
                        }
                        records += 1;
                    }
                }
            }
        }
        if let Some(why) = ov.connection_lost(gen) {
            return Err(unverifiable_message(&why));
        }
        ov.path_cache.clear();
        ov.rebuild_free_list();
        ov.restored = records > 0;
        if let Some(s) = &mut ov.sess {
            s.journal = if frames.is_empty() { None } else { Some(journal) };
            s.armed = true;
        }
        Ok(ov)
    }

    fn mark_drift(&mut self, detail: String) -> String {
        let mut remote = false;
        if let Some(s) = &mut self.sess {
            s.armed = false;
            s.manifest.drift = Some(detail.clone());
            let _ = s.manifest.save(&s.dir, true);
            remote = s.manifest.identity.is_remote();
        }
        drift_message(&detail, remote)
    }

    /// Remote sources: why a check started at connection generation `gen` may have looked at
    /// cached data instead of the server (it was or is unreachable). None: it saw the server
    /// (or the source is local, `gen` None).
    fn connection_lost(&mut self, gen: Option<u64>) -> Option<String> {
        let g = gen?;
        match self.lower.revalidate() {
            Some(Ok(now)) if now == g => None,
            Some(Err(e)) => Some(e.to_string()),
            _ => Some(crate::i18n::t("la connessione si è interrotta durante il controllo", "the connection dropped during the check")),
        }
    }

    /// Before a commit on a remote server: re-check every base the session recorded (the
    /// `Seen` facts of the journal) against the server, which may have been changed while the
    /// volume was mounted. A difference marks the session as drifted: only Discard is left.
    fn verify_bases(&mut self) -> FsResult<()> {
        let gen = match self.lower.revalidate() {
            None => return Ok(()),
            Some(r) => Some(r?),
        };
        let Some(s) = &self.sess else { return Ok(()) };
        let frames = if s.journal.is_some() { RecordLog::read(&s.dir.join(store::JOURNAL)).map_err(io)? } else { vec![] };
        let mut seen: Vec<Seen> = vec![];
        let mut dedup: HashSet<String> = HashSet::new();
        for f in &frames {
            if let Ok(Frame::Batch(recs)) = serde_json::from_slice::<Frame>(f) {
                for x in recs.into_iter().flat_map(|r| r.seen) {
                    if dedup.insert(serde_json::to_string(&x).unwrap_or_default()) {
                        seen.push(x);
                    }
                }
            }
        }
        self.path_cache.clear();
        let bad = seen.iter().find_map(|x| self.verify_seen(x).err());
        self.path_cache.clear();
        if let Some(why) = self.connection_lost(gen) {
            return Err(FsError::Io(unverifiable_message(&why)));
        }
        if let Some(d) = bad {
            let msg = self.mark_drift(d.clone());
            self.drift = Some(d);
            return Err(FsError::Io(msg));
        }
        Ok(())
    }

    /// Persistent session key, if this overlay is journaled on disk.
    pub fn session_key(&self) -> Option<String> {
        self.sess.as_ref().map(|s| s.manifest.key.clone())
    }

    pub fn is_persistent(&self) -> bool {
        self.sess.is_some()
    }

    /// A commit started and has not finished (new writes are refused until it does or the
    /// rest is abandoned with `rollback`).
    pub fn commit_pending(&self) -> bool {
        self.commit.is_some()
    }

    fn r(&self, ino: Ino) -> Ino {
        *self.alias.get(&ino).unwrap_or(&ino)
    }

    fn now(&self) -> Timestamp {
        self.op_now.unwrap_or_else(Timestamp::now)
    }

    fn replaying(&self) -> bool {
        self.replay_assign.is_some()
    }

    fn recording(&self) -> bool {
        self.sess.is_some() && !self.replaying()
    }

    fn check_mutable(&self) -> FsResult<()> {
        if self.commit.is_some() || self.drift.is_some() {
            return Err(FsError::ReadOnly);
        }
        Ok(())
    }

    fn live_node(&self, ino: Ino) -> FsResult<Option<&SNode>> {
        match self.nodes.get(&ino) {
            Some(n) if !n.alive => Err(FsError::Stale),
            Some(n) => Ok(Some(n)),
            None if self.deleted.contains_key(&ino) || ino >= SHADOW_BASE => Err(FsError::Stale),
            None => Ok(None),
        }
    }

    fn lower_path(&self, ino: Ino) -> String {
        let root = self.lower.root_ino();
        let mut parts = vec![];
        let mut cur = ino;
        for _ in 0..512 {
            if cur == root {
                break;
            }
            if let Some(n) = self.nodes.get(&cur) {
                parts.push(n.name.clone());
                cur = n.parent;
            } else if let Some((p, n)) = self.known_parent.get(&cur) {
                parts.push(n.clone());
                cur = *p;
            } else {
                parts.push(format!("<inode {cur}>"));
                break;
            }
        }
        parts.reverse();
        format!("/{}", parts.join("/"))
    }

    /// Path of a lower object on the lower volume as it is (not as the overlay shows it).
    fn orig_path(&self, ino: Ino) -> Option<String> {
        let root = self.lower.root_ino();
        let mut parts = vec![];
        let mut cur = ino;
        for _ in 0..4096 {
            if cur == root {
                parts.reverse();
                return Some(format!("/{}", parts.join("/")));
            }
            let (p, n) = self.known_parent.get(&cur)?;
            parts.push(n.as_str());
            cur = *p;
        }
        None
    }

    fn jref(&self, ino: Ino) -> FsResult<JRef> {
        if ino >= SHADOW_BASE {
            return Ok(JRef::O(ino));
        }
        match self.orig_path(ino) {
            Some(p) => Ok(JRef::P(p)),
            None if self.stable_inos => Ok(JRef::I(ino)),
            // No durable way to name it: let the NFS client look it up again.
            None => Err(FsError::Stale),
        }
    }

    fn jref_or_ino(&self, ino: Ino) -> JRef {
        self.jref(ino).unwrap_or(JRef::I(ino))
    }

    /// Resolve a journal reference against the lower volume (replay).
    fn resolve_ref(&mut self, r: &JRef) -> FsResult<Ino> {
        let p = match r {
            JRef::O(i) | JRef::I(i) => return Ok(*i),
            JRef::P(p) => p,
        };
        if let Some(&i) = self.path_cache.get(p) {
            return Ok(i);
        }
        let mut cur = self.lower.root_ino();
        let mut acc = String::new();
        for comp in p.split('/').filter(|c| !c.is_empty()) {
            acc.push('/');
            acc.push_str(comp);
            if let Some(&i) = self.path_cache.get(&acc) {
                cur = i;
                continue;
            }
            let child = self.lower.lookup(cur, comp)?;
            self.known_parent.insert(child, (cur, comp.to_string()));
            self.path_cache.insert(acc.clone(), child);
            cur = child;
        }
        Ok(cur)
    }

    fn note_seen(&mut self, s: impl FnOnce(&Self) -> Seen) {
        if self.rec.is_some() && self.recording() {
            let v = s(self);
            self.rec.as_mut().unwrap().seen.push(v);
        }
    }

    fn ensure_node(&mut self, ino: Ino) -> FsResult<()> {
        if self.live_node(ino)?.is_some() {
            return Ok(());
        }
        let attr = self.lower.getattr(ino)?;
        let (parent, name) = self.known_parent.get(&ino).cloned().unwrap_or((self.lower.root_ino(), String::new()));
        let target = if attr.kind == FileKind::Symlink { self.lower.readlink(ino).ok() } else { None };
        let (kind, size, mtime) = (attr.kind, attr.size, attr.mtime);
        let tag = if self.rec.is_some() && self.recording() { self.lower.version_tag(ino) } else { None };
        self.nodes.insert(
            ino,
            SNode { attr, lower: Some(ino), data: None, children: None, target, parent, name, alive: true, attr_dirty: false },
        );
        self.note_seen(|s| Seen::Attr { r: s.jref_or_ino(ino), kind, size, mtime, tag });
        Ok(())
    }

    fn dir_digest(entries: &[DirEntry]) -> String {
        use sha2::Digest;
        let mut v: Vec<(String, u8)> = entries.iter().map(|e| (e.name.clone(), e.kind as u8)).collect();
        v.sort();
        let mut h = sha2::Sha256::new();
        for (n, k) in v {
            h.update(n.as_bytes());
            h.update([0, k, b'\n']);
        }
        h.finalize()[..12].iter().map(|b| format!("{b:02x}")).collect()
    }

    fn materialize_dir(&mut self, ino: Ino) -> FsResult<()> {
        self.ensure_node(ino)?;
        let n = &self.nodes[&ino];
        if n.attr.kind != FileKind::Dir {
            return Err(FsError::NotDir);
        }
        if n.children.is_none() {
            let entries = self.lower.readdir(ino)?;
            let digest = Self::dir_digest(&entries);
            let mut m = BTreeMap::new();
            for e in entries {
                self.known_parent.insert(e.ino, (ino, e.name.clone()));
                m.insert(e.name, (e.ino, e.kind));
            }
            self.nodes.get_mut(&ino).unwrap().children = Some(m);
            self.note_seen(|s| Seen::Dir { r: s.jref_or_ino(ino), digest });
        }
        Ok(())
    }

    fn copy_up(&mut self, ino: Ino) -> FsResult<()> {
        self.ensure_node(ino)?;
        let n = self.nodes.get_mut(&ino).unwrap();
        if n.attr.kind == FileKind::Dir {
            return Err(FsError::IsDir);
        }
        if n.data.is_none() {
            n.data = Some(FileData { size: n.attr.size, base_limit: if n.lower.is_some() { n.attr.size } else { 0 }, chunks: BTreeMap::new() });
        }
        Ok(())
    }

    fn read_chunk(&mut self, ino: Ino, idx: u64) -> FsResult<Vec<u8>> {
        let mut buf = vec![0u8; CHUNK as usize];
        if self.replaying() {
            return Ok(buf); // the bytes are never used: write_chunk takes the recorded chunk
        }
        let (lower, base_limit, staged) = {
            let n = &self.nodes[&ino];
            let fd = n.data.as_ref().unwrap();
            (n.lower, fd.base_limit, fd.chunks.get(&idx).copied())
        };
        if let Some(off) = staged {
            self.stage.read_exact_at(&mut buf, off)?;
            return Ok(buf);
        }
        let start = idx * CHUNK;
        if let Some(l) = lower {
            if start < base_limit {
                let want = (base_limit - start).min(CHUNK) as u32;
                let d = self.lower.read(l, start, want)?;
                buf[..d.len()].copy_from_slice(&d);
            }
        }
        Ok(buf)
    }

    fn alloc_chunk(&mut self) -> u64 {
        if let Some(o) = self.free.pop() {
            return o;
        }
        let o = self.stage_len;
        self.stage_len += CHUNK;
        o
    }

    fn release_chunk(&mut self, off: u64) {
        if self.replaying() {
            return; // the free list is rebuilt after a replay
        }
        let seq = match &self.sess {
            None => {
                self.free.push(off);
                return;
            }
            Some(s) => s.pending_base + s.pending.len() as u64,
        };
        if self.unsealed.remove(&off) {
            self.free.push(off);
        } else {
            // Still referenced by a durable frame until the record releasing it is durable.
            self.pending_free.push((seq, off));
        }
    }

    /// Store chunk `idx` of `ino`. `buf` is None in replay mode, where the recorded stage
    /// chunk is taken instead.
    fn write_chunk(&mut self, ino: Ino, idx: u64, buf: Option<&[u8]>) -> FsResult<()> {
        if let Some(q) = &mut self.replay_assign {
            let (i, off) = q.pop_front().ok_or_else(|| FsError::Io("replay: no more recorded chunks".into()))?;
            if i != idx {
                return Err(FsError::Corrupt(format!("replay: chunk {i} recorded, {idx} expected")));
            }
            self.nodes.get_mut(&ino).unwrap().data.as_mut().unwrap().chunks.insert(idx, off);
            self.stage_len = self.stage_len.max(off + CHUNK);
            return Ok(());
        }
        let buf = buf.expect("chunk data");
        let existing = self.nodes[&ino].data.as_ref().unwrap().chunks.get(&idx).copied();
        let (off, fresh) = match existing {
            Some(o) if self.sess.is_none() || self.unsealed.contains(&o) => (o, false),
            _ => (self.alloc_chunk(), true),
        };
        if let Err(e) = self.stage.write_all_at(buf, off) {
            if fresh {
                self.free.push(off);
            }
            return Err(e.into());
        }
        if fresh {
            if self.sess.is_some() {
                self.unsealed.insert(off);
            }
            if let Some(old) = self.nodes.get_mut(&ino).unwrap().data.as_mut().unwrap().chunks.insert(idx, off) {
                self.release_chunk(old);
            }
        }
        if let Some(s) = &mut self.sess {
            s.stage_dirty = true;
        }
        if let Some(r) = &mut self.rec {
            r.assign.push((idx, off));
        }
        Ok(())
    }

    fn rebuild_free_list(&mut self) {
        let used: HashSet<u64> = self.nodes.values().filter(|n| n.alive).filter_map(|n| n.data.as_ref()).flat_map(|d| d.chunks.values().copied()).collect();
        let file_len = self.stage.metadata().map(|m| m.len()).unwrap_or(0).div_ceil(CHUNK) * CHUNK;
        let max_used = used.iter().max().map(|m| m + CHUNK).unwrap_or(0);
        self.stage_len = file_len.max(max_used);
        self.free = (0..self.stage_len / CHUNK).map(|i| i * CHUNK).filter(|o| !used.contains(o)).rev().collect();
        self.pending_free.clear();
        self.unsealed.clear();
    }

    fn new_id(&mut self) -> Ino {
        let i = SHADOW_BASE + self.next;
        self.next += 1;
        i
    }

    #[allow(clippy::too_many_arguments)]
    fn add_node(&mut self, parent: Ino, name: &str, kind: FileKind, perm: u16, uid: u32, gid: u32, target: Option<String>) -> FsResult<Ino> {
        check_name(name, 255)?;
        self.materialize_dir(parent)?;
        if self.nodes[&parent].children.as_ref().unwrap().contains_key(name) {
            return Err(FsError::Exists);
        }
        let ino = self.new_id();
        let now = self.now();
        let size = target.as_ref().map(|t| t.len() as u64).unwrap_or(0);
        let attr = Attr {
            ino,
            kind,
            size,
            perm,
            nlink: if kind == FileKind::Dir { 2 } else { 1 },
            uid,
            gid,
            atime: now,
            mtime: now,
            ctime: now,
            used_bytes: 0,
        };
        let node = SNode {
            attr,
            lower: None,
            data: if kind == FileKind::File { Some(FileData { size: 0, base_limit: 0, chunks: BTreeMap::new() }) } else { None },
            children: if kind == FileKind::Dir { Some(BTreeMap::new()) } else { None },
            target,
            parent,
            name: name.to_string(),
            alive: true,
            attr_dirty: false,
        };
        self.nodes.insert(ino, node);
        let p = self.nodes.get_mut(&parent).unwrap();
        p.children.as_mut().unwrap().insert(name.to_string(), (ino, kind));
        p.attr.mtime = now;
        Ok(ino)
    }

    /// Detach (parent,name) from the tree without logging. `expect_dir`: Some(true)=rmdir semantics.
    fn detach(&mut self, parent: Ino, name: &str, expect_dir: Option<bool>) -> FsResult<Ino> {
        self.materialize_dir(parent)?;
        let (ino, kind) = *self.nodes[&parent].children.as_ref().unwrap().get(name).ok_or(FsError::NotFound)?;
        match expect_dir {
            Some(true) if kind != FileKind::Dir => return Err(FsError::NotDir),
            Some(false) if kind == FileKind::Dir => return Err(FsError::IsDir),
            _ => {}
        }
        if kind == FileKind::Dir && !self.readdir(ino)?.is_empty() {
            return Err(FsError::NotEmpty);
        }
        let path = self.lower_path(ino);
        let is_lower = self.nodes.get(&ino).map(|n| n.lower.is_some()).unwrap_or(true);
        let size = if is_lower {
            match self.nodes.get(&ino) {
                Some(n) => self.lower.getattr(n.lower.unwrap()).map(|a| a.size).unwrap_or(n.attr.size),
                None => self.lower.getattr(ino).map(|a| a.size).unwrap_or(0),
            }
        } else {
            0
        };
        let now = self.now();
        self.nodes.get_mut(&parent).unwrap().children.as_mut().unwrap().remove(name);
        self.nodes.get_mut(&parent).unwrap().attr.mtime = now;
        if is_lower {
            let orig = self.original_path.remove(&ino).unwrap_or(path);
            self.deleted.insert(ino, (orig, size, kind == FileKind::Dir));
        }
        let mut dropped = vec![];
        if let Some(n) = self.nodes.get_mut(&ino) {
            n.alive = false;
            if let Some(d) = n.data.take() {
                dropped.extend(d.chunks.into_values());
            }
        }
        for off in dropped {
            self.release_chunk(off);
        }
        Ok(ino)
    }

    // ── recording ──

    /// Compute the journal form of an operation (only when recording).
    fn jop(&self, f: impl FnOnce(&Self) -> FsResult<JOp>) -> FsResult<Option<JOp>> {
        if self.recording() {
            Ok(Some(f(self)?))
        } else {
            Ok(None)
        }
    }

    fn begin_op(&mut self) {
        if self.sess.is_some() {
            self.rec = Some(RecCtx::default());
            if !self.replaying() {
                self.op_now = Some(Timestamp::now());
            }
        }
    }

    fn end_op<T>(&mut self, op: Option<JOp>, r: FsResult<T>) -> FsResult<T> {
        let rc = self.rec.take();
        let now = if self.replaying() { self.op_now } else { self.op_now.take() };
        if let (Some(op), Some(rc), Some(now)) = (op, rc, now) {
            if r.is_ok() || !rc.assign.is_empty() {
                let s = self.sess.as_mut().unwrap();
                if s.pending.is_empty() {
                    s.pending_since = Instant::now();
                }
                s.pending.push(JRec { op, now, ok: r.is_ok(), a: rc.assign, seen: rc.seen });
                s.counts_dirty = true;
                // Safety net under the lock (the group commits normally run without it): too
                // many buffered records, or the timer hasn't run for far too long.
                let due = s.pending.len() >= FLUSH_RECORDS || s.pending_since.elapsed() >= self.flush_interval * 10;
                if due {
                    if let Err(e) = self.flush() {
                        eprintln!("lumendisk: shadow journal: {e}");
                    }
                }
            }
        }
        r
    }

    /// Re-execute one journal record (recovery). Errors mean the lower volume no longer
    /// matches what the session saw.
    fn replay_record(&mut self, rec: JRec) -> Result<(), String> {
        self.op_now = Some(rec.now);
        self.replay_assign = Some(rec.a.iter().copied().collect());
        let res = self.replay_op(&rec.op);
        let left = self.replay_assign.take().map(|q| q.len()).unwrap_or(0);
        self.op_now = None;
        self.rec = None;
        for s in &rec.seen {
            if let Err(d) = self.verify_seen(s) {
                return Err(d);
            }
        }
        match (&res, rec.ok) {
            (Ok(()), true) if left == 0 => Ok(()),
            (Err(_), false) => Ok(()),
            (Ok(()), _) => Err(format!("replay diverged on {:?}", rec.op)),
            (Err(e), _) => Err(format!("{e} ({})", Self::describe(&rec.op))),
        }
    }

    fn describe(op: &JOp) -> String {
        let p = |r: &JRef, n: &str| match r {
            JRef::P(p) => format!("{}/{n}", p.trim_end_matches('/')),
            _ => n.to_string(),
        };
        match op {
            JOp::Create { p: r, n, .. } | JOp::Mkdir { p: r, n, .. } | JOp::Symlink { p: r, n, .. } | JOp::Unlink { p: r, n } | JOp::Rmdir { p: r, n } => p(r, n),
            JOp::Rename { fp, fname, .. } => p(fp, fname),
            JOp::SetAttr { i, .. } | JOp::Write { i, .. } => p(i, ""),
        }
    }

    fn replay_op(&mut self, op: &JOp) -> FsResult<()> {
        match op {
            JOp::Create { p, n, perm, uid, gid } => {
                let p = self.resolve_ref(p)?;
                self.create(p, n, *perm, *uid, *gid).map(|_| ())
            }
            JOp::Mkdir { p, n, perm, uid, gid } => {
                let p = self.resolve_ref(p)?;
                self.mkdir(p, n, *perm, *uid, *gid).map(|_| ())
            }
            JOp::Symlink { p, n, target, uid, gid } => {
                let p = self.resolve_ref(p)?;
                self.symlink(p, n, target, *uid, *gid).map(|_| ())
            }
            JOp::Unlink { p, n } => {
                let p = self.resolve_ref(p)?;
                self.unlink(p, n)
            }
            JOp::Rmdir { p, n } => {
                let p = self.resolve_ref(p)?;
                self.rmdir(p, n)
            }
            JOp::Rename { fp, fname, tp, tname } => {
                let fp = self.resolve_ref(fp)?;
                let tp = self.resolve_ref(tp)?;
                self.rename(fp, fname, tp, tname)
            }
            JOp::SetAttr { i, set } => {
                let i = self.resolve_ref(i)?;
                self.setattr(i, set).map(|_| ())
            }
            JOp::Write { i, off, len } => {
                let i = self.resolve_ref(i)?;
                self.begin_op();
                let r = self.write_inner(i, *off, *len, None);
                self.end_op(None, r)
            }
        }
    }

    fn verify_seen(&mut self, s: &Seen) -> Result<(), String> {
        let what = |r: &JRef| match r {
            JRef::P(p) => format!("'{p}'"),
            JRef::O(i) | JRef::I(i) => format!("inode {i}"),
        };
        match s {
            Seen::Attr { r, kind, size, mtime, tag } => {
                let i = self.resolve_ref(r).map_err(|_| crate::i18n::t(&format!("{} non c'è più", what(r)), &format!("{} is gone", what(r))))?;
                let a = self.lower.getattr(i).map_err(|e| e.to_string())?;
                let tag_changed = tag.is_some() && self.lower.version_tag(i) != *tag;
                if a.kind != *kind || a.size != *size || a.mtime != *mtime || tag_changed {
                    return Err(crate::i18n::t(&format!("{} è cambiato", what(r)), &format!("{} changed", what(r))));
                }
            }
            Seen::Dir { r, digest } => {
                let i = self.resolve_ref(r).map_err(|_| crate::i18n::t(&format!("{} non c'è più", what(r)), &format!("{} is gone", what(r))))?;
                let entries = self.lower.readdir(i).map_err(|e| e.to_string())?;
                if Self::dir_digest(&entries) != *digest {
                    return Err(crate::i18n::t(&format!("il contenuto di {} è cambiato", what(r)), &format!("the content of {} changed", what(r))));
                }
            }
        }
        Ok(())
    }

    // ── durability ──
    //
    // Two levels, in this order:
    // * written: the frame is handed to the kernel (`pwrite`), so it survives SIGKILL / a crash
    //   of this process (the page cache survives); `pending_base` counts these records;
    // * synced: the journal is `F_FULLFSYNC`ed, so it survives a power loss; `synced`.
    // Stage chunks are sealed and barrier-synced before the frame that references them is
    // written, so a durable frame never points at stage bytes that didn't make it.

    /// Group commit under the caller's lock: make every buffered record durable.
    fn flush(&mut self) -> FsResult<()> {
        let Some(s) = &mut self.sess else { return Ok(()) };
        let end = s.pending_base + s.pending.len() as u64;
        if end == s.synced {
            return Ok(());
        }
        if s.stage_dirty {
            store::barrier_sync(&self.stage).map_err(io)?;
            s.stage_dirty = false;
        }
        // Everything written so far is ordered before the frame: never overwrite it in place.
        self.unsealed.clear();
        self.write_frame(end)?;
        let s = self.sess.as_mut().unwrap();
        s.journal.as_ref().unwrap().sync().map_err(io)?;
        self.mark_synced(end);
        Ok(())
    }

    /// Append the buffered records with sequence number < `target` as one frame (written, not
    /// yet synced). Their stage chunks must be sealed and ordered before it.
    fn write_frame(&mut self, target: u64) -> FsResult<()> {
        let (first, k) = match &self.sess {
            Some(s) => {
                let end = s.pending_base + s.pending.len() as u64;
                (s.journal.is_none(), target.min(end).saturating_sub(s.pending_base) as usize)
            }
            None => return Ok(()),
        };
        if k == 0 {
            return Ok(());
        }
        let counts = if first { Some(self.counts()) } else { None };
        let s = self.sess.as_mut().unwrap();
        if let Some(c) = counts {
            // The manifest goes first: a journal is only ever found next to its manifest.
            s.manifest.counts = c;
            s.last_manifest = Instant::now();
            s.manifest.updated = Timestamp::now().secs;
            s.manifest.save(&s.dir, true).map_err(io)?;
            let (mut j, frames) = RecordLog::open(&s.dir.join(store::JOURNAL)).map_err(io)?;
            if !frames.is_empty() {
                return Err(FsError::Io("shadow journal: unexpected existing journal".into()));
            }
            let h = serde_json::to_vec(&Frame::Header { v: store::FORMAT_VERSION, next: s.next_start }).map_err(json_err)?;
            j.append(&h).map_err(io)?;
            // The session itself must be durable before its first acknowledged operation:
            // the directory entries of stage.bin / journal.ldj, and of the session directory.
            store::sync_dir(&s.dir).map_err(io)?;
            if let Some(p) = s.dir.parent() {
                store::sync_dir(p).map_err(io)?;
            }
            s.journal = Some(j);
        }
        let recs: Vec<JRec> = s.pending.drain(..k).collect();
        let batch = Frame::Batch(recs);
        let payload = serde_json::to_vec(&batch).map_err(json_err);
        let Frame::Batch(recs) = batch else { unreachable!() };
        let j = s.journal.as_mut().unwrap();
        if let Err(e) = payload.and_then(|p| j.append(&p).map_err(io)) {
            s.pending.splice(0..0, recs);
            return Err(e);
        }
        s.pending_base += k as u64;
        s.last_flush = Instant::now();
        Ok(())
    }

    /// Records below `upto` are durable: the chunks they released may be reused.
    fn mark_synced(&mut self, upto: u64) {
        let Some(s) = &mut self.sess else { return };
        s.synced = s.synced.max(upto.min(s.pending_base));
        let durable = s.synced;
        let (now_free, still): (Vec<_>, Vec<_>) = self.pending_free.drain(..).partition(|(seq, _)| *seq < durable);
        self.pending_free = still;
        self.free.extend(now_free.into_iter().map(|x| x.1));
    }

    fn journal_job(&self, target: u64) -> FsResult<Durable> {
        let s = self.sess.as_ref().unwrap();
        if s.synced >= target {
            return Ok(Durable::Done);
        }
        let journal = s.journal.as_ref().ok_or_else(|| FsError::Io("shadow journal: missing".into()))?.clone_file().map_err(io)?;
        Ok(Durable::Journal(JournalJob { journal, target, epoch: s.epoch }))
    }

    /// Group commit with the expensive parts outside the volume lock. Under the lock, this
    /// seals the stage chunks of every operation answered so far. The caller then alternates
    /// (see `volume.rs`): run the returned job without the lock, call the matching
    /// `durable_after_*` with it, until [`Durable::Done`]. Operations that run in between are
    /// not covered (the next round takes them); rounds may overlap safely, since each one only
    /// writes records it sealed, after ordering their chunks.
    pub fn durable_begin(&mut self) -> FsResult<Durable> {
        let Some(s) = &mut self.sess else { return Ok(Durable::Done) };
        if !s.armed {
            return Ok(Durable::Done);
        }
        let end = s.pending_base + s.pending.len() as u64;
        if end == s.synced {
            return Ok(Durable::Done);
        }
        if s.stage_dirty && !s.pending.is_empty() {
            let stage = self.stage.try_clone().map_err(io)?;
            let job = FlushJob { stage, target: end, epoch: s.epoch, dirty: true };
            s.stage_dirty = false;
            self.unsealed.clear();
            return Ok(Durable::Stage(job));
        }
        self.unsealed.clear();
        self.write_frame(end)?;
        self.journal_job(end)
    }

    /// The stage chunks of `job` are ordered: write their records' frame (from here on they
    /// survive a crash of this process).
    pub fn durable_after_stage(&mut self, job: FlushJob, synced: std::io::Result<()>) -> FsResult<Durable> {
        let Some(s) = &mut self.sess else { return Ok(Durable::Done) };
        if s.epoch != job.epoch {
            return Ok(Durable::Done); // committed or discarded meanwhile
        }
        if let Err(e) = synced {
            s.stage_dirty = true;
            return Err(io(e));
        }
        self.write_frame(job.target)?;
        self.journal_job(job.target)
    }

    /// The journal holding `job`'s frame is synced: those records survive a power loss.
    pub fn durable_after_journal(&mut self, job: JournalJob, synced: std::io::Result<()>) -> FsResult<()> {
        match &self.sess {
            Some(s) if s.epoch == job.epoch => {}
            _ => return Ok(()),
        }
        synced.map_err(io)?;
        self.mark_synced(job.target);
        Ok(())
    }

    /// Timer flavour of [`ShadowOverlay::durable_begin`] (tests): only when records are
    /// pending and the group-commit interval has passed. Call [`FlushJob::sync_stage`] without
    /// the lock, then [`ShadowOverlay::finish_background_flush`] with it.
    pub fn begin_background_flush(&mut self) -> Option<FlushJob> {
        let s = self.sess.as_mut()?;
        if s.pending.is_empty() || !s.armed || s.last_flush.elapsed() < self.flush_interval {
            return None;
        }
        let stage = self.stage.try_clone().ok()?;
        let job = FlushJob { stage, target: s.pending_base + s.pending.len() as u64, epoch: s.epoch, dirty: s.stage_dirty };
        s.stage_dirty = false;
        self.unsealed.clear();
        Some(job)
    }

    /// Second half of [`ShadowOverlay::begin_background_flush`] (syncs the journal under the
    /// caller's lock).
    pub fn finish_background_flush(&mut self, job: FlushJob, synced: std::io::Result<()>) {
        let r = match self.durable_after_stage(job, synced) {
            Ok(Durable::Journal(j)) => {
                let r = j.sync();
                self.durable_after_journal(j, r)
            }
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        };
        if let Err(e) = r {
            eprintln!("lumendisk: shadow journal: {e}");
        }
        let _ = self.update_manifest(false);
    }

    /// Timer hook: keep the session manifest (the change counts shown for sessions that
    /// aren't mounted, e.g. after a crash) up to date. Rate-limited to once per
    /// `MANIFEST_INTERVAL`.
    pub fn refresh_manifest(&mut self) {
        let _ = self.update_manifest(false);
    }

    /// Timer hook doing a whole group commit under the caller's lock.
    pub fn tick(&mut self) {
        if let Some(job) = self.begin_background_flush() {
            let r = job.sync_stage();
            self.finish_background_flush(job, r);
        }
        let _ = self.update_manifest(false);
    }

    fn counts(&mut self) -> Counts {
        let s = self.summary_counts();
        Counts { changes: s.added + s.modified + s.deleted, added: s.added, modified: s.modified, deleted: s.deleted, pending_bytes: s.pending_bytes }
    }

    /// Refresh the manifest (counters, commit progress). Rate-limited unless `force`.
    fn update_manifest(&mut self, force: bool) -> FsResult<()> {
        let Some(s) = &self.sess else { return Ok(()) };
        if !s.armed || s.journal.is_none() && self.commit.is_none() {
            return Ok(());
        }
        if !force && (!s.counts_dirty || s.last_manifest.elapsed() < MANIFEST_INTERVAL) {
            return Ok(());
        }
        let counts = self.counts();
        let (applied, total, err) = match &self.commit {
            Some(c) => (c.done as u64, c.plan.total() as u64, c.error.clone()),
            None => (0, 0, None),
        };
        let s = self.sess.as_mut().unwrap();
        if self.commit.is_none() || self.commit.as_ref().map(|c| c.overlay_valid).unwrap_or(false) {
            s.manifest.counts = counts;
        }
        s.manifest.commit_applied = applied;
        s.manifest.commit_total = total;
        s.manifest.commit_error = err;
        s.manifest.updated = Timestamp::now().secs;
        s.manifest.save(&s.dir, true).map_err(io)?;
        s.counts_dirty = false;
        s.last_manifest = Instant::now();
        Ok(())
    }

    /// Start over with an empty session (after a commit or a discard).
    fn restart_session(&mut self) {
        let Some(old) = self.sess.take() else { return };
        if let Err(e) = store::remove_session_dir(&old.dir) {
            eprintln!("lumendisk: removing Shadow session: {e}");
        }
        let fp = self.lower.fingerprint();
        let stage_path = old.dir.join(store::STAGE);
        let opened = std::fs::create_dir_all(&old.dir)
            .and_then(|_| std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&stage_path));
        match opened {
            Ok(f) => {
                self.stage = f;
                self.stage_path = stage_path;
                let mut identity = old.manifest.identity.clone();
                identity.size = self.lower.statfs().map(|s| s.total_bytes).unwrap_or(identity.size);
                let mut s = Session::fresh(old.dir, identity, fp, self.next);
                s.manifest.key = old.manifest.key;
                self.sess = Some(s);
            }
            Err(e) => {
                // Keep working in memory rather than failing the volume.
                eprintln!("lumendisk: new Shadow session: {e}");
                if let Ok(f) = tempfile_stage() {
                    self.stage = f.0;
                    self.stage_path = f.1;
                }
            }
        }
    }

    // ── public API for the UI ──

    pub fn changes(&mut self) -> Vec<ShadowChange> {
        let mut out = vec![];
        let ids: Vec<Ino> = self.nodes.keys().copied().collect();
        for ino in ids {
            let n = self.nodes[&ino].clone();
            if !n.alive || ino == self.lower.root_ino() {
                continue;
            }
            let path = self.lower_path(ino);
            let is_dir = n.attr.kind == FileKind::Dir;
            let new_size = if is_dir { 0 } else { n.data.as_ref().map(|d| d.size).unwrap_or(n.attr.size) };
            match n.lower {
                None => out.push(ShadowChange { path, kind: "added".into(), is_dir, old_size: 0, new_size, delta: new_size as i64 }),
                Some(l) => {
                    let old_size = if is_dir { 0 } else { self.lower.getattr(l).map(|a| a.size).unwrap_or(0) };
                    if let Some(orig) = self.original_path.get(&ino).cloned().filter(|o| *o != path) {
                        out.push(ShadowChange { path: orig, kind: "deleted".into(), is_dir, old_size, new_size: 0, delta: -(old_size as i64) });
                        out.push(ShadowChange { path, kind: "added".into(), is_dir, old_size: 0, new_size, delta: new_size as i64 });
                    } else if n.data.as_ref().map(|d| !d.chunks.is_empty() || d.size != old_size || d.base_limit < old_size).unwrap_or(false)
                        || n.attr_dirty
                    {
                        out.push(ShadowChange { path, kind: "modified".into(), is_dir, old_size, new_size, delta: new_size as i64 - old_size as i64 });
                    }
                }
            }
        }
        for (path, size, is_dir) in self.deleted.values() {
            out.push(ShadowChange { path: path.clone(), kind: "deleted".into(), is_dir: *is_dir, old_size: *size, new_size: 0, delta: -(*size as i64) });
        }
        out.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));
        out
    }

    fn summary_counts(&mut self) -> ShadowSummary {
        let mut s = ShadowSummary::default();
        for c in self.changes() {
            match c.kind.as_str() {
                "added" => s.added += 1,
                "modified" => s.modified += 1,
                _ => s.deleted += 1,
            }
            s.pending_bytes += c.delta;
        }
        s
    }

    pub fn summary(&mut self) -> ShadowSummary {
        let mut s = self.summary_counts();
        s.persistent = self.sess.is_some();
        s.session_key = self.session_key();
        s.restored = self.restored;
        s.state = if self.commit.is_some() { "committing" } else { "pending" };
        s.drift = self.drift.clone();
        if let Some(c) = &self.commit {
            s.commit_applied = c.done as u64;
            s.commit_total = c.plan.total() as u64;
            s.commit_error = c.error.clone();
        }
        s
    }

    pub fn has_changes(&self) -> bool {
        self.commit.is_some()
            || !self.log.is_empty()
            || self.nodes.values().any(|n| n.data.as_ref().map(|d| !d.chunks.is_empty() || d.base_limit < d.size || n.lower.is_some() && d.base_limit != d.size).unwrap_or(false) || n.attr_dirty)
    }

    fn clear(&mut self) {
        self.nodes.clear();
        self.deleted.clear();
        self.original_path.clear();
        self.log.clear();
        self.stage_len = 0;
        self.free.clear();
        self.pending_free.clear();
        self.unsealed.clear();
        if let Some(s) = &mut self.sess {
            s.pending.clear();
        }
        let _ = self.stage.set_len(0);
    }

    /// Discard every pending change. Returns the number of discarded changes. While a commit
    /// is interrupted this abandons the steps not applied yet (applied ones stay on the disk)
    /// and returns how many were abandoned.
    pub fn rollback(&mut self) -> u64 {
        let n = match self.commit.take() {
            Some(run) => {
                // The lower volume changed under the overlay's feet: forget what we knew.
                self.known_parent.clear();
                (run.plan.total() - run.done) as u64
            }
            None => self.changes().len() as u64,
        };
        // Overlay-only ids become stale; that's what a rollback means.
        self.clear();
        self.restored = false;
        self.drift = None;
        self.restart_session();
        n
    }

    // ── commit ──

    fn build_plan(&mut self, changes: u64) -> FsResult<Plan> {
        let root = self.lower.root_ino();
        let mut ids: Vec<Ino> = self.nodes.keys().copied().collect();
        ids.sort_unstable();
        let mut files = vec![];
        for ino in ids {
            let n = &self.nodes[&ino];
            if !n.alive {
                continue;
            }
            let Some(fd) = &n.data else { continue };
            if let Some(l) = n.lower {
                let cur = self.lower.getattr(l)?.size;
                if fd.chunks.is_empty() && fd.size == cur && fd.base_limit >= cur {
                    continue;
                }
            }
            files.push(FileItem {
                ino,
                size: fd.size,
                base_limit: if n.lower.is_some() { fd.base_limit } else { 0 },
                chunks: fd.chunks.iter().map(|(k, v)| (*k, *v)).collect(),
                atime: n.attr.atime,
                mtime: n.attr.mtime,
            });
        }
        // Lower objects referenced by the plan, with their ancestors.
        let mut refs: Vec<Ino> = vec![];
        for op in &self.log {
            match op {
                Op::Create { parent, ino, .. } | Op::Mkdir { parent, ino, .. } | Op::Symlink { parent, ino, .. } | Op::Unlink { parent, ino, .. } | Op::Rmdir { parent, ino, .. } => {
                    refs.extend([*parent, *ino])
                }
                Op::Rename { fp, tp, ino, over, .. } => refs.extend([*fp, *tp, *ino].into_iter().chain(*over)),
                Op::SetAttr { ino, .. } => refs.push(*ino),
            }
        }
        refs.extend(files.iter().map(|f| f.ino));
        let mut tree: BTreeMap<Ino, (Ino, String)> = BTreeMap::new();
        for r in refs {
            let mut cur = r;
            for _ in 0..4096 {
                if cur >= SHADOW_BASE || cur == root || tree.contains_key(&cur) {
                    break;
                }
                let Some((p, n)) = self.known_parent.get(&cur) else { break };
                tree.insert(cur, (*p, n.clone()));
                cur = *p;
            }
        }
        Ok(Plan {
            format: store::FORMAT_VERSION,
            root,
            ops: self.log.clone(),
            files,
            changes,
            tree: tree.into_iter().map(|(i, (p, n))| (i, p, n)).collect(),
        })
    }

    /// Apply all pending changes to the lower volume. Returns the number of applied changes.
    ///
    /// If it fails (or the process dies) half-way, calling it again — in this process or after
    /// the volume is mounted again in Shadow mode — continues from the last checkpoint.
    pub fn commit(&mut self) -> FsResult<u64> {
        if self.commit.is_none() {
            if let Some(d) = &self.drift {
                return Err(FsError::Io(drift_message(d, true)));
            }
            self.flush()?;
            // Remote servers can change while mounted: the bases must still match.
            self.verify_bases()?;
            let changes = self.changes().len() as u64;
            let plan = self.build_plan(changes)?;
            if plan.total() == 0 {
                self.clear();
                self.restart_session();
                return Ok(0);
            }
            let mut progress = None;
            if let Some(s) = &mut self.sess {
                // Plan first, then the state change: "committing" always finds its plan.
                if s.journal.is_none() {
                    s.manifest.save(&s.dir, true).map_err(io)?;
                }
                let bytes = serde_json::to_vec(&plan).map_err(json_err)?;
                store::write_atomic(&s.dir.join(store::PLAN), &bytes, true).map_err(io)?;
                let _ = std::fs::remove_file(s.dir.join(store::PROGRESS));
                progress = Some(RecordLog::open(&s.dir.join(store::PROGRESS)).map_err(io)?.0);
                s.manifest.state = "committing".into();
                s.manifest.commit_total = plan.total() as u64;
                s.manifest.commit_applied = 0;
                s.manifest.commit_error = None;
                s.manifest.counts.changes = plan.changes;
                s.manifest.updated = Timestamp::now().secs;
                s.manifest.save(&s.dir, true).map_err(io)?;
            }
            self.commit = Some(CommitRun { plan, done: 0, map: HashMap::new(), gone: HashSet::new(), stale: false, error: None, progress, overlay_valid: true });
        }
        let mut run = self.commit.take().unwrap();
        // Resuming: a remote server first applies what an interrupted window left in its
        // offline queue, so the redo below sees the server as it really is.
        let res = if run.stale { self.lower.commit_barrier() } else { Ok(()) }.and_then(|_| self.run_commit(&mut run));
        match res {
            Ok(()) => {
                let applied = run.plan.changes;
                self.finish_commit(run);
                Ok(applied)
            }
            Err(e) => {
                run.stale = true;
                let msg = crate::i18n::t(
                    &format!("applicazione interrotta dopo {} passi su {}: {e}", run.done, run.plan.total()),
                    &format!("commit stopped after {} of {} steps: {e}", run.done, run.plan.total()),
                );
                run.error = Some(msg.clone());
                self.commit = Some(run);
                if let Some(s) = &mut self.sess {
                    s.counts_dirty = true;
                }
                let _ = self.update_manifest(true);
                Err(FsError::Io(msg))
            }
        }
    }

    /// Lower inode of plan object `x`. An object that a resume found missing from the disk
    /// (see [`ShadowOverlay::resolve_at`]) has no inode: guessing one (its old number, freed
    /// and maybe reused since; FAT/exFAT numbers that changed) could hit a different object.
    fn lo(run: &CommitRun, alias: &HashMap<Ino, Ino>, x: Ino) -> FsResult<Ino> {
        if run.gone.contains(&x) {
            return Err(FsError::Io(crate::i18n::t(
                "un oggetto del piano di applicazione non si trova più sul disco",
                "an object of the commit plan is no longer on the disk",
            )));
        }
        Ok(*run.map.get(&x).or_else(|| alias.get(&x)).unwrap_or(&x))
    }

    /// Re-find, by path, every object alive after `ops[..upto]` that the rest of the plan
    /// refers to. An object whose path is known but which isn't on the disk is recorded as
    /// gone (a redo uses that, see `apply_op`); one whose path isn't known yet (created later
    /// in the plan) keeps its mapping. I/O errors stop the resume rather than count as "gone".
    fn resolve_at(&mut self, run: &mut CommitRun, upto: usize) -> FsResult<()> {
        let t = sim_tree(&run.plan, upto);
        let root = run.plan.root;
        let mut wanted: Vec<Ino> = vec![];
        for op in &run.plan.ops[upto..] {
            match op {
                Op::Create { parent, .. } | Op::Mkdir { parent, .. } | Op::Symlink { parent, .. } => wanted.push(*parent),
                Op::Unlink { parent, ino, .. } | Op::Rmdir { parent, ino, .. } => wanted.extend([*parent, *ino]),
                Op::Rename { fp, tp, ino, .. } => wanted.extend([*fp, *tp, *ino]),
                Op::SetAttr { ino, .. } => wanted.push(*ino),
            }
        }
        wanted.extend(run.plan.files.iter().map(|f| f.ino));
        let mut cache: HashMap<Vec<String>, Option<Ino>> = HashMap::new();
        cache.insert(vec![], Some(self.lower.root_ino()));
        for obj in wanted {
            if obj == root {
                continue;
            }
            let Some(path) = tree_path(&t, root, obj) else { continue };
            let mut cur = Some(self.lower.root_ino());
            for i in 0..path.len() {
                if let Some(&c) = cache.get(&path[..=i]) {
                    cur = c;
                } else {
                    cur = match self.lower.lookup(cur.unwrap(), &path[i]) {
                        Ok(c) => Some(c),
                        Err(FsError::NotFound | FsError::NotDir | FsError::Stale) => None,
                        Err(e) => return Err(e),
                    };
                    cache.insert(path[..=i].to_vec(), cur);
                }
                if cur.is_none() {
                    break;
                }
            }
            match cur {
                Some(c) => {
                    run.map.insert(obj, c);
                    run.gone.remove(&obj);
                }
                None => {
                    run.map.remove(&obj);
                    run.gone.insert(obj);
                }
            }
        }
        Ok(())
    }
    fn checkpoint(&mut self, run: &mut CommitRun, step: usize) -> FsResult<()> {
        // Remote servers: everything up to here is applied on the server, not just queued.
        self.lower.commit_barrier()?;
        if let Some(p) = &mut run.progress {
            let b = serde_json::to_vec(&Checkpoint { step: step as u64 }).map_err(json_err)?;
            p.append(&b).map_err(io)?;
            p.sync().map_err(io)?;
            stall_for_tests();
        }
        run.done = step;
        if let Some(s) = &mut self.sess {
            if s.last_manifest.elapsed() >= MANIFEST_INTERVAL {
                s.manifest.commit_applied = step as u64;
                s.manifest.updated = Timestamp::now().secs;
                let _ = s.manifest.save(&s.dir, false);
                s.last_manifest = Instant::now();
            }
        }
        Ok(())
    }

    fn run_commit(&mut self, run: &mut CommitRun) -> FsResult<()> {
        let n_ops = run.plan.ops.len();
        let total = run.plan.total();
        let mut step = run.done;
        if run.stale {
            self.resolve_at(run, step.min(n_ops))?;
            if step < n_ops {
                // The window after the last checkpoint may be partly applied already.
                let w = window_end(&run.plan.ops, step);
                for k in step..w {
                    self.apply_op(run, k, true)?;
                }
                self.checkpoint(run, w)?;
                step = w;
                self.resolve_at(run, step.min(n_ops))?;
            }
            run.stale = false;
        }
        while step < n_ops {
            let w = window_end(&run.plan.ops, step);
            for k in step..w {
                self.apply_op(run, k, false)?;
            }
            self.checkpoint(run, w)?;
            step = w;
        }
        let (mut files, mut bytes) = (0usize, 0u64);
        while step < total {
            let i = step - n_ops;
            self.apply_file(run, i)?;
            files += 1;
            bytes += run.plan.files[i].chunks.len() as u64 * CHUNK;
            step += 1;
            if files >= CONTENT_FILES_PER_CKPT || bytes >= CONTENT_BYTES_PER_CKPT || step == total {
                self.checkpoint(run, step)?;
                files = 0;
                bytes = 0;
            }
        }
        if run.done < total || total == 0 {
            self.checkpoint(run, total)?;
        }
        Ok(())
    }

    /// Re-applied object (redo): the name exists already — accept it only if it looks like
    /// what this very operation creates.
    fn redo_existing(&mut self, parent: Ino, name: &str, kind: FileKind, target: Option<&str>) -> FsResult<Option<Ino>> {
        match self.lower.lookup(parent, name) {
            Ok(li) => {
                let a = self.lower.getattr(li)?;
                let same = a.kind == kind
                    && (kind != FileKind::File || a.size == 0)
                    && target.map(|t| self.lower.readlink(li).map(|x| x == t).unwrap_or(false)).unwrap_or(true);
                if same {
                    Ok(Some(li))
                } else {
                    Err(FsError::Io(format!("'{name}' already exists on the disk")))
                }
            }
            Err(FsError::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Apply structural step `k`. With `redo` the step may already be applied (it belongs to
    /// the window after the last checkpoint, see the module docs): each kind checks for its
    /// own effect first.
    fn apply_op(&mut self, run: &mut CommitRun, k: usize, redo: bool) -> FsResult<()> {
        let op = run.plan.ops[k].clone();
        let unlink = matches!(op, Op::Unlink { .. });
        let gone = |e: &FsError| matches!(e, FsError::NotFound | FsError::Stale | FsError::NotDir);
        match op {
            Op::Create { parent, name, ino, perm, uid, gid } => {
                let p = Self::lo(run, &self.alias, parent)?;
                if redo {
                    if let Some(li) = self.redo_existing(p, &name, FileKind::File, None)? {
                        run.map.insert(ino, li);
                        return Ok(());
                    }
                }
                let li = self.lower.create(p, &name, perm, uid, gid)?;
                run.map.insert(ino, li);
            }
            Op::Mkdir { parent, name, ino, perm, uid, gid } => {
                let p = Self::lo(run, &self.alias, parent)?;
                if redo {
                    if let Some(li) = self.redo_existing(p, &name, FileKind::Dir, None)? {
                        run.map.insert(ino, li);
                        return Ok(());
                    }
                }
                let li = self.lower.mkdir(p, &name, perm, uid, gid)?;
                run.map.insert(ino, li);
            }
            Op::Symlink { parent, name, target, ino, uid, gid } => {
                let p = Self::lo(run, &self.alias, parent)?;
                if redo {
                    if let Some(li) = self.redo_existing(p, &name, FileKind::Symlink, Some(&target))? {
                        run.map.insert(ino, li);
                        return Ok(());
                    }
                }
                let li = self.lower.symlink(p, &name, &target, uid, gid)?;
                run.map.insert(ino, li);
            }
            Op::SetAttr { ino, set } => {
                let li = Self::lo(run, &self.alias, ino)?;
                self.lower.setattr(li, &set)?;
            }
            Op::Unlink { parent, name, .. } | Op::Rmdir { parent, name, .. } => {
                // Redo with the parent gone: a later step of this window removed it (emptied
                // directory, then rmdir), and steps are applied in order, so this one is done.
                if redo && run.gone.contains(&parent) {
                    return Ok(());
                }
                let p = Self::lo(run, &self.alias, parent)?;
                let r = if unlink { self.lower.unlink(p, &name) } else { self.lower.rmdir(p, &name) };
                match r {
                    Err(e) if redo && gone(&e) => {} // already removed
                    r => r?,
                }
            }
            Op::Rename { fp, fname, tp, tname, .. } => {
                let (f, t) = (Self::lo(run, &self.alias, fp)?, Self::lo(run, &self.alias, tp)?);
                if redo {
                    // A rename is alone in its window: the source missing means it was moved
                    // (also over an existing target), but only if the target is there now.
                    match self.lower.lookup(f, &fname) {
                        Ok(_) => {}
                        Err(FsError::NotFound) => {
                            return match self.lower.lookup(t, &tname) {
                                Ok(_) => Ok(()), // already renamed
                                Err(FsError::NotFound) => Err(FsError::Io(format!("'{fname}' and '{tname}' are both missing from the disk"))),
                                Err(e) => Err(e),
                            };
                        }
                        Err(e) => return Err(e),
                    }
                }
                self.lower.rename(f, &fname, t, &tname)?;
            }
        }
        Ok(())
    }
    /// Write the final content of one file. Safe to repeat.
    fn apply_file(&mut self, run: &mut CommitRun, i: usize) -> FsResult<()> {
        let it = run.plan.files[i].clone();
        let li = Self::lo(run, &self.alias, it.ino)?;
        let cur = self.lower.getattr(li)?.size;
        if it.base_limit < cur {
            self.lower.setattr(li, &SetAttr { size: Some(it.base_limit), ..Default::default() })?;
        }
        if it.size != cur.min(it.base_limit) {
            self.lower.setattr(li, &SetAttr { size: Some(it.size), ..Default::default() })?;
        }
        let mut buf = vec![0u8; CHUNK as usize];
        for (idx, off) in it.chunks {
            let start = idx * CHUNK;
            if start >= it.size {
                continue;
            }
            let len = (it.size - start).min(CHUNK) as usize;
            self.stage.read_exact_at(&mut buf[..len], off)?;
            self.lower.write(li, start, &buf[..len])?;
        }
        self.lower.setattr(li, &SetAttr { mtime: Some(it.mtime), atime: Some(it.atime), ..Default::default() })?;
        Ok(())
    }

    fn finish_commit(&mut self, run: CommitRun) {
        let map = run.map;
        if run.overlay_valid {
            // The lower tree now matches the overlay: learn the new parent/name of everything
            // the commit touched, so later journal records name them correctly.
            let lower_of = |x: Ino| *map.get(&x).or_else(|| self.alias.get(&x)).unwrap_or(&x);
            let root = self.lower.root_ino();
            let mut updates = vec![];
            for (ino, n) in &self.nodes {
                if *ino == root {
                    continue;
                }
                if n.alive {
                    updates.push((lower_of(*ino), Some((lower_of(n.parent), n.name.clone()))));
                } else {
                    updates.push((lower_of(*ino), None));
                }
            }
            for i in self.deleted.keys() {
                updates.push((lower_of(*i), None));
            }
            for (l, v) in updates {
                match v {
                    Some(pn) => {
                        self.known_parent.insert(l, pn);
                    }
                    None => {
                        self.known_parent.remove(&l);
                    }
                }
            }
        } else {
            self.known_parent.clear();
        }
        for (o, l) in map {
            if o >= SHADOW_BASE {
                self.alias.insert(o, l);
            }
        }
        self.clear();
        self.restored = false;
        self.restart_session();
    }

    #[cfg(test)]
    fn simulate_crash(mut self) {
        if let Some(s) = &mut self.sess {
            s.armed = false;
        }
    }

    #[cfg(test)]
    fn force_flush(&mut self) {
        self.flush().unwrap();
    }

    // ── FileSystem bodies ──

    fn setattr_inner(&mut self, ino: Ino, set: &SetAttr) -> FsResult<()> {
        self.ensure_node(ino)?;
        if let Some(sz) = set.size {
            self.copy_up(ino)?;
            let (old, tail) = {
                let fd = self.nodes[&ino].data.as_ref().unwrap();
                (fd.size, sz % CHUNK != 0 && fd.chunks.contains_key(&(sz / CHUNK)))
            };
            // Zero the tail of the last partial chunk when shrinking (fallible I/O first).
            if sz < old && tail {
                let idx = sz / CHUNK;
                let mut c = self.read_chunk(ino, idx)?;
                c[(sz % CHUNK) as usize..].fill(0);
                self.write_chunk(ino, idx, Some(&c))?;
            }
            let now = self.now();
            let keep_chunks = sz.div_ceil(CHUNK);
            let n = self.nodes.get_mut(&ino).unwrap();
            let fd = n.data.as_mut().unwrap();
            if sz < fd.base_limit {
                fd.base_limit = sz;
            }
            let dropped: Vec<u64> = fd.chunks.split_off(&keep_chunks).into_values().collect();
            fd.size = sz;
            n.attr.mtime = now;
            for off in dropped {
                self.release_chunk(off);
            }
        }
        let now = self.now();
        let n = self.nodes.get_mut(&ino).unwrap();
        let meta = SetAttr { size: None, ..set.clone() };
        if let Some(p) = meta.perm {
            n.attr.perm = p;
        }
        if let Some(u) = meta.uid {
            n.attr.uid = u;
        }
        if let Some(g) = meta.gid {
            n.attr.gid = g;
        }
        if let Some(t) = meta.atime {
            n.attr.atime = t;
        }
        if let Some(t) = meta.mtime {
            n.attr.mtime = t;
        }
        n.attr.ctime = now;
        if meta.perm.is_some() || meta.uid.is_some() || meta.gid.is_some() {
            n.attr_dirty = true;
            self.log.push(Op::SetAttr { ino, set: SetAttr { perm: meta.perm, uid: meta.uid, gid: meta.gid, ..Default::default() } });
        }
        Ok(())
    }

    /// `data` is None in replay mode (chunk assignments come from the record).
    fn write_inner(&mut self, ino: Ino, offset: u64, len: u64, data: Option<&[u8]>) -> FsResult<()> {
        self.copy_up(ino)?;
        let end = offset + len;
        let mut pos = offset;
        while pos < end {
            let idx = pos / CHUNK;
            let s = (pos % CHUNK) as usize;
            let n = ((end - pos) as usize).min(CHUNK as usize - s);
            match data {
                Some(d) => {
                    let o = (pos - offset) as usize;
                    if n == CHUNK as usize {
                        self.write_chunk(ino, idx, Some(&d[o..o + n]))?;
                    } else {
                        let mut c = self.read_chunk(ino, idx)?;
                        c[s..s + n].copy_from_slice(&d[o..o + n]);
                        self.write_chunk(ino, idx, Some(&c))?;
                    }
                }
                None => self.write_chunk(ino, idx, None)?,
            }
            pos += n as u64;
        }
        let now = self.now();
        let n = self.nodes.get_mut(&ino).unwrap();
        let fd = n.data.as_mut().unwrap();
        if end > fd.size {
            fd.size = end;
        }
        n.attr.mtime = now;
        n.attr.ctime = now;
        Ok(())
    }

    fn rename_inner(&mut self, fp: Ino, fname: &str, tp: Ino, tname: &str) -> FsResult<()> {
        check_name(tname, 255)?;
        self.materialize_dir(fp)?;
        self.materialize_dir(tp)?;
        let (ino, kind) = *self.nodes[&fp].children.as_ref().unwrap().get(fname).ok_or(FsError::NotFound)?;
        if kind == FileKind::Dir {
            // refuse moving a directory below itself
            let mut cur = tp;
            for _ in 0..512 {
                if cur == ino {
                    return Err(FsError::Invalid("cannot move a directory inside itself".into()));
                }
                if cur == self.lower.root_ino() {
                    break;
                }
                cur = match self.nodes.get(&cur) {
                    Some(n) => n.parent,
                    None => match self.known_parent.get(&cur) {
                        Some((p, _)) => *p,
                        None => break,
                    },
                };
            }
        }
        // Fallible lookups before anything changes.
        self.ensure_node(ino)?;
        let mut over = None;
        if let Some(&(dino, dkind)) = self.nodes[&tp].children.as_ref().unwrap().get(tname) {
            if dino == ino {
                return Ok(());
            }
            match (kind == FileKind::Dir, dkind == FileKind::Dir) {
                (true, false) => return Err(FsError::NotDir),
                (false, true) => return Err(FsError::IsDir),
                _ => {}
            }
            over = Some(self.detach(tp, tname, Some(dkind == FileKind::Dir))?);
        }
        let lower_backed = self.nodes[&ino].lower.is_some();
        if lower_backed && !self.original_path.contains_key(&ino) {
            let p = self.lower_path(ino);
            self.original_path.insert(ino, p);
        }
        let now = self.now();
        self.nodes.get_mut(&fp).unwrap().children.as_mut().unwrap().remove(fname);
        self.nodes.get_mut(&tp).unwrap().children.as_mut().unwrap().insert(tname.to_string(), (ino, kind));
        let n = self.nodes.get_mut(&ino).unwrap();
        n.parent = tp;
        n.name = tname.to_string();
        n.attr.ctime = now;
        self.log.push(Op::Rename { fp, fname: fname.into(), tp, tname: tname.into(), ino, over });
        Ok(())
    }
}

/// Fallback stage file when the session directory can't be recreated.
fn tempfile_stage() -> std::io::Result<(File, PathBuf)> {
    let p = std::env::temp_dir().join(format!("lumendisk-shadow-{}.stage", uuid::Uuid::new_v4().simple()));
    let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&p)?;
    Ok((f, p))
}

impl Drop for ShadowOverlay {
    fn drop(&mut self) {
        match &self.sess {
            None => {
                let _ = std::fs::remove_file(&self.stage_path);
            }
            Some(s) if !s.armed => {}
            Some(_) => {
                if let Err(e) = self.flush() {
                    eprintln!("lumendisk: shadow journal: {e}");
                }
                if self.commit.is_none() && self.changes().is_empty() {
                    if let Some(s) = &self.sess {
                        let _ = store::remove_session_dir(&s.dir);
                    }
                } else {
                    let _ = self.update_manifest(true);
                }
            }
        }
    }
}

impl FileSystem for ShadowOverlay {
    fn volume_info(&self) -> VolumeInfo {
        let mut v = self.lower.volume_info();
        v.read_only = false;
        v
    }

    fn statfs(&mut self) -> FsResult<FsStat> {
        let mut s = self.lower.statfs()?;
        let pending = self.summary_counts().pending_bytes;
        s.free_bytes = (s.free_bytes as i64 - pending.max(0)).max(0) as u64;
        Ok(s)
    }

    fn root_ino(&self) -> Ino {
        self.lower.root_ino()
    }

    fn lookup(&mut self, parent: Ino, name: &str) -> FsResult<Ino> {
        let parent = self.r(parent);
        if let Some(n) = self.live_node(parent)? {
            if let Some(c) = &n.children {
                return c.get(name).map(|x| x.0).ok_or(FsError::NotFound);
            }
        }
        let i = self.lower.lookup(parent, name)?;
        self.known_parent.insert(i, (parent, name.to_string()));
        Ok(i)
    }

    fn getattr(&mut self, ino: Ino) -> FsResult<Attr> {
        let ino = self.r(ino);
        if let Some(n) = self.live_node(ino)? {
            let mut a = n.attr.clone();
            if let Some(d) = &n.data {
                a.size = d.size;
            }
            if let Some(c) = &n.children {
                a.nlink = 2 + c.values().filter(|(_, k)| *k == FileKind::Dir).count() as u32;
            }
            return Ok(a);
        }
        self.lower.getattr(ino)
    }

    fn setattr(&mut self, ino: Ino, set: &SetAttr) -> FsResult<Attr> {
        let ino = self.r(ino);
        self.check_mutable()?;
        let op = self.jop(|s| Ok(JOp::SetAttr { i: s.jref(ino)?, set: set.clone() }))?;
        self.begin_op();
        let r = self.setattr_inner(ino, set);
        self.end_op(op, r)?;
        self.getattr(ino)
    }

    fn read(&mut self, ino: Ino, offset: u64, len: u32) -> FsResult<Vec<u8>> {
        let ino = self.r(ino);
        let size = match self.live_node(ino)? {
            Some(n) if n.data.is_some() => n.data.as_ref().unwrap().size,
            Some(n) if n.attr.kind == FileKind::Dir => return Err(FsError::IsDir),
            Some(n) if n.lower.is_none() => return Ok(vec![]),
            _ => return self.lower.read(ino, offset, len),
        };
        if offset >= size {
            return Ok(vec![]);
        }
        let end = (offset + len as u64).min(size);
        let mut out = Vec::with_capacity((end - offset) as usize);
        let mut pos = offset;
        while pos < end {
            let idx = pos / CHUNK;
            let c = self.read_chunk(ino, idx)?;
            let s = (pos % CHUNK) as usize;
            let e = ((end - idx * CHUNK).min(CHUNK)) as usize;
            out.extend_from_slice(&c[s..e]);
            pos = idx * CHUNK + e as u64;
        }
        Ok(out)
    }

    fn write(&mut self, ino: Ino, offset: u64, data: &[u8]) -> FsResult<u32> {
        let ino = self.r(ino);
        self.check_mutable()?;
        let op = self.jop(|s| Ok(JOp::Write { i: s.jref(ino)?, off: offset, len: data.len() as u64 }))?;
        self.begin_op();
        let r = self.write_inner(ino, offset, data.len() as u64, Some(data));
        self.end_op(op, r)?;
        Ok(data.len() as u32)
    }

    fn readdir(&mut self, dir: Ino) -> FsResult<Vec<DirEntry>> {
        let dir = self.r(dir);
        if let Some(n) = self.live_node(dir)? {
            if n.attr.kind != FileKind::Dir {
                return Err(FsError::NotDir);
            }
            if let Some(c) = &n.children {
                return Ok(c.iter().map(|(name, (ino, kind))| DirEntry { name: name.clone(), ino: *ino, kind: *kind }).collect());
            }
        }
        let v = self.lower.readdir(dir)?;
        for e in &v {
            self.known_parent.insert(e.ino, (dir, e.name.clone()));
        }
        Ok(v)
    }

    fn create(&mut self, parent: Ino, name: &str, perm: u16, uid: u32, gid: u32) -> FsResult<Ino> {
        let parent = self.r(parent);
        self.check_mutable()?;
        let op = self.jop(|s| Ok(JOp::Create { p: s.jref(parent)?, n: name.into(), perm, uid, gid }))?;
        self.begin_op();
        let r = self.add_node(parent, name, FileKind::File, perm, uid, gid, None).inspect(|&ino| {
            self.log.push(Op::Create { parent, name: name.into(), ino, perm, uid, gid });
        });
        self.end_op(op, r)
    }

    fn mkdir(&mut self, parent: Ino, name: &str, perm: u16, uid: u32, gid: u32) -> FsResult<Ino> {
        let parent = self.r(parent);
        self.check_mutable()?;
        let op = self.jop(|s| Ok(JOp::Mkdir { p: s.jref(parent)?, n: name.into(), perm, uid, gid }))?;
        self.begin_op();
        let r = self.add_node(parent, name, FileKind::Dir, perm, uid, gid, None).inspect(|&ino| {
            self.log.push(Op::Mkdir { parent, name: name.into(), ino, perm, uid, gid });
        });
        self.end_op(op, r)
    }

    fn symlink(&mut self, parent: Ino, name: &str, target: &str, uid: u32, gid: u32) -> FsResult<Ino> {
        let parent = self.r(parent);
        self.check_mutable()?;
        let op = self.jop(|s| Ok(JOp::Symlink { p: s.jref(parent)?, n: name.into(), target: target.into(), uid, gid }))?;
        self.begin_op();
        let r = self.add_node(parent, name, FileKind::Symlink, 0o777, uid, gid, Some(target.to_string())).inspect(|&ino| {
            self.log.push(Op::Symlink { parent, name: name.into(), target: target.into(), ino, uid, gid });
        });
        self.end_op(op, r)
    }

    fn readlink(&mut self, ino: Ino) -> FsResult<String> {
        let ino = self.r(ino);
        if let Some(n) = self.live_node(ino)? {
            if let Some(t) = &n.target {
                return Ok(t.clone());
            }
        }
        self.lower.readlink(ino)
    }

    fn unlink(&mut self, parent: Ino, name: &str) -> FsResult<()> {
        let parent = self.r(parent);
        self.check_mutable()?;
        let op = self.jop(|s| Ok(JOp::Unlink { p: s.jref(parent)?, n: name.into() }))?;
        self.begin_op();
        let r = self.detach(parent, name, Some(false)).map(|ino| {
            self.log.push(Op::Unlink { parent, name: name.into(), ino });
        });
        self.end_op(op, r)
    }

    fn rmdir(&mut self, parent: Ino, name: &str) -> FsResult<()> {
        let parent = self.r(parent);
        self.check_mutable()?;
        let op = self.jop(|s| Ok(JOp::Rmdir { p: s.jref(parent)?, n: name.into() }))?;
        self.begin_op();
        let r = self.detach(parent, name, Some(true)).map(|ino| {
            self.log.push(Op::Rmdir { parent, name: name.into(), ino });
        });
        self.end_op(op, r)
    }

    fn rename(&mut self, fp: Ino, fname: &str, tp: Ino, tname: &str) -> FsResult<()> {
        let (fp, tp) = (self.r(fp), self.r(tp));
        self.check_mutable()?;
        let op = self.jop(|s| Ok(JOp::Rename { fp: s.jref(fp)?, fname: fname.into(), tp: s.jref(tp)?, tname: tname.into() }))?;
        self.begin_op();
        let r = self.rename_inner(fp, fname, tp, tname);
        self.end_op(op, r)
    }

    fn sync(&mut self) -> FsResult<()> {
        // Nothing reaches the lower volume until commit; make the journal durable.
        self.flush()?;
        self.update_manifest(false)
    }
    fn dirty_state(&mut self) -> DirtyState {
        self.lower.dirty_state()
    }
    fn repair(&mut self) -> FsResult<String> {
        Err(FsError::Unsupported("repair is not available in Shadow mode".into()))
    }
    fn fingerprint(&mut self) -> Option<String> {
        self.lower.fingerprint()
    }
}

#[cfg(test)]
#[path = "shadow_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "shadow_remote_tests.rs"]
mod remote_tests;

/// Crash tests only: `LUMENDISK_TEST_STALL_COMMIT_AFTER=N` parks the committing thread forever
/// once the N-th checkpoint of this process is durable, so a test can SIGKILL the server at a
/// known point of a commit instead of racing it. Unset in normal use (read once).
fn stall_for_tests() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static AFTER: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    static DONE: AtomicU64 = AtomicU64::new(0);
    let Some(n) = *AFTER.get_or_init(|| std::env::var("LUMENDISK_TEST_STALL_COMMIT_AFTER").ok().and_then(|v| v.parse().ok())) else {
        return;
    };
    if DONE.fetch_add(1, Ordering::SeqCst) + 1 >= n {
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
}
