//! One conversation with the model: send, stream the reply, run tool calls.

use std::borrow::Cow;
use std::error::Error;
use std::io::{self, BufRead, BufReader, Write};
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Value};

use crate::{config, tools};

pub const SYSTEM: &str = "You are nibble, a small local assistant. Answer directly and briefly. \
Output only what was asked for, with no preamble and no closing remarks.";

// Worded firmly, because a 4B model otherwise guesses at file contents or
// claims it has no access to files.
pub const SYSTEM_TOOLS: &str = " You have tools that read and search files on this machine. You know \
nothing about any file until you read it, so when the task is about a file or this project, call \
a tool first and never guess. When the input already holds everything you need, answer without tools.";

pub const SYSTEM_CLAUDE: &str = " If the task is too hard for you, call ask_claude with a complete \
question, then pass on its answer.";

const DROPPED: &str = "[earlier result dropped to save space]";

/// Where a turn's output goes while it happens.
pub trait Events {
    fn text(&mut self, text: &str) -> io::Result<()>;
    fn tool(&mut self, name: &str, arguments: &Value) -> io::Result<()>;
    /// One model reply is complete. More may follow, after tool calls.
    fn reply_end(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The reply on stdout as it arrives, and tool calls on stderr.
#[derive(Default)]
pub struct Terminal {
    mid_line: bool,
}

impl Events for Terminal {
    fn text(&mut self, text: &str) -> io::Result<()> {
        let mut out = io::stdout().lock();
        out.write_all(text.as_bytes())?;
        if !text.is_empty() {
            self.mid_line = !text.ends_with('\n');
        }
        out.flush()
    }

    fn tool(&mut self, name: &str, arguments: &Value) -> io::Result<()> {
        eprintln!("nibble: {name} {arguments}");
        Ok(())
    }

    fn reply_end(&mut self) -> io::Result<()> {
        if std::mem::take(&mut self.mid_line) {
            println!();
        }
        Ok(())
    }
}

/// Nothing on stdout, for when stdout carries a protocol.
pub struct Quiet;

impl Events for Quiet {
    fn text(&mut self, _: &str) -> io::Result<()> {
        Ok(())
    }

    fn tool(&mut self, name: &str, arguments: &Value) -> io::Result<()> {
        eprintln!("nibble: {name} {arguments}");
        Ok(())
    }
}

static URL: OnceLock<String> = OnceLock::new();

/// Send requests here instead of to the configured address. `nibble serve`
/// uses this to reach itself, whatever address it was told to listen on.
pub fn set_url(url: String) {
    let _ = URL.set(url);
}

pub struct Outcome {
    pub reply: String,
    /// The model was still calling tools when the rounds ran out.
    pub exhausted: bool,
}

pub fn message(role: &str, content: &str) -> Value {
    json!({ "role": role, "content": content })
}

/// Keep the start and the end of an over-long text, since logs and diffs
/// tend to matter most at their edges.
pub fn clip(s: &str, budget: usize) -> Cow<'_, str> {
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
    Cow::Owned(format!("{}\n[... {} bytes cut ...]\n{}", &s[..head], tail - head, &s[tail..]))
}

/// Shrink the conversation until it fits the window: first forget old tool
/// results, then whole exchanges, oldest first.
pub fn trim(messages: &mut Vec<Value>) {
    let size = |m: &[Value]| m.iter().map(|m| m["content"].as_str().map_or(0, str::len)).sum::<usize>();
    while size(messages) > config::get().input_chars {
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

/// Send the conversation once and report the reply as it arrives.
/// Returns the reply text and any tool calls the model made.
fn request(
    messages: &[Value],
    tools: &[Value],
    max_tokens: u32,
    events: &mut dyn Events,
) -> Result<(String, Vec<Value>), Box<dyn Error>> {
    let config = config::get();
    let url = URL.get().unwrap_or(&config.url);
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

    // Fail with a message instead of waiting for ever. The response limit is
    // long because a cold start has to load the model first.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_response(Some(Duration::from_secs(300)))
        .build()
        .into();
    let mut post = agent.post(format!("{url}/v1/chat/completions"));
    if !config.token.is_empty() {
        post = post.header("Authorization", format!("Bearer {}", config.token));
    }
    let mut response = post
        .send_json(&body)
        .map_err(|e| format!("no answer from the model server at {url}: {e}"))?;

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
            events.text(text)?;
            reply.push_str(text);
        }
        // mlx-lm sends each tool call whole, in a single chunk.
        if let Some(new) = delta["tool_calls"].as_array_mut() {
            calls.append(new);
        }
    }
    events.reply_end()?;
    Ok((reply, calls))
}

/// Run one user turn to its final answer, with tool calls along the way.
pub fn run(
    messages: &mut Vec<Value>,
    tools: &[Value],
    max_tokens: u32,
    events: &mut dyn Events,
) -> Result<Outcome, Box<dyn Error>> {
    let max_steps = config::get().max_steps;
    let mut step = 0;
    loop {
        let exhausted = step >= max_steps;
        let offered = if exhausted { &[] } else { tools };
        let (reply, mut calls) = request(messages, offered, max_tokens, events)?;
        if calls.is_empty() {
            messages.push(message("assistant", &reply));
            return Ok(Outcome { reply, exhausted });
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
            events.tool(name, &arguments)?;
            let result = tools::call(name, &arguments);
            messages.push(json!({ "role": "tool", "tool_call_id": call["id"], "content": result }));
        }
        trim(messages);
        step += 1;
    }
}
