//! `nibble eval` end to end: the real binary, with a fake model server that
//! reads one file and then answers from the answer key, wrongly on purpose
//! for question 2, and that can switch models.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

const QUESTIONS: &str = include_str!("../eval/questions.json");

fn model_reply(request: &Value) -> Value {
    let messages = request["messages"].as_array().unwrap();
    let user = messages.iter().find(|m| m["role"] == "user").unwrap()["content"].as_str().unwrap();
    if user == "Say ok." {
        return json!({ "content": "ok" });
    }
    if !messages.iter().any(|m| m["role"] == "tool") {
        return json!({ "tool_calls": [{ "index": 0, "id": "call_1", "type": "function",
            "function": { "name": "read_file", "arguments": "{\"path\":\"README.md\"}" } }] });
    }
    let questions: Value = serde_json::from_str(QUESTIONS).unwrap();
    let question = questions.as_array().unwrap().iter().find(|q| q["question"] == user).unwrap();
    if question["expect"][0] == "45" {
        return json!({ "content": "Every 120 seconds." });
    }
    let terms: Vec<&str> =
        question["expect"].as_array().unwrap().iter().map(|t| t.as_str().unwrap().split('|').next().unwrap()).collect();
    json!({ "content": format!("It is {}.", terms.join(" and ")) })
}

struct Fake {
    model: Mutex<String>,
    switches: Mutex<Vec<String>>,
}

fn answer(stream: TcpStream, fake: &Fake) {
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
    let mut send = |kind: &str, body: String| {
        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nConnection: close\r\n\r\n{body}");
    };
    if first.starts_with("GET /info") {
        send("application/json", json!({ "model": *fake.model.lock().unwrap() }).to_string());
    } else if first.starts_with("POST /model ") {
        let request: Value = serde_json::from_slice(&body).unwrap();
        let model = request["model"].as_str().unwrap().to_string();
        *fake.model.lock().unwrap() = model.clone();
        fake.switches.lock().unwrap().push(model.clone());
        send("application/json", json!({ "model": model }).to_string());
    } else {
        let request: Value = serde_json::from_slice(&body).unwrap();
        let chunk = json!({ "choices": [{ "delta": model_reply(&request) }] });
        send("text/event-stream", format!("data: {chunk}\n\ndata: [DONE]\n\n"));
    }
}

#[test]
fn eval_scores_answers_counts_calls_and_switches_back() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let fake = Arc::new(Fake { model: Mutex::new("first-model".into()), switches: Mutex::new(Vec::new()) });
    let served = fake.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let fake = served.clone();
            thread::spawn(move || answer(stream.unwrap(), &fake));
        }
    });
    let home = std::env::temp_dir().join(format!("nibble-eval-test-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let nibble = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_nibble"))
            .args(args)
            .env("NIBBLE_CONFIG", home.join("config.json"))
            .env("NIBBLE_URL", &url)
            .env_remove("NIBBLE_MODEL")
            .env_remove("NIBBLE_TOKEN")
            .output()
            .unwrap();
        (output.status.success(), String::from_utf8(output.stdout).unwrap(), String::from_utf8(output.stderr).unwrap())
    };

    // The model in use, all questions.
    let (ok, out, err) = nibble(&["eval"]);
    assert!(ok, "{err}");
    assert!(out.starts_with("first-model\n"), "{out}");
    assert!(out.contains("   1  right   1 call     "), "{out}");
    assert!(out.contains("   2  wrong   1 call     "), "{out}");
    assert!(out.contains("expected 45; answered: Every 120 seconds."), "{out}");
    assert!(out.contains("first-model: 19 of 20 right, 20 tool calls, "), "{out}");
    assert!(fake.switches.lock().unwrap().is_empty());

    // Another model, some questions, and then back to the first.
    let (ok, out, err) = nibble(&["eval", "--model", "second-model", "--only", "2,5"]);
    assert!(ok, "{err}");
    assert!(out.starts_with("second-model\n") && out.contains("second-model: 1 of 2 right, 2 tool calls, "), "{out}");
    assert_eq!(*fake.switches.lock().unwrap(), ["second-model", "first-model"]);

    let (ok, _, err) = nibble(&["eval", "--only", "21"]);
    assert!(!ok && err.contains("there is no question 21"), "{err}");

    std::fs::remove_dir_all(&home).unwrap();
}
