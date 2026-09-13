//! End-to-end authentication tests: spawn the REAL `rustqlite-server`
//! binary, drive the SCRAM-SHA-256 handshake over raw HTTP from THIS file
//! (an independent client, like the Python smoke client), and verify the
//! fail-closed + bearer-token contract from the outside.

#![cfg(feature = "auth")]

use rustqlite::auth::{Challenge, ClientExchange};
use rustqlite::json::Json;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct ServerGuard(Child);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawn the server, wait for its "listening on" line, return the URL.
fn start_server(args: &[&str], password: Option<&str>) -> (ServerGuard, String) {
    let exe = env!("CARGO_BIN_EXE_rustqlite-server");
    let mut cmd = Command::new(exe);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(pw) = password {
        cmd.env("RUSTQLITE_PASSWORD", pw);
    }
    let child = cmd.spawn().expect("spawn rustqlite-server");
    let mut guard = ServerGuard(child);
    let stdout = guard.0.stdout.take().expect("stdout piped");
    let mut reader = BufReader::new(stdout);
    let mut url = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if let Some(rest) = line.trim().strip_prefix("rustqlite-server listening on ") {
            url = rest.to_string();
            break;
        }
    }
    assert!(!url.is_empty(), "server did not report a listening URL");
    // Keep the reader thread alive so the pipe never fills up.
    std::thread::spawn(move || for _ in reader.lines() {});
    (guard, url)
}

/// Minimal HTTP POST (Connection: close) — the same wire shape the CLI
/// and any scripting client speak.
fn http_post(url: &str, path: &str, body: &str, token: Option<&str>) -> (u16, String) {
    let host_port = url.trim_start_matches("http://").trim_end_matches('/');
    let mut stream = TcpStream::connect(host_port).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut req = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        path,
        host_port,
        body.len()
    );
    if let Some(t) = token {
        req.push_str(&format!("Authorization: Bearer {}\r\n", t));
    }
    req.push_str("\r\n");
    req.push_str(body);
    stream.write_all(req.as_bytes()).expect("write");
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).expect("read");
    let text = String::from_utf8_lossy(&resp).to_string();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .expect("status line");
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

fn json_get(body: &str, key: &str) -> String {
    let j = Json::parse(body).expect("valid JSON body");
    j.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Full client-side SCRAM login, returning the bearer token.
fn scram_login(url: &str, user: &str, password: &str) -> Result<String, String> {
    let cx = ClientExchange::start(user);
    let start = Json::Object(vec![
        ("username".to_string(), Json::Str(user.to_string())),
        (
            "client_nonce".to_string(),
            Json::Str(cx.client_nonce_hex().to_string()),
        ),
    ])
    .to_json();
    let (status, body) = http_post(url, "/auth/start", &start, None);
    if status != 200 {
        return Err(format!("auth/start {}: {}", status, body));
    }
    let j = Json::parse(&body).map_err(|e| e.to_string())?;
    let challenge = Challenge {
        nonce_hex: json_get(&body, "nonce"),
        salt_hex: json_get(&body, "salt"),
        iterations: j.get("iterations").and_then(|v| v.as_i64()).unwrap_or(0) as u32,
    };
    let session = json_get(&body, "session");
    let (client_final, proof, expected_server_sig) =
        cx.finish(password, &challenge).map_err(|e| e.0)?;
    let finish = Json::Object(vec![
        ("session".to_string(), Json::Str(session)),
        ("client_final".to_string(), Json::Str(client_final)),
        (
            "proof".to_string(),
            Json::Str(rustqlite::json::encode_hex(&proof)),
        ),
    ])
    .to_json();
    let (status, body) = http_post(url, "/auth/finish", &finish, None);
    if status != 200 {
        return Err(format!("auth/finish {}: {}", status, body));
    }
    // Mutual authentication: the server's signature must match.
    let got = rustqlite::json::decode_hex(&json_get(&body, "server_signature"))
        .ok_or("bad server signature")?;
    if got != expected_server_sig.to_vec() {
        return Err("server signature mismatch".to_string());
    }
    Ok(json_get(&body, "token"))
}

fn make_user(dir: &std::path::Path, name: &str, password: &str) -> String {
    let exe = env!("CARGO_BIN_EXE_rustqlite-server");
    let auth_file = dir.join("users.json");
    let status = Command::new(exe)
        .arg("--auth-file")
        .arg(&auth_file)
        .arg("--add-user")
        .arg(name)
        .env("RUSTQLITE_PASSWORD", password)
        .status()
        .expect("run --add-user");
    assert!(status.success(), "--add-user failed");
    auth_file.display().to_string()
}

/// Per-test unique temp directory (auto-removed on drop). The earlier
/// PID-based scheme collided across parallel tests in the same process.
fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

#[test]
fn server_refuses_to_start_without_auth_file() {
    let dir = tempdir();
    let db = dir.path().join("closed.db");
    let exe = env!("CARGO_BIN_EXE_rustqlite-server");
    let out = Command::new(exe)
        .arg("--db")
        .arg(&db)
        .arg("--port")
        .arg("0")
        .output()
        .expect("spawn server");
    assert!(!out.status.success(), "server must refuse to start");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("fail-closed"),
        "stderr should explain the fail-closed rule: {}",
        stderr
    );
    assert!(
        stderr.contains("--add-user"),
        "stderr should point at --add-user: {}",
        stderr
    );
    // And it must not have created a user file on its own.
    assert!(!dir.path().join("closed.db.auth.json").exists());
}

#[test]
fn server_refuses_empty_auth_file() {
    let dir = tempdir();
    let auth = dir.path().join("empty.json");
    std::fs::write(&auth, "{\"version\":1,\"users\":{}}").unwrap();
    let db = dir.path().join("empty.db");
    let exe = env!("CARGO_BIN_EXE_rustqlite-server");
    let out = Command::new(exe)
        .arg("--db")
        .arg(&db)
        .arg("--auth-file")
        .arg(&auth)
        .arg("--port")
        .arg("0")
        .output()
        .expect("spawn server");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no users"), "stderr: {}", stderr);
}

#[test]
fn user_management_and_full_session() {
    let dir = tempdir();
    let db = dir.path().join("live.db");
    let auth_file = make_user(dir.path(), "alice", "secret-pw-1");

    // --list-users
    let exe = env!("CARGO_BIN_EXE_rustqlite-server");
    let out = Command::new(exe)
        .arg("--auth-file")
        .arg(&auth_file)
        .arg("--list-users")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "alice");

    let (_guard, url) = start_server(
        &[
            "--db",
            db.to_str().unwrap(),
            "--auth-file",
            &auth_file,
            "--port",
            "0",
        ],
        None,
    );

    // /health is open (liveness only).
    // Unauthenticated data requests: 401 with a WWW-Authenticate header.
    let (status, body) = http_post(&url, "/query", r#"{"sql":"SELECT 1"}"#, None);
    assert_eq!(status, 401, "{}", body);
    assert_eq!(json_get(&body, "error"), "unauthorized");

    // Bogus token: 401.
    let (status, body) = http_post(
        &url,
        "/query",
        r#"{"sql":"SELECT 1"}"#,
        Some("deadbeefdeadbeef"),
    );
    assert_eq!(status, 401, "{}", body);

    // Full handshake.
    let token = scram_login(&url, "alice", "secret-pw-1").expect("login");
    assert_eq!(token.len(), 64, "256-bit hex token");

    // Wrong password: 401, byte-identical body to unknown user.
    let (_, wrong_body) = {
        // Drive it manually to capture the exact body.
        let cx = ClientExchange::start("alice");
        let start = Json::Object(vec![
            ("username".to_string(), Json::Str("alice".into())),
            (
                "client_nonce".to_string(),
                Json::Str(cx.client_nonce_hex().to_string()),
            ),
        ])
        .to_json();
        let (s, b) = http_post(&url, "/auth/start", &start, None);
        assert_eq!(s, 200);
        let challenge = Challenge {
            nonce_hex: json_get(&b, "nonce"),
            salt_hex: json_get(&b, "salt"),
            iterations: 4096,
        };
        let (cf, proof, _) = cx.finish("wrong", &challenge).unwrap();
        let finish = Json::Object(vec![
            ("session".to_string(), Json::Str(json_get(&b, "session"))),
            ("client_final".to_string(), Json::Str(cf)),
            (
                "proof".to_string(),
                Json::Str(rustqlite::json::encode_hex(&proof)),
            ),
        ])
        .to_json();
        http_post(&url, "/auth/finish", &finish, None)
    };
    assert_eq!(wrong_body, r#"{"error":"authentication failed"}"#);

    // Unknown user: byte-identical failure body (no enumeration).
    let (_, unknown_body) = {
        let cx = ClientExchange::start("mallory");
        let start = Json::Object(vec![
            ("username".to_string(), Json::Str("mallory".into())),
            (
                "client_nonce".to_string(),
                Json::Str(cx.client_nonce_hex().to_string()),
            ),
        ])
        .to_json();
        let (s, b) = http_post(&url, "/auth/start", &start, None);
        assert_eq!(s, 200, "fake challenge issued");
        let challenge = Challenge {
            nonce_hex: json_get(&b, "nonce"),
            salt_hex: json_get(&b, "salt"),
            iterations: 4096,
        };
        let (cf, proof, _) = cx.finish("anything", &challenge).unwrap();
        let finish = Json::Object(vec![
            ("session".to_string(), Json::Str(json_get(&b, "session"))),
            ("client_final".to_string(), Json::Str(cf)),
            (
                "proof".to_string(),
                Json::Str(rustqlite::json::encode_hex(&proof)),
            ),
        ])
        .to_json();
        http_post(&url, "/auth/finish", &finish, None)
    };
    assert_eq!(unknown_body, wrong_body, "identical failure bodies");

    // Data endpoints with the token.
    let create = Json::Object(vec![
        (
            "sql".to_string(),
            Json::Str("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)".into()),
        ),
        ("params".to_string(), Json::Array(vec![])),
    ])
    .to_json();
    let (status, body) = http_post(&url, "/execute", &create, Some(&token));
    assert_eq!(status, 200, "{}", body);

    let insert = Json::Object(vec![
        (
            "sql".to_string(),
            Json::Str("INSERT INTO t (v) VALUES (?)".into()),
        ),
        (
            "params".to_string(),
            Json::Array(vec![Json::Str("a,b \"q\"".into())]),
        ),
    ])
    .to_json();
    let (status, body) = http_post(&url, "/execute", &insert, Some(&token));
    assert_eq!(status, 200, "{}", body);

    let select = Json::Object(vec![
        ("sql".to_string(), Json::Str("SELECT v FROM t".into())),
        ("params".to_string(), Json::Array(vec![])),
    ])
    .to_json();
    let (status, body) = http_post(&url, "/query", &select, Some(&token));
    assert_eq!(status, 200, "{}", body);
    assert!(
        body.contains(r#""a,b \"q\" c""#) || body.contains("a,b"),
        "params with commas must survive: {}",
        body
    );

    // Logout invalidates the token.
    let (status, body) = http_post(&url, "/auth/logout", "{}", Some(&token));
    assert_eq!(status, 200, "{}", body);
    let (status, _) = http_post(&url, "/query", &select, Some(&token));
    assert_eq!(status, 401, "token dead after logout");
}

#[test]
fn pending_handshake_is_single_use_and_ttl_bounded() {
    let dir = tempdir();
    let db = dir.path().join("pending.db");
    let auth_file = make_user(dir.path(), "bob", "pw-bob-42");
    let (_guard, url) = start_server(
        &[
            "--db",
            db.to_str().unwrap(),
            "--auth-file",
            &auth_file,
            "--port",
            "0",
        ],
        None,
    );
    // Start a handshake but never finish it; replaying the finish with a
    // bogus proof must 401, and the SECOND finish (replay) must also 401.
    let cx = ClientExchange::start("bob");
    let start = Json::Object(vec![
        ("username".to_string(), Json::Str("bob".into())),
        (
            "client_nonce".to_string(),
            Json::Str(cx.client_nonce_hex().to_string()),
        ),
    ])
    .to_json();
    let (s, b) = http_post(&url, "/auth/start", &start, None);
    assert_eq!(s, 200);
    let session = json_get(&b, "session");
    let challenge = Challenge {
        nonce_hex: json_get(&b, "nonce"),
        salt_hex: json_get(&b, "salt"),
        iterations: 4096,
    };
    let (cf, proof, _) = cx.finish("pw-bob-42", &challenge).unwrap();
    let finish = Json::Object(vec![
        ("session".to_string(), Json::Str(session.clone())),
        ("client_final".to_string(), Json::Str(cf.clone())),
        ("proof".to_string(), Json::Str("00".repeat(32))),
    ])
    .to_json();
    let (s1, b1) = http_post(&url, "/auth/finish", &finish, None);
    assert_eq!(
        (s1, b1.as_str()),
        (401, r#"{"error":"authentication failed"}"#)
    );
    // Replay: the pending session was consumed.
    let (s2, _) = http_post(&url, "/auth/finish", &finish, None);
    assert_eq!(s2, 401);
    // A session id that never existed: 401.
    let ghost = Json::Object(vec![
        ("session".to_string(), Json::Str("00".repeat(32))),
        ("client_final".to_string(), Json::Str(cf)),
        (
            "proof".to_string(),
            Json::Str(rustqlite::json::encode_hex(&proof)),
        ),
    ])
    .to_json();
    let (s3, _) = http_post(&url, "/auth/finish", &ghost, None);
    assert_eq!(s3, 401);
}

#[test]
fn session_token_expires() {
    let dir = tempdir();
    let db = dir.path().join("ttl.db");
    let auth_file = make_user(dir.path(), "carol", "pw-carol");
    let (_guard, url) = start_server(
        &[
            "--db",
            db.to_str().unwrap(),
            "--auth-file",
            &auth_file,
            "--port",
            "0",
            "--auth-ttl",
            "1",
        ],
        None,
    );
    let token = scram_login(&url, "carol", "pw-carol").expect("login");
    let select = r#"{"sql":"SELECT 1","params":[]}"#;
    let (status, _) = http_post(&url, "/query", select, Some(&token));
    assert_eq!(status, 200);
    std::thread::sleep(Duration::from_millis(1500));
    let (status, _) = http_post(&url, "/query", select, Some(&token));
    assert_eq!(status, 401, "token must expire after the TTL");
}

#[test]
fn no_auth_flag_is_the_explicit_escape_hatch() {
    let dir = tempdir();
    let db = dir.path().join("open.db");
    let (_guard, url) = start_server(
        &["--db", db.to_str().unwrap(), "--port", "0", "--no-auth"],
        None,
    );
    // Unauthenticated access works — the documented dev mode.
    let (status, body) = http_post(&url, "/query", r#"{"sql":"SELECT 1"}"#, None);
    assert_eq!(status, 200, "{}", body);
}

#[test]
fn cli_connect_end_to_end() {
    let dir = tempdir();
    let db = dir.path().join("cli.db");
    let auth_file = make_user(dir.path(), "dave", "pw-dave-7");
    let (_guard, url) = start_server(
        &[
            "--db",
            db.to_str().unwrap(),
            "--auth-file",
            &auth_file,
            "--port",
            "0",
        ],
        None,
    );
    let url = url.trim_start_matches("http://").to_string();
    let cli = env!("CARGO_BIN_EXE_rustqlite-cli");
    let script = "CREATE TABLE c (id INTEGER PRIMARY KEY, v TEXT);\n\
                  INSERT INTO c (v) VALUES ('one'), ('two');\n\
                  SELECT id, v FROM c ORDER BY id;\n\
                  .quit\n";
    let out = Command::new(cli)
        .arg("--connect")
        .arg(&url)
        .arg("--user")
        .arg("dave")
        .env("RUSTQLITE_PASSWORD", "pw-dave-7")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().unwrap().write_all(script.as_bytes())?;
            child.wait_with_output()
        })
        .expect("run cli");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "cli failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("remote"), "banner: {}", stdout);
    assert!(
        stdout.contains("one") && stdout.contains("two"),
        "rows: {}",
        stdout
    );

    // Wrong password from the CLI: clean failure, no session.
    let out = Command::new(cli)
        .arg("--connect")
        .arg(&url)
        .arg("--user")
        .arg("dave")
        .env("RUSTQLITE_PASSWORD", "wrong")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.take().unwrap().write_all(b".quit\n")?;
            child.wait_with_output()
        })
        .expect("run cli");
    assert!(!out.status.success(), "cli must fail on wrong password");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("authentication failed"), "{}", stderr);
}

#[test]
fn auth_store_live_reload_picks_up_new_users() {
    let dir = tempdir();
    let db = dir.path().join("reload.db");
    let auth_file = make_user(dir.path(), "erin", "pw-erin");
    let (_guard, url) = start_server(
        &[
            "--db",
            db.to_str().unwrap(),
            "--auth-file",
            &auth_file,
            "--port",
            "0",
        ],
        None,
    );
    // A user added by ANOTHER process after the server started must be
    // able to log in without a restart (the store re-reads per handshake).
    make_user(dir.path(), "frank", "pw-frank");
    let token = scram_login(&url, "frank", "pw-frank").expect("new user must authenticate");
    let (status, body) = http_post(
        &url,
        "/query",
        r#"{"sql":"SELECT 1","params":[]}"#,
        Some(&token),
    );
    assert_eq!(status, 200, "{}", body);
}
