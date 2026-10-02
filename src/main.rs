mod serve;
mod tools;

use std::borrow::Cow;
use std::error::Error;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::process::ExitCode;

use serde_json::{json, Value};

const DEFAULT_URL: &str = "http://127.0.0.1:8765";

const SYSTEM: &str = "You are nibble, a small local assistant. Answer directly and briefly. \
Output only what was asked for, with no preamble and no closing remarks.";

// Worded firmly, because a 4B model otherwise guesses at file contents or
// claims it has no access to files.
const SYSTEM_TOOLS: &str = " You have tools that read and search files on this machine. You know \
nothing about any file until you read it, so when the task is about a file or this project, call \
a tool first and never guess. When the input already holds everything you need, answer without tools.";

const SYSTEM_CLAUDE: &str = " If the task is too hard for you, call ask_claude with a complete \
question, then pass on its answer.";

// About 6k tokens of text, which leaves room for the reply in an 8k window.
const INPUT_BUDGET: usize = 24_000;

// Rounds of tool calls before the tools are taken away to force an answer.
const MAX_STEPS: usize = 8;

const DROPPED: &str = "[earlier result dropped to save space]";

const USAGE: &str = "usage: nibble [options] [PROMPT...]
       nibble serve --model PATH [options]

Text piped on stdin is given to the model as input for the prompt.
With no prompt and no pipe, nibble starts a chat.

  -s, --system TEXT     replace the system prompt
  -n, --max-tokens N    reply length limit (default 1024)
      --no-tools        don't let the model read files
      --no-claude       don't let the model ask Claude for help

The model can read and search files but can't change anything.

NIBBLE_URL sets the server address (default http://127.0.0.1:8765).";

struct Args {
    system: String,
    max_tokens: u32,
    prompt: String,
    tools: Vec<Value>,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Option<Args>, Box<dyn Error>> {
    let mut system = None;
    let mut max_tokens = 1024;
    let (mut tools, mut claude) = (true, tools::claude_allowed());
    let mut words = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "-s" | "--system" => system = Some(args.next().ok_or("-s needs a value")?),
            "-n" | "--max-tokens" => max_tokens = args.next().ok_or("-n needs a value")?.parse()?,
            "--no-tools" => tools = false,
            "--no-claude" => claude = false,
            _ => words.push(arg),
        }
    }
    let system = system.unwrap_or_else(|| {
        let mut system = SYSTEM.to_string();
        if tools {
            system += SYSTEM_TOOLS;
        }
        if tools && claude {
            system += SYSTEM_CLAUDE;
        }
        if tools {
            system += &tools::context();
        }
        system
    });
    let tools = if tools { tools::schemas(claude) } else { Vec::new() };
    Ok(Some(Args { system, max_tokens, prompt: words.join(" "), tools }))
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

fn message(role: &str, content: &str) -> Value {
    json!({ "role": role, "content": content })
}

/// Shrink the conversation until it fits the window: first forget old tool
/// results, then whole exchanges, oldest first.
fn trim(messages: &mut Vec<Value>) {
    let size = |m: &[Value]| m.iter().map(|m| m["content"].as_str().map_or(0, str::len)).sum::<usize>();
    while size(messages) > INPUT_BUDGET {
        let last = messages.len() - 1;
        let stale = messages[..last].iter_mut().find(|m| m["role"] == "tool" && m["content"] != DROPPED);
        if let Some(stale) = stale {
            stale["content"] = DROPPED.into();
        } else if let Some(next) = messages.iter().skip(2).position(|m| m["role"] == "user") {
            messages.drain(1..next + 2);
        } else {
            break;
        }
    }
}

/// Send the conversation once and print the reply as it arrives. Returns the
/// reply text and any tool calls the model made.
fn request(messages: &[Value], tools: &[Value], max_tokens: u32) -> Result<(String, Vec<Value>), Box<dyn Error>> {
    let url = std::env::var("NIBBLE_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let mut body = json!({
        "messages": messages,
        "max_tokens": max_tokens,
        "stream": true,
        // mlx-lm batches any request that has no seed, and its batch cache breaks
        // some models (gemma-3n hangs the server). A seed keeps us on the plain path.
        "seed": 0,
    });
    if !tools.is_empty() {
        body["tools"] = tools.into();
    }

    let mut response = ureq::post(format!("{url}/v1/chat/completions"))
        .send_json(&body)
        .map_err(|e| format!("can't reach the model server at {url}: {e}"))?;

    let mut out = io::stdout().lock();
    let mut reply = String::new();
    let mut calls = Vec::new();
    for line in BufReader::new(response.body_mut().as_reader()).lines() {
        let line = line?;
        let Some(data) = line.strip_prefix("data: ") else { continue };
        if data == "[DONE]" {
            break;
        }
        let mut chunk: Value = serde_json::from_str(data)?;
        let delta = &mut chunk["choices"][0]["delta"];
        if let Some(text) = delta["content"].as_str() {
            out.write_all(text.as_bytes())?;
            out.flush()?;
            reply.push_str(text);
        }
        // mlx-lm sends each tool call whole, in a single chunk.
        if let Some(new) = delta["tool_calls"].as_array_mut() {
            calls.append(new);
        }
    }
    if !reply.is_empty() && !reply.ends_with('\n') {
        writeln!(out)?;
    }
    Ok((reply, calls))
}

/// Run one user turn to its final answer, with tool calls along the way.
fn chat(messages: &mut Vec<Value>, args: &Args) -> Result<(), Box<dyn Error>> {
    for step in 0.. {
        let tools = if step < MAX_STEPS { args.tools.as_slice() } else { &[] };
        let (reply, mut calls) = request(messages, tools, args.max_tokens)?;
        if calls.is_empty() {
            messages.push(message("assistant", &reply));
            break;
        }
        for (n, call) in calls.iter_mut().enumerate() {
            if !call["id"].is_string() {
                call["id"] = format!("call_{step}_{n}").into();
            }
        }
        messages.push(json!({ "role": "assistant", "content": reply, "tool_calls": calls }));
        for call in &calls {
            let name = call["function"]["name"].as_str().unwrap_or_default();
            let arguments = match &call["function"]["arguments"] {
                Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
                other => other.clone(),
            };
            eprintln!("nibble: {name} {arguments}");
            let result = tools::call(name, &arguments);
            messages.push(json!({ "role": "tool", "tool_call_id": call["id"], "content": result }));
        }
        trim(messages);
    }
    Ok(())
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
        if let Err(e) = chat(&mut messages, args) {
            eprintln!("nibble: {e}");
            messages.truncate(before);
        }
        trim(&mut messages);
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
    chat(&mut vec![message("system", &args.system), message("user", &user)], &args)
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
