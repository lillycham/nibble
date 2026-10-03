//! `nibble serve`: a proxy that starts the model server on the first request
//! and stops it after a quiet period, so the weights only hold memory while
//! something is using them. It also serves the chat page (see `web`).

use std::error::Error;
use std::io::{self, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::{chat, tools, web};

pub const USAGE: &str = "usage: nibble serve [options] [-- SERVER_ARGS...]

Listens for clients, starts the model server when one connects, and stops it
after it has been idle. Also serves a chat page at /.

  --model PATH          model directory
  --listen ADDR         address for clients
  --backend-port PORT   port for the model server itself
  --idle SECONDS        stop the model server after this long unused
  --server COMMAND      model server program
                        (any server with an OpenAI-style /v1/chat/completions)

Each option defaults to its setting in the config file; see `nibble config`.
Arguments after -- go to the model server unchanged.";

// Loading a few GB of weights from a cold disk cache can take a while.
const START_TIMEOUT: Duration = Duration::from_secs(180);

// std has no signal handling, and a crate for three libc calls isn't worth it.
extern "C" {
    fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
    fn kill(pid: i32, sig: i32) -> i32;
    fn _exit(status: i32) -> !;
}
const SIGHUP: i32 = 1;
const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;

/// The running model server, for the signal handler. 0 means none.
static CHILD_PID: AtomicI32 = AtomicI32::new(0);

/// Take the model server down with us, so a stopped proxy never leaves a few
/// GB of weights behind in an orphan.
extern "C" fn stop(sig: i32) {
    let pid = CHILD_PID.load(Ordering::SeqCst);
    unsafe {
        if pid > 0 {
            kill(pid, SIGTERM);
        }
        _exit(128 + sig)
    }
}

struct Config {
    model: String,
    listen: String,
    backend_port: u16,
    idle: Duration,
    server: String,
    server_args: Vec<String>,
}

struct Backend {
    /// The model the server runs, or will run when it next starts.
    model: String,
    child: Option<Child>,
    active: usize,
    last_used: Instant,
}

/// Counts one client connection for as long as it lives.
struct InUse(Arc<Mutex<Backend>>);

impl Drop for InUse {
    fn drop(&mut self) {
        let mut backend = self.0.lock().unwrap();
        backend.active -= 1;
        backend.last_used = Instant::now();
    }
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Option<Config>, Box<dyn Error>> {
    let defaults = crate::config::get();
    let mut config = Config {
        model: defaults.model.clone(),
        listen: defaults.listen.clone(),
        backend_port: defaults.backend_port,
        idle: Duration::from_secs(defaults.idle_seconds),
        server: defaults.server_command.clone(),
        server_args: defaults.server_args.clone(),
    };
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--model" => config.model = value()?,
            "--listen" => config.listen = value()?,
            "--backend-port" => config.backend_port = value()?.parse()?,
            "--idle" => config.idle = Duration::from_secs(value()?.parse()?),
            "--server" => config.server = value()?,
            "--" => config.server_args = args.by_ref().collect(),
            _ => return Err(format!("unknown option {arg}").into()),
        }
    }
    if config.model.is_empty() {
        return Err("no model given; use --model, NIBBLE_MODEL or the \"model\" setting".into());
    }
    Ok(Some(config))
}

/// Make sure the model server is up, and count the caller as using it.
fn acquire(config: &Config, state: &Arc<Mutex<Backend>>) -> io::Result<InUse> {
    // The lock is held while the server starts, so later clients wait for the
    // same start instead of racing to spawn a second one.
    let mut backend = state.lock().unwrap();
    if let Some(child) = &mut backend.child {
        if child.try_wait()?.is_some() {
            eprintln!("nibble serve: model server exited, starting it again");
            backend.child = None;
        }
    }
    if backend.child.is_none() {
        // Something else on the port would make the start check below pass
        // while our own server fails to bind.
        if TcpStream::connect(("127.0.0.1", config.backend_port)).is_ok() {
            return Err(io::Error::other(format!("port {} is already in use", config.backend_port)));
        }
        eprintln!("nibble serve: starting model server");
        let started = Instant::now();
        let port = config.backend_port.to_string();
        let model = backend.model.clone();
        let fill = |arg: &String| arg.replace("{model}", &model).replace("{port}", &port);
        // The default command line is mlx_lm.server's. Arguments that name
        // {model} or {port} replace it, for a server with other flags.
        let args: Vec<String> = if config.server_args.iter().any(|arg| *arg != fill(arg)) {
            config.server_args.iter().map(fill).collect()
        } else {
            let fixed = ["--model", &model, "--host", "127.0.0.1", "--port", &port];
            fixed.iter().map(|arg| arg.to_string()).chain(config.server_args.iter().cloned()).collect()
        };
        let mut child = Command::new(&config.server)
            .args(&args)
            .spawn()
            .map_err(|e| io::Error::new(e.kind(), format!("can't run {}: {e}", config.server)))?;
        CHILD_PID.store(child.id() as i32, Ordering::SeqCst);
        while TcpStream::connect(("127.0.0.1", config.backend_port)).is_err() {
            let failure = match child.try_wait()? {
                Some(status) => Some(format!("model server stopped during start ({status})")),
                None if started.elapsed() > START_TIMEOUT => Some("model server took too long to start".into()),
                None => None,
            };
            if let Some(failure) = failure {
                CHILD_PID.store(0, Ordering::SeqCst);
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::other(failure));
            }
            thread::sleep(Duration::from_millis(200));
        }
        eprintln!("nibble serve: model server ready after {:.1}s", started.elapsed().as_secs_f32());
        backend.child = Some(child);
    }
    backend.active += 1;
    Ok(InUse(state.clone()))
}

/// The model given at the start, which stays a choice even when it is
/// outside the model directory.
static FIRST_MODEL: OnceLock<String> = OnceLock::new();

/// The models to choose from, by name: each entry in the model directory, and
/// the model given at the start. Read afresh each time, so a model that was
/// just downloaded shows up.
pub fn models() -> Vec<(String, PathBuf)> {
    let first = FIRST_MODEL.get().map(PathBuf::from);
    let settings = crate::config::get();
    let dir = match settings.model_dir.as_str() {
        "" => first.as_deref().and_then(Path::parent).map(Path::to_path_buf),
        dir => Some(tools::expand(dir)),
    };
    let mut found: Vec<PathBuf> = dir
        .and_then(|dir| std::fs::read_dir(dir).ok())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .map(|entry| entry.path())
        .collect();
    // A model named by Hugging Face id is no path, but still a choice.
    found.extend(first.filter(|first| !found.contains(first)));
    let mut models: Vec<(String, PathBuf)> = found.into_iter().map(|path| (name(&path), path)).collect();
    models.sort();
    models.dedup_by(|a, b| a.0 == b.0);
    models
}

fn name(path: &Path) -> String {
    let path = path.to_string_lossy();
    path.trim_end_matches('/').rsplit('/').next().unwrap_or_default().to_string()
}

/// `POST /model` with {"model": NAME}: run another model from the next
/// request on. Only a model from the list, so a client can't make the server
/// load whatever it names.
fn switch(client: &mut TcpStream, request: &web::Request, state: &Mutex<Backend>) -> io::Result<()> {
    let body: Value = serde_json::from_slice(&request.body(client)?).unwrap_or_default();
    let wanted = body["model"].as_str().unwrap_or_default();
    let Some((_, path)) = models().into_iter().find(|(name, _)| name == wanted) else {
        let body = format!("no model called \"{wanted}\"\n");
        return web::respond(client, "404 Not Found", "text/plain", body.as_bytes());
    };
    let path = path.to_string_lossy().into_owned();
    let mut backend = state.lock().unwrap();
    if backend.model != path {
        if backend.active > 0 {
            return web::respond(client, "409 Conflict", "text/plain", b"the model is busy; try again when it finishes\n");
        }
        // Load the settings first: if the file has gone bad, keep the old model.
        if let Err(e) = crate::config::switch(&path) {
            return web::respond(client, "500 Internal Server Error", "text/plain", format!("{e}\n").as_bytes());
        }
        if let Some(mut child) = backend.child.take() {
            CHILD_PID.store(0, Ordering::SeqCst);
            let _ = child.kill();
            let _ = child.wait();
        }
        eprintln!("nibble serve: switched to {wanted}");
        backend.model = path;
    }
    web::respond(client, "200 OK", "application/json", info().to_string().as_bytes())
}

/// What `GET /info` says: the model, the others to choose from, and more.
pub fn info() -> Value {
    let config = crate::config::get();
    let models: Vec<String> = models().into_iter().map(|(name, _)| name).collect();
    json!({ "model": config.model_name, "models": models, "tools": web::tools_allowed(), "version": env!("CARGO_PKG_VERSION") })
}

fn pipe(mut from: TcpStream, mut to: TcpStream) {
    let _ = io::copy(&mut from, &mut to);
    let _ = to.shutdown(Shutdown::Write);
}

fn handle(mut client: TcpStream, config: &Config, state: &Arc<Mutex<Backend>>) -> io::Result<()> {
    let Some(request) = web::read_request(&mut client)? else { return Ok(()) };
    if !request.is_public() && !request.authorized() {
        // Take the body first. Closing on a client that is still sending
        // gives it a connection reset in place of the answer.
        let _ = request.body(&mut client);
        return web::respond(&mut client, "401 Unauthorized", "text/plain", b"nibble serve: a token is needed\n");
    }
    // Only the model API goes to the model server. The rest is ours.
    if (request.method.as_str(), request.path.as_str()) == ("POST", "/model") {
        return switch(&mut client, &request, state);
    }
    if !request.path.starts_with("/v1/") {
        return web::route(&mut client, &request);
    }
    if has_header(request.head(), "transfer-encoding") {
        return web::respond(&mut client, "411 Length Required", "text/plain", b"nibble serve: send a Content-Length\n");
    }
    if request.content_length() > MAX_API_BODY {
        return web::respond(&mut client, "413 Payload Too Large", "text/plain", b"nibble serve: request body too large\n");
    }
    if has_header(request.head(), "expect") {
        // The client waits for this before it sends the body we need to read.
        client.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
    }
    let body = request.body_within(&mut client, MAX_API_BODY)?;
    let _in_use = match acquire(config, state) {
        Ok(in_use) => in_use,
        Err(e) => {
            eprintln!("nibble serve: {e}");
            let body = format!("nibble serve: {e}\n");
            return web::respond(&mut client, "503 Service Unavailable", "text/plain", body.as_bytes());
        }
    };
    // While we count as active the model can't be switched, so this is the
    // model the server runs for the whole request.
    let model = state.lock().unwrap().model.clone();
    let forwarded = match pin_model(request.head(), &body, &model) {
        Ok(forwarded) => forwarded,
        Err(e) => return web::respond(&mut client, "400 Bad Request", "text/plain", format!("nibble serve: {e}\n").as_bytes()),
    };
    let mut server = TcpStream::connect(("127.0.0.1", config.backend_port))?;
    server.write_all(&forwarded)?;
    // Nothing more goes to the server from this client: a second request on
    // the same connection would pass by unchecked. The forwarded request asks
    // the server to close when it has answered.
    let _ = server.shutdown(Shutdown::Write);
    pipe(server, client);
    Ok(())
}

/// Requests for the model API may be at most this big. More than the chat
/// page's limit, because a client with a long context sends all of it.
const MAX_API_BODY: usize = 32 << 20;

/// Whether the head has a header of this name (in lower case).
fn has_header(head: &[u8], name: &str) -> bool {
    String::from_utf8_lossy(head)
        .split("\r\n")
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .any(|(header, _)| header.trim().eq_ignore_ascii_case(name))
}

/// The request to pass on to the model server, with the body's "model" set to
/// the one we run. A client with the token could otherwise name any model,
/// and mlx_lm.server would load it, even from Hugging Face. A request with
/// no body (`GET /v1/models`) goes as it is. The connection closes after the
/// answer, so each request comes through here.
fn pin_model(head: &[u8], body: &[u8], model: &str) -> Result<Vec<u8>, String> {
    let body = if body.is_empty() {
        Vec::new()
    } else {
        let mut json: Value = serde_json::from_slice(body).map_err(|e| format!("bad JSON: {e}"))?;
        let object = json.as_object_mut().ok_or("the body must be a JSON object")?;
        object.insert("model".into(), model.into());
        json.to_string().into_bytes()
    };
    let head = String::from_utf8_lossy(head);
    let mut lines = head.split("\r\n").filter(|line| !line.is_empty());
    let mut out = String::from(lines.next().unwrap_or_default());
    out += "\r\n";
    for line in lines {
        let name = line.split_once(':').map_or(line, |(name, _)| name).trim().to_ascii_lowercase();
        // Ours to set, or answered here already.
        if !matches!(name.as_str(), "content-length" | "connection" | "keep-alive" | "expect" | "transfer-encoding") {
            out += line;
            out += "\r\n";
        }
    }
    if !body.is_empty() {
        out += &format!("Content-Length: {}\r\n", body.len());
    }
    out += "Connection: close\r\n\r\n";
    let mut out = out.into_bytes();
    out.extend_from_slice(&body);
    Ok(out)
}

fn reap_when_idle(config: &Config, state: &Mutex<Backend>) {
    loop {
        thread::sleep(Duration::from_secs(5).min(config.idle.max(Duration::from_secs(1))));
        stop_if_idle(config, state);
    }
}

fn stop_if_idle(config: &Config, state: &Mutex<Backend>) {
    let mut backend = state.lock().unwrap();
    if backend.active == 0 && backend.last_used.elapsed() >= config.idle {
        if let Some(mut child) = backend.child.take() {
            eprintln!("nibble serve: idle, stopping model server");
            CHILD_PID.store(0, Ordering::SeqCst);
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub fn run(args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let Some(config) = parse_args(args)? else {
        println!("{USAGE}");
        return Ok(());
    };
    let _ = FIRST_MODEL.set(config.model.clone());
    let config = Arc::new(config);
    let backend = Backend { model: config.model.clone(), child: None, active: 0, last_used: Instant::now() };
    let state = Arc::new(Mutex::new(backend));

    let listener = TcpListener::bind(&config.listen)
        .map_err(|e| format!("can't listen on {}: {e}", config.listen))?;
    let address = listener.local_addr()?;
    let settings = crate::config::get();
    if !address.ip().is_loopback() && settings.token.is_empty() {
        return Err(format!(
            "refusing to listen on {address} without a token: anyone who can reach it could use the model \
             and read files through it. Set \"token\" or \"token_file\" in the config file."
        )
        .into());
    }
    eprintln!("nibble serve: listening on {address}");

    // The chat page's turns come back in through our own front door, so they
    // start the model server like any other client.
    let host = if address.ip().is_unspecified() { "127.0.0.1".to_string() } else { address.ip().to_string() };
    chat::set_url(format!("http://{host}:{}", address.port()));

    // Chats get the file tools inside the configured roots, or failing that
    // inside the directory we were started in, as `nibble mcp` does. Under
    // launchd that directory is /, which is no root at all: then chats get no
    // tools, and we say so, because a model without tools just looks unwilling.
    let mut roots: Vec<PathBuf> = settings.roots.iter().map(|root| tools::expand(root)).collect();
    if roots.is_empty() {
        roots.extend(std::env::current_dir().ok().filter(|dir| dir.parent().is_some()));
    }
    if !settings.tools {
        eprintln!("nibble serve: chats have no file tools, because \"tools\" is off");
    } else if let Some(first) = roots.first() {
        std::env::set_current_dir(first).map_err(|e| format!("{}: {e}", first.display()))?;
        tools::confine(&roots)?;
        let shown: Vec<_> = roots.iter().map(|root| root.display().to_string()).collect();
        eprintln!("nibble serve: chats may read files inside {}", shown.join(", "));
    } else {
        eprintln!("nibble serve: chats have no file tools; set \"roots\" in the config file to give them some");
    }
    web::allow_tools(!roots.is_empty());

    for sig in [SIGHUP, SIGINT, SIGTERM] {
        unsafe { signal(sig, stop) };
    }
    {
        let (config, state) = (config.clone(), state.clone());
        thread::spawn(move || reap_when_idle(&config, &state));
    }
    for client in listener.incoming() {
        let client = client?;
        let (config, state) = (config.clone(), state.clone());
        thread::spawn(move || {
            if let Err(e) = handle(client, &config, &state) {
                eprintln!("nibble serve: {e}");
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    const MODEL: &str = "/models/Qwen3-4B";

    fn split(request: &[u8]) -> (String, Value) {
        let end = request.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8(request[..end].to_vec()).unwrap();
        let body = if request.len() > end { serde_json::from_slice(&request[end..]).unwrap() } else { Value::Null };
        (head, body)
    }

    #[test]
    fn requests_name_only_the_model_we_run() {
        let head = b"POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer t\r\n\
                     Content-Length: 99\r\nConnection: keep-alive\r\nExpect: 100-continue\r\n\r\n";
        let body = br#"{"model":"mlx-community/some-other-model","messages":[]}"#;
        let (head, body) = split(&pin_model(head, body, MODEL).unwrap());
        assert_eq!(body["model"], MODEL);
        assert_eq!(body["messages"], json!([]));
        assert!(head.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
        assert!(head.contains("Authorization: Bearer t\r\n"));
        assert!(head.contains(&format!("Content-Length: {}\r\n", body.to_string().len())));
        assert!(head.contains("Connection: close\r\n"));
        assert!(!head.contains("99") && !head.contains("keep-alive") && !head.contains("Expect"));

        // A body that names no model gets ours too.
        let (_, body) = split(&pin_model(b"POST /v1/completions HTTP/1.1\r\n\r\n", br#"{"prompt":"hi"}"#, MODEL).unwrap());
        assert_eq!(body["model"], MODEL);

        // No body: nothing to pin.
        let (head, body) = split(&pin_model(b"GET /v1/models HTTP/1.1\r\nHost: x\r\n\r\n", b"", MODEL).unwrap());
        assert_eq!(head, "GET /v1/models HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        assert_eq!(body, Value::Null);

        // A body we can't pin goes nowhere.
        let head = b"POST /v1/chat/completions HTTP/1.1\r\n\r\n";
        assert!(pin_model(head, b"not json", MODEL).is_err());
        assert!(pin_model(head, br#"["model"]"#, MODEL).is_err());
    }

    #[test]
    fn headers_are_found_by_name_in_any_case() {
        let head = b"POST /v1/x HTTP/1.1\r\nTRANSFER-ENCODING: chunked\r\n\r\n";
        assert!(has_header(head, "transfer-encoding"));
        assert!(!has_header(head, "expect"));
        assert!(!has_header(b"POST /transfer-encoding: HTTP/1.1\r\n\r\n", "transfer-encoding"));
    }

    /// The whole path: a client asks for another model over a kept-alive
    /// connection, and the model server sees only ours, and only once.
    #[test]
    fn the_proxy_pins_the_model_and_closes_after_one_request() {
        let backend = TcpListener::bind("127.0.0.1:0").unwrap();
        let config = Config {
            model: MODEL.into(),
            listen: String::new(),
            backend_port: backend.local_addr().unwrap().port(),
            idle: Duration::from_secs(60),
            server: String::new(),
            server_args: Vec::new(),
        };
        // Stands in for a running model server, so `acquire` starts nothing.
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let state = Arc::new(Mutex::new(Backend {
            model: MODEL.into(),
            child: Some(child),
            active: 0,
            last_used: Instant::now(),
        }));

        let seen = thread::spawn(move || {
            let (mut server, _) = backend.accept().unwrap();
            let mut seen = Vec::new();
            server.read_to_end(&mut seen).unwrap();
            server.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").unwrap();
            seen
        });

        let front = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(front.local_addr().unwrap()).unwrap();
        let body = r#"{"model":"mlx-community/whatever-4bit","messages":[]}"#;
        let request = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
            body.len()
        );
        client.write_all(format!("{request}{request}").as_bytes()).unwrap();
        let (accepted, _) = front.accept().unwrap();
        handle(accepted, &config, &state).unwrap();

        let mut answer = String::new();
        client.read_to_string(&mut answer).unwrap();
        assert!(answer.ends_with("\r\n\r\nok"), "{answer}");
        let seen = seen.join().unwrap();
        let (head, body) = split(&seen);
        assert_eq!(body["model"], MODEL);
        assert_eq!(head.matches("POST").count(), 1);
        assert!(!String::from_utf8_lossy(&seen).contains("whatever"));
        assert_eq!(state.lock().unwrap().active, 0);

        let mut child = state.lock().unwrap().child.take().unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }


    fn config(server: &str, server_args: &[&str]) -> Config {
        Config {
            model: "test-model".into(),
            listen: "127.0.0.1:0".into(),
            backend_port: free_port(),
            idle: Duration::ZERO,
            server: server.into(),
            server_args: server_args.iter().map(|arg| arg.to_string()).collect(),
        }
    }

    fn state(model: &str) -> Arc<Mutex<Backend>> {
        Arc::new(Mutex::new(Backend { model: model.into(), child: None, active: 0, last_used: Instant::now() }))
    }

    /// Stops the model server when a test ends, even by failing.
    struct Reaper(Arc<Mutex<Backend>>);

    impl Drop for Reaper {
        fn drop(&mut self) {
            let backend = self.0.lock();
            if let Some(mut child) = backend.unwrap_or_else(|poisoned| poisoned.into_inner()).child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    /// A port for the model server, with nothing listening on it. Never found
    /// by binding a listener and closing it again: the port can be handed to
    /// the next socket any test opens, and a child that another test is just
    /// starting holds a copy of the listener until it execs, which makes the
    /// port look taken. So take ports below the range the system hands out
    /// (32768 up on Linux, 49152 up on macOS), different for each test run,
    /// and check them by connecting.
    fn free_port() -> u16 {
        static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
        let base = 20_000 + (std::process::id() % 1000) as u16 * 10;
        loop {
            let port = base + NEXT.fetch_add(1, Ordering::SeqCst);
            if TcpStream::connect(("127.0.0.1", port)).is_err() {
                return port;
            }
        }
    }

    /// Send one raw request through `handle` and return the whole response.
    fn ask(config: &Arc<Config>, state: &Arc<Mutex<Backend>>, request: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        let (config, state) = (config.clone(), state.clone());
        let server = thread::spawn(move || handle(accepted, &config, &state));
        client.write_all(request.as_bytes()).unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        server.join().unwrap().unwrap();
        response
    }

    fn get(path: &str) -> String {
        format!("GET {path} HTTP/1.1\r\nHost: test\r\n\r\n")
    }

    fn post(path: &str, body: &str) -> String {
        format!("POST {path} HTTP/1.1\r\nHost: test\r\nContent-Length: {}\r\n\r\n{body}", body.len())
    }

    fn args(list: &[&str]) -> impl Iterator<Item = String> {
        list.iter().map(|arg| arg.to_string()).collect::<Vec<_>>().into_iter()
    }

    /// Not a test: the model server that `starts_on_demand_and_stops_when_idle`
    /// runs, by starting this test binary again with this test picked out.
    #[test]
    #[ignore = "a fake model server, started by other tests"]
    fn fake_model_server() {
        let Some(port) = std::env::args().find_map(|arg| arg.strip_prefix("port=")?.parse::<u16>().ok()) else {
            return;
        };
        // Should the test that started it fail before stopping it, don't
        // outlive it for long.
        thread::spawn(|| {
            thread::sleep(Duration::from_secs(60));
            std::process::exit(0);
        });
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut seen = Vec::new();
            let mut buffer = [0; 1024];
            while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => seen.extend_from_slice(&buffer[..n]),
                }
            }
            let first = String::from_utf8_lossy(&seen).lines().next().unwrap_or_default().to_string();
            let body = format!("model server saw: {first}");
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        }
    }

    #[test]
    fn options_and_their_mistakes() {
        assert!(parse_args(args(&["--help"])).unwrap().is_none());
        let config = parse_args(args(&[
            "--model", "/models/qwen", "--listen", "127.0.0.1:9000", "--backend-port", "9001", "--idle", "30",
            "--server", "my-server", "--", "--model", "{model}",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(config.model, "/models/qwen");
        assert_eq!(config.listen, "127.0.0.1:9000");
        assert_eq!(config.backend_port, 9001);
        assert_eq!(config.idle, Duration::from_secs(30));
        assert_eq!(config.server, "my-server");
        // Everything after -- goes to the model server, flags included.
        assert_eq!(config.server_args, ["--model", "{model}"]);

        let error = |list: &[&str]| parse_args(args(list)).err().expect("should fail").to_string();
        assert!(error(&[]).contains("no model given"));
        assert!(error(&["--model"]).contains("--model needs a value"));
        assert!(error(&["--model", "m", "--verbose"]).contains("unknown option --verbose"));
        error(&["--model", "m", "--backend-port", "70000"]);
        error(&["--model", "m", "--idle", "soon"]);
    }

    #[test]
    fn model_names_are_the_last_part_of_the_path() {
        assert_eq!(name(Path::new("/models/Qwen3-4B-4bit")), "Qwen3-4B-4bit");
        assert_eq!(name(Path::new("/models/Qwen3-4B-4bit/")), "Qwen3-4B-4bit");
        assert_eq!(name(Path::new("mlx-community/gemma-3n")), "gemma-3n");
    }

    #[test]
    fn starts_on_demand_and_stops_when_idle() {
        let exe = std::env::current_exe().unwrap();
        let fake = ["serve::tests::fake_model_server", "--exact", "--ignored", "--nocapture", "port={port}"];
        let config = Arc::new(config(exe.to_str().unwrap(), &fake));
        let state = state("test-model");
        let _reaper = Reaper(state.clone());

        let response = ask(&config, &state, &get("/v1/models"));
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        // The request reaches the model server as the client sent it.
        assert!(response.ends_with("model server saw: GET /v1/models HTTP/1.1"), "{response}");
        {
            let backend = state.lock().unwrap();
            assert!(backend.child.is_some(), "the model server should still be running");
            assert_eq!(backend.active, 0, "the finished request should no longer count");
        }
        // A second request uses the server that is already up.
        let pid = state.lock().unwrap().child.as_ref().unwrap().id();
        assert!(ask(&config, &state, &get("/v1/models")).ends_with("model server saw: GET /v1/models HTTP/1.1"));
        assert_eq!(state.lock().unwrap().child.as_ref().unwrap().id(), pid);

        // Busy: not stopped, however long it has been.
        state.lock().unwrap().active = 1;
        stop_if_idle(&config, &state);
        assert!(state.lock().unwrap().child.is_some());
        state.lock().unwrap().active = 0;
        stop_if_idle(&config, &state);
        assert!(state.lock().unwrap().child.is_none());
        assert!(TcpStream::connect(("127.0.0.1", config.backend_port)).is_err(), "the model server should be gone");
    }

    #[test]
    fn a_model_server_that_cannot_start_is_a_503() {
        let state = state("test-model");

        let missing = Arc::new(config("/nonexistent/model-server", &[]));
        let response = ask(&missing, &state, &get("/v1/models"));
        assert!(response.starts_with("HTTP/1.1 503"), "{response}");
        assert!(response.contains("can't run /nonexistent/model-server"), "{response}");

        // `false` exits at once with any arguments.
        let quits = Arc::new(config("false", &[]));
        let response = ask(&quits, &state, &get("/v1/models"));
        assert!(response.contains("model server stopped during start"), "{response}");

        // Something else already has the port: our server could never bind it.
        let squatter = TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = Arc::new(Config { backend_port: squatter.local_addr().unwrap().port(), ..config("false", &[]) });
        let response = ask(&taken, &state, &get("/v1/models"));
        assert!(response.contains("is already in use"), "{response}");

        let backend = state.lock().unwrap();
        assert!(backend.child.is_none());
        assert_eq!(backend.active, 0);
    }

    #[test]
    fn only_the_model_api_starts_the_model_server() {
        // A server that would fail if it were started.
        let config = Arc::new(config("/nonexistent/model-server", &[]));
        let state = state("test-model");

        let page = ask(&config, &state, &get("/"));
        assert!(page.starts_with("HTTP/1.1 200 OK") && page.contains("text/html"), "{page}");
        let info = ask(&config, &state, &get("/info?fresh=1"));
        assert!(info.starts_with("HTTP/1.1 200 OK") && info.contains(env!("CARGO_PKG_VERSION")), "{info}");
        assert!(ask(&config, &state, &get("/elsewhere")).starts_with("HTTP/1.1 404"));
        // Nothing at all, then a half-sent head: no answer and no error.
        assert_eq!(ask(&config, &state, ""), "");
        assert_eq!(ask(&config, &state, "GET / HTTP/1.1\r\n"), "");
        assert!(state.lock().unwrap().child.is_none());
    }

    #[test]
    fn models_come_from_the_model_directory_and_switch_by_name() {
        let base = std::env::temp_dir().join(format!("nibble-serve-test-{}", std::process::id()));
        for dir in ["b-model", "a-model", ".cache"] {
            std::fs::create_dir_all(base.join(dir)).unwrap();
        }
        let first = base.join("b-model").to_string_lossy().into_owned();
        FIRST_MODEL.set(first.clone()).unwrap();

        // Sorted, by name, without hidden entries.
        let names: Vec<String> = models().into_iter().map(|(name, _)| name).collect();
        assert_eq!(names, ["a-model", "b-model"]);
        assert_eq!(info()["models"], json!(["a-model", "b-model"]));

        let config = Arc::new(config("/nonexistent/model-server", &[]));
        let state = state(&first);
        let switch_to = |model: &str| ask(&config, &state, &post("/model", &json!({ "model": model }).to_string()));

        // Only a model from the list, never a path the client names.
        let response = switch_to("/etc");
        assert!(response.starts_with("HTTP/1.1 404") && response.contains("no model called \"/etc\""), "{response}");
        let response = ask(&config, &state, &post("/model", "not json"));
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");
        // The model already in use: nothing to do.
        let response = switch_to("b-model");
        assert!(response.starts_with("HTTP/1.1 200 OK") && response.contains("\"a-model\""), "{response}");
        assert_eq!(state.lock().unwrap().model, first);
        // Never mid-reply.
        state.lock().unwrap().active = 1;
        let response = switch_to("a-model");
        assert!(response.starts_with("HTTP/1.1 409"), "{response}");
        assert_eq!(state.lock().unwrap().model, first);

        std::fs::remove_dir_all(&base).unwrap();
    }
}
