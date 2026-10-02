mod chat;
mod config;
mod mcp;
mod serve;
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

const USAGE: &str = "usage: nibble [options] [PROMPT...]
       nibble serve --model PATH [options]
       nibble mcp [--root DIR]...
       nibble config

Text piped on stdin is given to the model as input for the prompt.
With no prompt and no pipe, nibble starts a chat.

  -s, --system TEXT     replace the system prompt
  -n, --max-tokens N    reply length limit
      --no-tools        don't let the model read files
      --no-claude       don't let the model ask Claude for help
      --anywhere        let the model read files outside the current directory

The model can read and search files but can't change anything.

`nibble config` prints the settings in use and where the config file belongs.";

struct Args {
    system: String,
    max_tokens: u32,
    prompt: String,
    tools: Vec<Value>,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Option<Args>, Box<dyn Error>> {
    let mut system = None;
    let mut max_tokens = config::get().max_tokens;
    let (mut tools, mut claude, mut confined) = (config::get().tools, tools::claude_allowed(), true);
    let mut words = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "-s" | "--system" => system = Some(args.next().ok_or("-s needs a value")?),
            "-n" | "--max-tokens" => max_tokens = args.next().ok_or("-n needs a value")?.parse()?,
            "--no-tools" => tools = false,
            "--no-claude" => claude = false,
            "--anywhere" => confined = false,
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
    Ok(Some(Args { system, max_tokens, prompt: words.join(" "), tools }))
}

fn repl(args: &Args) -> Result<(), Box<dyn Error>> {
    let mut messages = vec![message("system", &args.system)];
    let mut line = String::new();
    loop {
        eprint!("> ");
        line.clear();
        if io::stdin().read_line(&mut line)? == 0 {
            eprintln!();
            return Ok(());
        }
        if line.trim().is_empty() {
            continue;
        }
        let before = messages.len();
        messages.push(message("user", line.trim()));
        if let Err(e) = chat::run(&mut messages, &args.tools, args.max_tokens, &mut chat::Terminal::default()) {
            eprintln!("nibble: {e}");
            messages.truncate(before);
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
    println!("{:#}", config::get().to_json());
}

fn run() -> Result<(), Box<dyn Error>> {
    config::init()?;
    let mut argv = std::env::args().skip(1).peekable();
    match argv.peek().map(String::as_str) {
        Some("serve") => return serve::run(argv.skip(1)),
        Some("mcp") => return mcp::run(argv.skip(1)),
        Some("config") => {
            show_config();
            return Ok(());
        }
        _ => {}
    }
    let Some(args) = parse_args(argv)? else {
        println!("{USAGE}");
        return Ok(());
    };

    let input = piped_input()?;
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
    let mut messages = vec![message("system", &args.system), message("user", &user)];
    chat::run(&mut messages, &args.tools, args.max_tokens, &mut chat::Terminal::default())?;
    Ok(())
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
