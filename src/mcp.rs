//! `nibble mcp`: an MCP server on stdin and stdout, so that Claude can hand
//! small reading tasks to the local model.
//!
//! Claude pays for every token it reads, so the tools take file paths and
//! return a few lines. Nothing here writes to stdout except protocol messages.

use std::error::Error;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::time::Instant;

use serde_json::{json, Value};

use crate::chat::{self, message};
use crate::{config, tools};

pub const USAGE: &str = "usage: nibble mcp [--root DIR]...

Runs an MCP server on stdin and stdout, with the tools `delegate` and `map`.

  --root DIR    a directory the tools may read inside; may be given more than once

Without --root, the roots are the directory nibble was started in (unless
that is /) and the \"roots\" setting.";

// Sent to the client once, at the start. This is where Claude learns when
// delegation is worth it.
const INSTRUCTIONS: &str = "nibble runs a small local model at no token cost. \
Use it to read files you have not read yourself, when the answer you need is short: summarise, \
classify, extract, find. Pass file paths, never file contents. It is much slower than you \
(seconds per file) and it can be wrong, so use it where a wrong answer is cheap to spot, and check anything \
that matters. Do not use it for reasoning, for code changes, or for files you have already read.";

const TOO_HARD: &str = " If you cannot do the task reliably from what you have, reply with \
exactly one line: TOO_HARD: and the reason.";

const SYSTEM_MAP: &str = "You are given one file and one question. Answer about this file only, \
in one short line, with no preamble.";

// Any of these will do: we only use tools with text results, which are the
// same in every version so far.
const PROTOCOLS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

fn tool_list() -> Value {
    let config = config::get();
    let paths = json!({ "type": "array", "items": { "type": "string" } });
    json!([
        {
            "name": "delegate",
            "description": "Give one small, self-contained task to a local model. With `paths`, the \
                model is shown those whole files and nothing else. Without `paths`, it can read and \
                search files itself. Returns a status (ok, too_large, too_hard, error), what it read, \
                and a short answer.",
            "inputSchema": {
                "type": "object",
                "properties": { "task": { "type": "string" }, "paths": paths },
                "required": ["task"],
            },
        },
        {
            "name": "map",
            "description": format!(
                "Ask the same question about each file separately, and get one short line back per \
                 file. For classifying or summarising many files. At most {} files per call; only the \
                 start and end of a file over {} characters are read.",
                config.map_max_files, config.map_file_chars
            ),
            "inputSchema": {
                "type": "object",
                "properties": { "prompt": { "type": "string" }, "paths": paths },
                "required": ["prompt", "paths"],
            },
        },
    ])
}

struct Report {
    status: &'static str,
    notes: Vec<String>,
    answer: String,
}

impl Report {
    fn error(message: impl ToString) -> Self {
        Report { status: "error", notes: Vec::new(), answer: message.to_string() }
    }

    fn text(&self) -> String {
        let mut out = format!("status: {}\n", self.status);
        for note in &self.notes {
            out += note;
            out.push('\n');
        }
        if !self.answer.trim().is_empty() {
            out += "answer:\n";
            out += self.answer.trim();
            out.push('\n');
        }
        out
    }
}

fn text_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, Report> {
    match args[key].as_str().map(str::trim) {
        Some(s) if !s.is_empty() => Ok(s),
        _ => Err(Report::error(format!("missing argument: {key}"))),
    }
}

fn paths_arg(args: &Value) -> Vec<&str> {
    args["paths"].as_array().map(|paths| paths.iter().filter_map(Value::as_str).collect()).unwrap_or_default()
}

fn attach(path: &str, text: &str) -> String {
    format!("\n\n<file path=\"{path}\">\n{}\n</file>", text.trim_end())
}

/// Turn the model's reply into a status. A reply that starts with TOO_HARD is
/// the model saying so itself, which is unreliable but costs nothing to honour.
fn judge(outcome: chat::Outcome, notes: Vec<String>) -> Report {
    let reply = outcome.reply.trim();
    if let Some(reason) = reply.strip_prefix("TOO_HARD") {
        return Report { status: "too_hard", notes, answer: reason.trim_start_matches(':').trim().to_string() };
    }
    if reply.is_empty() {
        return Report { status: "error", notes, answer: "the model gave an empty reply".to_string() };
    }
    let status = if outcome.exhausted { "too_hard" } else { "ok" };
    Report { status, notes, answer: reply.to_string() }
}

fn delegate(args: &Value) -> Result<Report, Report> {
    let config = config::get();
    let task = text_arg(args, "task")?;
    let paths = paths_arg(args);

    if paths.is_empty() {
        if !config.tools {
            return Err(Report::error("this model has no tools, so it can't look for files itself; pass `paths`"));
        }
        let system = format!("{}{}{TOO_HARD}{}", chat::system(), chat::system_tools(), tools::context());
        let mut messages = vec![message("system", &system), message("user", task)];
        let outcome = chat::run(&mut messages, &tools::schemas(false), config.max_tokens, &mut chat::Quiet)
            .map_err(Report::error)?;
        // Tell Claude what the answer rests on.
        let used: Vec<String> = messages
            .iter()
            .filter_map(|m| m["tool_calls"].as_array())
            .flatten()
            .map(|call| {
                let function = &call["function"];
                let arguments: Value = serde_json::from_str(function["arguments"].as_str().unwrap_or("")).unwrap_or_default();
                let about = arguments["text"].as_str().or(arguments["path"].as_str()).unwrap_or(".");
                format!("{} {about}", function["name"].as_str().unwrap_or("?"))
            })
            .collect();
        let used = if used.is_empty() { "none".to_string() } else { used.join("; ") };
        return Ok(judge(outcome, vec![format!("tools used: {used}")]));
    }

    let mut user = task.to_string();
    let mut sizes = Vec::new();
    let mut read = Vec::new();
    for path in &paths {
        let text = tools::resolve(path).and_then(|real| tools::read_text(&real)).map_err(Report::error)?;
        sizes.push(format!("{path}: {} characters", text.len()));
        read.push(format!("{path} ({} lines, complete)", text.lines().count()));
        user += &attach(path, &text);
    }
    // Refuse before the model runs: an answer from half a file would look
    // just as confident as one from the whole of it.
    if user.len() > config.input_chars {
        sizes.push(format!("total with the task: {} characters, limit {}", user.len(), config.input_chars));
        let answer = "Too much for one context. Send fewer or smaller files, or use map, which takes \
                      the files one at a time.";
        return Ok(Report { status: "too_large", notes: sizes, answer: answer.to_string() });
    }
    let system = format!("{}{TOO_HARD}", chat::system());
    let mut messages = vec![message("system", &system), message("user", &user)];
    let outcome = chat::run(&mut messages, &[], config.max_tokens, &mut chat::Quiet).map_err(Report::error)?;
    Ok(judge(outcome, vec![format!("read: {}", read.join(", "))]))
}

fn map(args: &Value) -> Result<Report, Report> {
    let config = config::get();
    let prompt = text_arg(args, "prompt")?;
    let paths = paths_arg(args);
    if paths.is_empty() {
        return Err(Report::error("missing argument: paths"));
    }
    if paths.len() > config.map_max_files {
        let note = format!("{} files given, limit {} per call", paths.len(), config.map_max_files);
        return Ok(Report { status: "too_large", notes: vec![note], answer: "Send the files in smaller batches.".into() });
    }

    let started = Instant::now();
    let mut lines = Vec::new();
    for path in &paths {
        let text = match tools::resolve(path).and_then(|real| tools::read_text(&real)) {
            Ok(text) => text,
            Err(e) => {
                lines.push(format!("{path}: error: {e}"));
                continue;
            }
        };
        let shown = chat::clip(text.trim_end(), config.map_file_chars);
        let user = format!("{prompt}{}", attach(path, &shown));
        // Each file gets a fresh conversation, so the context never grows.
        let mut messages = vec![message("system", SYSTEM_MAP), message("user", &user)];
        let outcome =
            chat::run(&mut messages, &[], config.map_max_tokens, &mut chat::Quiet).map_err(Report::error)?;
        let answer = outcome.reply.lines().find(|line| !line.trim().is_empty()).unwrap_or("(no answer)").trim();
        let mut line = format!("{path}: {}", tools::clip(answer, 300));
        if text.trim_end().len() > config.map_file_chars {
            line += &format!(" [partial: {} of {} characters read]", config.map_file_chars, text.len());
        }
        lines.push(line);
    }
    let note = format!("files: {}, seconds: {}", paths.len(), started.elapsed().as_secs());
    Ok(Report { status: "ok", notes: vec![note], answer: lines.join("\n") })
}

/// Claude starts this server once and keeps it, while `nibble serve` may
/// switch models in the meantime. So look again before anything that depends
/// on the model's presets. A bad config file keeps the settings we have.
fn refresh_config() {
    match config::refresh() {
        Ok(Some(model)) => eprintln!("nibble mcp: the model is now {model}; its presets apply"),
        Ok(None) => {}
        Err(e) => eprintln!("nibble mcp: keeping the old settings: {e}"),
    }
}

fn call_tool(params: &Value) -> Value {
    refresh_config();
    let name = params["name"].as_str().unwrap_or_default();
    let args = &params["arguments"];
    eprintln!("nibble mcp: {name} {args}");
    let report = match name {
        "delegate" => delegate(args),
        "map" => map(args),
        _ => Err(Report::error(format!("there is no tool called {name}"))),
    };
    let report = report.unwrap_or_else(|report| report);
    json!({
        "content": [{ "type": "text", "text": report.text() }],
        "isError": report.status == "error",
    })
}

fn initialize(params: &Value) -> Value {
    let asked = params["protocolVersion"].as_str().unwrap_or_default();
    let version = if PROTOCOLS.contains(&asked) { asked } else { PROTOCOLS[0] };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "nibble", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

fn roots(mut args: impl Iterator<Item = String>) -> Result<Option<Vec<PathBuf>>, Box<dyn Error>> {
    let mut roots = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--root" => roots.push(args.next().ok_or("--root needs a value")?),
            _ => return Err(format!("unknown option {arg}").into()),
        }
    }
    let given = !roots.is_empty();
    let mut roots: Vec<PathBuf> = roots.iter().map(|root| tools::expand(root)).collect();
    if !given {
        // The directory we were started in, then the configured roots. Claude
        // Code starts its servers in the project, so the project comes first
        // and relative paths in a call mean what Claude expects. The Claude
        // desktop app starts them in /, which is no project, so it is left out.
        roots.extend(std::env::current_dir().ok().filter(|dir| dir.parent().is_some()));
        roots.extend(config::get().roots.iter().map(|root| tools::expand(root)));
    }
    if roots.is_empty() {
        return Err("nowhere to read: started in / with no roots. Set \"roots\" in the config file or pass --root DIR".into());
    }
    // Reading the whole disk must be something the user asked for in so many
    // words. Check the real path, so that "." in / or a link to / counts too.
    for root in &roots {
        let real = std::fs::canonicalize(root).map_err(|e| format!("{}: {e}", root.display()))?;
        if real.parent().is_none() {
            return Err("refusing to use / as a root".into());
        }
    }
    Ok(Some(roots))
}

pub fn run(args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let Some(roots) = roots(args)? else {
        println!("{USAGE}");
        return Ok(());
    };
    // Relative paths in a call are relative to the first root.
    std::env::set_current_dir(&roots[0]).map_err(|e| format!("{}: {e}", roots[0].display()))?;
    tools::confine(&roots)?;

    let mut out = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let (id, outcome) = match serde_json::from_str::<Value>(&line) {
            Err(e) => (Some(Value::Null), Err((-32700, format!("parse error: {e}")))),
            Ok(request) => {
                let outcome = match request["method"].as_str().unwrap_or_default() {
                    "initialize" => Ok(initialize(&request["params"])),
                    "ping" => Ok(json!({})),
                    "tools/list" => {
                        refresh_config();
                        Ok(json!({ "tools": tool_list() }))
                    }
                    "tools/call" => Ok(call_tool(&request["params"])),
                    method => Err((-32601, format!("method not found: {method}"))),
                };
                (request.get("id").cloned(), outcome)
            }
        };
        // A message without an id is a notification, and gets no reply.
        let Some(id) = id else { continue };
        let response = match outcome {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
        };
        writeln!(out, "{response}")?;
        out.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(reply: &str, exhausted: bool) -> chat::Outcome {
        chat::Outcome { reply: reply.into(), exhausted, stats: Default::default() }
    }

    fn call(name: &str, arguments: Value) -> (String, bool) {
        let result = call_tool(&json!({ "name": name, "arguments": arguments }));
        (result["content"][0]["text"].as_str().unwrap().to_string(), result["isError"].as_bool().unwrap())
    }

    #[test]
    fn reports_read_as_status_notes_answer() {
        let report = Report { status: "ok", notes: vec!["read: a.txt".into()], answer: "  yes  \n".into() };
        assert_eq!(report.text(), "status: ok\nread: a.txt\nanswer:\nyes\n");
        // No answer, no answer heading.
        let report = Report { status: "too_large", notes: Vec::new(), answer: " ".into() };
        assert_eq!(report.text(), "status: too_large\n");
        assert_eq!(Report::error("boom").text(), "status: error\nanswer:\nboom\n");
    }

    #[test]
    fn replies_are_judged() {
        let notes = || vec!["note".to_string()];
        let report = judge(outcome("  The answer.\n", false), notes());
        assert_eq!((report.status, report.answer.as_str(), report.notes.len()), ("ok", "The answer.", 1));
        // The model may give up, with or without a reason.
        let report = judge(outcome("TOO_HARD: the file is minified", false), notes());
        assert_eq!((report.status, report.answer.as_str()), ("too_hard", "the file is minified"));
        assert_eq!(judge(outcome("TOO_HARD", false), notes()).status, "too_hard");
        // Out of rounds: whatever it said is a guess.
        assert_eq!(judge(outcome("probably main.rs", true), notes()).status, "too_hard");
        assert_eq!(judge(outcome(" \n", false), notes()).status, "error");
    }

    #[test]
    fn arguments_are_checked() {
        let args = json!({ "task": "  find it ", "blank": "  ", "number": 3, "paths": ["a", 7, "b"] });
        assert_eq!(text_arg(&args, "task").ok(), Some("find it"));
        for key in ["blank", "number", "absent"] {
            assert_eq!(text_arg(&args, key).err().unwrap().answer, format!("missing argument: {key}"));
        }
        assert_eq!(paths_arg(&args), ["a", "b"]);
        assert!(paths_arg(&json!({ "paths": "a" })).is_empty());
        assert!(paths_arg(&json!({})).is_empty());
        assert_eq!(attach("a.txt", "one\ntwo\n\n"), "\n\n<file path=\"a.txt\">\none\ntwo\n</file>");
    }

    #[test]
    fn bad_calls_fail_before_the_model_runs() {
        // None of these reach the model server, which isn't running.
        let (text, error) = call("summon", json!({}));
        assert!(error && text.contains("there is no tool called summon"), "{text}");
        let (text, error) = call("delegate", json!({ "paths": ["a.txt"] }));
        assert!(error && text.contains("missing argument: task"), "{text}");
        let (text, error) = call("map", json!({ "prompt": "what is it?" }));
        assert!(error && text.contains("missing argument: paths"), "{text}");
        let (text, error) = call("map", json!({ "paths": ["a.txt"] }));
        assert!(error && text.contains("missing argument: prompt"), "{text}");

        let limit = config::get().map_max_files;
        let paths: Vec<String> = (0..=limit).map(|n| format!("file{n}.txt")).collect();
        let (text, error) = call("map", json!({ "prompt": "what is it?", "paths": paths }));
        assert!(!error && text.starts_with("status: too_large\n"), "{text}");
        assert!(text.contains(&format!("{} files given, limit {limit} per call", limit + 1)), "{text}");
    }

    #[test]
    fn the_handshake_and_tool_list() {
        let result = initialize(&json!({ "protocolVersion": "2025-03-26" }));
        assert_eq!(result["protocolVersion"], "2025-03-26");
        assert_eq!(result["serverInfo"]["name"], "nibble");
        assert_eq!(result["instructions"], INSTRUCTIONS);
        // A version we don't know gets the newest we do.
        assert_eq!(initialize(&json!({ "protocolVersion": "1999-01-01" }))["protocolVersion"], PROTOCOLS[0]);
        assert_eq!(initialize(&json!({}))["protocolVersion"], PROTOCOLS[0]);

        let tools = tool_list();
        let names: Vec<&str> = tools.as_array().unwrap().iter().map(|tool| tool["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["delegate", "map"]);
        assert_eq!(tools[0]["inputSchema"]["required"], json!(["task"]));
        assert_eq!(tools[1]["inputSchema"]["required"], json!(["prompt", "paths"]));
        // The limits Claude is told are the ones in force.
        let described = tools[1]["description"].as_str().unwrap();
        assert!(described.contains(&format!("At most {} files", config::get().map_max_files)), "{described}");
    }

    #[test]
    fn roots_must_be_asked_for_and_never_slash() {
        let args = |list: &[&str]| list.iter().map(|arg| arg.to_string()).collect::<Vec<_>>().into_iter();
        let error = |list: &[&str]| roots(args(list)).expect_err("should fail").to_string();

        assert!(roots(args(&["--help"])).unwrap().is_none());
        assert!(error(&["--root"]).contains("--root needs a value"));
        assert!(error(&["--all"]).contains("unknown option --all"));
        assert!(error(&["--root", "/"]).contains("refusing to use / as a root"));
        // The real path counts, so a way round to / is refused too.
        assert!(error(&["--root", "/usr/.."]).contains("refusing to use / as a root"));
        assert!(error(&["--root", "/nonexistent/nibble"]).contains("/nonexistent/nibble"));

        let dir = std::env::temp_dir();
        let given = dir.to_string_lossy().into_owned();
        assert_eq!(roots(args(&["--root", &given, "--root", &given])).unwrap().unwrap(), [dir.clone(), dir]);
    }
}
