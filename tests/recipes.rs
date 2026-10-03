//! Recipes on the command line, end to end: the real binary, a config file
//! that declares them, and a fake model server that keeps what it is sent.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

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
    let chunk = json!({ "choices": [{ "delta": { "content": "Fix the thing" } }] });
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {chunk}\n\ndata: [DONE]\n\n"
    );
}

fn nibble(base: &Path, url: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_nibble"))
        .args(args)
        .current_dir(base)
        .env("NIBBLE_CONFIG", base.join("config.json"))
        .env("XDG_DATA_HOME", base.join("data"))
        .env("NIBBLE_URL", url)
        .env_remove("NIBBLE_MODEL")
        .env_remove("NIBBLE_TOKEN")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn recipes_run_as_commands_with_their_own_settings() {
    let base = std::env::temp_dir().join(format!("nibble-recipes-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();
    let (url, seen) = fake_model_server();
    let config = json!({ "max_tokens": 100, "recipes": {
        "review": { "description": "Review a change", "prompt": "Review this:\n{input}\nBe brief.",
                    "system": "You review code.", "max_tokens": 600, "tools": false },
        "commit": { "prompt": "Write a commit message.", "command": ["echo", "a staged diff"], "tools": false },
    } });
    fs::write(base.join("config.json"), config.to_string()).unwrap();
    let last = || seen.lock().unwrap().last().unwrap().clone();
    let messages = |request: &Value| -> Vec<(String, String)> {
        request["messages"].as_array().unwrap().iter().map(|m| (m["role"].as_str().unwrap().into(), m["content"].as_str().unwrap().into())).collect()
    };

    // The text after the name goes where {input} is; the recipe's settings apply.
    let out = nibble(&base, &url, &["review", "the", "diff"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "Fix the thing\n");
    let request = last();
    assert_eq!(request["max_tokens"], 600);
    assert!(request["tools"].is_null());
    assert_eq!(messages(&request), [("system".into(), "You review code.".into()), ("user".into(), "Review this:\nthe diff\nBe brief.".into())]);

    // Flags still win over the recipe.
    let out = nibble(&base, &url, &["review", "-n", "50", "x"]);
    assert!(out.status.success());
    assert_eq!(last()["max_tokens"], 50);

    // Given nothing, the recipe's command gives the input.
    let out = nibble(&base, &url, &["commit"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let request = last();
    assert_eq!(request["max_tokens"], 100);
    assert_eq!(messages(&request)[1].1, "Write a commit message.\n\n<input>\na staged diff\n</input>");

    // The built-in ones are there too, and the list shows them all.
    let out = nibble(&base, &url, &["recipes"]);
    let listed = String::from_utf8_lossy(&out.stdout);
    let names: Vec<&str> = listed.lines().map(|line| line.split_whitespace().next().unwrap()).collect();
    assert_eq!(names, ["commit", "review", "summarise"]);
    assert!(listed.contains("Review a change"));

    // A name that is not a recipe is still a plain prompt.
    let out = nibble(&base, &url, &["--no-tools", "reviewing", "is", "fun"]);
    assert!(out.status.success());
    assert_eq!(messages(&last())[1].1, "reviewing is fun");

    // A recipe that would shadow a command is refused, loudly.
    fs::write(base.join("config.json"), json!({ "recipes": { "chats": { "prompt": "x" } } }).to_string()).unwrap();
    let out = nibble(&base, &url, &["recipes"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("recipes.chats"), "{}", String::from_utf8_lossy(&out.stderr));

    let _ = fs::remove_dir_all(&base);
}
