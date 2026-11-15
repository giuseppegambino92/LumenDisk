//! HTTP(S) `Range:` byte source for streaming remote disk images (Feature 2).

use std::io::{self, Read};
use std::time::Duration;

use base64::Engine;

use crate::block::RangeSource;

pub struct HttpRangeSource {
    url: String,
    agent: ureq::Agent,
    auth: Option<String>,
    size: u64,
}

fn io_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e.to_string())
}

impl HttpRangeSource {
    /// Probe the URL: the server must report the size and honour byte ranges.
    pub fn open(url: &str, user: Option<&str>, password: Option<&str>) -> io::Result<Self> {
        let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(5)).timeout(Duration::from_secs(30)).build();
        let auth = match (user, password) {
            (Some(u), Some(p)) => Some(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}")))),
            _ => None,
        };
        let mut s = HttpRangeSource { url: url.to_string(), agent, auth, size: 0 };
        // A 1-byte range request tells us both "ranges supported" and the total size.
        let r = s.get(0, 1)?;
        if r.status() != 206 {
            return Err(io_err(format!("{url}: il server non supporta le richieste Range (HTTP {})", r.status())));
        }
        let cr = r.header("Content-Range").unwrap_or("").to_string();
        let total = cr.rsplit('/').next().and_then(|t| t.trim().parse::<u64>().ok());
        s.size = total.ok_or_else(|| io_err(format!("{url}: dimensione sconosciuta (Content-Range: {cr:?})")))?;
        Ok(s)
    }

    fn get(&self, offset: u64, len: usize) -> io::Result<ureq::Response> {
        let mut req = self.agent.get(&self.url).set("Range", &format!("bytes={}-{}", offset, offset + len as u64 - 1));
        if let Some(a) = &self.auth {
            req = req.set("Authorization", a);
        }
        match req.call() {
            Ok(r) => Ok(r),
            Err(ureq::Error::Status(code, _)) => Err(io_err(format!("HTTP {code}"))),
            Err(e) => Err(io_err(e)),
        }
    }
}

impl RangeSource for HttpRangeSource {
    fn describe(&self) -> String {
        self.url.clone()
    }
    fn size(&self) -> u64 {
        self.size
    }
    fn fetch(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut last = None;
        for attempt in 0..3 {
            match self.get(offset, len) {
                Ok(r) if r.status() == 206 => {
                    let mut buf = Vec::with_capacity(len);
                    r.into_reader().take(len as u64).read_to_end(&mut buf)?;
                    if buf.len() == len {
                        return Ok(buf);
                    }
                    last = Some(io_err("short read"));
                }
                Ok(r) => last = Some(io_err(format!("unexpected HTTP {}", r.status()))),
                Err(e) => last = Some(e),
            }
            std::thread::sleep(Duration::from_millis(200 * (attempt + 1)));
        }
        Err(last.unwrap())
    }
}
