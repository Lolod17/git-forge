#![recursion_limit = "256"]

pub mod git;
pub mod githttp;
pub mod graphql;
pub mod http;
pub mod mcp;
pub mod rest;
pub mod store;

use std::env;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

struct EnvAddress {
    authority: String,
    host: String,
    port: u16,
    base_path: String,
}

struct EnvResponse {
    status: u16,
    body: Vec<u8>,
}

fn parse_env_address(url: &str) -> Result<EnvAddress, String> {
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        "MAKEFUN_ENV_URL must use plain http:// for the native lifecycle".to_string()
    })?;
    let split = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..split];
    if authority.is_empty() {
        return Err("MAKEFUN_ENV_URL has no host".to_string());
    }
    let base_path = rest[split..].trim_end_matches('/').to_string();

    let (host, port) = if authority.starts_with('[') {
        let close = authority
            .find(']')
            .ok_or_else(|| "MAKEFUN_ENV_URL has an invalid IPv6 host".to_string())?;
        let host = authority[1..close].to_string();
        let port = if authority.len() > close + 1 {
            authority[close + 1..]
                .strip_prefix(':')
                .ok_or_else(|| "MAKEFUN_ENV_URL has an invalid port".to_string())?
                .parse::<u16>()
                .map_err(|_| "MAKEFUN_ENV_URL has an invalid port".to_string())?
        } else {
            80
        };
        (host, port)
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        if host.is_empty() {
            return Err("MAKEFUN_ENV_URL has no host".to_string());
        }
        (
            host.to_string(),
            port.parse::<u16>()
                .map_err(|_| "MAKEFUN_ENV_URL has an invalid port".to_string())?,
        )
    } else {
        (authority.to_string(), 80)
    };

    Ok(EnvAddress {
        authority: authority.to_string(),
        host,
        port,
        base_path,
    })
}

fn read_env_response(stream: &mut TcpStream) -> Result<EnvResponse, String> {
    let mut received = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut header_end = None;
    let mut expected_length = None;
    let mut is_chunked = false;

    loop {
        if let Some(end) = header_end {
            if let Some(length) = expected_length {
                if received.len() >= end + length {
                    break;
                }
            } else if is_chunked
                && received[end..]
                    .windows(5)
                    .any(|window| window == b"0\r\n\r\n")
            {
                break;
            }
        }

        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => received.extend_from_slice(&chunk[..count]),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                break;
            }
            Err(error) => return Err(format!("reading env response failed: {error}")),
        }

        if header_end.is_none() {
            if let Some(position) = received.windows(4).position(|window| window == b"\r\n\r\n") {
                let end = position + 4;
                let headers = std::str::from_utf8(&received[..position])
                    .map_err(|_| "env sent invalid response headers".to_string())?;
                for line in headers.lines().skip(1) {
                    if let Some((name, value)) = line.split_once(':') {
                        let value = value.trim();
                        if name.eq_ignore_ascii_case("content-length") {
                            expected_length = value.parse::<usize>().ok();
                        } else if name.eq_ignore_ascii_case("transfer-encoding")
                            && value.to_ascii_lowercase().contains("chunked")
                        {
                            is_chunked = true;
                        }
                    }
                }
                header_end = Some(end);
            }
        }
    }

    let end = header_end.ok_or_else(|| "env sent an incomplete HTTP response".to_string())?;
    let header_text = std::str::from_utf8(&received[..end])
        .map_err(|_| "env sent invalid response headers".to_string())?;
    let status = header_text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| "env sent an invalid HTTP status".to_string())?;
    let raw_body = &received[end..];
    let body = if is_chunked {
        decode_chunked(raw_body).unwrap_or_else(|| raw_body.to_vec())
    } else if let Some(length) = expected_length {
        raw_body[..raw_body.len().min(length)].to_vec()
    } else {
        raw_body.to_vec()
    };

    Ok(EnvResponse { status, body })
}

fn decode_chunked(raw: &[u8]) -> Option<Vec<u8>> {
    let mut decoded = Vec::new();
    let mut position = 0;
    loop {
        let line_end = raw[position..]
            .windows(2)
            .position(|window| window == b"\r\n")?
            + position;
        let size_text = std::str::from_utf8(&raw[position..line_end]).ok()?;
        let size_text = size_text.split(';').next()?;
        let size = usize::from_str_radix(size_text.trim(), 16).ok()?;
        position = line_end + 2;
        if size == 0 {
            return Some(decoded);
        }
        let chunk_end = position.checked_add(size)?;
        if raw.len() < chunk_end + 2 || &raw[chunk_end..chunk_end + 2] != b"\r\n" {
            return None;
        }
        decoded.extend_from_slice(&raw[position..chunk_end]);
        position = chunk_end + 2;
    }
}

fn env_request(
    address: &EnvAddress,
    method: &str,
    endpoint: &str,
    body: &[u8],
) -> Result<EnvResponse, String> {
    let target = format!("{}{}", address.base_path, endpoint);
    let socket = if address.host.contains(':') {
        format!("[{}]:{}", address.host, address.port)
    } else {
        format!("{}:{}", address.host, address.port)
    };
    let socket_address = socket
        .to_socket_addrs()
        .map_err(|error| format!("resolving env address failed: {error}"))?
        .next()
        .ok_or_else(|| "MAKEFUN_ENV_URL did not resolve to an address".to_string())?;
    let mut stream = TcpStream::connect_timeout(&socket_address, Duration::from_secs(2))
        .map_err(|error| format!("connecting to env failed: {error}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));

    let headers = format!(
        "{method} {target} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n",
        address.authority,
        body.len()
    );
    stream
        .write_all(headers.as_bytes())
        .and_then(|_| stream.write_all(body))
        .map_err(|error| format!("writing env request failed: {error}"))?;
    read_env_response(&mut stream)
}

fn register(address: &EnvAddress, port: u16) -> bool {
    let body = format!("{{\"port\":{port}}}");
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if let Ok(response) = env_request(address, "POST", "/register", body.as_bytes()) {
            if response.status == 200 || response.status == 409 {
                return true;
            }
        }
        thread::sleep(Duration::from_millis(250));
    }
    false
}

fn wait_for_done(address: &EnvAddress) {
    loop {
        if let Ok(response) = env_request(address, "GET", "/done", &[]) {
            let compact: Vec<u8> = response
                .body
                .iter()
                .copied()
                .filter(|byte| !byte.is_ascii_whitespace())
                .collect();
            if compact
                .windows(b"\"done\":true".len())
                .any(|window| window == b"\"done\":true")
            {
                return;
            }
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn root_response(request: &http::Request) -> http::Response {
    let host = request
        .headers
        .get("host")
        .map(String::as_str)
        .unwrap_or("127.0.0.1");
    let origin = format!("http://{host}");
    let body = format!(
        "{{\"current_user_url\":{},\"repository_url\":{},\"user_repositories_url\":{},\"issues_url\":{},\"rate_limit_url\":{}}}",
        http::json_quote(&format!("{origin}/api/v3/user")),
        http::json_quote(&format!("{origin}/api/v3/repos/{{owner}}/{{repo}}")),
        http::json_quote(&format!("{origin}/api/v3/user/repos")),
        http::json_quote(&format!("{origin}/api/v3/issues")),
        http::json_quote(&format!("{origin}/api/v3/rate_limit")),
    );
    let mut response = http::Response::send_json(200, &body);
    response
        .extra
        .push(("x-oauth-scopes".to_string(), "repo, read:org".to_string()));
    response
        .extra
        .push(("x-accepted-oauth-scopes".to_string(), "repo".to_string()));
    response.extra.push((
        "x-github-media-type".to_string(),
        "github.v3; format=json".to_string(),
    ));
    response
}

fn authorize(request: &http::Request, token: Option<&str>) -> Result<(), &'static str> {
    let authorization = match request.headers.get("authorization") {
        Some(value) if !value.trim().is_empty() => value.trim(),
        _ => return Err("missing"),
    };
    let mut parts = authorization.split_whitespace();
    let scheme = parts.next().unwrap_or_default();
    let value = parts.collect::<Vec<_>>().join(" ");
    let presented = if scheme.eq_ignore_ascii_case("bearer") || scheme.eq_ignore_ascii_case("token")
    {
        value
    } else if scheme.eq_ignore_ascii_case("basic") {
        decode_base64(&value)
            .and_then(|decoded| String::from_utf8(decoded).ok())
            .map(|decoded| match decoded.split_once(':') {
                Some((_, password)) => password.to_string(),
                None => decoded,
            })
            .unwrap_or_default()
    } else {
        return Err("missing");
    };
    if presented.is_empty() {
        return Err("missing");
    }
    if let Some(expected) = token {
        if presented != expected {
            return Err("bad");
        }
    }
    Ok(())
}

fn git_route(
    forge: &Arc<std::sync::Mutex<store::Forge>>,
    request: &http::Request,
    captures: &[String],
    action: &str,
) -> http::Response {
    let mut forge = forge.lock().expect("forge mutex poisoned");
    match authorize(request, forge.token.as_deref()) {
        Ok(()) => {}
        Err("missing") => return githttp::auth_error("Authentication required"),
        _ => return githttp::auth_error("Bad credentials"),
    }
    let Some(owner) = captures.first() else {
        return githttp::missing_repo();
    };
    let Some(repo_segment) = captures.get(1) else {
        return githttp::missing_repo();
    };
    let Some(repo_name) = repo_segment.strip_suffix(".git") else {
        return githttp::missing_repo();
    };
    let Some(mut repo) = forge.repo_by_name(owner, repo_name) else {
        return githttp::missing_repo();
    };
    let result = match action {
        "info/refs" => {
            let service = request
                .query
                .iter()
                .find(|(key, _)| key == "service")
                .map(|(_, value)| value.as_str())
                .unwrap_or("");
            githttp::info_refs(
                &forge,
                &repo,
                service,
                githttp::request_git_protocol(request),
            )
        }
        "git-upload-pack" => githttp::upload_pack(
            &forge,
            &mut repo,
            &request.body,
            githttp::request_git_protocol(request),
        ),
        "git-receive-pack" => githttp::receive_pack(&mut forge, &mut repo, &request.body),
        _ => {
            let mut response = http::Response::send_json(405, "{\"message\":\"Method Not Allowed\"}");
            response.content_type = "text/plain; charset=utf-8".to_string();
            response.body = b"Method Not Allowed\n".to_vec();
            response
        }
    };
    if request.method == "HEAD" {
        let mut result = result;
        result.body.clear();
        result
    } else {
        result
    }
}

fn decode_base64(value: &str) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    let mut accumulator = 0u32;
    let mut bits = 0u8;
    for byte in value.bytes() {
        if byte == b'=' {
            break;
        }
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32;
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push(((accumulator >> bits) & 0xff) as u8);
        }
    }
    Some(output)
}

fn main() {
    let _ = flate2::Compression::fast();
    let env_url = env::var("MAKEFUN_ENV_URL")
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_string();
    if env_url.is_empty() {
        eprintln!("[forge] MAKEFUN_ENV_URL is not set: there is no env to register with");
        std::process::exit(1);
    }
    let address = match parse_env_address(&env_url) {
        Ok(address) => address,
        Err(message) => {
            eprintln!("[forge] {message}");
            std::process::exit(1);
        }
    };

    let listener = match TcpListener::bind("0.0.0.0:0") {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("[forge] failed to bind server socket: {error}");
            std::process::exit(1);
        }
    };
    let port = match listener.local_addr() {
        Ok(address) => address.port(),
        Err(error) => {
            eprintln!("[forge] failed to read bound port: {error}");
            std::process::exit(1);
        }
    };
    println!("[forge] bound {port}");
    eprintln!("[forge] bound {port}");
    let _ = std::io::stdout().flush();

    if !register(&address, port) {
        eprintln!("[forge] env did not accept registration within 60 seconds");
        std::process::exit(1);
    }
    println!("[forge] registered");
    eprintln!("[forge] registered");
    let _ = std::io::stdout().flush();

    let login = env::var("FORGE_LOGIN")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "adaiversen".to_string());
    let token = env::var("MAKEFUN_SHOP_TOKEN")
        .ok()
        .filter(|value| !value.is_empty());
    let forge = store::Forge::new(login, token);
    let forge = Arc::new(std::sync::Mutex::new(forge));
    let mut router = http::Router::new();
    router.register("GET", "/healthz", |_request, _captures| {
        http::Response::send_json(200, "{\"ok\":true}")
    });
    router.register("GET", "/api/v3", {
        let token = forge.lock().expect("forge mutex poisoned").token.clone();
        move |request, _captures| match authorize(request, token.as_deref()) {
            Ok(()) => root_response(request),
            Err("missing") => {
                let mut response = http::Response::json_error(401, "Requires authentication");
                response.extra.push((
                    "www-authenticate".to_string(),
                    "Bearer realm=\"gitforge\"".to_string(),
                ));
                response
                    .extra
                    .push(("x-oauth-scopes".to_string(), "repo, read:org".to_string()));
                response
            }
            _ => {
                let mut response = http::Response::json_error(401, "Bad credentials");
                response.extra.push((
                    "www-authenticate".to_string(),
                    "Bearer realm=\"gitforge\"".to_string(),
                ));
                response
                    .extra
                    .push(("x-oauth-scopes".to_string(), "repo, read:org".to_string()));
                response
            }
        }
    });
    for method in ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"] {
        router.register(method, "/api/v3/*path", {
            let forge = Arc::clone(&forge);
            let token = forge.lock().expect("forge mutex poisoned").token.clone();
            move |request, captures| match authorize(request, token.as_deref()) {
                Ok(()) => {
                    let path = captures.first().map(String::as_str).unwrap_or("");
                    let host = request.headers.get("host").map(String::as_str).unwrap_or("127.0.0.1");
                    let origin = format!("http://{host}");
                    let mut result = rest::handle(&mut forge.lock().expect("forge mutex poisoned"), request, path, &origin);
                    if request.method == "HEAD" { result.body.clear(); }
                    result
                }
                Err("missing") => {
                    let mut response = http::Response::json_error(401, "Requires authentication");
                    response.extra.push(("www-authenticate".to_string(), "Bearer realm=\"gitforge\"".to_string()));
                    response.extra.push(("x-oauth-scopes".to_string(), "repo, read:org".to_string()));
                    response
                }
                _ => {
                    let mut response = http::Response::json_error(401, "Bad credentials");
                    response.extra.push(("www-authenticate".to_string(), "Bearer realm=\"gitforge\"".to_string()));
                    response.extra.push(("x-oauth-scopes".to_string(), "repo, read:org".to_string()));
                    response
                }
            }
        });
    }
    for method in ["GET", "HEAD"] {
        router.register(method, "/:owner/:repo.git/info/refs", {
            let forge = Arc::clone(&forge);
            move |request, captures| git_route(&forge, request, captures, "info/refs")
        });
    }
    router.register("POST", "/:owner/:repo.git/git-upload-pack", {
        let forge = Arc::clone(&forge);
        move |request, captures| git_route(&forge, request, captures, "git-upload-pack")
    });
    router.register("POST", "/:owner/:repo.git/git-receive-pack", {
        let forge = Arc::clone(&forge);
        move |request, captures| git_route(&forge, request, captures, "git-receive-pack")
    });
    router.register("POST", "/mcp", {
        let forge = Arc::clone(&forge);
        let token = forge.lock().expect("forge mutex poisoned").token.clone();
        move |request, _captures| match authorize(request, token.as_deref()) {
            Ok(()) => {
                let host = request.headers.get("host").map(String::as_str).unwrap_or("127.0.0.1");
                mcp::handle(&mut forge.lock().expect("forge mutex poisoned"), request, &format!("http://{host}"))
            }
            Err("missing") => {
                let mut response = http::Response::json_error(401, "Requires authentication");
                response.extra.push(("www-authenticate".to_string(), "Bearer realm=\"gitforge\"".to_string()));
                response
            }
            _ => {
                let mut response = http::Response::json_error(401, "Bad credentials");
                response.extra.push(("www-authenticate".to_string(), "Bearer realm=\"gitforge\"".to_string()));
                response
            }
        }
    });
    router.register("POST", "/api/graphql", {
        let forge = Arc::clone(&forge);
        let token = forge.lock().expect("forge mutex poisoned").token.clone();
        move |request, _captures| match authorize(request, token.as_deref()) {
            Ok(()) => {
                let host = request.headers.get("host").map(String::as_str).unwrap_or("127.0.0.1");
                let origin = format!("http://{host}");
                graphql::handle(&mut forge.lock().expect("forge mutex poisoned"), request, &origin)
            }
            Err("missing") => http::Response::json_error(401, "Requires authentication"),
            _ => http::Response::json_error(401, "Bad credentials"),
        }
    });
    let router = Arc::new(router);

    let server_router = Arc::clone(&router);
    let _server_thread = thread::spawn(move || http::serve(listener, server_router));
    wait_for_done(&address);
    std::process::exit(0);
}
