//! Plugins end to end: the real binary, a fake model server that calls a
//! plugin's tool, and a plugin that is a few lines of shell.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

/// An MCP server with one tool, `shout`, which answers in capitals.
const PLUGIN: &str = r#"
n=0
while read -r line; do
  case "$line" in *'"method":"notifications/'*) continue ;; esac
  n=$((n+1))
  case "$line" in
    *'"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%d,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"loud","version":"1"}}}\n' $n ;;
    *'"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%d,"result":{"tools":[{"name":"shout","description":"Say it loud","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}},{"name":"search","description":"Search the web"}]}}\n' $n ;;
    *'"text":"hi"'*)
      printf '{"jsonrpc":"2.0","id":%d,"result":{"content":[{"type":"text","text":"HI"}]}}\n' $n ;;
  esac
done
"#;

/// Calls `shout` once, then passes on what it said.
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
    let request: Value = serde_json::from_slice(&body).unwrap();
    let offered = request["tools"].as_array().is_some_and(|tools| tools.iter().any(|t| t["function"]["name"] == "shout"));
    let result = request["messages"].as_array().unwrap().iter().find(|m| m["role"] == "tool").map(|m| m["content"].clone());
    let delta = match result {
        Some(result) => json!({ "content": format!("the plugin said {}", result.as_str().unwrap()) }),
        None if offered => json!({ "tool_calls": [{ "index": 0, "id": "call_1", "type": "function",
            "function": { "name": "shout", "arguments": "{\"text\":\"hi\"}" } }] }),
        None => json!({ "content": "no plugin" }),
    };
    seen.lock().unwrap().push(request);
    let chunk = json!({ "choices": [{ "delta": delta }] });
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
        .env("NIBBLE_URL", url)
        .env("HOME", base)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("NIBBLE_MODEL")
        .env_remove("NIBBLE_TOKEN")
        .output()
        .unwrap()
}

#[test]
fn a_plugin_is_offered_only_when_asked_for() {
    let base = std::env::temp_dir().join(format!("nibble-plugins-{}", std::process::id()));
    fs::create_dir_all(&base).unwrap();
    let config = json!({ "plugins": { "loud": { "command": ["sh", "-c", PLUGIN] } } });
    fs::write(base.join("config.json"), config.to_string()).unwrap();
    let (url, seen) = fake_model_server();

    // Off by default.
    let out = nibble(&base, &url, &["--no-save", "say", "hi"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "no plugin");

    // Asked for: its tools come after nibble's own, and one whose name is
    // taken is renamed.
    seen.lock().unwrap().clear();
    let out = nibble(&base, &url, &["--no-save", "--no-claude", "--plugin", "loud", "say", "hi"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "the plugin said HI");
    let first = seen.lock().unwrap()[0].clone();
    let names: Vec<&str> = first["tools"].as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
    assert_eq!(names, ["read_file", "list_dir", "search", "shout", "loud_search"]);

    // Even with piped input, which otherwise means no tools.
    seen.lock().unwrap().clear();
    let mut child = Command::new(env!("CARGO_BIN_EXE_nibble"))
        .args(["--no-save", "-p", "loud", "say it"])
        .current_dir(&base)
        .env("NIBBLE_CONFIG", base.join("config.json"))
        .env("NIBBLE_URL", &url)
        .env("HOME", &base)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"hi").unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "the plugin said HI");
    let first = seen.lock().unwrap()[0].clone();
    let names: Vec<&str> = first["tools"].as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str()).collect();
    assert_eq!(names, ["shout", "loud_search"]);

    // A name that isn't set up is an error that says which are.
    let out = nibble(&base, &url, &["--no-save", "--plugin", "quiet", "say", "hi"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no plugin called quiet: there are loud"));

    // `nibble plugins` says what each costs.
    let out = nibble(&base, &url, &["plugins"]);
    let listed = String::from_utf8_lossy(&out.stdout);
    assert!(listed.starts_with("loud: 2 tools, "), "{listed}");
    assert!(listed.contains("shout, loud_search"), "{listed}");

    // A recipe can bring plugins with it.
    let config = json!({
        "plugins": { "loud": { "command": ["sh", "-c", PLUGIN], "tools": ["shout"] } },
        "recipes": { "loudly": { "prompt": "say {input}", "tools": false, "plugins": ["loud"] } },
    });
    fs::write(base.join("config.json"), config.to_string()).unwrap();
    seen.lock().unwrap().clear();
    let out = nibble(&base, &url, &["loudly", "hi"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "the plugin said HI");
    let first = seen.lock().unwrap()[0].clone();
    assert_eq!(first["tools"], json!([{ "type": "function", "function": {
        "name": "shout", "description": "Say it loud",
        "parameters": { "type": "object", "properties": { "text": { "type": "string" } } } } }]));

    // A mistake in the config file shows at once.
    fs::write(base.join("config.json"), r#"{ "plugins": { "loud": { "command": "sh" } } }"#).unwrap();
    let out = nibble(&base, &url, &["--no-save", "say", "hi"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("plugins.loud.command must be a list"));

    fs::remove_dir_all(&base).unwrap();
}
