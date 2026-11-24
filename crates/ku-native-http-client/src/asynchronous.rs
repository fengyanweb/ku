//! Owner-stepped HTTP transport: DNS is the caller's bounded resolver job;
//! all socket callbacks are nonblocking and consume an explicit readiness mask.
use super::*;
use std::{
    io::{self, Read, Write},
    sync::Arc,
};
const HEADER_LIMIT: usize = 64 * 1024;
const STEP_CALLS: usize = 16;
#[repr(C)]
pub struct Io {
    pub context: *mut c_void,
    pub read: unsafe extern "C" fn(*mut c_void, *mut u8, usize) -> isize,
    pub write: unsafe extern "C" fn(*mut c_void, *const u8, usize) -> isize,
}
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    pub context: *mut c_void,
    pub reserve: unsafe extern "C" fn(*mut c_void, usize) -> u32,
    pub refund: unsafe extern "C" fn(*mut c_void, usize) -> u32,
}
// A Vec growth may move while both allocations are live. Reserve the entire
// replacement before asking the allocator, then release only the old receipt.
// Drop remains callback-free: physical job retirement owns its remaining charge.
fn grow_owned_bytes(
    budget: Budget,
    bytes: &mut Vec<u8>,
    wanted: usize,
) -> Result<(), &'static str> {
    let old_capacity = bytes.capacity();
    if wanted <= old_capacity {
        return Ok(());
    }
    if unsafe { (budget.reserve)(budget.context, wanted) } != 0 {
        return Err("memory_limit");
    }
    if bytes.try_reserve_exact(wanted - bytes.len()).is_err() {
        return if unsafe { (budget.refund)(budget.context, wanted) } == 0 {
            Err("out_of_memory")
        } else {
            Err("internal_error")
        };
    }
    if old_capacity != 0 && unsafe { (budget.refund)(budget.context, old_capacity) } != 0 {
        return Err("internal_error");
    }
    Ok(())
}
#[repr(C)]
pub struct Info {
    pub host: Slice,
    pub port: u32,
    pub tls: u32,
    pub done: u32,
}
struct Transport<'a> {
    io: &'a Io,
    events: u32,
    calls: usize,
}
impl Read for Transport<'_> {
    fn read(&mut self, data: &mut [u8]) -> io::Result<usize> {
        if self.events & 1 == 0 || self.calls == STEP_CALLS {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.calls += 1;
        let n =
            unsafe { (self.io.read)(self.io.context, data.as_mut_ptr(), data.len().min(65536)) };
        if n >= 0 && n as usize <= data.len().min(65536) {
            Ok(n as usize)
        } else if n == -1 {
            Err(io::ErrorKind::WouldBlock.into())
        } else {
            Err(io::ErrorKind::ConnectionReset.into())
        }
    }
}
impl Write for Transport<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.events & 2 == 0 || self.calls == STEP_CALLS {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.calls += 1;
        let n = unsafe { (self.io.write)(self.io.context, data.as_ptr(), data.len().min(65536)) };
        if n >= 0 && n as usize <= data.len().min(65536) {
            Ok(n as usize)
        } else if n == -1 {
            Err(io::ErrorKind::WouldBlock.into())
        } else {
            Err(io::ErrorKind::ConnectionReset.into())
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[derive(Debug)]
struct Decoded {
    bytes: Vec<u8>,
    limit: usize,
    budget: Budget,
    fault: Option<&'static str>,
    live: bool,
}
impl Write for Decoded {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if !self.live {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let Some(wanted) = self.bytes.len().checked_add(data.len()) else {
            self.fault = Some("body_too_large");
            return Err(io::ErrorKind::InvalidData.into());
        };
        if wanted > self.limit {
            self.fault = Some("body_too_large");
            return Err(io::ErrorKind::InvalidData.into());
        }
        grow_owned_bytes(self.budget, &mut self.bytes, wanted).map_err(|code| {
            self.fault = Some(code);
            io::Error::from(io::ErrorKind::OutOfMemory)
        })?;
        self.bytes.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub struct Operation {
    request: engine::HttpRequest,
    url: url::Url,
    host: String,
    wire: Vec<u8>,
    sent: usize,
    incoming: Vec<u8>,
    body: Vec<u8>,
    gzip: Option<flate2::write::MultiGzDecoder<Decoded>>,
    headers: HashMap<String, String>,
    status: i64,
    header_end: usize,
    cursor: usize,
    chunk_remaining: Option<usize>,
    chunked: bool,
    expected: Option<usize>,
    encoded_read: usize,
    compressed: bool,
    eof_body: bool,
    trailers: bool,
    chunk_terminator: bool,
    interim_count: u8,
    redirects: u8,
    tls: Option<rustls::ClientConnection>,
    tls_done: bool,
    completed: bool,
    peer_eof: bool,
    keep_alive: bool,
    outcome: Option<Result<engine::HttpResponse, engine::HttpError>>,
    budget: Budget,
}
impl Drop for Operation {
    fn drop(&mut self) {
        if let Some(decoder) = &mut self.gzip {
            decoder.get_mut().live = false;
        }
    }
}
fn error(code: &str, message: impl Into<String>) -> engine::HttpError {
    engine::HttpError {
        code: code.into(),
        message: message.into(),
    }
}
impl Operation {
    fn charge(&self, n: usize) -> Result<(), engine::HttpError> {
        if unsafe { (self.budget.reserve)(self.budget.context, n) } == 0 {
            Ok(())
        } else {
            Err(error(
                "memory_limit",
                "HTTP operation exceeds Task byte budget",
            ))
        }
    }
    fn grow(budget: Budget, bytes: &mut Vec<u8>, wanted: usize) -> Result<(), engine::HttpError> {
        grow_owned_bytes(budget, bytes, wanted)
            .map_err(|code| error(code, "HTTP buffer growth rejected"))
    }
    fn prepare(&mut self) -> Result<(), engine::HttpError> {
        // The synchronous engine's public validation is shared before either transport.
        self.url = engine::validate_request(&self.request)?;
        self.host = match self.url.host() {
            Some(url::Host::Ipv6(v)) => v.to_string(),
            Some(h) => h.to_string(),
            None => String::new(),
        };
        let target = match self.url.query() {
            Some(q) => format!("{}?{}", self.url.path(), q),
            None => self.url.path().to_owned(),
        };
        let authority = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let authority = if let Some(port) = self.url.port() {
            format!("{authority}:{port}")
        } else {
            authority
        };
        let estimate = self
            .request
            .method
            .len()
            .checked_add(self.request.url.len().saturating_mul(2))
            .and_then(|n| n.checked_add(self.request.body.as_ref().map_or(0, String::len)))
            .and_then(|n| {
                self.request
                    .headers
                    .iter()
                    .try_fold(n + 512, |sum, (k, v)| {
                        sum.checked_add(k.len() + v.len() + 4)
                    })
            })
            .ok_or_else(|| error("out_of_memory", "HTTP request size overflow"))?;
        self.charge(estimate)?;
        let mut wire = String::new();
        wire.try_reserve_exact(estimate)
            .map_err(|_| error("out_of_memory", "HTTP wire allocation failed"))?;
        use std::fmt::Write as _;
        write!(
            &mut wire,
            "{} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: ureq/2.12.1\r\n",
            self.request.method.to_ascii_uppercase(),
            target,
            authority
        )
        .map_err(|_| error("out_of_memory", "HTTP wire construction failed"))?;
        if (!self.url.username().is_empty() || self.url.password().is_some())
            && !self
                .request
                .headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case("authorization"))
        {
            const ALPHABET: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let raw = format!(
                "{}:{}",
                self.url.username(),
                self.url.password().unwrap_or("")
            );
            wire.push_str("Authorization: Basic ");
            for chunk in raw.as_bytes().chunks(3) {
                let n = (u32::from(chunk[0]) << 16)
                    | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
                    | u32::from(*chunk.get(2).unwrap_or(&0));
                wire.push(ALPHABET[((n >> 18) & 63) as usize] as char);
                wire.push(ALPHABET[((n >> 12) & 63) as usize] as char);
                wire.push(if chunk.len() > 1 {
                    ALPHABET[((n >> 6) & 63) as usize] as char
                } else {
                    '='
                });
                wire.push(if chunk.len() > 2 {
                    ALPHABET[(n & 63) as usize] as char
                } else {
                    '='
                });
            }
            wire.push_str("\r\n");
        }
        if !self
            .request
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("accept-encoding"))
        {
            wire.push_str("Accept-Encoding: gzip\r\n")
        }
        for (key, value) in &self.request.headers {
            wire.push_str(key);
            wire.push_str(": ");
            wire.push_str(value);
            wire.push_str("\r\n")
        }
        if let Some(body) = &self.request.body {
            wire.push_str(&format!("Content-Length: {}\r\n", body.len()));
            if !self
                .request
                .headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case("content-type"))
            {
                wire.push_str("Content-Type: text/plain; charset=utf-8\r\n")
            }
        }
        wire.push_str("\r\n");
        if let Some(body) = &self.request.body {
            wire.push_str(body)
        }
        self.wire = wire.into_bytes();
        self.sent = 0;
        self.incoming.clear();
        self.body.clear();
        self.gzip = None;
        self.headers.clear();
        self.header_end = 0;
        self.cursor = 0;
        self.chunk_remaining = None;
        self.chunked = false;
        self.expected = None;
        self.encoded_read = 0;
        self.compressed = false;
        self.eof_body = false;
        self.trailers = false;
        self.chunk_terminator = false;
        self.interim_count = 0;
        self.tls_done = false;
        self.tls = None;
        self.peer_eof = false;
        self.keep_alive = false;
        Ok(())
    }
    fn append(&mut self, bytes: &[u8]) -> Result<(), engine::HttpError> {
        let limit = self
            .request
            .max_body_bytes
            .checked_add(HEADER_LIMIT * 2)
            .ok_or_else(|| error("body_too_large", "HTTP size overflow"))?;
        let wanted = self
            .incoming
            .len()
            .checked_add(bytes.len())
            .ok_or_else(|| error("body_too_large", "HTTP size overflow"))?;
        if wanted > limit {
            return Err(error(
                "body_too_large",
                "HTTP wire response exceeds bounded input limit",
            ));
        }
        Self::grow(self.budget, &mut self.incoming, wanted)?;
        self.incoming.extend_from_slice(bytes);
        Ok(())
    }
    fn body_append(&mut self, start: usize, end: usize) -> Result<(), engine::HttpError> {
        if self.compressed {
            // Encoded bytes conservatively cover gzip library retained header state as
            // well as decoded buffers; release this receipt only at job retirement.
            self.charge(end - start)?;
            let decoder = self
                .gzip
                .as_mut()
                .ok_or_else(|| error("read_failed", "HTTP gzip state missing"))?;
            decoder.write_all(&self.incoming[start..end]).map_err(|e| {
                error(
                    decoder.get_ref().fault.unwrap_or("read_failed"),
                    e.to_string(),
                )
            })?;
            return Ok(());
        }
        let wanted = self
            .body
            .len()
            .checked_add(end - start)
            .ok_or_else(|| error("body_too_large", "HTTP size overflow"))?;
        if wanted > self.request.max_body_bytes {
            return Err(error(
                "body_too_large",
                format!(
                    "http response too large: exceeds {} bytes",
                    self.request.max_body_bytes
                ),
            ));
        }
        Self::grow(self.budget, &mut self.body, wanted)?;
        self.body.extend_from_slice(&self.incoming[start..end]);
        Ok(())
    }
    fn parse(&mut self, eof: bool) -> Result<bool, engine::HttpError> {
        while self.header_end == 0 {
            let Some(end) = self
                .incoming
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|i| i + 4)
            else {
                if self.incoming.len() > HEADER_LIMIT || eof {
                    return Err(error(
                        "read_failed",
                        "incomplete or oversized HTTP response headers",
                    ));
                }
                return Ok(false);
            };
            if end > HEADER_LIMIT {
                return Err(error("read_failed", "HTTP response headers exceed limit"));
            }
            let text = std::str::from_utf8(&self.incoming[..end])
                .map_err(|_| error("read_failed", "HTTP headers are not UTF-8"))?;
            let response: ureq::Response = text
                .parse()
                .map_err(|e: ureq::Error| error("read_failed", e.to_string()))?;
            self.status = i64::from(response.status());
            if self.status < 200 {
                self.interim_count = self.interim_count.saturating_add(1);
                if self.status == 101 || self.interim_count > 32 {
                    return Err(error(
                        "read_failed",
                        "unsupported HTTP upgrade or excessive interim responses",
                    ));
                }
                self.incoming.drain(..end);
                continue;
            }
            let names = response.headers_names();
            if names.len() > 1024 {
                return Err(error("read_failed", "HTTP response has too many headers"));
            }
            self.charge(names.len().saturating_mul(96))?;
            self.headers
                .try_reserve(names.len())
                .map_err(|_| error("out_of_memory", "HTTP headers allocation failed"))?;
            for name in names {
                if let Some(value) = response.header(&name) {
                    self.charge(name.len() + value.len() + 64)?;
                    self.headers
                        .insert(name.to_ascii_lowercase(), value.to_owned());
                }
            }
            let header = |key: &str| {
                text.split("\r\n")
                    .skip(1)
                    .filter_map(|l| l.split_once(':'))
                    .find(|(n, _)| n.eq_ignore_ascii_case(key))
                    .map(|(_, v)| v.trim())
            };
            self.keep_alive = if text.starts_with("HTTP/1.1 ") {
                !header("connection")
                    .is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("close")))
            } else {
                header("connection").is_some_and(|v| v.eq_ignore_ascii_case("keep-alive"))
            };
            self.compressed =
                header("content-encoding").is_some_and(|v| matches!(v, "gzip" | "x-gzip"));
            if self.compressed {
                self.charge(65536)?;
                self.gzip = Some(flate2::write::MultiGzDecoder::new(Decoded {
                    bytes: Vec::new(),
                    limit: self.request.max_body_bytes,
                    budget: self.budget,
                    fault: None,
                    live: true,
                }));
            }
            self.chunked =
                header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
            self.expected = if self.status == 204 || self.status == 304 {
                Some(0)
            } else {
                header("content-length")
                    .map(|v| {
                        v.parse::<usize>()
                            .map_err(|_| error("read_failed", "invalid HTTP content-length"))
                    })
                    .transpose()?
            };
            if self.chunked {
                self.expected = None
            }
            self.eof_body = !self.chunked && self.expected.is_none();
            self.header_end = end;
            self.cursor = end;
            if (matches!(self.status, 301..=303)
                || matches!(self.status, 307 | 308)
                    && self.request.method.eq_ignore_ascii_case("GET"))
                && self.headers.contains_key("location")
            {
                return Ok(true);
            }
        }
        if self.chunked {
            for _ in 0..65536 {
                if self.trailers {
                    if self.incoming.get(self.cursor..self.cursor + 2) == Some(b"\r\n") {
                        self.cursor += 2;
                        return Ok(true);
                    }
                    let Some(end) = self.incoming[self.cursor..]
                        .windows(2)
                        .position(|w| w == b"\r\n")
                    else {
                        if self.incoming.len() - self.cursor > HEADER_LIMIT || eof {
                            return Err(error("read_failed", "invalid HTTP chunk trailers"));
                        }
                        return Ok(false);
                    };
                    // Reuse ureq's header parser for the exact trailer line grammar.
                    let line = &self.incoming[self.cursor..self.cursor + end];
                    let text = std::str::from_utf8(line)
                        .map_err(|_| error("read_failed", "invalid HTTP trailer"))?;
                    let probe = format!("HTTP/1.1 200 OK\r\n{text}\r\n\r\n");
                    let _: ureq::Response = probe
                        .parse()
                        .map_err(|e: ureq::Error| error("read_failed", e.to_string()))?;
                    self.cursor += end + 2;
                    continue;
                }
                if self.chunk_terminator {
                    if self.incoming.len() - self.cursor < 2 {
                        if eof {
                            return Err(error("read_failed", "truncated HTTP chunk terminator"));
                        }
                        return Ok(false);
                    }
                    if self.incoming.get(self.cursor..self.cursor + 2) != Some(b"\r\n") {
                        return Err(error("read_failed", "invalid HTTP chunk terminator"));
                    }
                    self.cursor += 2;
                    self.chunk_terminator = false;
                }
                if self.chunk_remaining.is_none() {
                    let Some(at) = self.incoming[self.cursor..]
                        .windows(2)
                        .position(|w| w == b"\r\n")
                    else {
                        if eof || self.incoming.len() - self.cursor > 8192 {
                            return Err(error(
                                "read_failed",
                                "truncated or oversized HTTP chunk length",
                            ));
                        }
                        return Ok(false);
                    };
                    let token = std::str::from_utf8(&self.incoming[self.cursor..self.cursor + at])
                        .map_err(|_| error("read_failed", "invalid HTTP chunk length"))?
                        .split(';')
                        .next()
                        .unwrap_or("");
                    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
                        return Err(error("read_failed", "invalid HTTP chunk length"));
                    }
                    let length = usize::from_str_radix(token, 16)
                        .map_err(|_| error("read_failed", "invalid HTTP chunk length"))?;
                    self.cursor += at + 2;
                    if length == 0 {
                        self.trailers = true;
                        continue;
                    }
                    self.chunk_remaining = Some(length);
                }
                let length = self.chunk_remaining.unwrap_or(0);
                let available = self.incoming.len() - self.cursor;
                let n = available.min(length);
                if n == 0 {
                    if eof {
                        return Err(error("read_failed", "truncated HTTP chunk"));
                    }
                    return Ok(false);
                }
                self.body_append(self.cursor, self.cursor + n)?;
                self.cursor += n;
                if n == length {
                    self.chunk_remaining = None;
                    self.chunk_terminator = true
                } else {
                    self.chunk_remaining = Some(length - n)
                }
            }
            return Err(error(
                "read_failed",
                "HTTP parsing exceeded one bounded step",
            ));
        }
        let available = self.incoming.len() - self.cursor;
        let wanted = self
            .expected
            .map(|n| n.saturating_sub(self.encoded_read))
            .unwrap_or(available)
            .min(available);
        self.body_append(self.cursor, self.cursor + wanted)?;
        self.cursor += wanted;
        self.encoded_read += wanted;
        if self.expected == Some(self.encoded_read) || self.eof_body && eof {
            return Ok(true);
        }
        if eof {
            return Err(error(
                "read_failed",
                "HTTP response ended before content-length",
            ));
        }
        Ok(false)
    }
    fn compact(&mut self) {
        if self.header_end != 0 && self.cursor != 0 {
            self.incoming.drain(..self.cursor);
            self.cursor = 0;
            self.header_end = 1;
        }
    }
    fn finish(&mut self) -> Result<u32, engine::HttpError> {
        if (300..399).contains(&self.status) && self.redirects >= 4 {
            return Err(error("transport", "HTTP redirect limit reached"));
        }
        if matches!(self.status, 301..=303)
            || matches!(self.status, 307 | 308) && self.request.method.eq_ignore_ascii_case("GET")
        {
            if let Some(location) = self.headers.get("location") {
                let next = self
                    .url
                    .join(location)
                    .map_err(|e| error("transport", e.to_string()))?;
                self.request.headers.retain(|k, _| {
                    !k.eq_ignore_ascii_case("authorization") && !k.eq_ignore_ascii_case("cookie")
                });
                if self.status == 303
                    || matches!(self.status, 301 | 302)
                        && self.request.method.eq_ignore_ascii_case("POST")
                {
                    self.request.method = "GET".into();
                    self.request.body = None
                }
                self.request.body = None;
                self.request.url = next.into();
                self.redirects += 1;
                self.prepare()?;
                return Ok(8);
            }
        }
        let bytes = if self.compressed {
            let mut decoder = self
                .gzip
                .take()
                .ok_or_else(|| error("read_failed", "HTTP gzip state missing"))?;
            if let Err(e) = decoder.try_finish() {
                let code = decoder.get_ref().fault.unwrap_or("read_failed");
                decoder.get_mut().live = false;
                return Err(error(code, e.to_string()));
            }
            let decoded = decoder
                .finish()
                .map_err(|e| error("read_failed", e.to_string()))?;
            decoded.bytes
        } else {
            std::mem::take(&mut self.body)
        };
        let body = String::from_utf8(bytes)
            .map_err(|e| error("invalid_utf8", format!("http response is not utf-8: {e}")))?;
        self.outcome = Some(Ok(engine::HttpResponse {
            status: self.status,
            headers: std::mem::take(&mut self.headers),
            body,
        }));
        self.completed = true;
        Ok(4)
    }
    fn ensure_tls(&mut self) -> Result<(), engine::HttpError> {
        if self.url.scheme() == "https" && self.tls.is_none() {
            self.charge(
                131072
                    + size_of::<rustls::ClientConnection>()
                    + std::mem::size_of_val(webpki_roots::TLS_SERVER_ROOTS),
            )?;
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let roots =
                rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|e| error("transport", e.to_string()))?
                .with_root_certificates(roots)
                .with_no_client_auth();
            let name = rustls::pki_types::ServerName::try_from(self.host.clone())
                .map_err(|e| error("invalid_url", e.to_string()))?;
            let mut connection = rustls::ClientConnection::new(Arc::new(config), name)
                .map_err(|e| error("transport", e.to_string()))?;
            connection.set_buffer_limit(Some(65536));
            self.tls = Some(connection);
        }
        Ok(())
    }
    fn step(&mut self, io: &Io, events: u32) -> Result<u32, engine::HttpError> {
        self.ensure_tls()?;
        let mut stream = Transport {
            io,
            events,
            calls: 0,
        };
        let mut received = [0u8; 65536];
        let mut eof = false;
        if let Some(tls) = &mut self.tls {
            if let Err(e) = tls.complete_io(&mut stream) {
                if e.kind() != io::ErrorKind::WouldBlock {
                    return Err(error("transport", e.to_string()));
                }
            }
            if tls.is_handshaking() {
                return Ok(u32::from(tls.wants_read()) | (u32::from(tls.wants_write()) * 2));
            }
            self.tls_done = true;
            if self.sent < self.wire.len() {
                match tls.writer().write(&self.wire[self.sent..]) {
                    Ok(n) => self.sent += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(error("transport", e.to_string())),
                }
            }
            if let Err(e) = tls.complete_io(&mut stream) {
                if e.kind() != io::ErrorKind::WouldBlock {
                    return Err(error("transport", e.to_string()));
                }
            }
            match tls.reader().read(&mut received) {
                Ok(0) => eof = true,
                Ok(n) => self.append(&received[..n])?,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(error("read_failed", e.to_string())),
            }
        } else {
            if self.sent < self.wire.len() {
                match stream.write(&self.wire[self.sent..]) {
                    Ok(0) => return Err(error("transport", "HTTP write made no progress")),
                    Ok(n) => self.sent += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(error("transport", e.to_string())),
                }
            }
            if self.sent == self.wire.len() {
                match stream.read(&mut received) {
                    Ok(0) => eof = true,
                    Ok(n) => self.append(&received[..n])?,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(error("read_failed", e.to_string())),
                }
            }
        }
        self.peer_eof |= eof;
        if self.parse(eof)? {
            return self.finish();
        }
        self.compact();
        if let Some(tls) = &self.tls {
            Ok(u32::from(tls.wants_read())
                | (u32::from(tls.wants_write() || self.sent < self.wire.len()) * 2))
        } else {
            Ok(if self.sent < self.wire.len() { 2 } else { 1 })
        }
    }
}
fn guarded(
    op: &mut Operation,
    f: impl FnOnce(&mut Operation) -> Result<u32, engine::HttpError>,
) -> u32 {
    match catch_unwind(AssertUnwindSafe(|| f(op))) {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            op.completed = true;
            op.outcome = Some(Err(e));
            4
        }
        Err(_) => {
            op.completed = true;
            op.outcome = Some(Err(error(
                "internal_panic",
                "HTTP client failed internally",
            )));
            4
        }
    }
}
/// # Safety
/// Input and budget callbacks satisfy the private header's bounded live-storage contract.
#[no_mangle]
pub unsafe extern "C" fn __ku_http_client_async_new_v1(
    input: *const Input,
    budget: Budget,
    out: *mut *mut Operation,
) -> u32 {
    if input.is_null()
        || out.is_null()
        || !(input as usize).is_multiple_of(align_of::<Input>())
        || !(out as usize).is_multiple_of(align_of::<*mut Operation>())
        || overlaps(
            input as usize,
            size_of::<Input>(),
            out as usize,
            size_of::<*mut Operation>(),
        )
    {
        return 2;
    }
    let i = unsafe { &*input };
    if i.size != size_of::<Input>() as u32
        || i.flags > 1
        || i.timeout_ms == 0
        || i.timeout_ms > engine::MAX_TIMEOUT_MS
        || i.max_body_bytes == 0
        || i.max_body_bytes > engine::MAX_BODY_BYTES
        || i.header_count > 1024
        || i.header_count > 0 && i.headers.is_null()
    {
        return 2;
    }
    let Some(header_bytes) = i.header_count.checked_mul(size_of::<Header>()) else {
        return 2;
    };
    if i.header_count > 0
        && (!(i.headers as usize).is_multiple_of(align_of::<Header>())
            || (i.headers as usize).checked_add(header_bytes).is_none()
            || overlaps(
                i.headers as usize,
                header_bytes,
                out as usize,
                size_of::<*mut Operation>(),
            ))
    {
        return 2;
    }
    for s in [i.url, i.method, i.body] {
        if unsafe { text(s) }.is_err()
            || overlaps(
                s.data as usize,
                s.len,
                out as usize,
                size_of::<*mut Operation>(),
            )
        {
            return 2;
        }
    }
    let raw = if i.header_count == 0 {
        &[][..]
    } else {
        unsafe { slice::from_raw_parts(i.headers, i.header_count) }
    };
    for h in raw {
        for s in [h.name, h.value] {
            if unsafe { text(s) }.is_err()
                || overlaps(
                    s.data as usize,
                    s.len,
                    out as usize,
                    size_of::<*mut Operation>(),
                )
            {
                return 2;
            }
        }
    }
    unsafe { *out = ptr::null_mut() };
    let attempt = catch_unwind(AssertUnwindSafe(|| -> Result<Operation, u32> {
        let mut charge = size_of::<Operation>();
        for s in [i.url, i.method, i.body] {
            charge = charge.checked_add(s.len.saturating_mul(3)).ok_or(2u32)?;
        }
        for h in raw {
            charge = charge
                .checked_add(h.name.len)
                .and_then(|n| n.checked_add(h.value.len + 96))
                .ok_or(2u32)?;
        }
        if unsafe { (budget.reserve)(budget.context, charge) } != 0 {
            return Err(3);
        }
        let request = unsafe { decode_request(i) }?;
        let mut op = Operation {
            url: url::Url::parse("http://localhost/").map_err(|_| 2u32)?,
            host: String::new(),
            request,
            wire: Vec::new(),
            sent: 0,
            incoming: Vec::new(),
            body: Vec::new(),
            gzip: None,
            headers: HashMap::new(),
            status: 0,
            header_end: 0,
            cursor: 0,
            chunk_remaining: None,
            chunked: false,
            expected: None,
            encoded_read: 0,
            compressed: false,
            eof_body: false,
            trailers: false,
            chunk_terminator: false,
            interim_count: 0,
            redirects: 0,
            tls: None,
            tls_done: false,
            completed: false,
            peer_eof: false,
            keep_alive: false,
            outcome: None,
            budget,
        };
        guarded(&mut op, |op| {
            op.prepare()?;
            Ok(0)
        });
        Ok(op)
    }));
    let op = match attempt {
        Ok(Ok(v)) => v,
        Ok(Err(n)) => return n,
        Err(_) => return 255,
    };
    let memory = unsafe { alloc(Layout::new::<Operation>()) } as *mut Operation;
    if memory.is_null() {
        return 1;
    }
    unsafe {
        ptr::write(memory, op);
        *out = memory
    };
    0
}
/// # Safety
/// A live, externally serialized operation and distinct initialized output.
#[no_mangle]
pub unsafe extern "C" fn __ku_http_client_async_info_v1(op: *mut Operation, out: *mut Info) -> u32 {
    if op.is_null()
        || out.is_null()
        || !(op as usize).is_multiple_of(align_of::<Operation>())
        || !(out as usize).is_multiple_of(align_of::<Info>())
        || overlaps(
            op as usize,
            size_of::<Operation>(),
            out as usize,
            size_of::<Info>(),
        )
    {
        return 2;
    }
    let op = unsafe { &*op };
    unsafe {
        *out = Info {
            host: Slice::view(op.host.as_bytes()),
            port: u32::from(op.url.port_or_known_default().unwrap_or(80)),
            tls: u32::from(op.url.scheme() == "https"),
            done: u32::from(op.completed),
        }
    };
    0
}
/// # Safety
/// Socket callbacks return -1 for WouldBlock, -2 for fatal, or a bounded byte count.
#[no_mangle]
pub unsafe extern "C" fn __ku_http_client_async_step_v1(
    op: *mut Operation,
    io: *const Io,
    events: u32,
) -> u32 {
    if op.is_null() || io.is_null() {
        return 0;
    }
    let op = unsafe { &mut *op };
    if op.completed {
        return 4;
    }
    guarded(op, |op| op.step(unsafe { &*io }, events))
}
/// # Safety
/// Operation/output are live disjoint storage; call once at completed state.
#[no_mangle]
pub unsafe extern "C" fn __ku_http_client_async_take_v1(
    op: *mut Operation,
    out: *mut Output,
) -> u32 {
    if op.is_null()
        || out.is_null()
        || !(op as usize).is_multiple_of(align_of::<Operation>())
        || !(out as usize).is_multiple_of(align_of::<Output>())
        || overlaps(
            op as usize,
            size_of::<Operation>(),
            out as usize,
            size_of::<Output>(),
        )
    {
        return 2;
    }
    let op = unsafe { &mut *op };
    let out = unsafe { &mut *out };
    if out.size != size_of::<Output>() as u32 || !out.owner.is_null() {
        return 2;
    }
    let Some(result) = op.outcome.take() else {
        return 2;
    };
    let produced = catch_unwind(AssertUnwindSafe(|| -> Result<Owner, u32> {
        op.charge(size_of::<Owner>()).map_err(|_| 3u32)?;
        Ok(match result {
            Ok(response) => {
                op.charge(
                    response
                        .headers
                        .len()
                        .checked_mul(size_of::<Header>())
                        .ok_or(1u32)?,
                )
                .map_err(|_| 3u32)?;
                let mut headers = Vec::new();
                headers
                    .try_reserve_exact(response.headers.len())
                    .map_err(|_| 1u32)?;
                for (n, v) in &response.headers {
                    headers.push(Header {
                        name: Slice::view(n.as_bytes()),
                        value: Slice::view(v.as_bytes()),
                    });
                }
                Owner {
                    response: Some(response),
                    headers,
                    code: String::new(),
                    message: String::new(),
                }
            }
            Err(e) => Owner {
                response: None,
                headers: Vec::new(),
                code: e.code,
                message: e.message,
            },
        })
    }));
    match produced {
        Ok(Ok(owner)) => unsafe { publish_owner(owner, out) },
        Ok(Err(code)) => {
            out.status = 1;
            out.code = if code == 3 {
                Slice::view(b"memory_limit")
            } else {
                Slice::view(b"out_of_memory")
            };
            out.message = Slice::view(b"HTTP result allocation rejected");
            1
        }
        Err(_) => {
            out.status = 255;
            out.code = Slice::view(b"internal_panic");
            out.message = Slice::view(b"HTTP client failed internally");
            255
        }
    }
}
/// # Safety
/// A live operation is consumed exactly once, with no concurrent callback.
#[no_mangle]
pub unsafe extern "C" fn __ku_http_client_async_drop_v1(op: *mut Operation) -> u32 {
    if op.is_null() {
        return 0;
    }
    match catch_unwind(AssertUnwindSafe(|| unsafe {
        ptr::drop_in_place(op);
        dealloc(op.cast(), Layout::new::<Operation>())
    })) {
        Ok(()) => 0,
        Err(_) => 255,
    }
}

/// # Safety
/// A physically completed operation whose response was taken; no concurrent access.
/// Zero means it cannot be reused. The returned charge is for the bounded owned
/// cache record/buffers, separate from the former Task's receipt.
#[no_mangle]
pub unsafe extern "C" fn __ku_http_client_async_park_v1(op: *mut Operation) -> usize {
    catch_unwind(AssertUnwindSafe(|| unsafe { park(op) })).unwrap_or_default()
}
unsafe fn park(op: *mut Operation) -> usize {
    if op.is_null() {
        return 0;
    }
    let op = unsafe { &mut *op };
    if !op.completed
        || op.outcome.is_some()
        || op.peer_eof
        || !op.keep_alive
        || op.eof_body
        || op.cursor != op.incoming.len()
        || op.tls.as_ref().is_some_and(|t| t.wants_write())
    {
        return 0;
    }
    // No decrypted bytes may be carried into a different request. EOF, error
    // or unsolicited plaintext makes the transport ineligible for reuse.
    if let Some(tls) = &mut op.tls {
        let mut byte = [0u8; 1];
        match tls.reader().read(&mut byte) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            _ => return 0,
        }
    }
    // No request/response bytes or allocator capacity remain in the idle record.
    op.wire = Vec::new();
    op.incoming = Vec::new();
    op.body = Vec::new();
    op.headers = HashMap::new();
    op.request.method = String::new();
    op.request.url = String::new();
    op.request.body = None;
    op.request.headers = HashMap::new();
    let origin = op.url.origin().ascii_serialization();
    let Ok(minimal) = url::Url::parse(&(origin + "/")) else {
        return 0;
    };
    op.url = minimal;
    size_of::<Operation>()
        + op.url.as_str().len()
        + op.host.len()
        + if op.tls.is_some() { 131072 } else { 0 }
}
/// # Safety
/// Both operations are distinct, live and serialized. Return 2 leaves cached
/// untouched; 0 and 255 consume cached exactly once (255 is terminal panic).
#[no_mangle]
pub unsafe extern "C" fn __ku_http_client_async_reuse_v1(
    op: *mut Operation,
    cached: *mut Operation,
) -> u32 {
    if op.is_null() || cached.is_null() || op == cached {
        return 2;
    }
    let (op, old) = unsafe { (&mut *op, &mut *cached) };
    if op.completed
        || !old.completed
        || op.url.scheme() != old.url.scheme()
        || op.host != old.host
        || op.url.port_or_known_default() != old.url.port_or_known_default()
    {
        return 2;
    }
    op.tls = old.tls.take();
    op.tls_done = op.tls.is_some();
    unsafe { __ku_http_client_async_drop_v1(cached) }
}

#[cfg(test)]
mod tests {
    use super::*;
    unsafe extern "C" fn reserve(_: *mut c_void, _: usize) -> u32 {
        0
    }
    unsafe extern "C" fn reject(_: *mut c_void, _: usize) -> u32 {
        1
    }
    #[derive(Default)]
    struct PeakBudget {
        limit: usize,
        used: usize,
        peak: usize,
        reservations: usize,
        refunds: usize,
    }
    unsafe extern "C" fn peak_reserve(raw: *mut c_void, bytes: usize) -> u32 {
        let budget = unsafe { &mut *raw.cast::<PeakBudget>() };
        budget.reservations += 1;
        let Some(next) = budget.used.checked_add(bytes) else {
            return 1;
        };
        if next > budget.limit {
            return 1;
        }
        budget.used = next;
        budget.peak = budget.peak.max(next);
        0
    }
    unsafe extern "C" fn peak_refund(raw: *mut c_void, bytes: usize) -> u32 {
        let budget = unsafe { &mut *raw.cast::<PeakBudget>() };
        budget.refunds += 1;
        let Some(next) = budget.used.checked_sub(bytes) else {
            return 1;
        };
        budget.used = next;
        0
    }
    fn peak_callbacks(budget: &mut PeakBudget) -> Budget {
        Budget {
            context: (budget as *mut PeakBudget).cast(),
            reserve: peak_reserve,
            refund: peak_refund,
        }
    }
    #[test]
    fn async_owned_growth_accounts_old_plus_new_peak_without_accumulating() {
        let mut bytes = Vec::with_capacity(16);
        bytes.extend_from_slice(b"keep");
        let mut account = PeakBudget {
            limit: 32,
            used: bytes.capacity(),
            ..PeakBudget::default()
        };
        let callbacks = peak_callbacks(&mut account);
        assert_eq!(
            grow_owned_bytes(callbacks, &mut bytes, 32),
            Err("memory_limit")
        );
        assert_eq!(
            (&bytes[..], bytes.capacity(), account.used),
            (&b"keep"[..], 16, 16)
        );
        assert_eq!(account.refunds, 0);
        account.limit = 128;
        grow_owned_bytes(callbacks, &mut bytes, 32).unwrap();
        assert_eq!((account.used, account.peak), (32, 48));
        grow_owned_bytes(callbacks, &mut bytes, 48).unwrap();
        assert_eq!((account.used, account.peak), (48, 80));
        assert_eq!(&bytes[..], b"keep");
        let before = (account.reservations, account.refunds);
        grow_owned_bytes(callbacks, &mut bytes, 48).unwrap();
        assert_eq!((account.reservations, account.refunds), before);
    }
    #[test]
    fn async_owned_growth_allocation_failure_restores_receipt_and_old_data() {
        let mut bytes = Vec::with_capacity(16);
        bytes.extend_from_slice(b"keep");
        let mut account = PeakBudget {
            limit: usize::MAX,
            used: bytes.capacity(),
            ..PeakBudget::default()
        };
        // Vec rejects this layout deterministically, without a huge allocation.
        let impossible = (isize::MAX as usize) + 1;
        assert_eq!(
            grow_owned_bytes(peak_callbacks(&mut account), &mut bytes, impossible),
            Err("out_of_memory")
        );
        assert_eq!(
            (&bytes[..], bytes.capacity(), account.used),
            (&b"keep"[..], 16, 16)
        );
        assert_eq!((account.reservations, account.refunds), (1, 1));
    }
    #[test]
    fn async_decoded_growth_uses_same_peak_contract_and_stops_callbacks_on_drop() {
        let mut account = PeakBudget {
            limit: 32,
            used: 16,
            ..PeakBudget::default()
        };
        let mut decoded = Decoded {
            bytes: Vec::with_capacity(16),
            limit: 64,
            budget: peak_callbacks(&mut account),
            fault: None,
            live: true,
        };
        decoded.bytes.extend_from_slice(b"keep");
        assert!(decoded.write(&[b'x'; 17]).is_err());
        assert_eq!(decoded.fault, Some("memory_limit"));
        assert_eq!(
            (&decoded.bytes[..], decoded.bytes.capacity(), account.used),
            (&b"keep"[..], 16, 16)
        );
        account.limit = 64;
        decoded.fault = None;
        assert_eq!(decoded.write(&[b'x'; 17]).unwrap(), 17);
        assert_eq!((account.used, account.peak), (21, 37));
        decoded.live = false;
        let before = (account.reservations, account.refunds, account.used);
        assert!(decoded.write(b"late").is_err());
        drop(decoded);
        assert_eq!(
            (account.reservations, account.refunds, account.used),
            before
        );
    }
    fn operation(method: &str, url: &str) -> *mut Operation {
        let input = Input {
            size: size_of::<Input>() as u32,
            flags: 0,
            method: Slice::view(method.as_bytes()),
            url: Slice::view(url.as_bytes()),
            body: Slice::EMPTY,
            headers: ptr::null(),
            header_count: 0,
            timeout_ms: 500,
            max_body_bytes: 4096,
            caller_budget_ms: 0,
        };
        let mut out = ptr::null_mut();
        assert_eq!(
            unsafe {
                __ku_http_client_async_new_v1(
                    &input,
                    Budget {
                        context: ptr::null_mut(),
                        reserve,
                        refund: reserve,
                    },
                    &mut out,
                )
            },
            0
        );
        assert!(!out.is_null());
        out
    }
    fn feed(op: &mut Operation, bytes: &[u8], eof: bool) -> u32 {
        op.append(bytes).unwrap();
        match op.parse(eof) {
            Ok(true) => op.finish().unwrap(),
            Ok(false) => {
                op.compact();
                0
            }
            Err(e) => panic!("{} {}", e.code, e.message),
        }
    }
    #[test]
    fn async_codec_streams_chunked_gzip_non_2xx_and_preserves_public_headers() {
        let raw = operation("GET", "http://localhost/");
        let op = unsafe { &mut *raw };
        assert_eq!(
            feed(
                op,
                b"HTTP/1.1 404 Missing\r\nTransfer-Encoding: chunked\r\nX-Ku: yes\r\n\r\n",
                false
            ),
            0
        );
        for data in [b"1\r\na\r\n".as_slice(), b"1\r\nb\r\n", b"1\r\nc\r\n"] {
            assert_eq!(feed(op, data, false), 0);
            assert!(op.incoming.capacity() < HEADER_LIMIT)
        }
        assert_eq!(feed(op, b"0\r\nTrailer: yes\r\n\r\n", false), 4);
        let mut out = Output::empty();
        assert_eq!(unsafe { __ku_http_client_async_take_v1(raw, &mut out) }, 0);
        assert_eq!(out.http_status, 404);
        assert_eq!(unsafe { text(out.body) }.unwrap(), "abc");
        unsafe {
            __ku_http_client_release_v1(&mut out);
            __ku_http_client_async_drop_v1(raw);
        }
        for encoding in ["gzip", "x-gzip"] {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all("中文🙂".as_bytes()).unwrap();
            let compressed = encoder.finish().unwrap();
            let raw = operation("GET", "http://localhost/");
            let op = unsafe { &mut *raw };
            op.request.max_body_bytes = "中文🙂".len();
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Encoding: {encoding}\r\nContent-Length: {}\r\n\r\n",
                compressed.len()
            );
            assert_eq!(feed(op, header.as_bytes(), false), 0);
            assert_eq!(feed(op, &compressed, false), 4);
            let mut out = Output::empty();
            assert_eq!(unsafe { __ku_http_client_async_take_v1(raw, &mut out) }, 0);
            assert_eq!(unsafe { text(out.body) }.unwrap(), "中文🙂");
            let names = unsafe { slice::from_raw_parts(out.headers, out.header_count) };
            assert!(!names
                .iter()
                .any(|h| unsafe { text(h.name) }.unwrap() == "content-encoding"));
            unsafe {
                __ku_http_client_release_v1(&mut out);
                __ku_http_client_async_drop_v1(raw);
            }
        }
    }
    #[test]
    fn async_redirect_ipv6_auth_and_allocation_boundaries() {
        let raw = operation("POST", "http://alice:pw@[::1]:8080/path");
        let op = unsafe { &mut *raw };
        assert_eq!(op.host, "::1");
        assert!(std::str::from_utf8(&op.wire)
            .unwrap()
            .contains("Host: [::1]:8080\r\n"));
        assert!(std::str::from_utf8(&op.wire)
            .unwrap()
            .contains("Authorization: Basic YWxpY2U6cHc=\r\n"));
        // A redirect is followed at the header boundary; no body read or POST replay.
        assert_eq!(
            feed(
                op,
                b"HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 999999999\r\n\r\n",
                false
            ),
            8
        );
        assert_eq!(op.request.method, "GET");
        assert!(op.request.body.is_none());
        assert_eq!(op.url.path(), "/next");
        unsafe {
            __ku_http_client_async_drop_v1(raw);
        }
        let input = Input {
            size: size_of::<Input>() as u32,
            flags: 0,
            method: Slice::view(b"GET"),
            url: Slice::view(b"http://localhost/"),
            body: Slice::EMPTY,
            headers: ptr::null(),
            header_count: 0,
            timeout_ms: 100,
            max_body_bytes: 4096,
            caller_budget_ms: 0,
        };
        let mut out = ptr::null_mut();
        assert_eq!(
            unsafe {
                __ku_http_client_async_new_v1(
                    &input,
                    Budget {
                        context: ptr::null_mut(),
                        reserve: reject,
                        refund: reserve,
                    },
                    &mut out,
                )
            },
            3
        );
        assert!(out.is_null());
        let mut aliased = input;
        let before = aliased.size;
        let out = (&mut aliased as *mut Input).cast::<*mut Operation>();
        assert_eq!(
            unsafe {
                __ku_http_client_async_new_v1(
                    &aliased,
                    Budget {
                        context: ptr::null_mut(),
                        reserve,
                        refund: reserve,
                    },
                    out,
                )
            },
            2
        );
        assert_eq!(aliased.size, before);
    }
    #[test]
    fn async_result_owner_budget_rejection_preserves_memory_limit_and_releases() {
        let raw = operation("GET", "http://localhost/");
        let op = unsafe { &mut *raw };
        assert_eq!(
            feed(op, b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nx", false),
            4
        );
        op.budget.reserve = reject;
        let mut out = Output::empty();
        assert_eq!(unsafe { __ku_http_client_async_take_v1(raw, &mut out) }, 1);
        assert_eq!(unsafe { text(out.code) }.unwrap(), "memory_limit");
        assert!(out.owner.is_null());
        assert_eq!(unsafe { __ku_http_client_release_v1(&mut out) }, 0);
        assert_eq!(unsafe { __ku_http_client_async_drop_v1(raw) }, 0);
    }
    #[test]
    fn async_park_and_reuse_drop_old_request_payload_and_keep_same_origin() {
        let old = operation("GET", "http://localhost:8080/one");
        let op = unsafe { &mut *old };
        assert_eq!(
            feed(op, b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nx", false),
            4
        );
        let mut out = Output::empty();
        assert_eq!(unsafe { __ku_http_client_async_take_v1(old, &mut out) }, 0);
        unsafe {
            __ku_http_client_release_v1(&mut out);
        }
        assert!(unsafe { __ku_http_client_async_park_v1(old) } > 0);
        assert_eq!(op.wire.capacity(), 0);
        assert_eq!(op.incoming.capacity(), 0);
        assert_eq!(op.body.capacity(), 0);
        let other = operation("GET", "http://other.localhost:8080/two");
        assert_eq!(unsafe { __ku_http_client_async_reuse_v1(other, old) }, 2);
        // The rejected cross-origin operation did not consume the cache owner.
        assert_eq!(unsafe { __ku_http_client_async_drop_v1(other) }, 0);
        let new = operation("GET", "http://localhost:8080/two");
        assert_eq!(unsafe { __ku_http_client_async_reuse_v1(new, old) }, 0);
        unsafe {
            __ku_http_client_async_drop_v1(new);
        }
    }
}

#[cfg(test)]
mod tls_tests {
    use super::*;
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    use std::collections::VecDeque;
    unsafe extern "C" fn reserve(_: *mut c_void, _: usize) -> u32 {
        0
    }
    struct Peer {
        server: rustls::ServerConnection,
        incoming: Vec<u8>,
        outgoing: VecDeque<u8>,
        plaintext: Vec<u8>,
        responded: bool,
    }
    unsafe extern "C" fn read(raw: *mut c_void, data: *mut u8, capacity: usize) -> isize {
        let peer = unsafe { &mut *raw.cast::<Peer>() };
        if peer.outgoing.is_empty() {
            return -1;
        }
        let n = capacity.min(peer.outgoing.len());
        let target = unsafe { slice::from_raw_parts_mut(data, n) };
        for byte in target {
            *byte = peer.outgoing.pop_front().unwrap();
        }
        n as isize
    }
    unsafe extern "C" fn write(raw: *mut c_void, data: *const u8, length: usize) -> isize {
        let peer = unsafe { &mut *raw.cast::<Peer>() };
        peer.incoming
            .extend_from_slice(unsafe { slice::from_raw_parts(data, length) });
        length as isize
    }
    impl Peer {
        fn advance(&mut self) {
            if !self.incoming.is_empty() {
                let bytes = std::mem::take(&mut self.incoming);
                let mut input = io::Cursor::new(bytes);
                while input.position() < (input.get_ref().len() as u64) {
                    if self.server.read_tls(&mut input).unwrap() == 0 {
                        break;
                    }
                    if self.server.process_new_packets().is_err() {
                        return;
                    }
                }
            }
            let mut data = [0u8; 8192];
            loop {
                match self.server.reader().read(&mut data) {
                    Ok(0) => break,
                    Ok(n) => self.plaintext.extend_from_slice(&data[..n]),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => return,
                }
            }
            if !self.responded && self.plaintext.windows(4).any(|w| w == b"\r\n\r\n") {
                self.server
                    .writer()
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\ntls-ok")
                    .unwrap();
                self.responded = true;
            }
            let mut encrypted = Vec::new();
            while self.server.wants_write() {
                self.server.write_tls(&mut encrypted).unwrap();
            }
            self.outgoing.extend(encrypted);
        }
    }
    #[test]
    fn owner_tls_pump_uses_verified_custom_roots_and_hostname() {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        for (trusted, hostname_ok) in [(true, true), (true, false), (false, true)] {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let server = rustls::ServerConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![cert.cert.der().clone()],
                    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der())),
                )
                .unwrap();
            let mut peer = Peer {
                server: rustls::ServerConnection::new(Arc::new(server)).unwrap(),
                incoming: Vec::new(),
                outgoing: VecDeque::new(),
                plaintext: Vec::new(),
                responded: false,
            };
            let url = "https://localhost/";
            let input = Input {
                size: size_of::<Input>() as u32,
                flags: 0,
                method: Slice::view(b"GET"),
                url: Slice::view(url.as_bytes()),
                body: Slice::EMPTY,
                headers: ptr::null(),
                header_count: 0,
                timeout_ms: 500,
                max_body_bytes: 4096,
                caller_budget_ms: 0,
            };
            let mut op = ptr::null_mut();
            assert_eq!(
                unsafe {
                    __ku_http_client_async_new_v1(
                        &input,
                        Budget {
                            context: ptr::null_mut(),
                            reserve,
                            refund: reserve,
                        },
                        &mut op,
                    )
                },
                0
            );
            let mut roots = rustls::RootCertStore::empty();
            if trusted {
                roots.add(cert.cert.der().clone()).unwrap();
            }
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
            unsafe {
                (*op).tls = Some(
                    rustls::ClientConnection::new(
                        Arc::new(config),
                        rustls::pki_types::ServerName::try_from(if hostname_ok {
                            "localhost"
                        } else {
                            "wrong.localhost"
                        })
                        .unwrap()
                        .to_owned(),
                    )
                    .unwrap(),
                )
            };
            let io = Io {
                context: (&mut peer as *mut Peer).cast(),
                read,
                write,
            };
            let mut pending = 0;
            let mut completed = false;
            for _ in 0..128 {
                let events = 2 | u32::from(!peer.outgoing.is_empty());
                let flags = unsafe { __ku_http_client_async_step_v1(op, &io, events) };
                peer.advance();
                if flags & 4 != 0 {
                    completed = true;
                    break;
                }
                pending += 1;
            }
            assert!(completed && pending > 0);
            let mut out = Output::empty();
            let status = unsafe { __ku_http_client_async_take_v1(op, &mut out) };
            if trusted && hostname_ok {
                assert_eq!(status, 0);
                assert_eq!(unsafe { text(out.body) }.unwrap(), "tls-ok");
                assert!(peer.responded);
            } else {
                assert_eq!(status, 1);
                assert!(!peer.responded);
            }
            unsafe {
                __ku_http_client_release_v1(&mut out);
                __ku_http_client_async_drop_v1(op);
            }
        }
    }
}
