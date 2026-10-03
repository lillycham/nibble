//! Saved chats from the command line, end to end: the real binary, a fake
//! model server, and a chat file written as the window would write it.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

/// An OpenAI-style server that answers "the answer" and keeps every request.
fn fake_model_server() -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let kept = seen.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            answer(stream.unwrap(), &kept);
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
        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    }
    seen.lock().unwrap().push(serde_json::from_slice(&body).unwrap());
    let chunk = json!({ "choices": [{ "delta": { "content": "the answer" } }] });
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {chunk}\n\ndata: [DONE]\n\n"
    );
}

fn nibble(base: &Path, url: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_nibble"))
        .args(args)
        .current_dir(base)
        // Only our settings and our data, never those of whoever runs the tests.
        .env("NIBBLE_CONFIG", base.join("config.json"))
        .env("XDG_DATA_HOME", base.join("data"))
        .env("NIBBLE_URL", url)
        .env_remove("NIBBLE_MODEL")
        .env_remove("NIBBLE_TOKEN")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn saved(base: &Path, id: &str) -> Value {
    serde_json::from_str(&fs::read_to_string(base.join(format!("data/nibble/chats/{id}.json"))).unwrap()).unwrap()
}

#[test]
fn a_chat_from_the_window_goes_on_in_the_terminal() {
    let base = std::env::temp_dir().join(format!("nibble-sessions-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let chats = base.join("data/nibble/chats");
    fs::create_dir_all(&chats).unwrap();
    let (url, seen) = fake_model_server();

    // Nothing saved yet.
    let out = nibble(&base, &url, &["--no-tools", "-c", "hello"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no saved chats"));

    // As the window saves a chat, including a tool line.
    let window = json!({ "title": "What is in src?", "turns": [
        { "user": "What is in src?", "parts": [{ "tool": "list_dir src" }, { "text": "main.rs" }] },
    ] });
    fs::write(chats.join("1000.json"), window.to_string()).unwrap();
    fs::write(chats.join("999.json"), json!({ "title": "Older", "turns": [] }).to_string()).unwrap();

    // A one-shot prompt without a flag is not saved.
    let out = nibble(&base, &url, &["--no-tools", "unsaved question"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(fs::read_dir(&chats).unwrap().count(), 2);

    // Resume by id: the model is sent the history, and the turn is kept.
    let out = nibble(&base, &url, &["--no-tools", "-r", "1000", "And tests?"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "the answer\n");
    let sent = seen.lock().unwrap().last().unwrap()["messages"].clone();
    let sent: Vec<(&str, &str)> = sent.as_array().unwrap().iter().map(|m| (m["role"].as_str().unwrap(), m["content"].as_str().unwrap())).collect();
    assert_eq!(sent[1..], [("user", "What is in src?"), ("assistant", "main.rs"), ("user", "And tests?")]);
    let chat = saved(&base, "1000");
    assert_eq!(chat["turns"].as_array().unwrap().len(), 2);
    assert_eq!(chat["turns"][1], json!({ "user": "And tests?", "parts": [{ "text": "the answer" }] }));
    assert_eq!(chat["title"], "What is in src?");

    // -c picks the newest, comparing ids as numbers.
    let out = nibble(&base, &url, &["--no-tools", "-c", "Third"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(saved(&base, "1000")["turns"].as_array().unwrap().len(), 3);

    // --no-save goes on with the chat but leaves the file alone.
    let out = nibble(&base, &url, &["--no-tools", "--no-save", "-c", "Fourth"]);
    assert!(out.status.success());
    assert_eq!(saved(&base, "1000")["turns"].as_array().unwrap().len(), 3);

    // Ids that are not ours are refused.
    for id in ["../config", "123"] {
        let out = nibble(&base, &url, &["--no-tools", "-r", id, "x"]);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("no saved chat"), "{id}");
    }

    let out = nibble(&base, &url, &["chats"]);
    let listed = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = listed.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].starts_with("1000 ") && lines[0].ends_with("What is in src?"), "{listed}");
    assert!(lines[1].starts_with("999 ") && lines[1].ends_with("Older"), "{listed}");

    let _ = fs::remove_dir_all(&base);
}
