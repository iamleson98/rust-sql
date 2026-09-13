//! HTTP/JSON server for rustqlite.
//!
//! Exposes a simple REST API for executing SQL queries against a database.
//!
//! ## Authentication (SCRAM-SHA-256, Postgres-style)
//!
//! The server is **fail-closed**: it refuses to start unless a user store
//! with at least one user exists (or you explicitly pass `--no-auth` for
//! an unsecured development server). Every `/query` and `/execute` request
//! then requires a session token obtained through a two-step SCRAM
//! handshake — the password itself never crosses the wire and is stored
//! only as a salted, iterated verifier (see `rustqlite::auth`).
//!
//! User management (the server exits after these):
//!   rustqlite-server --auth-file users.json --add-user alice
//!     (password via prompt, or $RUSTQLITE_PASSWORD, or --password)
//!   rustqlite-server --auth-file users.json --del-user alice
//!   rustqlite-server --auth-file users.json --list-users
//!
//! ## Endpoints
//!
//! POST /auth/start
//!   Body: {"username":"alice","client_nonce":"<hex>"}
//!   Returns: {"session":"<hex>","nonce":"<combined hex>","salt":"<hex>","iterations":N}
//!
//! POST /auth/finish
//!   Body: {"session":"<hex>","client_final":"c=biws,r=<combined>","proof":"<hex>"}
//!   Returns: {"token":"<hex>","expires_in":N,"server_signature":"<hex>"}
//!   Failure: 401 {"error":"authentication failed"} — byte-identical for
//!   wrong password, unknown user, and bad session (no enumeration).
//!
//! POST /query  (Authorization: Bearer <token>)
//!   Body: {"sql":"SELECT ...","params":[...]}
//!   Returns: {"columns":[...],"rows":[[...],...]}
//!   Params: null | number | "string" | {"blob":"<hex>"}
//!
//! POST /execute  (Authorization: Bearer <token>)
//!   Body: {"sql":"INSERT ...","params":[...]}
//!   Returns: {"ok":true} or 400 {"error":"..."}
//!
//! POST /auth/logout  (Authorization: Bearer <token>)
//!   Returns: {"ok":true}
//!
//! GET /health
//!   Returns: {"status":"ok"} — the only unauthenticated endpoint, and it
//!   exposes nothing but liveness.
//!
//! ## Concurrency model
//!
//! The database is wrapped in `Arc<RwLock<Database>>`. The TCP accept loop
//! is multi-threaded (N worker threads pull from the listener in parallel),
//! and request parsing happens in parallel across cores.
//!
//! **Reads (`/query`)** take a READ lock and call `Database::query_shared()`
//! (a `&self` method). Multiple readers run concurrently — no writer-head
//! contention, no serial lock across readers.
//!
//! **Writes (`/execute`)** take a WRITE lock and call `Database::execute()`
//! (a `&mut self` method). Writers are serialized, but a writer doesn't
//! block concurrent readers (it just queues for the next available write
//! window — readers proceed).
//!
//! This is enabled by the interior-mutability refactor on `Pager` (cache is
//! `RwLock<HashMap>`, page size / n_pages / freelist are `AtomicU32`, file
//! I/O uses positioned `pread`/`pwrite` so threads don't share an offset)
//! and on `Database` (`stmt_cache`/`root_overrides`/`max_rowids` are `RwLock`).
//!
//! ## Usage
//!
//!   rustqlite-server --db /path/to/db.sqlite --port 8080 --threads 8
//!   rustqlite-server --db /path/to/db.sqlite --auth-file users.json
//!   rustqlite-server --db /path/to/db.sqlite --auth-ttl 3600
//!   rustqlite-server --db /path/to/db.sqlite --no-auth   # UNSECURED

#[cfg(feature = "auth")]
use parking_lot::Mutex;
#[cfg(not(feature = "auth"))]
use parking_lot::RwLock;
#[cfg(feature = "auth")]
use parking_lot::RwLock;
use rustqlite::{Database, Value};
use std::env;
use std::sync::Arc;
use std::thread;
#[cfg(feature = "auth")]
use std::time::Duration;
use tiny_http::{Header, Method, Response, Server};

#[cfg(feature = "auth")]
use rustqlite::auth::{random_bytes, SessionStore, UserStore};

struct State {
    /// RwLock: read-locked for `/query` (concurrent reads), write-locked
    /// for `/execute` (serialized writes).
    db: RwLock<Database>,
    /// Authentication state. Present whenever auth is enabled; the
    /// fail-closed startup check guarantees a non-empty user store.
    #[cfg(feature = "auth")]
    auth: Option<AuthState>,
}

#[cfg(feature = "auth")]
struct AuthState {
    /// The user store file the server started with; re-read on every
    /// `/auth/start` so external `--add-user` edits are picked up without
    /// a restart.
    path: Option<std::path::PathBuf>,
    /// Current in-memory user store (source of truth = the file).
    users: RwLock<UserStore>,
    /// Single-use pending handshakes (`/auth/start` -> `/auth/finish`),
    /// pruned after PENDING_TTL.
    pending: Mutex<
        std::collections::HashMap<String, (rustqlite::auth::ServerExchange, std::time::Instant)>,
    >,
    /// Live bearer tokens.
    sessions: SessionStore,
    /// Token lifetime, for the `expires_in` field.
    ttl: Duration,
}

/// A pending handshake dies after 60 seconds.
#[cfg(feature = "auth")]
const PENDING_TTL: Duration = Duration::from_secs(60);

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut db_path = ":memory:".to_string();
    let mut port: u16 = 8080;
    let mut host = "127.0.0.1".to_string();
    let mut n_threads: usize = 4;

    // Auth-related flags.
    #[cfg(feature = "auth")]
    let mut auth_file: Option<String> = None;
    let mut no_auth = false;
    #[cfg(feature = "auth")]
    let mut auth_ttl: u64 = 3600;
    #[cfg(feature = "auth")]
    let mut add_user: Option<String> = None;
    #[cfg(feature = "auth")]
    let mut del_user: Option<String> = None;
    #[cfg(feature = "auth")]
    let mut list_users = false;
    #[cfg(feature = "auth")]
    let mut password_flag: Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        let arg = args[i].as_str();
        let value = |i: &mut usize| -> Option<String> {
            *i += 1;
            args.get(*i).cloned()
        };
        match arg {
            "--db" | "-d" => {
                if let Some(v) = value(&mut i) {
                    db_path = v;
                }
            }
            "--port" | "-p" => {
                if let Some(v) = value(&mut i) {
                    port = v.parse().unwrap_or(8080);
                }
            }
            "--host" | "-H" => {
                if let Some(v) = value(&mut i) {
                    host = v;
                }
            }
            "--threads" | "-t" => {
                if let Some(v) = value(&mut i) {
                    n_threads = v.parse().unwrap_or(4);
                }
            }
            #[cfg(feature = "auth")]
            "--auth-file" => {
                if let Some(v) = value(&mut i) {
                    auth_file = Some(v);
                }
            }
            #[cfg(feature = "auth")]
            "--auth-ttl" => {
                if let Some(v) = value(&mut i) {
                    auth_ttl = v.parse().unwrap_or(3600);
                }
            }
            #[cfg(feature = "auth")]
            "--add-user" => {
                if let Some(v) = value(&mut i) {
                    add_user = Some(v);
                }
            }
            #[cfg(feature = "auth")]
            "--del-user" => {
                if let Some(v) = value(&mut i) {
                    del_user = Some(v);
                }
            }
            #[cfg(feature = "auth")]
            "--list-users" => {
                list_users = true;
            }
            #[cfg(feature = "auth")]
            "--password" => {
                if let Some(v) = value(&mut i) {
                    password_flag = Some(v);
                }
            }
            #[cfg(not(feature = "auth"))]
            "--auth-file" | "--auth-ttl" | "--add-user" | "--del-user" | "--list-users"
            | "--password" => {
                eprintln!("error: flag {} needs a build with the `auth` feature", arg);
                eprintln!("       cargo build --bin rustqlite-server --features auth");
                std::process::exit(1);
            }
            "--no-auth" => {
                no_auth = true;
            }
            "--help" | "-h" => {
                print_help();
                return;
            }
            _ => {
                eprintln!("unknown argument: {}", arg);
                eprintln!("try --help");
                std::process::exit(1);
            }
        }
        i += 1;
    }

    // ---- User management modes (no server) --------------------------------
    #[cfg(feature = "auth")]
    if add_user.is_some() || del_user.is_some() || list_users {
        let auth_path = auth_file
            .clone()
            .unwrap_or_else(|| default_auth_file(&db_path));
        exit_on_user_management(
            &auth_path,
            add_user.as_deref(),
            del_user.as_deref(),
            list_users,
            password_flag.as_deref(),
        );
        return;
    }
    // (In a no-auth build the user-management flags are rejected during
    // argument parsing with a feature hint, so there is no path here.)

    // ---- Fail-closed auth configuration -----------------------------------
    // The rule: unless --no-auth is EXPLICITLY passed, the server refuses
    // to serve a single unauthenticated request — it will not start
    // without a user store holding at least one user.
    #[cfg(feature = "auth")]
    let auth_state = if no_auth {
        eprintln!("WARNING: --no-auth passed — the server is UNSECURED.");
        eprintln!("         Anyone who can reach the port can read and modify the database.");
        None
    } else {
        let auth_path = auth_file
            .clone()
            .unwrap_or_else(|| default_auth_file(&db_path));
        let users = match UserStore::load(std::path::Path::new(&auth_path)) {
            Ok(u) => u,
            Err(e) => {
                eprintln!("error: {}", e);
                eprintln!();
                eprintln!("This server is fail-closed: it refuses to start without a user store.");
                eprintln!("Create one first:");
                eprintln!(
                    "  rustqlite-server --auth-file {} --add-user <name>",
                    auth_path
                );
                eprintln!();
                eprintln!("Only pass --no-auth if you deliberately want an UNSECURED server.");
                std::process::exit(1);
            }
        };
        if users.is_empty() {
            eprintln!(
                "error: auth file {} contains no users — refusing to start",
                auth_path
            );
            eprintln!(
                "Add one: rustqlite-server --auth-file {} --add-user <name>",
                auth_path
            );
            std::process::exit(1);
        }
        Some(AuthState {
            path: Some(std::path::PathBuf::from(&auth_path)),
            users: RwLock::new(users),
            pending: Mutex::new(std::collections::HashMap::new()),
            sessions: SessionStore::new(Duration::from_secs(auth_ttl)),
            ttl: Duration::from_secs(auth_ttl),
        })
    };
    #[cfg(not(feature = "auth"))]
    if !no_auth {
        eprintln!("error: authentication is required by default, but this binary was built");
        eprintln!("       without the `auth` feature. Either rebuild with --features auth,");
        eprintln!("       or pass --no-auth for an UNSECURED development server.");
        std::process::exit(1);
    }

    let db = if db_path == ":memory:" {
        Database::open_in_memory().unwrap_or_else(|e| {
            eprintln!("error opening in-memory database: {}", e);
            std::process::exit(1);
        })
    } else {
        Database::open(&db_path).unwrap_or_else(|e| {
            eprintln!("error opening {}: {}", db_path, e);
            std::process::exit(1);
        })
    };

    let state = Arc::new(State {
        db: RwLock::new(db),
        #[cfg(feature = "auth")]
        auth: auth_state,
    });
    let addr = format!("{}:{}", host, port);
    let server = Arc::new(Server::http(&addr).unwrap_or_else(|e| {
        eprintln!("error binding to {}: {}", addr, e);
        std::process::exit(1);
    }));
    // Report the ACTUAL bound address (port 0 = kernel-assigned).
    let bound = server.server_addr().to_string();
    println!("rustqlite-server listening on http://{}", bound);
    println!("Database: {}", db_path);
    println!("Threads:  {}", n_threads);
    #[cfg(feature = "auth")]
    if let Some(a) = &state.auth {
        println!(
            "Auth:     SCRAM-SHA-256 ({} user(s), token TTL {}s)",
            a.users.read().len(),
            a.ttl.as_secs()
        );
    } else if no_auth {
        println!("Auth:     DISABLED (--no-auth) — UNSECURED");
    }
    #[cfg(not(feature = "auth"))]
    if no_auth {
        println!("Auth:     unavailable in this build (--no-auth) — UNSECURED");
    }

    // Multi-threaded: spawn N worker threads, each pulling requests from the
    // shared server. tiny_http's `incoming_requests()` is thread-safe and
    // distributes connections across consumers.
    let mut handles = Vec::new();
    for _ in 0..n_threads {
        let state = Arc::clone(&state);
        let server = Arc::clone(&server);
        handles.push(thread::spawn(move || {
            for request in server.incoming_requests() {
                handle_request(request, &state);
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
}

fn print_help() {
    println!("rustqlite-server — HTTP/JSON SQL server for rustqlite");
    println!();
    println!("Usage:");
    println!("  rustqlite-server [OPTIONS]");
    println!();
    println!("Options:");
    println!("  --db PATH        Database file path (default: in-memory)");
    println!("  --port PORT      TCP port (0 = kernel-assigned; default: 8080)");
    println!("  --host HOST      Bind address (default: 127.0.0.1)");
    println!("  --threads N      Worker thread count (default: 4)");
    println!();
    println!("Authentication (SCRAM-SHA-256, fail-closed by default):");
    println!("  --auth-file PATH  User store (default: <db>.auth.json)");
    println!("  --auth-ttl SECS   Session token lifetime (default: 3600)");
    println!("  --add-user NAME   Add/replace a user, then exit");
    println!("  --del-user NAME   Remove a user, then exit");
    println!("  --list-users      List users, then exit");
    println!("  --password PW     Password for --add-user (prompt otherwise;");
    println!("                    the RUSTQLITE_PASSWORD env var also works)");
    println!("  --no-auth         Run UNSECURED, no authentication (explicit opt-out)");
}

/// Default auth-file path: `<db>.auth.json` beside the database. In-memory
/// databases have no sensible default — the caller errors out with guidance.
#[cfg(feature = "auth")]
fn default_auth_file(db_path: &str) -> String {
    if db_path == ":memory:" {
        "rustqlite.auth.json".to_string()
    } else {
        format!("{}.auth.json", db_path)
    }
}

/// Password input for `--add-user`: env var / hidden prompt handling lives
/// in `rustqlite::auth::read_password_interactive` (shared with the CLI).
#[cfg(feature = "auth")]
fn read_password(flag: Option<&str>, prompt: &str) -> Result<String, String> {
    rustqlite::auth::read_password_interactive(flag, prompt).map_err(|e| e.0)
}

#[cfg(feature = "auth")]
fn exit_on_user_management(
    auth_path: &str,
    add_user: Option<&str>,
    del_user: Option<&str>,
    list_users: bool,
    password_flag: Option<&str>,
) {
    let path = std::path::Path::new(auth_path);
    // Load-or-empty: --add-user bootstraps a missing file; pure
    // --list-users/--del-user on a missing file is an error.
    let mut store = match UserStore::load(path) {
        Ok(s) => s,
        Err(e) => {
            if add_user.is_some() && !path.exists() {
                UserStore::new()
            } else {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
        }
    };
    if let Some(name) = add_user {
        let pw =
            read_password(password_flag, &format!("password for {}: ", name)).unwrap_or_else(|e| {
                eprintln!("error: {}", e);
                std::process::exit(1);
            });
        if pw.is_empty() {
            eprintln!("error: empty password");
            std::process::exit(1);
        }
        let confirm = read_password(password_flag, &format!("confirm password for {}: ", name))
            .unwrap_or_else(|e| {
                eprintln!("error: {}", e);
                std::process::exit(1);
            });
        if pw != confirm {
            eprintln!("error: passwords do not match");
            std::process::exit(1);
        }
        let iterations = rustqlite::auth::DEFAULT_ITERATIONS;
        store.upsert(name, &pw, iterations).unwrap_or_else(|e| {
            eprintln!("error: {}", e);
            std::process::exit(1);
        });
        store.save(path).unwrap_or_else(|e| {
            eprintln!("error: {}", e);
            std::process::exit(1);
        });
        println!("user {} added to {}", name, auth_path);
    }
    if let Some(name) = del_user {
        if !store.remove(name) {
            eprintln!("error: no such user: {}", name);
            std::process::exit(1);
        }
        store.save(path).unwrap_or_else(|e| {
            eprintln!("error: {}", e);
            std::process::exit(1);
        });
        println!("user {} removed from {}", name, auth_path);
    }
    if list_users {
        if store.is_empty() {
            println!("(no users in {})", auth_path);
        } else {
            for u in store.usernames() {
                println!("{}", u);
            }
        }
    }
}

fn handle_request(request: tiny_http::Request, state: &State) {
    let url = request.url().to_string();
    let method = request.method().clone();

    // Routes that never require a token:
    //   /health — liveness only, exposes nothing.
    //   /auth/start, /auth/finish — the handshake itself.
    match (&method, url.as_str()) {
        (Method::Get, "/health") => {
            let _ = request.respond(
                Response::from_string(r#"{"status":"ok"}"#).with_header(content_type_json()),
            );
        }
        (Method::Post, "/auth/start") => {
            #[cfg(feature = "auth")]
            handle_auth_start(request, state);
            #[cfg(not(feature = "auth"))]
            {
                let _ = request;
                respond_unavailable(request);
            }
        }
        (Method::Post, "/auth/finish") => {
            #[cfg(feature = "auth")]
            handle_auth_finish(request, state);
            #[cfg(not(feature = "auth"))]
            {
                let _ = request;
                respond_unavailable(request);
            }
        }
        (Method::Post, "/query") => {
            #[cfg(feature = "auth")]
            if state.auth.is_some() && !authorized(&request, state) {
                respond_unauthorized(request);
                return;
            }
            handle_query(request, state);
        }
        (Method::Post, "/execute") => {
            #[cfg(feature = "auth")]
            if state.auth.is_some() && !authorized(&request, state) {
                respond_unauthorized(request);
                return;
            }
            handle_execute(request, state);
        }
        (Method::Post, "/auth/logout") => {
            #[cfg(feature = "auth")]
            handle_auth_logout(request, state);
            #[cfg(not(feature = "auth"))]
            {
                let _ = request;
                respond_unavailable(request);
            }
        }
        _ => {
            let _ = request.respond(
                Response::from_string(r#"{"error":"not found"}"#)
                    .with_status_code(404)
                    .with_header(content_type_json()),
            );
        }
    }
}

#[cfg(feature = "auth")]
fn respond_unavailable(request: tiny_http::Request) {
    let _ = request.respond(
        Response::from_string(
            r#"{"error":"auth endpoints unavailable: server built without the auth feature"}"#,
        )
        .with_status_code(501)
        .with_header(content_type_json()),
    );
}

#[cfg(not(feature = "auth"))]
fn respond_unavailable(request: tiny_http::Request) {
    let _ = request.respond(
        Response::from_string(
            r#"{"error":"auth endpoints unavailable: server built without the auth feature"}"#,
        )
        .with_status_code(501)
        .with_header(content_type_json()),
    );
}

/// Extract the bearer token from `Authorization: Bearer <hex>`.
#[cfg(feature = "auth")]
fn bearer_token(request: &tiny_http::Request) -> Option<String> {
    for h in request.headers() {
        if h.field
            .as_str()
            .as_str()
            .eq_ignore_ascii_case("Authorization")
        {
            let v = h.value.as_str().trim();
            if let Some(rest) = v
                .strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
            {
                return Some(rest.trim().to_string());
            }
            return None;
        }
    }
    None
}

/// Check the bearer token WITHOUT consuming the request: returns `true`
/// when the request may proceed. Callers respond 401 via
/// [`respond_unauthorized`] on `false` (that helper consumes the request).
#[cfg(feature = "auth")]
fn authorized(request: &tiny_http::Request, state: &State) -> bool {
    let auth = state
        .auth
        .as_ref()
        .expect("authorized() called with auth enabled");
    bearer_token(request).is_some_and(|token| auth.sessions.validate(&token))
}

#[cfg(feature = "auth")]
fn respond_unauthorized(request: tiny_http::Request) {
    let _ = request.respond(
        Response::from_string(r#"{"error":"unauthorized"}"#)
            .with_status_code(401)
            .with_header(content_type_json())
            .with_header(www_authenticate()),
    );
}

#[cfg(feature = "auth")]
fn www_authenticate() -> Header {
    Header::from_bytes(&b"WWW-Authenticate"[..], &b"Bearer"[..]).unwrap()
}

#[cfg(feature = "auth")]
fn handle_auth_start(mut request: tiny_http::Request, state: &State) {
    let auth = match state.auth.as_ref() {
        Some(a) => a,
        None => return respond_unavailable(request),
    };
    let body = match read_body(&mut request) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error reading body: {}", e);
            return;
        }
    };
    let parsed = match rustqlite::json::Json::parse(&body) {
        Ok(p) => p,
        Err(e) => {
            respond_error(request, &format!("bad request: {}", e));
            return;
        }
    };
    let username = match parsed.get("username").and_then(|v| v.as_str()) {
        Some(u) => u.to_string(),
        None => {
            respond_error(request, "missing 'username' field");
            return;
        }
    };
    let client_nonce = match parsed.get("client_nonce").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            respond_error(request, "missing 'client_nonce' field");
            return;
        }
    };

    // Re-read the user store each handshake: external --add-user edits are
    // picked up without a restart (the verifier map is cheap to load).
    if let Some(p) = &auth.path {
        if let Ok(fresh) = UserStore::load(p) {
            *auth.users.write() = fresh;
        }
    }
    let verifier = auth.users.read().verifier(&username).cloned();

    let (exchange, challenge) =
        match rustqlite::auth::ServerExchange::start(&username, &client_nonce, verifier.as_ref()) {
            Ok(x) => x,
            Err(_) => {
                // Bad nonce or bad username shape: generic 401, same body
                // as a failed proof — no information leaks.
                let _ = request.respond(
                    Response::from_string(r#"{"error":"authentication failed"}"#)
                        .with_status_code(401)
                        .with_header(content_type_json()),
                );
                return;
            }
        };
    let session_id = rustqlite::json::encode_hex(&random_bytes(32));
    {
        let mut pending = auth.pending.lock();
        prune_pending(&mut pending);
        pending.insert(session_id.clone(), (exchange, std::time::Instant::now()));
    }
    let resp = rustqlite::json::Json::Object(vec![
        (
            "session".to_string(),
            rustqlite::json::Json::Str(session_id),
        ),
        (
            "nonce".to_string(),
            rustqlite::json::Json::Str(challenge.nonce_hex),
        ),
        (
            "salt".to_string(),
            rustqlite::json::Json::Str(challenge.salt_hex),
        ),
        (
            "iterations".to_string(),
            rustqlite::json::Json::Int(challenge.iterations as i64),
        ),
    ]);
    let _ = request.respond(Response::from_string(resp.to_json()).with_header(content_type_json()));
}

#[cfg(feature = "auth")]
fn prune_pending(
    pending: &mut std::collections::HashMap<
        String,
        (rustqlite::auth::ServerExchange, std::time::Instant),
    >,
) {
    let now = std::time::Instant::now();
    pending.retain(|_, (_, at)| now.duration_since(*at) < PENDING_TTL);
}

#[cfg(feature = "auth")]
fn handle_auth_finish(mut request: tiny_http::Request, state: &State) {
    let auth = match state.auth.as_ref() {
        Some(a) => a,
        None => return respond_unavailable(request),
    };
    let body = match read_body(&mut request) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error reading body: {}", e);
            return;
        }
    };
    let parsed = match rustqlite::json::Json::parse(&body) {
        Ok(p) => p,
        Err(_) => {
            respond_auth_failed(request);
            return;
        }
    };
    let session = parsed
        .get("session")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let client_final = parsed
        .get("client_final")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let proof_hex = parsed
        .get("proof")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let proof = rustqlite::json::decode_hex(&proof_hex).unwrap_or_default();

    // Single-use: the exchange is REMOVED before verification (replaying
    // the same finish, or racing two finishes, both land on generic 401).
    let exchange = {
        let mut pending = auth.pending.lock();
        prune_pending(&mut pending);
        pending.remove(&session)
    };
    let result = match exchange {
        Some((exchange, _)) => exchange.finish(&client_final, &proof),
        None => Err(rustqlite::auth::AuthError(
            "authentication failed".to_string(),
        )),
    };
    match result {
        Ok(server_signature) => {
            let token = auth.sessions.issue();
            let resp = rustqlite::json::Json::Object(vec![
                ("token".to_string(), rustqlite::json::Json::Str(token)),
                (
                    "expires_in".to_string(),
                    rustqlite::json::Json::Int(auth.ttl.as_secs() as i64),
                ),
                (
                    "server_signature".to_string(),
                    rustqlite::json::Json::Str(rustqlite::json::encode_hex(&server_signature)),
                ),
            ]);
            let _ = request
                .respond(Response::from_string(resp.to_json()).with_header(content_type_json()));
        }
        Err(_) => respond_auth_failed(request),
    }
}

#[cfg(feature = "auth")]
fn respond_auth_failed(request: tiny_http::Request) {
    let _ = request.respond(
        Response::from_string(r#"{"error":"authentication failed"}"#)
            .with_status_code(401)
            .with_header(content_type_json()),
    );
}

#[cfg(feature = "auth")]
fn handle_auth_logout(request: tiny_http::Request, state: &State) {
    let auth = match state.auth.as_ref() {
        Some(a) => a,
        None => return respond_unavailable(request),
    };
    match bearer_token(&request) {
        Some(token) if auth.sessions.validate(&token) => {
            auth.sessions.revoke(&token);
            let _ = request
                .respond(Response::from_string(r#"{"ok":true}"#).with_header(content_type_json()));
        }
        _ => {
            let _ = request.respond(
                Response::from_string(r#"{"error":"unauthorized"}"#)
                    .with_status_code(401)
                    .with_header(content_type_json()),
            );
        }
    }
}

fn handle_query(mut request: tiny_http::Request, state: &State) {
    let body = match read_body(&mut request) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error reading body: {}", e);
            return;
        }
    };
    let parsed = match parse_request(&body) {
        Ok(p) => p,
        Err(e) => {
            respond_error(request, &e);
            return;
        }
    };

    // READ lock — multiple readers can run concurrently. This is the key
    // change that enables true concurrency: before the interior-mutability
    // refactor on `Pager` and `Database`, we had to take the write lock
    // here because `query()` required `&mut self`. Now `query_shared()`
    // takes `&self` and uses interior mutability for cache fills.
    let guard = state.db.read();
    match guard.query_with_columns(&parsed.sql, parsed.params) {
        Ok((cols, rows)) => {
            let json = format_query_result(&cols, &rows);
            let _ = request.respond(Response::from_string(json).with_header(content_type_json()));
        }
        Err(e) => {
            respond_error(request, &e.to_string());
        }
    }
}

fn handle_execute(mut request: tiny_http::Request, state: &State) {
    let body = match read_body(&mut request) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error reading body: {}", e);
            return;
        }
    };
    let parsed = match parse_request(&body) {
        Ok(p) => p,
        Err(e) => {
            respond_error(request, &e);
            return;
        }
    };

    let mut guard = state.db.write();
    match guard.execute(&parsed.sql, parsed.params) {
        Ok(_) => {
            let _ = request
                .respond(Response::from_string(r#"{"ok":true}"#).with_header(content_type_json()));
        }
        Err(e) => {
            respond_error(request, &e.to_string());
        }
    }
}

fn read_body(request: &mut tiny_http::Request) -> Result<String, String> {
    let mut body = String::new();
    request
        .as_reader()
        .read_to_string(&mut body)
        .map_err(|e| e.to_string())?;
    Ok(body)
}

struct ParsedRequest {
    sql: String,
    params: Vec<Value>,
}

/// Parse `{"sql": "...", "params": [...]}` with the real JSON parser —
/// the old hand-rolled splitter broke on any string containing a comma.
fn parse_request(body: &str) -> Result<ParsedRequest, String> {
    let parsed = rustqlite::json::Json::parse(body).map_err(|e| format!("bad request: {}", e))?;
    let sql = parsed
        .get("sql")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing 'sql' field".to_string())?
        .to_string();
    let mut params = Vec::new();
    if let Some(arr) = parsed.get("params").and_then(|v| v.as_array()) {
        for item in arr {
            params.push(item.to_value().map_err(|e| format!("bad param: {}", e))?);
        }
    }
    Ok(ParsedRequest { sql, params })
}

fn format_query_result(cols: &[String], rows: &[Vec<Value>]) -> String {
    let col_json: Vec<rustqlite::json::Json> = cols
        .iter()
        .map(|c| rustqlite::json::Json::Str(c.clone()))
        .collect();
    let rows_json: Vec<rustqlite::json::Json> = rows
        .iter()
        .map(|row| {
            rustqlite::json::Json::Array(
                row.iter().map(rustqlite::json::Json::from_value).collect(),
            )
        })
        .collect();
    rustqlite::json::Json::Object(vec![
        (
            "columns".to_string(),
            rustqlite::json::Json::Array(col_json),
        ),
        ("rows".to_string(), rustqlite::json::Json::Array(rows_json)),
    ])
    .to_json()
}

fn respond_error(request: tiny_http::Request, msg: &str) {
    let body = rustqlite::json::Json::Object(vec![(
        "error".to_string(),
        rustqlite::json::Json::Str(msg.to_string()),
    )])
    .to_json();
    let _ = request.respond(
        Response::from_string(body)
            .with_status_code(400)
            .with_header(content_type_json()),
    );
}

fn content_type_json() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap()
}
