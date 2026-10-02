//! The small HTTP layer in front of `nibble serve`: the chat page, and the
//! endpoint behind it that runs a chat turn with tools on this machine.
//! Requests for the model API itself (`/v1/...`) are passed on by `serve`.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::OnceLock;

use serde_json::{json, Value};

use crate::chat::{self, message};
use crate::{config, tools};

const PAGE: &str = include_str!("chat.html");

const MAX_HEAD: usize = 64 << 10;
const MAX_BODY: usize = 16 << 20;

/// Whether chats may use the file tools. Only when roots are configured,
/// because a server has no working directory worth trusting.
static TOOLS: OnceLock<bool> = OnceLock::new();

pub fn allow_tools(allow: bool) {
    let _ = TOOLS.set(allow);
}

/// The folders chats may read, as `serve` settled on them.
static ROOTS: OnceLock<Vec<String>> = OnceLock::new();

pub fn set_roots(roots: Vec<String>) {
    let _ = ROOTS.set(roots);
}

pub fn roots() -> &'static [String] {
    ROOTS.get().map_or(&[], Vec::as_slice)
}

pub fn tools_allowed() -> bool {
    config::get().tools && TOOLS.get().copied().unwrap_or(false)
}

pub struct Request {
    pub method: String,
    pub path: String,
    authorization: String,
    content_length: usize,
    chunked: bool,
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

    let (mut authorization, mut content_length, mut chunked) = (String::new(), 0, false);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        match name.to_ascii_lowercase().as_str() {
            "authorization" => authorization = value.trim().to_string(),
            "content-length" => content_length = value.trim().parse().unwrap_or(0),
            "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
            _ => {}
        }
    }
    Ok(Some(Request { method, path, authorization, content_length, chunked, raw, head_len }))
}

impl Request {
    /// The page carries no secrets, and a browser can't send a token when it
    /// first opens it, so it needs none. Nor does /info, which only names the
    /// models, and which a client asks before it has read its settings.
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

    /// The request as it should reach the model server, with the "model" of a
    /// JSON body set to `model`. Otherwise a client with the token could make
    /// the server load any model it names, even one from Hugging Face.
    /// A body that is no JSON object goes on as it is: the server rejects it.
    pub fn pinned(&self, client: &mut TcpStream, model: &str) -> io::Result<Vec<u8>> {
        if self.method != "POST" {
            return Ok(self.raw.clone());
        }
        if self.chunked {
            return Err(io::Error::other("a chunked request body is not supported"));
        }
        let raw_body = self.body(client)?;
        let Ok(Value::Object(mut body)) = serde_json::from_slice(&raw_body) else {
            return Ok([&self.raw[..self.head_len], &raw_body[..]].concat());
        };
        body.insert("model".into(), model.into());
        let body = Value::Object(body).to_string();
        let head = String::from_utf8_lossy(&self.raw[..self.head_len]);
        let mut out = String::new();
        for line in head.split("\r\n").filter(|line| !line.is_empty()) {
            if !line.to_ascii_lowercase().starts_with("content-length:") {
                out += &format!("{line}\r\n");
            }
        }
        out += &format!("Content-Length: {}\r\n\r\n{body}", body.len());
        Ok(out.into_bytes())
    }

    pub fn body(&self, client: &mut TcpStream) -> io::Result<Vec<u8>> {
        if self.content_length > MAX_BODY {
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
/// for each tool call, then {"done"} or {"error"}.
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
        let about = arguments["text"].as_str().or(arguments["path"].as_str()).unwrap_or(".");
        self.send(json!({ "tool": format!("{name} {about}") }))
    }
}

fn chat_turn(client: &mut TcpStream, request: &Request) -> io::Result<()> {
    let body: Value = match serde_json::from_slice(&request.body(client)?) {
        Ok(body) => body,
        Err(e) => return respond(client, "400 Bad Request", "text/plain", format!("bad JSON: {e}\n").as_bytes()),
    };
    let use_tools = tools_allowed();
    // No ask_claude here: a remote chat should not be able to spend Claude
    // usage on this machine.
    let tools = if use_tools { tools::schemas(false) } else { Vec::new() };
    let mut system = chat::system().to_string();
    if use_tools {
        system += &chat::system_tools();
        system += &tools::context();
    }

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
    chat::trim(&mut messages);

    write!(client, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n")?;
    let mut stream = Stream(client);
    match chat::run(&mut messages, &tools, config::get().max_tokens, &mut stream) {
        Ok(_) => stream.send(json!({ "done": true })),
        Err(e) => stream.send(json!({ "error": e.to_string() })),
    }
}

/// Answer a request that is ours, not the model server's.
pub fn route(client: &mut TcpStream, request: &Request) -> io::Result<()> {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") => respond(client, "200 OK", "text/html; charset=utf-8", PAGE.as_bytes()),
        ("GET", "/info") => respond(client, "200 OK", "application/json", crate::serve::info().to_string().as_bytes()),
        ("GET", "/settings") => respond(client, "200 OK", "application/json", crate::serve::effective().to_string().as_bytes()),
        ("POST", "/chat") => chat_turn(client, request),
        _ => respond(client, "404 Not Found", "text/plain", b"not found\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn pinned_sets_the_model_and_the_length() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut sender = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let body = r#"{"model":"evil/model","x":1}"#;
        write!(sender, "POST /v1/chat/completions HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        let (mut client, _) = listener.accept().unwrap();
        let request = read_request(&mut client).unwrap().unwrap();
        let out = String::from_utf8(request.pinned(&mut client, "/m/good").unwrap()).unwrap();
        let (head, new_body) = out.split_once("\r\n\r\n").unwrap();
        let parsed: Value = serde_json::from_str(new_body).unwrap();
        assert_eq!(parsed["model"], "/m/good");
        assert_eq!(parsed["x"], 1);
        assert!(head.contains(&format!("Content-Length: {}", new_body.len())));
        assert_eq!(head.matches("Content-Length").count(), 1);
    }
}
