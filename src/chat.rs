//! One conversation with the model: send, stream the reply, run tool calls.

use std::borrow::Cow;
use std::error::Error;
use std::io::{self, BufRead, BufReader, Write};
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Value};

use crate::{config, tools};

const SYSTEM: &str = "You are nibble, a small local assistant. Answer directly and briefly. \
Output only what was asked for, with no preamble and no closing remarks.";

// Worded firmly, because a small model otherwise guesses at file contents,
// claims it has no access to files, or refuses a question that names no file.
// A larger model may want something lighter: both prompts are settings.
const SYSTEM_TOOLS: &str = " You have tools that read and search files on this machine. Assume a \
question is about the files in this directory unless it is plainly general knowledge. For such a \
question your first step is always a tool call: search for a key word from the question, or \
list_dir. Never say that you lack information before you have searched, and never ask the user \
which file to look at. When the input already holds everything you need, answer without tools.";

pub const SYSTEM_CLAUDE: &str = " If the task is too hard for you, call ask_claude with a complete \
question, then pass on its answer.";

/// The base system prompt: the "system" setting, or the built-in one.
pub fn system() -> &'static str {
    let custom = &config::get().system;
    if custom.is_empty() { SYSTEM } else { custom }
}

/// What is added when the model has file tools: the "system_tools" setting,
/// or the built-in text.
pub fn system_tools() -> String {
    let custom = &config::get().system_tools;
    if custom.is_empty() { SYSTEM_TOOLS.to_string() } else { format!(" {custom}") }
}

/// Add the tool calls in one streamed chunk to those collected so far.
/// Servers send them in two ways: whole (mlx-lm), or in pieces that share an
/// index (OpenAI, llama.cpp), where the arguments arrive a few characters at
/// a time.
fn merge_calls(calls: &mut Vec<Value>, pieces: &mut Vec<Value>) {
    for piece in pieces.drain(..) {
        let index = piece["index"].as_u64();
        let Some(call) = calls.iter_mut().find(|call| index.is_some() && call["index"].as_u64() == index) else {
            calls.push(piece);
            continue;
        };
        if let Some(more) = piece["function"]["arguments"].as_str() {
            let so_far = call["function"]["arguments"].as_str().unwrap_or_default();
            call["function"]["arguments"] = format!("{so_far}{more}").into();
        }
        for (owner, key) in [("function", "name"), ("", "id")] {
            let (from, to) = if owner.is_empty() { (&piece, &mut *call) } else { (&piece[owner], &mut call[owner]) };
            if from[key].as_str().is_some_and(|v| !v.is_empty()) && !to[key].is_string() {
                to[key] = from[key].clone();
            }
        }
    }
}

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
        // some models (gemma-3n hangs the server). A seed keeps us on the plain
        // path. Other servers take it as the usual sampling seed.
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
        if let Some(pieces) = delta["tool_calls"].as_array_mut() {
            merge_calls(&mut calls, pieces);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_calls_arrive_whole_or_in_pieces() {
        // mlx-lm: each call complete, each with its own index.
        let mut calls = Vec::new();
        merge_calls(&mut calls, &mut vec![json!({ "index": 0, "id": "a", "function": { "name": "list_dir", "arguments": "{}" } })]);
        merge_calls(&mut calls, &mut vec![json!({ "index": 1, "id": "b", "function": { "name": "search", "arguments": "{\"text\":\"x\"}" } })]);
        assert_eq!(calls.len(), 2);

        // OpenAI style: a name first, then the arguments in fragments.
        let mut calls = Vec::new();
        for piece in [
            json!({ "index": 0, "id": "c", "type": "function", "function": { "name": "read_file", "arguments": "" } }),
            json!({ "index": 0, "function": { "arguments": "{\"path\":" } }),
            json!({ "index": 0, "function": { "arguments": "\"a.txt\"}" } }),
        ] {
            merge_calls(&mut calls, &mut vec![piece]);
        }
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "read_file");
        assert_eq!(calls[0]["function"]["arguments"], "{\"path\":\"a.txt\"}");
        assert_eq!(calls[0]["id"], "c");
    }
}
