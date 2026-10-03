mod chat;
mod config;
mod mcp;
mod serve;
mod sessions;
mod tools;
mod web;

use std::error::Error;
use std::fs::File;
use std::io::{self, IsTerminal, Read};
use std::os::fd::AsFd;
use std::os::unix::fs::FileTypeExt;
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::Value;

use chat::message;
use sessions::{Recorder, Session};

const USAGE: &str = "usage: nibble [options] [PROMPT...]
       nibble serve --model PATH [options]
       nibble mcp [--root DIR]...
       nibble config
       nibble chats

Text piped on stdin is given to the model as input for the prompt.
With no prompt and no pipe, nibble starts a chat, which is saved as it goes.

  -s, --system TEXT     replace the system prompt
  -n, --max-tokens N    reply length limit
      --no-tools        don't let the model read files
      --tools           let it read files even when input is piped in
      --no-claude       don't let the model ask Claude for help
      --anywhere        let the model read files outside the current directory
  -c, --continue        go on with the newest saved chat
  -r, --resume ID       go on with a saved chat; `nibble chats` lists them
      --no-save         don't save this chat

Saved chats are shared with the window. A one-shot prompt is saved only when
it continues a chat.

The model can read and search files but can't change anything.

`nibble config` prints the settings in use and where the config file belongs.";

struct Args {
    system: String,
    max_tokens: u32,
    prompt: String,
    tools: Vec<Value>,
    resume: Option<Resume>,
    save: bool,
}

enum Resume {
    Latest,
    Id(String),
}

fn parse_args(mut args: impl Iterator<Item = String>, piped: bool) -> Result<Option<Args>, Box<dyn Error>> {
    let mut system = None;
    let mut max_tokens = config::get().max_tokens;
    // Piped input is the whole task, so the model gets no tools with it: an
    // eager model otherwise goes looking for files, and the schemas cost tokens.
    let (mut tools, mut claude, mut confined) = (config::get().tools && !piped, tools::claude_allowed(), true);
    let (mut resume, mut save) = (None, true);
    let mut words = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "-s" | "--system" => system = Some(args.next().ok_or("-s needs a value")?),
            "-n" | "--max-tokens" => max_tokens = args.next().ok_or("-n needs a value")?.parse()?,
            "--no-tools" => tools = false,
            "--tools" => tools = true,
            "--no-claude" => claude = false,
            "--anywhere" => confined = false,
            "-c" | "--continue" => resume = Some(Resume::Latest),
            "-r" | "--resume" => resume = Some(Resume::Id(args.next().ok_or("-r needs a chat id")?)),
            "--no-save" => save = false,
            _ => words.push(arg),
        }
    }
    if tools && confined {
        tools::confine(&[std::env::current_dir()?])?;
    }
    let system = system.unwrap_or_else(|| {
        let mut system = chat::system().to_string();
        if tools {
            system += &chat::system_tools();
        }
        if tools && claude {
            system += chat::SYSTEM_CLAUDE;
        }
        if tools {
            system += &tools::context();
        }
        system
    });
    let tools = if tools { tools::schemas(claude) } else { Vec::new() };
    Ok(Some(Args { system, max_tokens, prompt: words.join(" "), tools, resume, save }))
}

/// The saved chat to go on with, if one was asked for.
fn resumed(args: &Args) -> Result<Option<Session>, Box<dyn Error>> {
    let session = match &args.resume {
        None => return Ok(None),
        Some(Resume::Latest) => Session::latest()?,
        Some(Resume::Id(id)) => Session::load(id)?,
    };
    let turns = session.turns.len();
    eprintln!("nibble: going on with \"{}\" ({turns} turn{})", session.title(), if turns == 1 { "" } else { "s" });
    Ok(Some(session))
}

/// Run one user turn, and keep it in the saved chat if there is one. A turn
/// that fails is forgotten, so that it can be asked again.
fn turn(messages: &mut Vec<Value>, user: &str, args: &Args, session: Option<&mut Session>) -> Result<(), Box<dyn Error>> {
    let before = messages.len();
    messages.push(message("user", user));
    let mut terminal = chat::Terminal::default();
    let mut recorder = Recorder { inner: &mut terminal, parts: Vec::new() };
    if let Err(e) = chat::run(messages, &args.tools, args.max_tokens, &mut recorder) {
        messages.truncate(before);
        return Err(e);
    }
    if let Some(session) = session.filter(|_| args.save) {
        session.turns.push(serde_json::json!({ "user": user, "parts": recorder.parts }));
        if let Err(e) = session.save() {
            eprintln!("nibble: the chat was not saved: {e}");
        }
    }
    Ok(())
}

fn repl(args: &Args) -> Result<(), Box<dyn Error>> {
    let mut session = resumed(args)?.unwrap_or_else(Session::new);
    let mut messages = session.messages(&args.system);
    chat::trim(&mut messages);
    let mut line = String::new();
    loop {
        eprint!("> ");
        line.clear();
        if io::stdin().read_line(&mut line)? == 0 {
            eprintln!();
            if args.save && !session.turns.is_empty() {
                eprintln!("nibble: saved; `nibble -r {}` goes on with it", session.id);
            }
            return Ok(());
        }
        if line.trim().is_empty() {
            continue;
        }
        if let Err(e) = turn(&mut messages, line.trim(), args, Some(&mut session)) {
            eprintln!("nibble: {e}");
        }
        chat::trim(&mut messages);
    }
}

/// Whatever was piped or redirected into stdin, or nothing.
fn piped_input() -> io::Result<String> {
    let stdin = io::stdin();
    if stdin.is_terminal() {
        return Ok(String::new());
    }
    let kind = File::from(stdin.as_fd().try_clone_to_owned()?).metadata()?.file_type();
    let mut input = String::new();
    if kind.is_fifo() || kind.is_file() {
        // A shell pipe or a redirect. It will end, however slow the writer is.
        stdin.lock().read_to_string(&mut input)?;
        return Ok(input);
    }
    // Anything else was handed to us by the program that started us: often
    // /dev/null, but agent harnesses pass a socket that stays open and silent,
    // and reading that to its end would wait for ever. Give it a moment.
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let mut input = String::new();
        let _ = send.send(io::stdin().read_to_string(&mut input).map(|_| input));
    });
    receive.recv_timeout(Duration::from_millis(500)).unwrap_or(Ok(input))
}

fn show_config() {
    match config::path() {
        Some(path) if path.exists() => eprintln!("config file: {}", path.display()),
        Some(path) => eprintln!("config file: {} (not there; these are the defaults)", path.display()),
        None => eprintln!("config file: none, because HOME is not set"),
    }
    match config::get().model_name.as_str() {
        "" => eprintln!("model: unknown, so no per-model presets apply"),
        name => eprintln!("model: {name}"),
    }
    println!("{:#}", config::get().to_json());
}

fn run() -> Result<(), Box<dyn Error>> {
    // The model decides which presets apply, so find it before the settings
    // are read: `serve` may name it with a flag, and the others ask the server.
    let all: Vec<String> = std::env::args().skip(1).collect();
    let serving = all.first().is_some_and(|mode| mode == "serve");
    let flag_model = all.iter().position(|arg| arg == "--model").and_then(|at| all.get(at + 1)).cloned();
    config::init(flag_model.filter(|_| serving), !serving)?;
    let mut argv = all.into_iter().peekable();
    match argv.peek().map(String::as_str) {
        Some("serve") => return serve::run(argv.skip(1)),
        Some("mcp") => return mcp::run(argv.skip(1)),
        Some("config") => {
            show_config();
            return Ok(());
        }
        Some("chats") => {
            sessions::show_list();
            return Ok(());
        }
        _ => {}
    }
    let input = piped_input()?;
    let Some(args) = parse_args(argv, !input.trim().is_empty())? else {
        println!("{USAGE}");
        return Ok(());
    };

    let budget = config::get().input_chars;
    if input.trim().len() > budget {
        eprintln!("nibble: input is {} characters, so the middle is cut to fit {budget}", input.trim().len());
    }
    let input = chat::clip(input.trim(), budget);

    let user = match (args.prompt.is_empty(), input.is_empty()) {
        (true, true) if io::stdin().is_terminal() => return repl(&args),
        (true, true) => return Err("no prompt".into()),
        (true, false) => input.into_owned(),
        (false, true) => args.prompt.clone(),
        (false, false) => format!("{}\n\n<input>\n{input}\n</input>", args.prompt),
    };
    let mut session = resumed(&args)?;
    let mut messages = match &session {
        Some(session) => session.messages(&args.system),
        None => vec![message("system", &args.system)],
    };
    chat::trim(&mut messages);
    turn(&mut messages, &user, &args, session.as_mut())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("nibble: {e}");
            ExitCode::FAILURE
        }
    }
}
