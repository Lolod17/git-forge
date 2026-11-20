use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);
const READ_BUFFER_SIZE: usize = 64 * 1024;
const MAX_HEADER_SIZE: usize = 64 * 1024;
const MAX_BODY_SIZE: usize = 512 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
    pub content_type: String,
    pub extra: Vec<(String, String)>,
}

impl Response {
    pub fn send_json(status: u16, body: &str) -> Self {
        let mut bytes = body.as_bytes().to_vec();
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        Self::send_json_bytes(status, bytes)
    }

    pub fn send_json_bytes(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            body,
            content_type: "application/json; charset=utf-8".to_string(),
            extra: Vec::new(),
        }
    }

    pub fn json_error(status: u16, message: &str) -> Self {
        Self::send_json(status, &format!("{{\"message\":{}}}", json_quote(message)))
    }
}

pub type Handler = dyn Fn(&Request, &[String]) -> Response + Send + Sync + 'static;

#[derive(Default)]
pub struct Router {
    routes: Vec<(String, Vec<String>, Box<Handler>)>,
}

impl Router {
    pub fn new() -> Self {
        Self { routes: Vec::new() }
    }

    pub fn register<F>(&mut self, method: &str, pattern: &str, handler: F)
    where
        F: Fn(&Request, &[String]) -> Response + Send + Sync + 'static,
    {
        self.routes.push((
            method.to_ascii_uppercase(),
            route_segments(pattern),
            Box::new(handler),
        ));
    }

    pub fn handle(&self, request: &Request) -> Response {
        let path = route_segments(&request.path);
        for (method, pattern, handler) in &self.routes {
            if *method != request.method.to_ascii_uppercase() {
                continue;
            }
            let Some(captures) = match_route(pattern, &path) else {
                continue;
            };
            return handler(request, &captures);
        }
        Response::json_error(404, "Not Found")
    }
}

fn route_segments(path: &str) -> Vec<String> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        Vec::new()
    } else {
        trimmed.split('/').map(str::to_string).collect()
    }
}

fn match_route(pattern: &[String], path: &[String]) -> Option<Vec<String>> {
    let mut captures = Vec::new();
    let wildcard = pattern.last().and_then(|part| part.strip_prefix('*'));
    let fixed_len = if wildcard.is_some() { pattern.len().saturating_sub(1) } else { pattern.len() };
    if (wildcard.is_none() && pattern.len() != path.len()) || path.len() < fixed_len {
        return None;
    }
    for (expected, actual) in pattern[..fixed_len].iter().zip(path.iter()) {
        if expected.starts_with('*') {
            return None;
        } else if let Some(name) = expected.strip_prefix(':') {
            if name.is_empty() || actual.is_empty() {
                return None;
            }
            captures.push(actual.clone());
        } else if expected != actual {
            return None;
        }
    }
    if wildcard.is_some() {
        if wildcard == Some("") || path.len() == fixed_len { return None; }
        captures.push(path[fixed_len..].join("/"));
    }
    Some(captures)
}

// ------------------------------------------------------------------ server: one poll() thread
//
// One thread owns every connection: accept, read, handle, write, all through poll(2).
// A thread per connection costs the trial pod (two CPUs, the suite beside us) far more than it costs
// a laptop: fifty concurrent clones became fifty schedulable tasks fighting one mutex. Here the store
// is touched by exactly one thread, so the lock is never contended and no request waits on a switch.

use std::net::Shutdown;
use std::os::unix::io::AsRawFd;


struct Conn {
    fd: i32,
    stream: TcpStream,
    inbuf: Vec<u8>,
    out: Vec<u8>,
    out_pos: usize,
    closing: bool,
}

impl Conn {
    fn new(stream: TcpStream) -> Self {
        let fd = stream.as_raw_fd();
        Self { fd, stream, inbuf: Vec::with_capacity(2048), out: Vec::new(), out_pos: 0, closing: false }
    }

    fn pending(&self) -> bool {
        self.out_pos < self.out.len()
    }

    /// Read everything the socket holds, then answer every complete request in the buffer.
    /// Returns false when the connection is dead.
    fn read_available(&mut self, router: &Router) -> bool {
        let mut buf = [0u8; READ_BUFFER_SIZE];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => {
                    self.closing = true;
                    break;
                }
                Ok(count) => self.inbuf.extend_from_slice(&buf[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.closing = true;
                    break;
                }
            }
        }
        loop {
            match parse_one(&mut self.inbuf) {
                Ok(Some(request)) => {
                    let response = match catch_unwind(AssertUnwindSafe(|| router.handle(&request))) {
                        Ok(response) => response,
                        Err(_) => Response::json_error(500, "Server Error"),
                    };
                    self.out.extend_from_slice(&response_bytes(response));
                }
                Ok(None) => break,
                Err(()) => {
                    self.out
                        .extend_from_slice(&response_bytes(Response::json_error(400, "Bad Request")));
                    self.closing = true;
                    break;
                }
            }
        }
        true
    }

    /// Write what is queued. Returns false when the connection is dead.
    fn flush_out(&mut self) -> bool {
        while self.out_pos < self.out.len() {
            match self.stream.write(&self.out[self.out_pos..]) {
                Ok(0) => return false,
                Ok(count) => self.out_pos += count,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return true,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return false,
            }
        }
        self.out.clear();
        self.out_pos = 0;
        true
    }
}

pub fn serve(listener: TcpListener, router: Arc<Router>) -> ! {
    if let Err(error) = listener.set_nonblocking(true) {
        panic!("the listener cannot go non-blocking: {error}");
    }
    let listen_fd = listener.as_raw_fd();
    let mut connections: Vec<Conn> = Vec::new();
    let mut poll_set: Vec<libc::pollfd> = Vec::with_capacity(64);

    loop {
        poll_set.clear();
        poll_set.push(libc::pollfd { fd: listen_fd, events: libc::POLLIN, revents: 0 });
        for conn in &connections {
            let mut events = libc::POLLIN;
            if conn.pending() {
                events |= libc::POLLOUT;
            }
            poll_set.push(libc::pollfd { fd: conn.fd, events, revents: 0 });
        }
        let ready = unsafe { libc::poll(poll_set.as_mut_ptr(), poll_set.len() as libc::nfds_t, -1) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            panic!("poll failed: {error}");
        }
        if ready == 0 {
            continue;
        }

        if poll_set[0].revents & libc::POLLIN != 0 {
            loop {
                match listener.accept() {
                    Ok((stream, _peer)) => {
                        let _ = stream.set_nodelay(true);
                        let _ = stream.set_nonblocking(true);
                        connections.push(Conn::new(stream));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        }

        let mut index = 0;
        while index < connections.len() {
            let revents = match poll_set.get(index + 1) {
                Some(entry) => entry.revents,
                None => 0,
            };
            let mut keep = true;
            if revents != 0 {
                let conn = &mut connections[index];
                // A peer that hangs up right after its request still gets an answer: read what is
                // buffered, answer it, flush, and only then let the connection go.
                let hung_up = revents & (libc::POLLHUP | libc::POLLERR) != 0;
                if revents & libc::POLLNVAL != 0 {
                    keep = false;
                }
                if keep && (revents & libc::POLLIN != 0 || hung_up) {
                    keep = conn.read_available(&router);
                }
                if keep && conn.pending() {
                    keep = conn.flush_out();
                }
                if keep && !conn.pending() && (conn.closing || hung_up) {
                    keep = false;
                }
            }
            if keep {
                index += 1;
            } else {
                let conn = connections.remove(index);
                let _ = conn.stream.shutdown(Shutdown::Both);
            }
        }
    }
}

/// Take one complete request off the front of `buffered`, or `Ok(None)` when more bytes are needed.
fn parse_one(buffered: &mut Vec<u8>) -> Result<Option<Request>, ()> {
    let Some(header_position) = buffered.windows(4).position(|window| window == b"\r\n\r\n") else {
        if buffered.len() > MAX_HEADER_SIZE {
            return Err(());
        }
        return Ok(None);
    };
    let header_end = header_position + 4;
    if header_end > MAX_HEADER_SIZE {
        return Err(());
    }
    let header_text = String::from_utf8(buffered[..header_position].to_vec()).map_err(|_| ())?;
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().ok_or(())?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().ok_or(())?;
    let target = request_parts.next().ok_or(())?;
    let version = request_parts.next().ok_or(())?;
    if request_parts.next().is_some()
        || method.is_empty()
        || !(version == "HTTP/1.1" || version == "HTTP/1.0")
        || !target.starts_with('/')
    {
        return Err(());
    }

    let mut headers: HashMap<String, String> = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line.split_once(':').ok_or(())?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
        {
            return Err(());
        }
        let name = name.to_ascii_lowercase();
        let value = value.trim().to_string();
        if let Some(existing) = headers.get_mut(&name) {
            if name == "content-length" && existing != &value {
                return Err(());
            }
            existing.push_str(", ");
            existing.push_str(&value);
        } else {
            headers.insert(name, value);
        }
    }

    if headers
        .get("transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    {
        return Err(());
    }
    let content_length = match headers.get("content-length") {
        Some(value) => value.parse::<usize>().map_err(|_| ())?,
        None => 0,
    };
    if content_length > MAX_BODY_SIZE {
        return Err(());
    }
    let request_end = header_end.checked_add(content_length).ok_or(())?;
    if buffered.len() < request_end {
        return Ok(None);
    }

    let (path, query) = parse_target(target).ok_or(())?;
    let body = buffered[header_end..request_end].to_vec();
    buffered.drain(..request_end);
    Ok(Some(Request {
        method: method.to_ascii_uppercase(),
        path,
        query,
        headers,
        body,
    }))
}
/// Take one complete request off the front of `buffered`, or `Ok(None)` when more bytes are needed.
fn parse_target(target: &str) -> Option<(String, Vec<(String, String)>)> {
    let (raw_path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    let mut decoded_path = Vec::new();
    for segment in raw_path.split('/') {
        decoded_path.push(percent_decode(segment, false)?);
    }
    let path = decoded_path.join("/");
    let mut query = Vec::new();
    if !raw_query.is_empty() {
        for item in raw_query.split('&') {
            let (key, value) = item.split_once('=').unwrap_or((item, ""));
            query.push((percent_decode(key, true)?, percent_decode(value, true)?));
        }
    }
    Some((path, query))
}

fn percent_decode(value: &str, plus_as_space: bool) -> Option<String> {
    let input = value.as_bytes();
    let mut decoded = Vec::with_capacity(input.len());
    let mut position = 0;
    while position < input.len() {
        match input[position] {
            b'%' => {
                if position + 2 >= input.len() {
                    return None;
                }
                let high = hex_value(input[position + 1])?;
                let low = hex_value(input[position + 2])?;
                decoded.push((high << 4) | low);
                position += 3;
            }
            b'+' if plus_as_space => {
                decoded.push(b' ');
                position += 1;
            }
            byte => {
                decoded.push(byte);
                position += 1;
            }
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn response_bytes(response: Response) -> Vec<u8> {
    let request_id = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut output = Vec::with_capacity(response.body.len() + 256);
    output.extend_from_slice(
        format!(
            "HTTP/1.1 {} {}\r\ncontent-length: {}\r\nconnection: keep-alive\r\ncontent-type: {}\r\nx-github-request-id: {:016x}\r\n",
            response.status,
            reason_phrase(response.status),
            response.body.len(),
            response.content_type,
            request_id
        )
        .as_bytes(),
    );
    for (name, value) in response.extra {
        if valid_header_value(&name) && valid_header_value(&value) {
            output.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
    }
    output.extend_from_slice(b"\r\n");
    output.extend_from_slice(&response.body);
    output
}

fn valid_header_value(value: &str) -> bool {
    !value.contains('\r') && !value.contains('\n')
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        500 => "Internal Server Error",
        _ => "Response",
    }
}

pub fn json_quote(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character < ' ' => {
                output.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => output.push(character),
        }
    }
    output.push('"');
    output
}
