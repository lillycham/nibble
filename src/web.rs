//! The small HTTP layer in front of `nibble serve`: the chat page, and the
//! endpoint behind it that runs a chat turn with tools on this machine.
//! Requests for the model API itself (`/v1/...`) are passed on by `serve`.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::OnceLock;

use serde_json::{json, Value};

use crate::chat::{self, message};
use crate::quotes::{self, Sources};
use crate::{config, tools};

const PAGE: &str = include_str!("chat.html");

const MAX_HEAD: usize = 64 << 10;
const MAX_BODY: usize = 1 << 20;

/// Whether chats may use the file tools. Only when roots are configured,
/// because a server has no working directory worth trusting.
static TOOLS: OnceLock<bool> = OnceLock::new();

pub fn allow_tools(allow: bool) {
    let _ = TOOLS.set(allow);
}

pub fn tools_allowed() -> bool {
    config::get().tools && TOOLS.get().copied().unwrap_or(false)
}

pub struct Request {
    pub method: String,
    pub path: String,
    authorization: String,
    content_length: usize,
    /// Everything read from the client so far: the head, and perhaps some body.
    pub raw: Vec<u8>,
    head_len: usize,
}

/// Read up to the end of the request head. None means the client sent
/// nothing we can use.
pub fn read_request(client: &mut TcpStream) -> io::Result<Option<Request>> {
    let mut raw = Vec::new();
    let mut buffer = [0; 4096];
    let head_len = loop {
        if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
        let count = client.read(&mut buffer)?;
        if count == 0 || raw.len() > MAX_HEAD {
            return Ok(None);
        }
        raw.extend_from_slice(&buffer[..count]);
    };

    let head = String::from_utf8_lossy(&raw[..head_len]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let method = first.next().unwrap_or_default().to_string();
    let target = first.next().unwrap_or_default();
    let path = target.split('?').next().unwrap_or_default().to_string();

    let (mut authorization, mut content_length) = (String::new(), 0);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        match name.to_ascii_lowercase().as_str() {
            "authorization" => authorization = value.trim().to_string(),
            "content-length" => content_length = value.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    Ok(Some(Request { method, path, authorization, content_length, raw, head_len }))
}

impl Request {
    /// The page carries no secrets, and a browser can't send a token when it
    /// first opens it, so it needs none. Nor does /info, which only names the
    /// models, and which a client asks before it has read its settings. It
    /// adds the settings in use only for a client with the token.
    pub fn is_public(&self) -> bool {
        self.method == "GET" && (self.path == "/" || self.path == "/info")
    }

    /// With no token configured, anyone who can connect may use the server,
    /// which is why `serve` refuses to listen beyond this machine without one.
    pub fn authorized(&self) -> bool {
        let token = &config::get().token;
        if token.is_empty() {
            return true;
        }
        let Some(given) = self.authorization.strip_prefix("Bearer ") else { return false };
        // Compare every byte, so the time taken says nothing about where they differ.
        given.len() == token.len() && given.bytes().zip(token.bytes()).fold(0, |diff, (a, b)| diff | (a ^ b)) == 0
    }

    pub fn content_length(&self) -> usize {
        self.content_length
    }

    /// The request line and headers, up to and including the blank line.
    pub fn head(&self) -> &[u8] {
        &self.raw[..self.head_len]
    }

    pub fn body(&self, client: &mut TcpStream) -> io::Result<Vec<u8>> {
        self.body_within(client, MAX_BODY)
    }

    pub fn body_within(&self, client: &mut TcpStream, limit: usize) -> io::Result<Vec<u8>> {
        if self.content_length > limit {
            return Err(io::Error::other("request body too large"));
        }
        let mut body = self.raw[self.head_len..].to_vec();
        let have = body.len();
        if have < self.content_length {
            body.resize(self.content_length, 0);
            client.read_exact(&mut body[have..])?;
        }
        body.truncate(self.content_length);
        Ok(body)
    }
}

pub fn respond(client: &mut TcpStream, status: &str, kind: &str, body: &[u8]) -> io::Result<()> {
    write!(
        client,
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    client.write_all(body)
}

/// A chat turn as server-sent events: {"text"} as the reply grows, {"tool"}
/// for each tool call, then {"done"} with the turn's stats, or {"error"}.
/// The request may name a "recipe" whose settings apply to the turn, and ask
/// for "quote": then {"done"} says which quotes were found in the files.
struct Stream<'a>(&'a mut TcpStream);

impl Stream<'_> {
    fn send(&mut self, event: Value) -> io::Result<()> {
        write!(self.0, "data: {event}\n\n")?;
        self.0.flush()
    }
}

impl chat::Events for Stream<'_> {
    fn text(&mut self, text: &str) -> io::Result<()> {
        self.send(json!({ "text": text }))
    }

    fn tool(&mut self, name: &str, arguments: &Value) -> io::Result<()> {
        self.send(json!({ "tool": chat::describe(name, arguments) }))
    }
}

fn chat_turn(client: &mut TcpStream, request: &Request) -> io::Result<()> {
    let body: Value = match serde_json::from_slice(&request.body(client)?) {
        Ok(body) => body,
        Err(e) => return respond(client, "400 Bad Request", "text/plain", format!("bad JSON: {e}\n").as_bytes()),
    };
    // A recipe's settings apply to this turn; the client has already put its
    // prompt in the message, so the history it sends next reads the same.
    let recipe = match body["recipe"].as_str() {
        None => None,
        Some(name) => match config::get().recipe(name) {
            Some(recipe) => Some(recipe),
            None => return respond(client, "400 Bad Request", "text/plain", format!("no recipe called \"{name}\"\n").as_bytes()),
        },
    };
    // A client turns them off for a chat with files attached, as `-f` does:
    // the files are the whole task.
    let use_tools = tools_allowed() && body["tools"].as_bool() != Some(false) && recipe.and_then(|r| r.tools) != Some(false);
    // No ask_claude here: a remote chat should not be able to spend Claude
    // usage on this machine.
    let tools = if use_tools { tools::schemas(false) } else { Vec::new() };
    let mut system = match recipe.and_then(|r| r.system.as_deref()) {
        Some(system) => system.to_string(),
        None => {
            let mut system = chat::system().to_string();
            if use_tools {
                system += &chat::system_tools();
                system += &tools::context();
            }
            system
        }
    };
    let quote = body["quote"].as_bool() == Some(true) || recipe.is_some_and(|r| r.quote);
    if quote {
        system += quotes::SYSTEM_QUOTE;
    }
    let max_tokens = recipe.and_then(|r| r.max_tokens).unwrap_or(config::get().max_tokens);

    // The client keeps the history and sends it each time. Take only its user
    // and assistant turns, so the system prompt stays ours.
    let mut messages = vec![message("system", &system)];
    for turn in body["messages"].as_array().into_iter().flatten() {
        if let (Some(role @ ("user" | "assistant")), Some(content)) = (turn["role"].as_str(), turn["content"].as_str()) {
            messages.push(message(role, content));
        }
    }
    if messages.last().is_none_or(|last| last["role"] != "user") {
        return respond(client, "400 Bad Request", "text/plain", b"the last message must be from the user\n");
    }
    // Quotes may come from the files attached anywhere in the chat, and from
    // the files the tools can read.
    let sources = quote.then(|| {
        let mut sources = Sources::new(use_tools);
        for turn in messages.iter().filter(|m| m["role"] == "user") {
            for (path, text) in quotes::attached(turn["content"].as_str().unwrap_or_default()) {
                sources.give(&path, &text);
            }
        }
        sources
    });
    chat::trim(&mut messages);

    write!(client, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n")?;
    let mut stream = Stream(client);
    match chat::run(&mut messages, &tools, max_tokens, &mut stream) {
        Ok(outcome) => {
            // With the room there is, so a client can show how full the chat is.
            let mut stats = outcome.stats.to_json();
            stats["input_chars"] = config::get().input_chars.into();
            let mut done = json!({ "done": true, "stats": stats });
            if let Some(sources) = &sources {
                let check = quotes::check(&outcome.reply, sources);
                done["quotes"] = json!({ "found": check.found.len(), "missing": check.missing.len(), "report": check.report() });
            }
            stream.send(done)
        }
        Err(e) => stream.send(json!({ "error": e.to_string() })),
    }
}

/// Answer a request that is ours, not the model server's.
pub fn route(client: &mut TcpStream, request: &Request) -> io::Result<()> {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") => respond(client, "200 OK", "text/html; charset=utf-8", PAGE.as_bytes()),
        ("GET", "/info") => respond(client, "200 OK", "application/json", crate::serve::info(request.authorized()).to_string().as_bytes()),
        ("POST", "/chat") => chat_turn(client, request),
        _ => respond(client, "404 Not Found", "text/plain", b"not found\n"),
    }
}
