//! The tools the model can call. The file tools only read, and `ask_claude`
//! hands a question to a bigger model.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use serde_json::{json, Value};

use crate::config;

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
            "Find lines that contain the words you give, in any order, in one file or in every file under a directory. Ignores case. One or two distinctive words work best.",
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
    result.unwrap_or_else(|e| {
        if !e.contains("No such file") {
            return format!("error: {e}");
        }
        // Say what to do next, or the model tries the same path again. If a
        // file of that name exists somewhere here, name it: the usual mistake
        // is a right name behind a wrong directory.
        let mut places = Vec::new();
        if let Some(name) = args["path"].as_str().and_then(|path| Path::new(path).file_name()) {
            find_named(Path::new("."), name, &mut places, 0);
        }
        if places.is_empty() {
            format!("error: {e}. Paths are relative to the current directory. Call list_dir to see what is there.")
        } else {
            format!("error: {e}. A file of that name is at: {}. Use that path.", places.join(", "))
        }
    })
}

/// Files called `name` under `dir`, as paths relative to the current directory.
fn find_named(dir: &Path, name: &std::ffi::OsStr, places: &mut Vec<String>, depth: usize) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if places.len() >= 3 {
            return;
        }
        let Ok(kind) = entry.file_type() else { continue };
        let file_name = entry.file_name();
        if kind.is_file() && file_name == name {
            let path = entry.path();
            places.push(path.strip_prefix(".").unwrap_or(&path).display().to_string());
        } else if kind.is_dir() && depth < 6 {
            let hidden = file_name.to_string_lossy().starts_with('.');
            if !hidden && !SKIP_DIRS.contains(&file_name.to_string_lossy().as_ref()) {
                find_named(&entry.path(), name, places, depth + 1);
            }
        }
    }
}

fn text_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    match args[key].as_str() {
        Some(s) if !s.is_empty() => Ok(s),
        _ => Err(format!("missing argument: {key}")),
    }
}

/// The directories the file tools may read inside. Empty means anywhere.
static ROOTS: OnceLock<Vec<PathBuf>> = OnceLock::new();

/// Keep the file tools inside these directories. Without this call they can
/// read anything the user can.
pub fn confine(dirs: &[PathBuf]) -> Result<(), String> {
    let roots: Result<Vec<_>, _> =
        dirs.iter().map(|dir| fs::canonicalize(dir).map_err(|e| format!("{}: {e}", dir.display()))).collect();
    ROOTS.set(roots?).map_err(|_| "already confined".to_string())
}

fn roots() -> &'static [PathBuf] {
    ROOTS.get().map_or(&[], Vec::as_slice)
}

/// Give a leading ~/ its usual meaning.
pub fn expand(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => Path::new(&home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// Turn a path from the model or from Claude into one we may read.
pub fn resolve(path: &str) -> Result<PathBuf, String> {
    let mut path = expand(path);
    // Models invent a directory in front of a real path: one wrote
    // nibble/flake.nix, another /nix/flake.nix and /current_directory/TODO.md.
    // If the path as given does not exist but its tail does, take the tail.
    // The reply names the path that was read, so the model sees the correction.
    if !path.exists() {
        let parts: Vec<_> = path.components().filter(|part| matches!(part, std::path::Component::Normal(_))).collect();
        let tail = (1..parts.len()).map(|skip| parts[skip..].iter().collect::<PathBuf>()).find(|tail| tail.exists());
        if let Some(tail) = tail {
            path = tail;
        } else if path.is_absolute() && parts.len() == 1 {
            // A made-up name for "the directory I am in".
            path = PathBuf::from(".");
        }
    }
    if !roots().is_empty() {
        // Canonical form, so neither .. nor a symlink can lead outside.
        let real = fs::canonicalize(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if !roots().iter().any(|root| real.starts_with(root)) {
            let allowed: Vec<_> = roots().iter().map(|root| root.display().to_string()).collect();
            return Err(format!("{} is outside {}, and you may only read inside it", path.display(), allowed.join(", ")));
        }
    }
    Ok(path)
}

fn path_arg(args: &Value) -> Result<PathBuf, String> {
    resolve(args["path"].as_str().filter(|p| !p.is_empty()).unwrap_or("."))
}

/// Small models sometimes send numbers as strings.
fn number_arg(args: &Value, key: &str) -> Option<usize> {
    let value = &args[key];
    value.as_u64().map(|n| n as usize).or_else(|| value.as_str()?.trim().parse().ok())
}

pub fn clip(line: &str, max: usize) -> &str {
    let mut end = line.len().min(max);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    &line[..end]
}

pub fn read_text(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.iter().take(8000).any(|&b| b == 0) {
        return Err(format!("{} is a binary file", path.display()));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn read_file(args: &Value) -> Result<String, String> {
    text_arg(args, "path")?;
    let path = path_arg(args)?;
    let text = read_text(&path)?;
    let total = text.lines().count();
    let start = number_arg(args, "start_line").unwrap_or(1).max(1);
    if start > total.max(1) {
        return Err(format!("{} has only {total} lines", path.display()));
    }

    let mut body = String::new();
    let mut end = start - 1;
    for line in text.lines().skip(start - 1) {
        let line = clip(line, config::get().result_chars);
        if !body.is_empty() && body.len() + line.len() >= config::get().result_chars {
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
    let reach = if roots().is_empty() {
        "Files outside it are also yours to read: give the full path, such as /etc/hosts."
    } else {
        "You may only read files inside it."
    };
    // No path and no name for the directory itself. Models miscopy a long
    // absolute path, and given just the name they put it in front of every
    // path: one wrote nibble/flake.nix, another /nibble/flake.nix.
    // An example built from what is really here teaches the path form better
    // than a rule does.
    let file = names.iter().find(|name| !name.ends_with('/')).map_or("notes.txt", String::as_str);
    let folder = names.iter().find(|name| name.ends_with('/') && !name.starts_with('.')).map_or("src/", String::as_str);
    format!(
        "\n\nThe current directory holds: {}{more}\n\
         Give paths relative to it, with no leading slash: \"{file}\" for a file listed here, \
         \"{folder}name\" for one inside a folder. {reach}",
        names.join(" ")
    )
}

fn list_dir(args: &Value) -> Result<String, String> {
    let path = path_arg(args)?;
    let mut names = entries(&path)?;
    let total = names.len();
    names.truncate(MAX_ENTRIES);
    let mut out = format!("{}: {total} entries\n{}\n", path.display(), names.join("\n"));
    if total > MAX_ENTRIES {
        out.push_str(&format!("... and {} more\n", total - MAX_ENTRIES));
    }
    Ok(out)
}

/// Lines found so far: those with every word, and the nearest misses.
#[derive(Default)]
struct Found {
    full: Vec<String>,
    partial: Vec<(usize, String)>,
}

impl Found {
    fn is_full(&self) -> bool {
        self.full.len() > MAX_MATCHES
    }
}

fn search_file(path: &Path, words: &[String], found: &mut Found) {
    let Ok(text) = read_text(path) else { return };
    for (number, line) in text.lines().enumerate() {
        if found.is_full() {
            return;
        }
        let lower = line.to_lowercase();
        let score = words.iter().filter(|word| lower.contains(word.as_str())).count();
        let hit = || format!("{}:{}: {}", path.display(), number + 1, clip(line.trim(), 200));
        if score == words.len() {
            found.full.push(hit());
        } else if score >= 2 && found.partial.len() < 500 {
            found.partial.push((score, hit()));
        }
    }
}

fn search_dir(dir: &Path, words: &[String], found: &mut Found) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if found.is_full() {
            return;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // file_type() doesn't follow symlinks, so a link can't lead us in circles.
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            if !name.starts_with('.') && !SKIP_DIRS.contains(&name.as_ref()) {
                search_dir(&entry.path(), words, found);
            }
        } else if kind.is_file() && entry.metadata().is_ok_and(|m| m.len() <= MAX_SEARCH_FILE) {
            search_file(&entry.path(), words, found);
        }
    }
}

/// A small model searches the way people type into a search box: a few words
/// from the question, rarely an exact phrase from the file. So match the
/// words in any order, and when no line has them all, show the nearest lines.
fn search(args: &Value) -> Result<String, String> {
    let text = text_arg(args, "text")?.to_lowercase();
    let mut words: Vec<String> =
        text.split(|c: char| !c.is_alphanumeric() && c != '_').filter(|w| w.len() >= 2).map(str::to_string).collect();
    words.sort();
    words.dedup();
    if words.is_empty() {
        words.push(text);
    }
    let path = path_arg(args)?;
    let mut found = Found::default();
    if path.is_dir() {
        search_dir(&path, &words, &mut found);
    } else {
        // Report a missing file, which the directory walk would skip in silence.
        read_text(&path)?;
        search_file(&path, &words, &mut found);
    }

    if !found.full.is_empty() {
        let more = found.is_full();
        found.full.truncate(MAX_MATCHES);
        let mut out = found.full.join("\n") + "\n";
        if more {
            out.push_str("... more matches not shown. Search a smaller path or add a word.\n");
        }
        return Ok(out);
    }
    if !found.partial.is_empty() {
        // Best first; the sort is stable, so ties stay in file order.
        found.partial.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        let lines: Vec<_> = found.partial.into_iter().take(15).map(|(_, line)| line).collect();
        return Ok(format!("No line has all of those words. The closest lines:\n{}\n", lines.join("\n")));
    }
    // Say what to do next, or a small model takes "no matches" as the answer.
    Ok("no matches. Try one distinctive word, or list_dir to see which files there are.\n".to_string())
}

/// Whether `ask_claude` may be offered. When Claude is the one that called us,
/// it must not be, or the two would pass a task back and forth.
pub fn claude_allowed() -> bool {
    std::env::var_os("CLAUDECODE").is_none() && std::env::var_os("NIBBLE_DEPTH").is_none()
}

fn ask_claude(args: &Value) -> Result<String, String> {
    let question = text_arg(args, "question")?;
    let program = &config::get().claude_command;
    // Claude gets read-only tools too, so it can look at the files the question names.
    let output = Command::new(program)
        .args(["-p", question, "--tools", "Read,Grep,Glob", "--no-session-persistence"])
        .env("NIBBLE_DEPTH", "1")
        .output()
        .map_err(|e| format!("can't run {program}: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{program} failed: {}", clip(stderr.trim(), 500)));
    }
    let answer = String::from_utf8_lossy(&output.stdout);
    Ok(clip(answer.trim(), config::get().result_chars).to_string() + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    // One test, because the confinement root and the current directory are
    // both process-wide.
    #[test]
    fn confined_reads_stay_inside() {
        let base = std::env::temp_dir().join(format!("nibble-test-{}", std::process::id()));
        let (inside, outside) = (base.join("in"), base.join("out"));
        fs::create_dir_all(&inside).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(inside.join("ok.txt"), "fine\n").unwrap();
        fs::write(outside.join("secret.txt"), "hidden words\n").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), inside.join("link.txt")).unwrap();
        std::env::set_current_dir(&inside).unwrap();
        confine(std::slice::from_ref(&inside)).unwrap();

        let read = |path: &str| call("read_file", &json!({ "path": path }));
        assert!(read("ok.txt").contains("fine"));
        let absolute = outside.join("secret.txt");
        for path in ["../out/secret.txt", "link.txt", absolute.to_str().unwrap()] {
            let result = read(path);
            assert!(result.starts_with("error:") && !result.contains("hidden words"), "{path}: {result}");
        }
        assert!(call("list_dir", &json!({ "path": ".." })).starts_with("error:"));
        assert!(call("search", &json!({ "text": "hidden", "path": "../out" })).starts_with("error:"));
        // The walk must not follow the symlink out either.
        assert!(call("search", &json!({ "text": "hidden" })).starts_with("no matches"));
        // Words match in any order, and the directory's own name is forgiven.
        fs::write(inside.join("notes.txt"), "the server stops when idle\nstops for lunch\n").unwrap();
        let hits = call("search", &json!({ "text": "idle server", "path": "/made_up_root" }));
        assert!(hits.contains("notes.txt:1:") && !hits.contains("lunch"), "{hits}");
        assert!(call("search", &json!({ "text": "idle server banana" })).starts_with("No line has all"));
        // A right name behind a wrong directory gets pointed to the real file.
        fs::create_dir(inside.join("sub")).unwrap();
        fs::write(inside.join("sub/deep.txt"), "x\n").unwrap();
        let wrong = call("read_file", &json!({ "path": "nowhere/deep.txt" }));
        assert!(wrong.starts_with("error:") && wrong.contains("sub/deep.txt"), "{wrong}");
        // An invented directory in front of a real path is dropped.
        assert!(call("read_file", &json!({ "path": "/current_directory/sub/deep.txt" })).starts_with("sub/deep.txt"));
        assert!(call("read_file", &json!({ "path": "in/ok.txt" })).contains("fine"));

        fs::remove_dir_all(&base).unwrap();
    }
}
