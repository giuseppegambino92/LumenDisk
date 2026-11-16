//! Streaming checksums (SHA-256, SHA-1, MD5) of images and disks, in one pass: each
//! algorithm runs on a thread of its own over the same chunks, so asking for all three costs
//! about as much as the slowest one.
//!
//! Also: parsing an expected checksum as users paste it ("sha256:…", bare hex), and finding
//! the image's line in a SHA256SUMS-style file next to it.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Value};

use super::image::{self, Image, CHUNK};
use super::job::{JobCtx, Phase};
use super::plan::HashRequest;
use super::{ForgeError, ForgeResult};
use crate::i18n::t;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Algo {
    Sha256,
    Sha1,
    Md5,
}

impl Algo {
    pub const ALL: [Algo; 3] = [Algo::Sha256, Algo::Sha1, Algo::Md5];

    pub fn name(self) -> &'static str {
        match self {
            Algo::Sha256 => "sha256",
            Algo::Sha1 => "sha1",
            Algo::Md5 => "md5",
        }
    }

    pub fn parse(s: &str) -> Option<Algo> {
        match s.trim().to_ascii_lowercase().replace(['-', '_'], "").as_str() {
            "sha256" => Some(Algo::Sha256),
            "sha1" => Some(Algo::Sha1),
            "md5" => Some(Algo::Md5),
            _ => None,
        }
    }

    /// The algorithm a hex digest of this length belongs to.
    pub fn from_hex_len(n: usize) -> Option<Algo> {
        match n {
            64 => Some(Algo::Sha256),
            40 => Some(Algo::Sha1),
            32 => Some(Algo::Md5),
            _ => None,
        }
    }

    pub fn hex_len(self) -> usize {
        match self {
            Algo::Sha256 => 64,
            Algo::Sha1 => 40,
            Algo::Md5 => 32,
        }
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

enum State {
    Sha256(sha2::Sha256),
    Sha1(sha1::Sha1),
    Md5(md5::Md5),
}

impl State {
    fn new(a: Algo) -> Self {
        use sha2::Digest;
        match a {
            Algo::Sha256 => State::Sha256(sha2::Sha256::new()),
            Algo::Sha1 => State::Sha1(sha1::Sha1::new()),
            Algo::Md5 => State::Md5(md5::Md5::new()),
        }
    }
    fn update(&mut self, d: &[u8]) {
        use sha2::Digest;
        match self {
            State::Sha256(h) => h.update(d),
            State::Sha1(h) => h.update(d),
            State::Md5(h) => h.update(d),
        }
    }
    fn finish(self) -> String {
        use sha2::Digest;
        match self {
            State::Sha256(h) => hex(&h.finalize()),
            State::Sha1(h) => hex(&h.finalize()),
            State::Md5(h) => hex(&h.finalize()),
        }
    }
}

/// Several digests of the same bytes, one thread per algorithm.
pub struct MultiHasher {
    workers: Vec<(Algo, SyncSender<Arc<Vec<u8>>>, JoinHandle<String>)>,
    pub bytes: u64,
}

impl MultiHasher {
    pub fn new(algos: &[Algo]) -> Self {
        let mut workers = vec![];
        for &a in algos {
            if workers.iter().any(|(b, _, _)| *b == a) {
                continue;
            }
            let (tx, rx) = sync_channel::<Arc<Vec<u8>>>(4);
            let h = std::thread::Builder::new()
                .name(format!("lumendisk-hash-{}", a.name()))
                .spawn(move || {
                    let mut st = State::new(a);
                    for d in rx {
                        st.update(&d);
                    }
                    st.finish()
                })
                .expect("spawn hash thread");
            workers.push((a, tx, h));
        }
        MultiHasher { workers, bytes: 0 }
    }

    pub fn update(&mut self, data: Arc<Vec<u8>>) {
        self.bytes += data.len() as u64;
        for (_, tx, _) in &self.workers {
            let _ = tx.send(data.clone());
        }
    }

    /// `{"sha256": "…", …}`
    pub fn finish(self) -> BTreeMap<Algo, String> {
        let mut out = BTreeMap::new();
        for (a, tx, h) in self.workers {
            drop(tx);
            out.insert(a, h.join().unwrap_or_default());
        }
        out
    }
}

fn to_json(d: &BTreeMap<Algo, String>) -> serde_json::Map<String, Value> {
    d.iter().map(|(a, h)| (a.name().to_string(), Value::String(h.clone()))).collect()
}

/// Digest a byte range of a file (or the whole file), reporting progress in the current phase.
pub fn hash_file_range(path: &Path, offset: u64, len: Option<u64>, algos: &[Algo], ctx: &JobCtx) -> ForgeResult<(u64, BTreeMap<Algo, String>)> {
    let f = File::open(path)?;
    let size = f.metadata()?.len();
    let end = match len {
        Some(l) => offset.checked_add(l).filter(|&e| e <= size).ok_or_else(|| ForgeError::Invalid(t("intervallo oltre la fine del file", "range beyond the end of the file")))?,
        None => size,
    };
    if offset > end {
        return Err(ForgeError::Invalid(t("intervallo oltre la fine del file", "range beyond the end of the file")));
    }
    hash_with(algos, ctx, |pos, buf| {
        f.read_exact_at(buf, offset + pos)?;
        Ok(())
    }, end - offset)
}

/// Digest `total` bytes produced by `read(pos, buf)` in [`CHUNK`]-sized pieces.
pub fn hash_with(algos: &[Algo], ctx: &JobCtx, mut read: impl FnMut(u64, &mut [u8]) -> io::Result<()>, total: u64) -> ForgeResult<(u64, BTreeMap<Algo, String>)> {
    let mut mh = MultiHasher::new(algos);
    let mut pos = 0u64;
    while pos < total {
        let n = (total - pos).min(CHUNK as u64) as usize;
        let mut buf = vec![0u8; n];
        read(pos, &mut buf)?;
        mh.update(Arc::new(buf));
        pos += n as u64;
        ctx.advance(n as u64)?;
    }
    Ok((pos, mh.finish()))
}

/// Digest the decompressed contents of an image.
pub fn hash_decoded(img: &Image, algos: &[Algo], ctx: &JobCtx) -> ForgeResult<(u64, BTreeMap<Algo, String>)> {
    let mut pipe = img.pipe(CHUNK, 4);
    let mut mh = MultiHasher::new(algos);
    let estimate = img.size.or(img.size_hint).unwrap_or(0);
    ctx.set_total(estimate);
    while let Some(c) = pipe.next_chunk() {
        let c = c.map_err(|e| image::damaged(img, e))?;
        let n = c.len() as u64;
        mh.update(Arc::new(c));
        ctx.advance(n)?;
        if img.size.is_none() {
            // Unknown size: extrapolate from how much of the file was consumed.
            let used = pipe.consumed.load(std::sync::atomic::Ordering::Relaxed).max(1);
            ctx.set_total((ctx.done() as f64 * img.file_size as f64 / used as f64) as u64);
        }
    }
    let n = mh.bytes;
    ctx.set_total(n);
    Ok((n, mh.finish()))
}

/// An expected checksum.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Expected {
    pub algorithm: Algo,
    /// Lower-case hex.
    pub hex: String,
}

/// Parse "sha256:…", "SHA-256=…", "md5 …", or bare hex (algorithm by length). A pasted
/// checksum line ("<hex>  file.iso") is accepted too.
pub fn parse_expected(s: &str) -> ForgeResult<Expected> {
    let bad = || ForgeError::Invalid(t(&format!("checksum non valido: «{}»", s.trim()), &format!("invalid checksum: \"{}\"", s.trim())));
    let s = s.trim();
    let (algo, digest) = match s.find([':', '=']) {
        Some(i) => (Some(Algo::parse(&s[..i]).ok_or_else(bad)?), s[i + 1..].trim()),
        None => {
            let mut parts = s.split_whitespace();
            let first = parts.next().ok_or_else(bad)?;
            match Algo::parse(first) {
                Some(a) => (Some(a), parts.next().ok_or_else(bad)?),
                None => (None, first),
            }
        }
    };
    let digest = digest.split_whitespace().next().ok_or_else(bad)?.to_ascii_lowercase();
    if !digest.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(bad());
    }
    let algorithm = match algo {
        Some(a) if a.hex_len() == digest.len() => a,
        Some(_) => return Err(bad()),
        None => Algo::from_hex_len(digest.len()).ok_or_else(bad)?,
    };
    Ok(Expected { algorithm, hex: digest })
}

/// The image's line in a checksum file next to it.
#[derive(Clone, Debug, Serialize)]
pub struct SumsEntry {
    /// The checksum file.
    pub path: String,
    pub algorithm: Algo,
    pub expected: String,
}

/// Parse a checksum file (GNU `sha256sum` "<hex>  name" / "<hex> *name", BSD
/// "SHA256 (name) = <hex>", or a lone digest in "<image>.sha256") and return the digest for
/// `name`. Lines of other algorithms (SHA-512…) and PGP armour are ignored.
pub fn parse_sums(text: &str, name: &str, lone_digest_ok: bool) -> Option<(Algo, String)> {
    let mut lone = None;
    let mut lines = 0;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("-----") || line.starts_with("Hash:") {
            continue;
        }
        lines += 1;
        // BSD: "SHA256 (file) = hex"
        if let (Some(o), Some(c)) = (line.find(" ("), line.rfind(") = ")) {
            if o < c {
                let (tag, file, digest) = (&line[..o], &line[o + 2..c], line[c + 4..].trim());
                if file.trim_start_matches("./") == name {
                    if let Some(a) = Algo::parse(tag).filter(|a| a.hex_len() == digest.len() && digest.chars().all(|c| c.is_ascii_hexdigit())) {
                        return Some((a, digest.to_ascii_lowercase()));
                    }
                }
                continue;
            }
        }
        let mut it = line.splitn(2, char::is_whitespace);
        let digest = it.next().unwrap_or("");
        let Some(a) = Algo::from_hex_len(digest.len()).filter(|_| digest.chars().all(|c| c.is_ascii_hexdigit())) else { continue };
        match it.next().map(|f| f.trim().trim_start_matches('*').trim_start_matches("./")) {
            Some(f) if f == name || f.rsplit('/').next() == Some(name) => return Some((a, digest.to_ascii_lowercase())),
            Some(f) if !f.is_empty() => {}
            _ => lone = Some((a, digest.to_ascii_lowercase())),
        }
    }
    if lone_digest_ok && lines == 1 {
        return lone;
    }
    None
}

/// Look for a checksum file next to `image` that lists it.
pub fn find_sums_entry(image: &Path) -> Option<SumsEntry> {
    let dir = image.parent()?;
    let name = image.file_name()?.to_string_lossy().into_owned();
    let mut candidates: Vec<(PathBuf, bool)> = vec![];
    for ext in ["sha256", "sha256sum", "sha1", "sha1sum", "md5", "md5sum"] {
        candidates.push((dir.join(format!("{name}.{ext}")), true));
    }
    for fixed in ["SHA256SUMS", "SHA256SUMS.txt", "sha256sums", "sha256sums.txt", "sha256sum.txt", "SHA1SUMS", "sha1sum.txt", "MD5SUMS", "md5sum.txt", "CHECKSUM", "CHECKSUMS", "checksums.txt", "CHECKSUMS.txt"] {
        candidates.push((dir.join(fixed), false));
    }
    // Fedora-style "<name>-CHECKSUM" files.
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten().take(10_000) {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.ends_with("-CHECKSUM") || n.ends_with("-CHECKSUMS") {
                candidates.push((e.path(), false));
            }
        }
    }
    for (p, lone_ok) in candidates {
        let Ok(md) = std::fs::metadata(&p) else { continue };
        if !md.is_file() || md.len() > (1 << 20) {
            continue;
        }
        let Ok(text) = std::fs::read(&p) else { continue };
        if let Some((algorithm, expected)) = parse_sums(&String::from_utf8_lossy(&text), &name, lone_ok) {
            return Some(SumsEntry { path: p.to_string_lossy().into_owned(), algorithm, expected });
        }
    }
    None
}

/// `disk4`, `/dev/disk4`, `/dev/rdisk4` → `/dev/rdisk4`.
fn disk_node(source: &str) -> Option<String> {
    let s = source.trim_start_matches("/dev/");
    let s = s.strip_prefix('r').filter(|x| x.starts_with("disk")).unwrap_or(s);
    let rest = s.strip_prefix("disk")?;
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit() || c == 's') || !rest.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some(format!("/dev/r{s}"))
}

/// Run a [`HashRequest`].
pub fn run(r: &HashRequest, ctx: &JobCtx) -> ForgeResult<Value> {
    let algos: Vec<Algo> = if r.algorithms.is_empty() {
        Algo::ALL.to_vec()
    } else {
        r.algorithms.iter().map(|a| Algo::parse(a).ok_or_else(|| ForgeError::Invalid(format!("unknown algorithm \"{a}\"")))).collect::<ForgeResult<_>>()?
    };
    let t0 = Instant::now();
    let offset = r.offset.unwrap_or(0);
    let mut out = serde_json::Map::new();
    out.insert("source".into(), json!(r.source));
    let is_range = r.offset.is_some() || r.length.is_some();
    let total_bytes;
    if let Some(node) = disk_node(&r.source).filter(|_| !Path::new(&r.source).is_file()) {
        let dev = crate::block::open_source(&node, true, None)?;
        let size = dev.size();
        let len = r.length.unwrap_or(size.saturating_sub(offset));
        if offset.checked_add(len).map_or(true, |e| e > size) {
            return Err(ForgeError::Invalid(t("intervallo oltre la fine del disco", "range beyond the end of the disk")));
        }
        ctx.phase(Phase::Hashing, len);
        ctx.info(format!("hashing {node} ({len} bytes from {offset})"));
        let (n, d) = hash_with(&algos, ctx, |pos, buf| dev.read_at(offset + pos, buf), len)?;
        total_bytes = n;
        out.insert("kind".into(), json!("disk"));
        out.insert("what".into(), json!(if is_range { "range" } else { "disk" }));
        out.insert("offset".into(), json!(offset));
        out.insert("bytes".into(), json!(n));
        out.extend(to_json(&d));
    } else {
        let path = Path::new(&r.source);
        let size = std::fs::metadata(path).map_err(|e| ForgeError::Invalid(format!("{}: {e}", r.source)))?.len();
        let len = r.length.unwrap_or(size.saturating_sub(offset));
        ctx.phase(Phase::Hashing, len);
        let (n, d) = hash_file_range(path, offset, Some(len), &algos, ctx)?;
        let mut tb = n;
        out.insert("kind".into(), json!("file"));
        out.insert("what".into(), json!(if is_range { "range" } else { "file" }));
        out.insert("offset".into(), json!(offset));
        out.insert("bytes".into(), json!(n));
        out.extend(to_json(&d));
        let mut decoded = Value::Null;
        if r.decompressed && !is_range {
            let img = Image::open(path)?;
            if img.is_container() {
                ctx.phase(Phase::Hashing, img.size.unwrap_or(0));
                ctx.info(t("calcolo del checksum dell'immagine decompressa", "hashing the decompressed image"));
                let (dn, dd) = hash_decoded(&img, &algos, ctx)?;
                tb += dn;
                let mut m = to_json(&dd);
                m.insert("format".into(), json!(img.format));
                m.insert("bytes".into(), json!(dn));
                decoded = Value::Object(m);
            }
        }
        out.insert("decompressed".into(), decoded);
        let sums = if is_range { None } else { find_sums_entry(path) };
        out.insert(
            "checksum_file".into(),
            match sums {
                Some(s) => {
                    let matches = d.get(&s.algorithm).map(|h| *h == s.expected);
                    json!({"path": s.path, "algorithm": s.algorithm, "expected": s.expected, "matches": matches})
                }
                None => Value::Null,
            },
        );
        total_bytes = tb;
    }
    let secs = t0.elapsed().as_secs_f64();
    out.insert("seconds".into(), json!(secs));
    out.insert("mb_s".into(), json!(total_bytes as f64 / 1e6 / secs.max(1e-6)));
    Ok(Value::Object(out))
}

/// Check the source against an expected checksum: the file itself first; for a compressed
/// image whose file doesn't match, the decompressed image (publishers use either). Returns
/// what matched ("file" / "decompressed"), or a Mismatch error.
pub fn verify_expected(img: &Image, exp: &Expected, ctx: &JobCtx, origin: &str) -> ForgeResult<&'static str> {
    ctx.phase(Phase::Hashing, img.file_size);
    ctx.info(t(&format!("verifica del checksum {} ({origin})", exp.algorithm.name()), &format!("checking the {} checksum ({origin})", exp.algorithm.name())));
    let (_, d) = hash_file_range(&img.path, 0, None, &[exp.algorithm], ctx)?;
    if d.get(&exp.algorithm) == Some(&exp.hex) {
        return Ok("file");
    }
    if img.is_container() {
        ctx.info(t("il checksum non corrisponde al file: provo con l'immagine decompressa", "the checksum does not match the file: trying the decompressed image"));
        ctx.phase(Phase::Hashing, img.size.unwrap_or(0));
        let (_, d) = hash_decoded(img, &[exp.algorithm], ctx)?;
        if d.get(&exp.algorithm) == Some(&exp.hex) {
            return Ok("decompressed");
        }
    }
    Err(ForgeError::Mismatch(t(
        &format!("il checksum {} dell'immagine non corrisponde a quello atteso ({origin}): il file è danneggiato o non è quello giusto. Nulla è stato scritto.", exp.algorithm.name()),
        &format!("the image's {} checksum does not match the expected one ({origin}): the file is damaged or not the right one. Nothing was written.", exp.algorithm.name()),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(data: &[u8], a: Algo) -> String {
        let mut mh = MultiHasher::new(&[a]);
        // Several updates of uneven size must give the same digest as one.
        for c in data.chunks(7) {
            mh.update(Arc::new(c.to_vec()));
        }
        mh.finish()[&a].clone()
    }

    #[test]
    fn known_vectors() {
        // FIPS 180-4 / RFC 3174 / RFC 1321 test vectors.
        assert_eq!(digest(b"abc", Algo::Sha256), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(digest(b"abc", Algo::Sha1), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(digest(b"abc", Algo::Md5), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(digest(b"", Algo::Sha256), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(digest(b"", Algo::Md5), "d41d8cd98f00b204e9800998ecf8427e");
        let long = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(digest(long, Algo::Sha256), "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
        assert_eq!(digest(long, Algo::Sha1), "84983e441c3bd26ebaae4aa1f95129e5e54670f1");
        let million_a = vec![b'a'; 1_000_000];
        assert_eq!(digest(&million_a, Algo::Sha256), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
    }

    #[test]
    fn all_three_in_one_pass_over_a_file_and_a_range() {
        let dir = std::env::temp_dir().join(format!("ld-hash-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.bin");
        let data: Vec<u8> = (0..(CHUNK * 2 + 12345)).map(|i| (i * 31 % 251) as u8).collect();
        std::fs::write(&p, &data).unwrap();
        let ctx = JobCtx::detached(false);
        ctx.phase(Phase::Hashing, data.len() as u64);
        let (n, d) = hash_file_range(&p, 0, None, &Algo::ALL, &ctx).unwrap();
        assert_eq!(n, data.len() as u64);
        assert_eq!(ctx.done(), n);
        for a in Algo::ALL {
            assert_eq!(d[&a], digest(&data, a), "{a:?}");
        }
        let (n, d) = hash_file_range(&p, 100, Some(5000), &[Algo::Md5], &ctx).unwrap();
        assert_eq!(n, 5000);
        assert_eq!(d[&Algo::Md5], digest(&data[100..5100], Algo::Md5));
        assert!(hash_file_range(&p, 100, Some(data.len() as u64), &[Algo::Md5], &ctx).is_err());

        // run(): file digests + a SHA256SUMS next to it.
        let sha = digest(&data, Algo::Sha256);
        std::fs::write(dir.join("SHA256SUMS"), format!("{}  other.iso\n{sha} *x.bin\n", "0".repeat(64))).unwrap();
        let req = HashRequest { source: p.to_string_lossy().into(), algorithms: vec![], decompressed: false, offset: None, length: None };
        let v = run(&req, &ctx).unwrap();
        assert_eq!(v["sha256"], sha.as_str());
        assert_eq!(v["md5"], digest(&data, Algo::Md5).as_str());
        assert_eq!(v["what"], "file");
        assert_eq!(v["checksum_file"]["matches"], true);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn expected_checksums_are_parsed_as_users_paste_them() {
        let h64 = "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD";
        for s in [format!("sha256:{h64}"), format!("SHA-256={h64}"), format!("  {h64}  "), format!("sha256 {h64}"), format!("{h64}  ubuntu.iso")] {
            let e = parse_expected(&s).unwrap();
            assert_eq!(e.algorithm, Algo::Sha256, "{s}");
            assert_eq!(e.hex, h64.to_ascii_lowercase());
        }
        assert_eq!(parse_expected("a9993e364706816aba3e25717850c26c9cd0d89d").unwrap().algorithm, Algo::Sha1);
        assert_eq!(parse_expected("md5:900150983cd24fb0d6963f7d28e17f72").unwrap().algorithm, Algo::Md5);
        for bad in ["", "sha256:", "sha256:abc", "md5:a9993e364706816aba3e25717850c26c9cd0d89d", "sha512:00", "xyz", &"g".repeat(64), "crc32:12345678"] {
            assert!(parse_expected(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn checksum_files_in_every_common_layout() {
        let h = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let gnu = format!("{}  a.iso\n{h} *ubuntu.iso\n", "1".repeat(64));
        assert_eq!(parse_sums(&gnu, "ubuntu.iso", false), Some((Algo::Sha256, h.into())));
        assert_eq!(parse_sums(&gnu, "missing.iso", false), None);
        let bsd = format!("-----BEGIN PGP SIGNED MESSAGE-----\nHash: SHA256\n\n# Fedora\nSHA256 (Fedora.iso) = {h}\nSHA512 (Fedora.iso) = {}\n", "a".repeat(128));
        assert_eq!(parse_sums(&bsd, "Fedora.iso", false), Some((Algo::Sha256, h.into())));
        assert_eq!(parse_sums(&format!("{h}\n"), "x.img.xz", true), Some((Algo::Sha256, h.into())));
        assert_eq!(parse_sums(&format!("{h}\n"), "x.img.xz", false), None, "a lone digest only counts in <image>.sha256");
        assert_eq!(parse_sums(&format!("{h}  ./dir/x.img\n"), "x.img", false), Some((Algo::Sha256, h.into())));
        assert_eq!(parse_sums("900150983cd24fb0d6963f7d28e17f72  y.bin", "y.bin", false), Some((Algo::Md5, "900150983cd24fb0d6963f7d28e17f72".into())));
    }

    #[test]
    fn disk_names() {
        assert_eq!(disk_node("disk4").as_deref(), Some("/dev/rdisk4"));
        assert_eq!(disk_node("/dev/disk4s1").as_deref(), Some("/dev/rdisk4s1"));
        assert_eq!(disk_node("/dev/rdisk12").as_deref(), Some("/dev/rdisk12"));
        assert_eq!(disk_node("/tmp/disk4.img"), None);
        assert_eq!(disk_node("diskette"), None);
    }
}
