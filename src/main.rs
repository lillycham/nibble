mod serve;

use std::borrow::Cow;
use std::error::Error;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::process::ExitCode;

use serde_json::{json, Value};

const DEFAULT_URL: &str = "http://127.0.0.1:8765";

const SYSTEM: &str = "You are nibble, a small local assistant. Answer directly and briefly. \
Output only what was asked for, with no preamble and no closing remarks.";

// About 6k tokens of text, which leaves room for the reply in an 8k window.
const INPUT_BUDGET: usize = 24_000;

const USAGE: &str = "usage: nibble [-s SYSTEM] [-n MAX_TOKENS] [PROMPT...]
       nibble serve --model PATH [options]

Text piped on stdin is given to the model as input for the prompt.
With no prompt and no pipe, nibble starts a chat.

  -s, --system TEXT     replace the system prompt
  -n, --max-tokens N    reply length limit (default 1024)

NIBBLE_URL sets the server address (default http://127.0.0.1:8765).";

struct Args {
    system: String,
    max_tokens: u32,
    prompt: String,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Option<Args>, Box<dyn Error>> {
    let mut system = SYSTEM.to_string();
    let mut max_tokens = 1024;
    let mut words = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "-s" | "--system" => system = args.next().ok_or("-s needs a value")?,
            "-n" | "--max-tokens" => max_tokens = args.next().ok_or("-n needs a value")?.parse()?,
            _ => words.push(arg),
        }
    }
    Ok(Some(Args { system, max_tokens, prompt: words.join(" ") }))
}

/// Keep the start and the end of an over-long input, since logs and diffs
/// tend to matter most at their edges.
fn clip(s: &str, budget: usize) -> Cow<'_, str> {
    if s.len() <= budget {
        return Cow::Borrowed(s);
    }
    let mut head = budget * 2 / 3;
    while !s.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = s.len() - budget / 3;
    while !s.is_char_boundary(tail) {
        tail += 1;
    }
    eprintln!("nibble: input is {} bytes, cut {} from the middle", s.len(), tail - head);
    Cow::Owned(format!("{}\n[... {} bytes cut ...]\n{}", &s[..head], tail - head, &s[tail..]))
}

/// Send the conversation, print the reply as it arrives, and return it.
fn chat(messages: &[Value], max_tokens: u32) -> Result<String, Box<dyn Error>> {
    let url = std::env::var("NIBBLE_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let body = json!({
        "messages": messages,
        "max_tokens": max_tokens,
        "stream": true,
        // mlx-lm batches any request that has no seed, and its batch cache breaks
        // some models (gemma-3n hangs the server). A seed keeps us on the plain path.
        "seed": 0,
    });

    let mut response = ureq::post(format!("{url}/v1/chat/completions"))
        .send_json(&body)
        .map_err(|e| format!("can't reach the model server at {url}: {e}"))?;

    let mut out = io::stdout().lock();
    let mut reply = String::new();
    for line in BufReader::new(response.body_mut().as_reader()).lines() {
        let line = line?;
        let Some(data) = line.strip_prefix("data: ") else { continue };
        if data == "[DONE]" {
            break;
        }
        let chunk: Value = serde_json::from_str(data)?;
        if let Some(text) = chunk["choices"][0]["delta"]["content"].as_str() {
            out.write_all(text.as_bytes())?;
            out.flush()?;
            reply.push_str(text);
        }
    }
    if !reply.ends_with('\n') {
        writeln!(out)?;
    }
    Ok(reply)
}

fn message(role: &str, content: &str) -> Value {
    json!({ "role": role, "content": content })
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
        messages.push(message("user", line.trim()));
        match chat(&messages, args.max_tokens) {
            Ok(reply) => messages.push(message("assistant", &reply)),
            Err(e) => {
                eprintln!("nibble: {e}");
                messages.pop();
            }
        }
        // Forget the oldest exchanges once the chat outgrows the window.
        let size = |m: &[Value]| m.iter().map(|m| m["content"].as_str().map_or(0, str::len)).sum::<usize>();
        while size(&messages) > INPUT_BUDGET && messages.len() > 3 {
            messages.drain(1..3);
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut argv = std::env::args().skip(1).peekable();
    if argv.peek().map(String::as_str) == Some("serve") {
        return serve::run(argv.skip(1));
    }
    let Some(args) = parse_args(argv)? else {
        println!("{USAGE}");
        return Ok(());
    };

    let mut input = String::new();
    if !io::stdin().is_terminal() {
        io::stdin().read_to_string(&mut input)?;
    }
    let input = clip(input.trim(), INPUT_BUDGET);

    let user = match (args.prompt.is_empty(), input.is_empty()) {
        (true, true) if io::stdin().is_terminal() => return repl(&args),
        (true, true) => return Err("no prompt".into()),
        (true, false) => input.into_owned(),
        (false, true) => args.prompt.clone(),
        (false, false) => format!("{}\n\n<input>\n{input}\n</input>", args.prompt),
    };
    chat(&[message("system", &args.system), message("user", &user)], args.max_tokens)?;
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
