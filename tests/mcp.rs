//! `nibble mcp` end to end: the real binary, spoken to over stdin and stdout
//! as Claude would, with a fake model server behind it. A separate process,
//! because the tools' confinement and working directory are process-wide.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

/// What the fake model says, chosen by what it was sent.
fn model_reply(request: &Value) -> Value {
    let messages = request["messages"].as_array().unwrap();
    let system = messages[0]["content"].as_str().unwrap_or_default();
    let user = messages.iter().find(|m| m["role"] == "user").unwrap()["content"].as_str().unwrap_or_default();
    let text = |text: &str| json!({ "content": text });
    if system.contains("give the evidence") && !user.contains("look for yourself") {
        // One true quote, one invented.
        return text("apples and cherries\n> a.txt: apples\n> b.txt: cherries");
    }
    if system.contains("give the evidence") && messages.iter().any(|m| m["role"] == "tool") {
        return text("a.txt says apples\n> ./a.txt:1: apples\n> ../secret.txt: outside the root");
    }
    if messages.iter().any(|m| m["role"] == "tool") {
        let result = messages.iter().rev().find(|m| m["role"] == "tool").unwrap()["content"].as_str().unwrap();
        text(&format!("a.txt says {}", if result.contains("apples") { "apples" } else { "nothing" }))
    } else if user.contains("look for yourself") {
        json!({ "tool_calls": [{ "index": 0, "id": "call_1", "type": "function",
            "function": { "name": "read_file", "arguments": "{\"path\":\"a.txt\"}" } }] })
    } else if user.contains("too hard") {
        text("TOO_HARD: I can't tell from this")
    } else if system.starts_with("You are given one file and one question.") {
        // Which file it was shown, and whether it saw all of it.
        let path = user.split("<file path=\"").nth(1).unwrap().split('"').next().unwrap();
        let cut = if user.contains("bytes cut") { ", cut" } else { "" };
        text(&format!("\nabout {path}{cut}\nand a second line nobody asked for"))
    } else {
        text("the answer")
    }
}

/// An OpenAI-style server that streams `model_reply` and keeps every request.
fn fake_model_server() -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let kept = seen.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.unwrap();
            let kept = kept.clone();
            thread::spawn(move || answer(stream, &kept));
        }
    });
    (url, seen)
}

fn answer(stream: TcpStream, seen: &Mutex<Vec<Value>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    let mut length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap();
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let mut stream = stream;
    if !first.starts_with("POST /v1/chat/completions ") {
        // nibble asks /info for the model name at start; this server has none.
        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    }
    let request: Value = serde_json::from_slice(&body).unwrap();
    let delta = model_reply(&request);
    seen.lock().unwrap().push(request);
    let chunk = json!({ "choices": [{ "delta": delta }] });
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {chunk}\n\ndata: [DONE]\n\n"
    );
}

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl Mcp {
    fn start(root: &PathBuf, url: &str) -> Mcp {
        let mut child = Command::new(env!("CARGO_BIN_EXE_nibble"))
            .args(["mcp", "--root"])
            .arg(root)
            // Only our settings, never the config file of whoever runs the tests.
            .env("NIBBLE_CONFIG", root.join("config.json"))
            .env("NIBBLE_URL", url)
            .env_remove("NIBBLE_MODEL")
            .env_remove("NIBBLE_TOKEN")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let (stdin, stdout) = (child.stdin.take().unwrap(), BufReader::new(child.stdout.take().unwrap()));
        Mcp { child, stdin, stdout, next_id: 0 }
    }

    fn send(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").unwrap();
    }

    fn receive(&mut self) -> Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line:?}"))
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string());
        let response = self.receive();
        assert_eq!(response["id"], id);
        response
    }

    /// Call a tool and return its text, and whether it was an error.
    fn call(&mut self, name: &str, arguments: Value) -> (String, bool) {
        let response = self.request("tools/call", json!({ "name": name, "arguments": arguments }));
        let result = &response["result"];
        (result["content"][0]["text"].as_str().unwrap().to_string(), result["isError"].as_bool().unwrap())
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn claude_can_delegate_and_map_through_nibble_mcp() {
    let base = std::env::temp_dir().join(format!("nibble-mcp-test-{}", std::process::id()));
    let root = base.join("project");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(base.join("secret.txt"), "outside the root\n").unwrap();
    std::fs::write(root.join("a.txt"), "apples\n").unwrap();
    std::fs::write(root.join("b.txt"), "bananas\n").unwrap();
    std::fs::write(root.join("big.txt"), "x".repeat(3000)).unwrap();
    std::fs::write(root.join("config.json"), r#"{ "input_chars": 2000, "map_file_chars": 500 }"#).unwrap();
    let (url, seen) = fake_model_server();
    let mut mcp = Mcp::start(&root, &url);

    // The handshake, as Claude Code does it.
    let response = mcp.request("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {} }));
    assert_eq!(response["result"]["protocolVersion"], "2025-06-18");
    assert!(response["result"]["instructions"].as_str().unwrap().contains("Pass file paths"));
    mcp.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    mcp.send("");
    assert_eq!(mcp.request("ping", json!({}))["result"], json!({}));
    let response = mcp.request("tools/list", json!({}));
    assert_eq!(response["result"]["tools"].as_array().unwrap().len(), 2);
    // The limits come from the config file.
    assert!(response["result"]["tools"][1]["description"].as_str().unwrap().contains("over 500 characters"));

    // Protocol errors get JSON-RPC errors, and the server keeps going.
    let response = mcp.request("resources/list", json!({}));
    assert_eq!(response["error"]["code"], -32601);
    mcp.send("{ not json");
    let response = mcp.receive();
    assert_eq!((response["id"].clone(), response["error"]["code"].clone()), (Value::Null, json!(-32700)));

    // delegate with paths: the model is shown the whole files, and nothing else.
    let (text, error) = mcp.call("delegate", json!({ "task": "What fruit?", "paths": ["a.txt", "b.txt"] }));
    assert!(!error);
    assert_eq!(text, "status: ok\nread: a.txt (1 lines, complete), b.txt (1 lines, complete)\nanswer:\nthe answer\n");
    {
        let seen = seen.lock().unwrap();
        let request = seen.last().unwrap();
        let user = request["messages"][1]["content"].as_str().unwrap();
        assert!(user.starts_with("What fruit?"), "{user}");
        assert!(user.contains("<file path=\"a.txt\">\napples\n</file>"), "{user}");
        assert!(user.contains("<file path=\"b.txt\">\nbananas\n</file>"), "{user}");
        assert!(request.get("tools").is_none(), "a model shown the files needs no tools");
        assert_eq!(request["stream"], true);
    }

    // The model may say the task is beyond it.
    let (text, error) = mcp.call("delegate", json!({ "task": "This is too hard", "paths": ["a.txt"] }));
    assert!(!error);
    assert!(text.starts_with("status: too_hard\n") && text.ends_with("answer:\nI can't tell from this\n"), "{text}");

    // Too much to show at once: refused before the model sees any of it.
    let before = seen.lock().unwrap().len();
    let (text, error) = mcp.call("delegate", json!({ "task": "Summarise", "paths": ["big.txt"] }));
    assert!(!error && text.starts_with("status: too_large\n"), "{text}");
    assert!(text.contains("big.txt: 3000 characters") && text.contains("limit 2000"), "{text}");
    assert_eq!(seen.lock().unwrap().len(), before);

    // Nothing outside the root, by any path.
    for path in ["../secret.txt", base.join("secret.txt").to_str().unwrap()] {
        let (text, error) = mcp.call("delegate", json!({ "task": "Read it", "paths": [path] }));
        assert!(error && text.contains("outside") && !text.contains("outside the root\n"), "{path}: {text}");
    }
    assert_eq!(seen.lock().unwrap().len(), before);

    // delegate without paths: the model reads for itself, and Claude is told what it read.
    let (text, error) = mcp.call("delegate", json!({ "task": "look for yourself: what is in a.txt?" }));
    assert!(!error);
    assert_eq!(text, "status: ok\ntools used: read_file a.txt\nanswer:\na.txt says apples\n");
    assert!(seen.lock().unwrap().iter().rev().nth(1).unwrap()["tools"].is_array());

    // With quote, each quote is looked for in the file it names.
    let (text, error) = mcp.call("delegate", json!({ "task": "What fruit?", "paths": ["a.txt", "b.txt"], "quote": true }));
    assert!(!error);
    assert!(text.contains("\nquotes: 1 of 2 quotes not found:\n  b.txt: cherries (not in the file)\n"), "{text}");
    let (text, error) =
        mcp.call("delegate", json!({ "task": "look for yourself: what is in a.txt?", "quote": true }));
    assert!(!error);
    assert!(text.contains("\nquotes: 1 of 2 quotes not found:\n  ../secret.txt: outside the root (not a file it could read)\n"), "{text}");

    // map: one fresh conversation and one line per file. A file that can't be
    // read is a line saying so, not a failed call.
    let before = seen.lock().unwrap().len();
    let paths = ["a.txt", "missing.txt", "big.txt"];
    let (text, error) = mcp.call("map", json!({ "prompt": "What is this?", "paths": paths }));
    assert!(!error, "{text}");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "status: ok");
    assert!(lines[1].starts_with("files: 3, seconds: "), "{text}");
    assert_eq!(lines[2], "answer:");
    assert_eq!(lines[3], "a.txt: about a.txt");
    assert!(lines[4].starts_with("missing.txt: error: "), "{text}");
    assert_eq!(lines[5], "big.txt: about big.txt, cut [partial: 500 of 3000 characters read]");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), before + 2);
    for request in &seen[before..] {
        assert_eq!(request["messages"].as_array().unwrap().len(), 2);
    }
    drop(seen);

    drop(mcp);
    std::fs::remove_dir_all(&base).unwrap();
}
