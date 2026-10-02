//! `nibble serve`: a proxy that starts the model server on the first request
//! and stops it after a quiet period, so the weights only hold memory while
//! something is using them. It also serves the chat page (see `web`).

use std::error::Error;
use std::io::{self, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::{chat, tools, web};

pub const USAGE: &str = "usage: nibble serve [options] [-- SERVER_ARGS...]

Listens for clients, starts the model server when one connects, and stops it
after it has been idle. Also serves a chat page at /.

  --model PATH          model directory
  --listen ADDR         address for clients
  --backend-port PORT   port for the model server itself
  --idle SECONDS        stop the model server after this long unused
  --server COMMAND      model server program

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
        let mut child = Command::new(&config.server)
            .args(["--model", &config.model, "--host", "127.0.0.1"])
            .args(["--port", &config.backend_port.to_string()])
            .args(&config.server_args)
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

fn pipe(mut from: TcpStream, mut to: TcpStream) {
    let _ = io::copy(&mut from, &mut to);
    let _ = to.shutdown(Shutdown::Write);
}

fn handle(mut client: TcpStream, config: &Config, state: &Arc<Mutex<Backend>>) -> io::Result<()> {
    let Some(request) = web::read_request(&mut client)? else { return Ok(()) };
    if !request.is_page() && !request.authorized() {
        // Take the body first. Closing on a client that is still sending
        // gives it a connection reset in place of the answer.
        let _ = request.body(&mut client);
        return web::respond(&mut client, "401 Unauthorized", "text/plain", b"nibble serve: a token is needed\n");
    }
    // Only the model API goes to the model server. The rest is ours.
    if !request.path.starts_with("/v1/") {
        return web::route(&mut client, &request);
    }
    let _in_use = match acquire(config, state) {
        Ok(in_use) => in_use,
        Err(e) => {
            eprintln!("nibble serve: {e}");
            let body = format!("nibble serve: {e}\n");
            return web::respond(&mut client, "503 Service Unavailable", "text/plain", body.as_bytes());
        }
    };
    let mut server = TcpStream::connect(("127.0.0.1", config.backend_port))?;
    // Pass on what we read while deciding where the request belongs.
    server.write_all(&request.raw)?;
    let upload = {
        let (client, server) = (client.try_clone()?, server.try_clone()?);
        thread::spawn(move || pipe(client, server))
    };
    pipe(server, client);
    let _ = upload.join();
    Ok(())
}

fn reap_when_idle(config: &Config, state: &Mutex<Backend>) {
    loop {
        thread::sleep(Duration::from_secs(5).min(config.idle.max(Duration::from_secs(1))));
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
}

pub fn run(args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let Some(config) = parse_args(args)? else {
        println!("{USAGE}");
        return Ok(());
    };
    let config = Arc::new(config);
    let state = Arc::new(Mutex::new(Backend { child: None, active: 0, last_used: Instant::now() }));

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

    // Chats get the file tools only inside configured roots. A server's own
    // working directory means nothing: under launchd it is /.
    let roots: Vec<PathBuf> = settings.roots.iter().map(|root| tools::expand(root)).collect();
    if let Some(first) = roots.first() {
        std::env::set_current_dir(first).map_err(|e| format!("{}: {e}", first.display()))?;
        tools::confine(&roots)?;
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
