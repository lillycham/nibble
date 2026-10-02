//! `nibble serve`: a TCP proxy that starts the model server on the first
//! connection and stops it after a quiet period, so the weights only hold
//! memory while something is using them.

use std::error::Error;
use std::io::{self, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const USAGE: &str = "usage: nibble serve --model PATH [options] [-- SERVER_ARGS...]

Listens for clients, starts the model server when one connects, and stops it
after it has been idle.

  --model PATH          model directory (or set NIBBLE_MODEL)
  --listen ADDR         address for clients (default 127.0.0.1:8765)
  --backend-port PORT   port for the model server itself (default 8766)
  --idle SECONDS        stop the model server after this long unused (default 600)
  --server COMMAND      model server program (default nibble-mlx-server)

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
    let mut config = Config {
        model: std::env::var("NIBBLE_MODEL").unwrap_or_default(),
        listen: "127.0.0.1:8765".to_string(),
        backend_port: 8766,
        idle: Duration::from_secs(600),
        server: "nibble-mlx-server".to_string(),
        server_args: Vec::new(),
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
        return Err("no model given; use --model or NIBBLE_MODEL".into());
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
    let _in_use = match acquire(config, state) {
        Ok(in_use) => in_use,
        Err(e) => {
            eprintln!("nibble serve: {e}");
            let body = format!("nibble serve: {e}\n");
            write!(client, "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())?;
            return Ok(());
        }
    };
    let server = TcpStream::connect(("127.0.0.1", config.backend_port))?;
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
    eprintln!("nibble serve: listening on {}", config.listen);

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
