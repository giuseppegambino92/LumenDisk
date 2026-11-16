//! SHA-512 crypt (`$6$[rounds=N$]salt$hash`), the password hash format of Linux `/etc/shadow`.
//!
//! Implemented exactly as Ulrich Drepper's "Unix crypt using SHA-256 and SHA-512"
//! specification (<https://www.akkadia.org/drepper/SHA-crypt.txt>), the algorithm of glibc's
//! `crypt(3)` `$6$`, `mkpasswd -m sha-512` and `openssl passwd -6`:
//! - the salt is at most 16 characters (longer salts are truncated, a `$` ends it);
//! - `rounds=N` is optional (default 5000) and clamped to 1000..=999 999 999; it is written
//!   back into the result only when the setting carried it;
//! - the 64-byte digest is encoded with the crypt base-64 alphabet in the specification's
//!   byte order.
//!
//! [`hash`] picks a 16-character salt from the operating system's CSPRNG. [`verify`] checks a
//! password against a `$6$` hash in constant time.
//!
//! Owner: the Raspberry Pi / utilities workstream.

use sha2::{Digest, Sha512};

const ITOA64: &[u8; 64] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const SALT_MAX: usize = 16;
pub const ROUNDS_DEFAULT: u32 = 5000;
pub const ROUNDS_MIN: u32 = 1000;
pub const ROUNDS_MAX: u32 = 999_999_999;
/// Upper bound on the password length we hash (glibc has none, but the work is
/// O(len × rounds) and a password never needs to be this long).
pub const MAX_PASSWORD: usize = 4096;

/// Why a setting or a password can't be hashed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CryptError {
    /// Not a `$6$` setting.
    NotSha512,
    /// `rounds=` present but not a decimal number.
    BadRounds,
    /// The salt holds characters that would corrupt `/etc/shadow` (`:`, newline, NUL).
    BadSalt,
    TooLong,
    /// The operating system's random generator failed.
    Random(String),
}

impl std::fmt::Display for CryptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CryptError::NotSha512 => f.write_str("not a SHA-512 crypt setting ($6$)"),
            CryptError::BadRounds => f.write_str("invalid rounds= value"),
            CryptError::BadSalt => f.write_str("invalid salt"),
            CryptError::TooLong => f.write_str("password too long"),
            CryptError::Random(e) => write!(f, "random generator: {e}"),
        }
    }
}

impl std::error::Error for CryptError {}

/// `$6$<16 random salt chars>$<hash>` with the default 5000 rounds (what `mkpasswd -m sha-512`
/// and `openssl passwd -6` produce).
pub fn hash(password: &str) -> Result<String, CryptError> {
    let salt = random_salt(SALT_MAX)?;
    hash_with(password.as_bytes(), &salt, None)
}

/// Hash with an explicit salt and, optionally, an explicit number of rounds (which then
/// appears in the output, as in `$6$rounds=N$`).
pub fn hash_with(password: &[u8], salt: &str, rounds: Option<u32>) -> Result<String, CryptError> {
    let mut setting = String::from("$6$");
    if let Some(r) = rounds {
        setting.push_str(&format!("rounds={r}$"));
    }
    setting.push_str(salt);
    crypt(password, &setting)
}

/// `crypt(3)` for a `$6$` setting: `$6$salt`, `$6$rounds=N$salt`, or a complete hash (its
/// salt and rounds are reused, the rest ignored).
pub fn crypt(password: &[u8], setting: &str) -> Result<String, CryptError> {
    if password.len() > MAX_PASSWORD {
        return Err(CryptError::TooLong);
    }
    let (rounds, explicit, salt) = parse_setting(setting)?;
    let digest = sha512_crypt_raw(password, salt, rounds);
    let mut out = String::with_capacity(123);
    out.push_str("$6$");
    if explicit {
        out.push_str(&format!("rounds={rounds}$"));
    }
    out.push_str(std::str::from_utf8(salt).map_err(|_| CryptError::BadSalt)?);
    out.push('$');
    encode(&digest, &mut out);
    Ok(out)
}

/// `(rounds, rounds given explicitly, salt bytes)` of a `$6$` setting.
fn parse_setting(setting: &str) -> Result<(u32, bool, &[u8]), CryptError> {
    let rest = setting.strip_prefix("$6$").ok_or(CryptError::NotSha512)?;
    let (rounds, explicit, rest) = match rest.strip_prefix("rounds=") {
        Some(r) => {
            let end = r.find('$').ok_or(CryptError::BadRounds)?;
            let digits = &r[..end];
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(CryptError::BadRounds);
            }
            // glibc parses with strtoul and clamps: an absurdly long number is "huge".
            let n: u64 = digits.parse().unwrap_or(u64::MAX);
            let n = n.clamp(ROUNDS_MIN as u64, ROUNDS_MAX as u64) as u32;
            (n, true, &r[end + 1..])
        }
        None => (ROUNDS_DEFAULT, false, rest),
    };
    let salt_end = rest.find('$').unwrap_or(rest.len());
    let salt = &rest.as_bytes()[..salt_end.min(SALT_MAX)];
    // Nothing that would corrupt a shadow line may be in it, and truncating to 16 bytes must
    // not have split a character (the salt is ASCII in practice).
    if salt.iter().any(|&b| b == b':' || b == b'\n' || b == b'\r' || b == 0) || std::str::from_utf8(salt).is_err() {
        return Err(CryptError::BadSalt);
    }
    Ok((rounds, explicit, salt))
}

/// Does `password` match the `$6$` hash `hashed`? False for anything that is not a `$6$`
/// hash. The comparison does not stop at the first different byte.
pub fn verify(password: &str, hashed: &str) -> bool {
    let Ok(again) = crypt(password.as_bytes(), hashed) else { return false };
    let (a, b) = (again.as_bytes(), hashed.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `n` characters of the crypt alphabet from the OS CSPRNG (rejection sampling, no bias).
pub fn random_salt(n: usize) -> Result<String, CryptError> {
    let mut out = String::with_capacity(n);
    let mut buf = [0u8; 64];
    while out.len() < n {
        getrandom::getrandom(&mut buf).map_err(|e| CryptError::Random(e.to_string()))?;
        for &b in &buf {
            // 256 = 4 × 64: every byte maps uniformly onto the alphabet.
            if out.len() < n {
                out.push(ITOA64[(b & 63) as usize] as char);
            }
        }
    }
    Ok(out)
}

/// The specification's steps 1–21: the 64-byte digest before encoding.
fn sha512_crypt_raw(key: &[u8], salt: &[u8], rounds: u32) -> [u8; 64] {
    // Digest B = SHA512(key | salt | key).
    let b: [u8; 64] = Sha512::new().chain_update(key).chain_update(salt).chain_update(key).finalize().into();

    // Digest A.
    let mut a = Sha512::new();
    a.update(key);
    a.update(salt);
    let mut cnt = key.len();
    while cnt > 64 {
        a.update(b);
        cnt -= 64;
    }
    a.update(&b[..cnt]);
    let mut cnt = key.len();
    while cnt > 0 {
        if cnt & 1 != 0 {
            a.update(b);
        } else {
            a.update(key);
        }
        cnt >>= 1;
    }
    let mut a: [u8; 64] = a.finalize().into();

    // Digest DP and the byte sequence P.
    let mut dp = Sha512::new();
    for _ in 0..key.len() {
        dp.update(key);
    }
    let dp: [u8; 64] = dp.finalize().into();
    let p: Vec<u8> = (0..key.len()).map(|i| dp[i % 64]).collect();

    // Digest DS and the byte sequence S.
    let mut ds = Sha512::new();
    for _ in 0..(16 + a[0] as usize) {
        ds.update(salt);
    }
    let ds: [u8; 64] = ds.finalize().into();
    let s: Vec<u8> = (0..salt.len()).map(|i| ds[i % 64]).collect();

    // The rounds.
    for i in 0..rounds {
        let mut c = Sha512::new();
        if i & 1 != 0 {
            c.update(&p);
        } else {
            c.update(a);
        }
        if i % 3 != 0 {
            c.update(&s);
        }
        if i % 7 != 0 {
            c.update(&p);
        }
        if i & 1 != 0 {
            c.update(a);
        } else {
            c.update(&p);
        }
        a = c.finalize().into();
    }
    a
}

/// Step 22: the specification's byte permutation, 4 characters per 3 bytes, least
/// significant 6 bits first.
fn encode(d: &[u8; 64], out: &mut String) {
    const ORDER: [(usize, usize, usize); 21] = [
        (0, 21, 42),
        (22, 43, 1),
        (44, 2, 23),
        (3, 24, 45),
        (25, 46, 4),
        (47, 5, 26),
        (6, 27, 48),
        (28, 49, 7),
        (50, 8, 29),
        (9, 30, 51),
        (31, 52, 10),
        (53, 11, 32),
        (12, 33, 54),
        (34, 55, 13),
        (56, 14, 35),
        (15, 36, 57),
        (37, 58, 16),
        (59, 17, 38),
        (18, 39, 60),
        (40, 61, 19),
        (62, 20, 41),
    ];
    let mut put = |w: u32, n: usize| {
        let mut w = w;
        for _ in 0..n {
            out.push(ITOA64[(w & 63) as usize] as char);
            w >>= 6;
        }
    };
    for (x, y, z) in ORDER {
        put(((d[x] as u32) << 16) | ((d[y] as u32) << 8) | d[z] as u32, 4);
    }
    put(d[63] as u32, 2);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The SHA-512 test vectors of the specification (SHA-crypt.txt), all of them.
    const VECTORS: &[(&str, &str, &str)] = &[
        ("$6$saltstring", "Hello world!", "$6$saltstring$svn8UoSVapNtMuq1ukKS4tPQd8iKwSMHWjl/O817G3uBnIFNjnQJuesI68u4OTLiBFdcbYEdFCoEOfaS35inz1"),
        (
            "$6$rounds=10000$saltstringsaltstring",
            "Hello world!",
            "$6$rounds=10000$saltstringsaltst$OW1/O6BYHV6BcXZu8QVeXbDWra3Oeqh0sbHbbMCVNSnCM/UrjmM0Dp8vOuZeHBy/YTBmSK6H9qs/y3RnOaw5v.",
        ),
        (
            "$6$rounds=5000$toolongsaltstring",
            "This is just a test",
            "$6$rounds=5000$toolongsaltstrin$lQ8jolhgVRVhY4b5pZKaysCLi0QBxGoNeKQzQ3glMhwllF7oGDZxUhx1yxdYcz/e1JSbq3y6JMxxl8audkUEm0",
        ),
        (
            "$6$rounds=1400$anotherlongsaltstring",
            "a very much longer text to encrypt.  This one even stretches over morethan one line.",
            "$6$rounds=1400$anotherlongsalts$POfYwTEok97VWcjxIiSOjiykti.o/pQs.wPvMxQ6Fm7I6IoYN3CmLs66x9t0oSwbtEW7o7UmJEiDwGqd8p4ur1",
        ),
        (
            "$6$rounds=77777$short",
            "we have a short salt string but not a short password",
            "$6$rounds=77777$short$WuQyW2YR.hBNpjjRhpYD/ifIw05xdfeEyQoMxIXbkvr0gge1a1x3yRULJ5CCaUeOxFmtlcGZelFl5CxtgfiAc0",
        ),
        (
            "$6$rounds=123456$asaltof16chars..",
            "a short string",
            "$6$rounds=123456$asaltof16chars..$BtCwjqMJGx5hrJhZywWvt0RLE8uZ4oPwcelCjmw2kSYu.Ec6ycULevoBK25fs2xXgMNrCzIMVcgEJAstJeonj1",
        ),
        (
            "$6$rounds=10$roundstoolow",
            "the minimum number is still observed",
            "$6$rounds=1000$roundstoolow$kUMsbe306n21p9R.FRkW3IGn.S9NPN0x50YhH1xhLsPuWGsUSklZt58jaTfF4ZEQpyUNGc0dqbpBYYBaHHrsX.",
        ),
    ];

    #[test]
    fn specification_test_vectors() {
        for (setting, key, want) in VECTORS {
            assert_eq!(crypt(key.as_bytes(), setting).unwrap(), *want, "{setting}");
            assert!(verify(key, want), "{setting}");
            assert!(!verify(&format!("{key}x"), want));
        }
    }

    #[test]
    fn a_complete_hash_is_a_valid_setting_and_rounds_clamp_both_ways() {
        let h = VECTORS[1].2;
        assert_eq!(crypt(b"Hello world!", h).unwrap(), h);
        // Out of range: clamped both ways (parsed only: a billion rounds would take minutes).
        assert_eq!(parse_setting("$6$rounds=99999999999999999999999$s").unwrap(), (ROUNDS_MAX, true, &b"s"[..]));
        assert_eq!(parse_setting("$6$rounds=0$s$rest").unwrap(), (ROUNDS_MIN, true, &b"s"[..]));
        assert_eq!(parse_setting("$6$abcdefghijklmnopqrstuvwxyz").unwrap(), (ROUNDS_DEFAULT, false, &b"abcdefghijklmnop"[..]));
    }

    #[test]
    fn random_hashes_use_a_fresh_16_char_salt_and_verify() {
        let a = hash("pässwörd ✓").unwrap();
        let b = hash("pässwörd ✓").unwrap();
        assert_ne!(a, b, "salt must differ");
        for h in [&a, &b] {
            let parts: Vec<&str> = h.split('$').collect();
            assert_eq!(parts[1], "6");
            assert_eq!(parts[2].len(), 16);
            assert!(parts[2].bytes().all(|c| ITOA64.contains(&c)));
            assert_eq!(parts[3].len(), 86);
            assert!(verify("pässwörd ✓", h));
            assert!(!verify("password", h));
        }
    }

    #[test]
    fn bad_settings_are_refused() {
        assert_eq!(crypt(b"x", "$5$abc").unwrap_err(), CryptError::NotSha512);
        assert_eq!(crypt(b"x", "$6$rounds=abc$salt").unwrap_err(), CryptError::BadRounds);
        assert_eq!(crypt(b"x", "$6$rounds=$salt").unwrap_err(), CryptError::BadRounds);
        assert_eq!(crypt(b"x", "$6$sa:lt").unwrap_err(), CryptError::BadSalt);
        assert_eq!(crypt(&vec![b'a'; MAX_PASSWORD + 1], "$6$salt").unwrap_err(), CryptError::TooLong);
        assert!(!verify("x", "not a hash"));
        assert!(!verify("x", ""));
        // An empty salt is legal (glibc accepts it).
        assert!(crypt(b"x", "$6$").unwrap().starts_with("$6$$"));
    }

    #[test]
    fn long_keys_cross_the_64_byte_boundaries() {
        // Keys of 63/64/65/128/129 bytes take the different branches of steps 9–11; the hash
        // must be stable and verifiable for each.
        for n in [0usize, 1, 63, 64, 65, 128, 129, 200] {
            let k = "k".repeat(n);
            let h = hash_with(k.as_bytes(), "abcdefgh", None).unwrap();
            assert!(verify(&k, &h), "{n}");
        }
    }
}
