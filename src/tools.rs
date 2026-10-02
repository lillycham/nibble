//! The tools the model can call. The file tools only read, and `ask_claude`
//! hands a question to a bigger model.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

// One result may use about a quarter of an 8k window.
const RESULT_BUDGET: usize = 6_000;
const MAX_MATCHES: usize = 40;
const MAX_ENTRIES: usize = 200;
const MAX_SEARCH_FILE: u64 = 1 << 20;
const SKIP_DIRS: [&str; 3] = ["target", "node_modules", "result"];

fn schema(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": { "type": "object", "properties": properties, "required": required },
        },
    })
}

pub fn schemas(claude: bool) -> Vec<Value> {
    let mut tools = vec![
        schema(
            "read_file",
            "Read a text file. Long files come back in parts.",
            json!({
                "path": { "type": "string" },
                "start_line": { "type": "integer", "description": "First line to return, from 1" },
            }),
            &["path"],
        ),
        schema(
            "list_dir",
            "List the files and folders in a directory.",
            json!({ "path": { "type": "string", "description": "Defaults to the current directory" } }),
            &[],
        ),
        schema(
            "search",
            "Find lines that contain a piece of text, in one file or in every file under a directory. Not a regex. Ignores case.",
            json!({
                "text": { "type": "string" },
                "path": { "type": "string", "description": "Defaults to the current directory" },
            }),
            &["text"],
        ),
    ];
    if claude {
        tools.push(schema(
            "ask_claude",
            "Ask Claude, a much more capable model, when the task is too hard for you. Claude sees only the question, so include everything it needs.",
            json!({ "question": { "type": "string" } }),
            &["question"],
        ));
    }
    tools
}

pub fn call(name: &str, args: &Value) -> String {
    let result = match name {
        "read_file" => read_file(args),
        "list_dir" => list_dir(args),
        "search" => search(args),
        "ask_claude" => ask_claude(args),
        _ => Err(format!("there is no tool called {name}")),
    };
    result.unwrap_or_else(|e| format!("error: {e}"))
}

fn text_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    match args[key].as_str() {
        Some(s) if !s.is_empty() => Ok(s),
        _ => Err(format!("missing argument: {key}")),
    }
}

fn path_arg(args: &Value) -> PathBuf {
    let path = args["path"].as_str().filter(|p| !p.is_empty()).unwrap_or(".");
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// Small models sometimes send numbers as strings.
fn number_arg(args: &Value, key: &str) -> Option<usize> {
    let value = &args[key];
    value.as_u64().map(|n| n as usize).or_else(|| value.as_str()?.trim().parse().ok())
}

fn clip(line: &str, max: usize) -> &str {
    let mut end = line.len().min(max);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    &line[..end]
}

fn read_text(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.iter().take(8000).any(|&b| b == 0) {
        return Err(format!("{} is a binary file", path.display()));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn read_file(args: &Value) -> Result<String, String> {
    text_arg(args, "path")?;
    let path = path_arg(args);
    let text = read_text(&path)?;
    let total = text.lines().count();
    let start = number_arg(args, "start_line").unwrap_or(1).max(1);
    if start > total.max(1) {
        return Err(format!("{} has only {total} lines", path.display()));
    }

    let mut body = String::new();
    let mut end = start - 1;
    for line in text.lines().skip(start - 1) {
        let line = clip(line, RESULT_BUDGET);
        if !body.is_empty() && body.len() + line.len() >= RESULT_BUDGET {
            break;
        }
        body.push_str(line);
        body.push('\n');
        end += 1;
    }

    // The model can't count lines reliably, so tell it.
    let name = path.display();
    if start == 1 && end == total {
        return Ok(format!("{name}: {total} lines\n{body}"));
    }
    let mut out = format!("{name}: lines {start}-{end} of {total}\n{body}");
    if end < total {
        // At the end, where a small model is most likely to act on it.
        out.push_str(&format!(
            "[{} more lines not shown. Call read_file with start_line={} to read on.]\n",
            total - end,
            end + 1
        ));
    }
    Ok(out)
}

fn entries(path: &Path) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = fs::read_dir(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .flatten()
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.path().is_dir() { name + "/" } else { name }
        })
        .collect();
    names.sort();
    Ok(names)
}

/// Where we are and what is here, for the system prompt. A small model is far
/// more willing to open a file it can see the name of.
pub fn context() -> String {
    let Ok(dir) = std::env::current_dir() else { return String::new() };
    let mut names = entries(&dir).unwrap_or_default();
    let more = if names.len() > 40 { " ..." } else { "" };
    names.truncate(40);
    format!("\n\nCurrent directory: {}\nIts contents: {}{more}", dir.display(), names.join(" "))
}

fn list_dir(args: &Value) -> Result<String, String> {
    let path = path_arg(args);
    let mut names = entries(&path)?;
    let total = names.len();
    names.truncate(MAX_ENTRIES);
    let mut out = format!("{}: {total} entries\n{}\n", path.display(), names.join("\n"));
    if total > MAX_ENTRIES {
        out.push_str(&format!("... and {} more\n", total - MAX_ENTRIES));
    }
    Ok(out)
}

fn search_file(path: &Path, needle: &str, matches: &mut Vec<String>) {
    let Ok(text) = read_text(path) else { return };
    for (number, line) in text.lines().enumerate() {
        if matches.len() > MAX_MATCHES {
            return;
        }
        if line.to_lowercase().contains(needle) {
            matches.push(format!("{}:{}: {}", path.display(), number + 1, clip(line.trim(), 200)));
        }
    }
}

fn search_dir(dir: &Path, needle: &str, matches: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if matches.len() > MAX_MATCHES {
            return;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // file_type() doesn't follow symlinks, so a link can't lead us in circles.
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            if !name.starts_with('.') && !SKIP_DIRS.contains(&name.as_ref()) {
                search_dir(&entry.path(), needle, matches);
            }
        } else if kind.is_file() && entry.metadata().is_ok_and(|m| m.len() <= MAX_SEARCH_FILE) {
            search_file(&entry.path(), needle, matches);
        }
    }
}

fn search(args: &Value) -> Result<String, String> {
    let needle = text_arg(args, "text")?.to_lowercase();
    let path = path_arg(args);
    let mut matches = Vec::new();
    if path.is_dir() {
        search_dir(&path, &needle, &mut matches);
    } else {
        // Report a missing file, which the directory walk would skip in silence.
        read_text(&path)?;
        search_file(&path, &needle, &mut matches);
    }
    if matches.is_empty() {
        return Ok("no matches\n".to_string());
    }
    let more = matches.len() > MAX_MATCHES;
    matches.truncate(MAX_MATCHES);
    let mut out = matches.join("\n") + "\n";
    if more {
        out.push_str("... more matches not shown. Search a smaller path or a longer text.\n");
    }
    Ok(out)
}

/// Whether `ask_claude` may be offered. When Claude is the one that called us,
/// it must not be, or the two would pass a task back and forth.
pub fn claude_allowed() -> bool {
    std::env::var_os("CLAUDECODE").is_none() && std::env::var_os("NIBBLE_DEPTH").is_none()
}

fn ask_claude(args: &Value) -> Result<String, String> {
    let question = text_arg(args, "question")?;
    let program = std::env::var("NIBBLE_CLAUDE").unwrap_or_else(|_| "claude".to_string());
    // Claude gets read-only tools too, so it can look at the files the question names.
    let output = Command::new(&program)
        .args(["-p", question, "--tools", "Read,Grep,Glob", "--no-session-persistence"])
        .env("NIBBLE_DEPTH", "1")
        .output()
        .map_err(|e| format!("can't run {program}: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{program} failed: {}", clip(stderr.trim(), 500)));
    }
    let answer = String::from_utf8_lossy(&output.stdout);
    Ok(clip(answer.trim(), RESULT_BUDGET).to_string() + "\n")
}
