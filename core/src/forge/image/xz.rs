//! XZ: the stream index (exact decompressed size without decompressing) and a bounded
//! multi-threaded block decoder.
//!
//! Format reference: "The .xz File Format", version 1.2.1 (tukaani.org/xz/xz-file-format.txt):
//! - Stream = Stream Header (12) + Blocks + Index + Stream Footer (12); streams may be
//!   concatenated, separated by Stream Padding (null bytes, a multiple of 4).
//! - Stream Footer: CRC32 (of the next 6 bytes), Backward Size (u32 LE, real size =
//!   (value + 1) * 4), Stream Flags (2), magic "YZ".
//! - Index: indicator 0x00, Number of Records (multibyte integer), records of (Unpadded Size,
//!   Uncompressed Size), Index Padding to a multiple of 4, CRC32.
//! - A Block occupies `Unpadded Size` rounded up to a multiple of 4 bytes.
//!
//! The multi-threaded reader decodes independent blocks (what `xz -T0` writes) in parallel. Each
//! block is wrapped into a minimal single-block stream (the original Stream Header, the block,
//! a one-record Index, a Stream Footer) and handed to `lzma_rust2::XzReader`, so the block's
//! check (CRC32 / CRC64 / SHA-256) and sizes are verified exactly as in a sequential decode.
//! Memory is bounded: at most `window` blocks are in flight, and the reader is only used when
//! every block is at most [`MT_MAX_BLOCK`] bytes (compressed and decompressed).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

/// Memory limit handed to the LZMA2 decoder (dictionary + buffers). `xz -9` uses a 64 MiB
/// dictionary; the format allows up to 1.5 GiB, which no real image uses.
pub(super) const MEM_LIMIT_KB: u32 = 1024 * 1024;
/// Blocks larger than this (either side) are decoded sequentially instead of in parallel.
pub(super) const MT_MAX_BLOCK: u64 = 48 << 20;
/// Largest Index we read into memory (32 MiB holds well over a million records).
const MAX_INDEX: u64 = 32 << 20;
const MAX_RECORDS: u64 = 1 << 20;
const MAX_STREAMS: usize = 100_000;
const HEADER_MAGIC: [u8; 6] = [0xFD, b'7', b'z', b'X', b'Z', 0x00];

#[derive(Clone, Debug)]
pub(super) struct XzBlock {
    /// Absolute file offset of the Block Header.
    pub off: u64,
    pub unpadded: u64,
    pub uncompressed: u64,
}

#[derive(Clone, Debug)]
pub(super) struct XzStream {
    pub header: [u8; 12],
    pub blocks: Vec<XzBlock>,
}

/// What the indexes of an `.xz` file say.
#[derive(Clone, Debug, Default)]
pub(super) struct XzLayout {
    pub streams: Vec<XzStream>,
    pub size: u64,
    pub blocks: usize,
    pub max_block: u64,
}

impl XzLayout {
    /// Whether the bounded parallel decoder applies.
    pub fn parallel(&self) -> bool {
        self.blocks >= 2 && self.max_block <= MT_MAX_BLOCK
    }
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("xz: {msg}"))
}

fn read_exact_at(f: &File, off: u64, len: u64) -> io::Result<Vec<u8>> {
    let len = usize::try_from(len).map_err(|_| bad("size"))?;
    let mut v = vec![0u8; len];
    f.read_exact_at(&mut v, off)?;
    Ok(v)
}

/// xz multibyte integer (at most 9 bytes, 63 bits).
fn varint(b: &[u8], pos: &mut usize) -> io::Result<u64> {
    let mut v = 0u64;
    for i in 0..9 {
        let byte = *b.get(*pos).ok_or_else(|| bad("index truncated"))?;
        *pos += 1;
        v |= u64::from(byte & 0x7F) << (7 * i);
        if byte & 0x80 == 0 {
            if i > 0 && byte == 0 {
                return Err(bad("non-minimal integer"));
            }
            return Ok(v);
        }
    }
    Err(bad("integer too long"))
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn round4(v: u64) -> Option<u64> {
    v.checked_add(3).map(|x| x & !3)
}

/// Parse every stream's Index, walking backwards from the end of the file. Returns None when
/// the indexes are too large to be worth reading (the image is then decoded without a size).
pub(super) fn layout(f: &File, file_size: u64) -> io::Result<Option<XzLayout>> {
    let mut end = file_size;
    let mut streams = vec![];
    loop {
        // Stream Padding: null bytes, a multiple of four (bounded scan).
        let mut scanned = 0u64;
        while end >= 4 {
            let mut w = [0u8; 4];
            f.read_exact_at(&mut w, end - 4)?;
            if w != [0; 4] {
                break;
            }
            end -= 4;
            scanned += 4;
            if scanned > (1 << 20) {
                return Ok(None);
            }
        }
        if end == 0 && !streams.is_empty() {
            break;
        }
        if end < 32 {
            return Err(bad("file too short or truncated"));
        }
        let footer = read_exact_at(f, end - 12, 12)?;
        if &footer[10..12] != b"YZ" {
            return Err(bad("stream footer missing (truncated file?)"));
        }
        if crc32fast::hash(&footer[4..10]) != u32::from_le_bytes(footer[0..4].try_into().unwrap()) {
            return Err(bad("stream footer CRC mismatch"));
        }
        let backward = (u64::from(u32::from_le_bytes(footer[4..8].try_into().unwrap())) + 1) * 4;
        if backward > MAX_INDEX {
            return Ok(None);
        }
        let index_start = (end - 12).checked_sub(backward).ok_or_else(|| bad("index outside the file"))?;
        let index = read_exact_at(f, index_start, backward)?;
        if index[0] != 0 {
            return Err(bad("index indicator"));
        }
        let crc_pos = index.len() - 4;
        if crc32fast::hash(&index[..crc_pos]) != u32::from_le_bytes(index[crc_pos..].try_into().unwrap()) {
            return Err(bad("index CRC mismatch"));
        }
        let mut pos = 1usize;
        let n = varint(&index[..crc_pos], &mut pos)?;
        if n > MAX_RECORDS {
            return Ok(None);
        }
        let mut recs = Vec::with_capacity(n as usize);
        let mut blocks_total = 0u64;
        for _ in 0..n {
            let unpadded = varint(&index[..crc_pos], &mut pos)?;
            let uncompressed = varint(&index[..crc_pos], &mut pos)?;
            if unpadded < 5 {
                return Err(bad("invalid block size"));
            }
            blocks_total = blocks_total.checked_add(round4(unpadded).ok_or_else(|| bad("size"))?).ok_or_else(|| bad("size"))?;
            recs.push((unpadded, uncompressed));
        }
        if index[pos..crc_pos].iter().any(|&b| b != 0) || (crc_pos - pos) >= 4 {
            return Err(bad("index padding"));
        }
        let stream_start = index_start.checked_sub(blocks_total).and_then(|v| v.checked_sub(12)).ok_or_else(|| bad("blocks outside the file"))?;
        let header: [u8; 12] = read_exact_at(f, stream_start, 12)?.try_into().unwrap();
        if header[..6] != HEADER_MAGIC {
            return Err(bad("stream header magic"));
        }
        if header[6..8] != footer[8..10] {
            return Err(bad("stream flags differ between header and footer"));
        }
        if crc32fast::hash(&header[6..8]) != u32::from_le_bytes(header[8..12].try_into().unwrap()) {
            return Err(bad("stream header CRC mismatch"));
        }
        let mut off = stream_start + 12;
        let mut blocks = Vec::with_capacity(recs.len());
        for (unpadded, uncompressed) in recs {
            blocks.push(XzBlock { off, unpadded, uncompressed });
            off += round4(unpadded).unwrap_or(u64::MAX);
        }
        streams.push(XzStream { header, blocks });
        if streams.len() > MAX_STREAMS {
            return Ok(None);
        }
        end = stream_start;
        if end == 0 {
            break;
        }
    }
    streams.reverse();
    let mut out = XzLayout { streams, ..Default::default() };
    for s in &out.streams {
        for b in &s.blocks {
            out.size = out.size.checked_add(b.uncompressed).ok_or_else(|| bad("size overflow"))?;
            out.blocks += 1;
            out.max_block = out.max_block.max(b.uncompressed).max(round4(b.unpadded).unwrap_or(u64::MAX));
        }
    }
    Ok(Some(out))
}

/// The one-block stream that decodes block `b` of a stream with header `header`.
fn mini_stream(header: &[u8; 12], block: &[u8], b: &XzBlock) -> Vec<u8> {
    let mut out = Vec::with_capacity(block.len() + 64);
    out.extend_from_slice(header);
    out.extend_from_slice(block);
    let mut index = vec![0u8];
    put_varint(&mut index, 1);
    put_varint(&mut index, b.unpadded);
    put_varint(&mut index, b.uncompressed);
    while index.len() % 4 != 0 {
        index.push(0);
    }
    let crc = crc32fast::hash(&index);
    index.extend_from_slice(&crc.to_le_bytes());
    let backward = (index.len() as u32 / 4) - 1;
    out.extend_from_slice(&index);
    let mut tail = backward.to_le_bytes().to_vec();
    tail.extend_from_slice(&header[6..8]);
    out.extend_from_slice(&crc32fast::hash(&tail).to_le_bytes());
    out.extend_from_slice(&tail);
    out.extend_from_slice(b"YZ");
    out
}

fn decode_block(f: &File, header: &[u8; 12], b: &XzBlock) -> io::Result<Vec<u8>> {
    let len = round4(b.unpadded).ok_or_else(|| bad("size"))?;
    let raw = read_exact_at(f, b.off, len)?;
    let mini = mini_stream(header, &raw, b);
    drop(raw);
    let mut out = Vec::with_capacity(b.uncompressed as usize);
    lzma_rust2::XzReader::new_mem_limit(io::Cursor::new(mini), false, MEM_LIMIT_KB).take(b.uncompressed + 1).read_to_end(&mut out)?;
    if out.len() as u64 != b.uncompressed {
        return Err(bad("block size differs from the index"));
    }
    Ok(out)
}

type Job = (u64, [u8; 12], XzBlock);
type Done = (u64, io::Result<Vec<u8>>);

/// Parallel, in-order decoder of a multi-block `.xz` file (see the module documentation).
pub(super) struct XzMtReader {
    jobs: Vec<([u8; 12], XzBlock)>,
    next_dispatch: usize,
    next_return: u64,
    window: usize,
    tx: Option<Sender<Job>>,
    rx: Receiver<Done>,
    ready: BTreeMap<u64, io::Result<Vec<u8>>>,
    cur: Vec<u8>,
    cur_pos: usize,
    stop: Arc<AtomicBool>,
}

impl XzMtReader {
    pub fn new(file: File, layout: &XzLayout, workers: usize, consumed: Arc<AtomicU64>) -> Self {
        let file = Arc::new(file);
        let workers = workers.clamp(1, 16);
        let (tx, job_rx) = mpsc::channel::<Job>();
        let (done_tx, rx) = mpsc::channel::<Done>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        let stop = Arc::new(AtomicBool::new(false));
        for i in 0..workers {
            let (file, job_rx, done_tx, stop, consumed) = (file.clone(), job_rx.clone(), done_tx.clone(), stop.clone(), consumed.clone());
            let _ = std::thread::Builder::new().name(format!("lumendisk-xz-{i}")).spawn(move || loop {
                let job = { job_rx.lock().unwrap_or_else(|e| e.into_inner()).recv() };
                let Ok((seq, header, b)) = job else { return };
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decode_block(&file, &header, &b)))
                    .unwrap_or_else(|_| Err(bad("decoder failure on damaged data")));
                consumed.fetch_add(round4(b.unpadded).unwrap_or(0), Ordering::Relaxed);
                if done_tx.send((seq, r)).is_err() {
                    return;
                }
            });
        }
        let jobs = layout.streams.iter().flat_map(|s| s.blocks.iter().map(move |b| (s.header, b.clone()))).collect();
        XzMtReader { jobs, next_dispatch: 0, next_return: 0, window: workers + 2, tx: Some(tx), rx, ready: BTreeMap::new(), cur: vec![], cur_pos: 0, stop }
    }

    fn fill(&mut self) -> io::Result<bool> {
        while self.next_dispatch < self.jobs.len() && (self.next_dispatch as u64) < self.next_return + self.window as u64 {
            let (h, b) = self.jobs[self.next_dispatch].clone();
            if let Some(tx) = &self.tx {
                tx.send((self.next_dispatch as u64, h, b)).map_err(|_| bad("decoder threads stopped"))?;
            }
            self.next_dispatch += 1;
        }
        if self.next_return as usize >= self.jobs.len() {
            return Ok(false);
        }
        loop {
            if let Some(r) = self.ready.remove(&self.next_return) {
                self.next_return += 1;
                self.cur = r?;
                self.cur_pos = 0;
                return Ok(true);
            }
            let (seq, r) = self.rx.recv().map_err(|_| bad("decoder threads stopped"))?;
            self.ready.insert(seq, r);
        }
    }
}

impl Read for XzMtReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.cur_pos >= self.cur.len() {
            if !self.fill()? {
                return Ok(0);
            }
        }
        let n = buf.len().min(self.cur.len() - self.cur_pos);
        buf[..n].copy_from_slice(&self.cur[self.cur_pos..self.cur_pos + n]);
        self.cur_pos += n;
        Ok(n)
    }
}

impl Drop for XzMtReader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.tx = None; // workers exit once the queue is empty
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip_and_reject_garbage() {
        for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, (1 << 63) - 1] {
            let mut b = vec![];
            put_varint(&mut b, v);
            let mut p = 0;
            assert_eq!(varint(&b, &mut p).unwrap(), v);
            assert_eq!(p, b.len());
        }
        assert!(varint(&[0x80, 0x00], &mut 0).is_err(), "non-minimal");
        assert!(varint(&[0xFF; 10], &mut 0).is_err(), "too long");
        assert!(varint(&[0x80], &mut 0).is_err(), "truncated");
    }
}
