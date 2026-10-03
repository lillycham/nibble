//! Saved chats, in the same files as the window's: one JSON file each under
//! `~/.local/share/nibble/chats`, so a chat started in one can go on in the other.

use std::error::Error;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::chat::{self, message, Events};

pub struct Session {
    pub id: String,
    /// As saved: `{"user", "parts": [{"text"} | {"tool"} | {"error"}]}`.
    pub turns: Vec<Value>,
}

fn dir() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    var("XDG_DATA_HOME")
        .map(|data| data.join("nibble/chats"))
        .or_else(|| var("HOME").map(|home| home.join(".local/share/nibble/chats")))
}

fn file(id: &str) -> Option<PathBuf> {
    // Ids are times, and a user types them here: never let one walk out of the directory.
    (!id.is_empty() && id.chars().all(|c| c.is_ascii_digit())).then(|| dir().map(|dir| dir.join(format!("{id}.json"))))?
}

fn now_millis() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_millis())
}

/// The model's words in a saved turn, without the tool lines.
fn answer(turn: &Value) -> String {
    turn["parts"].as_array().into_iter().flatten().filter_map(|part| part["text"].as_str()).collect()
}

impl Session {
    /// A new, empty chat. Its id is the time, so ids sort by age.
    pub fn new() -> Self {
        Session { id: now_millis().to_string(), turns: Vec::new() }
    }

    pub fn load(id: &str) -> Result<Self, Box<dyn Error>> {
        let missing = || format!("no saved chat {id}; `nibble chats` lists them");
        let path = file(id).ok_or_else(missing)?;
        let text = fs::read_to_string(&path).map_err(|_| missing())?;
        let saved: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let turns = saved["turns"].as_array().cloned().unwrap_or_default();
        Ok(Session { id: id.to_string(), turns })
    }

    /// The newest saved chat, from here or from the window.
    pub fn latest() -> Result<Self, Box<dyn Error>> {
        let entry = list().into_iter().next().ok_or("no saved chats yet")?;
        Session::load(&entry.0)
    }

    pub fn title(&self) -> String {
        let first = self.turns.first().and_then(|turn| turn["user"].as_str()).unwrap_or_default();
        let line = first.lines().next().unwrap_or_default().trim();
        let title: String = line.chars().take(60).collect();
        if title.is_empty() { "New chat".to_string() } else { title }
    }

    /// The conversation so far as messages for the model, after `system`.
    /// Tool results aren't kept, so only the questions and answers go back in.
    pub fn messages(&self, system: &str) -> Vec<Value> {
        let mut messages = vec![message("system", system)];
        for turn in &self.turns {
            messages.push(message("user", turn["user"].as_str().unwrap_or_default()));
            let answer = answer(turn);
            if !answer.is_empty() {
                messages.push(message("assistant", &answer));
            }
        }
        messages
    }

    /// Write the chat to disk. An empty chat is not worth a file.
    pub fn save(&self) -> Result<(), Box<dyn Error>> {
        if self.turns.is_empty() {
            return Ok(());
        }
        let path = file(&self.id).ok_or("no data directory, because HOME is not set")?;
        let dir = path.parent().ok_or("no data directory")?;
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let saved = json!({ "title": self.title(), "turns": self.turns });
        fs::write(&path, format!("{saved:#}\n")).map_err(|e| format!("{}: {e}", path.display()).into())
    }
}

/// Every saved chat as (id, title), newest first.
pub fn list() -> Vec<(String, String)> {
    let Some(files) = dir().and_then(|dir| fs::read_dir(dir).ok()) else { return Vec::new() };
    let mut entries: Vec<(String, String)> = files
        .flatten()
        .filter_map(|file| {
            let path = file.path();
            let id = path.file_stem()?.to_str()?.to_string();
            if path.extension()? != "json" || !id.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            let saved: Value = serde_json::from_str(&fs::read_to_string(&path).ok()?).ok()?;
            Some((id, saved["title"].as_str().unwrap_or("Chat").to_string()))
        })
        .collect();
    // Ids are times, but compare them as numbers: as text, "9" sorts after "10".
    entries.sort_by_key(|(id, _)| std::cmp::Reverse(id.parse::<u128>().unwrap_or(0)));
    entries
}

/// How long ago an id's time was, roughly.
fn age(id: &str) -> String {
    let seconds = now_millis().saturating_sub(id.parse().unwrap_or(0)) / 1000;
    match seconds {
        0..60 => "just now".to_string(),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..86400 => format!("{} h ago", seconds / 3600),
        _ => format!("{} days ago", seconds / 86400),
    }
}

/// `nibble chats`: the saved chats, newest first.
pub fn show_list() {
    let entries = list();
    if entries.is_empty() {
        eprintln!("no saved chats yet");
    }
    for (id, title) in entries {
        println!("{id}  {:>12}  {title}", age(&id));
    }
}

/// Passes events on, and keeps what they say as the parts of a saved turn.
pub struct Recorder<'a> {
    pub inner: &'a mut dyn Events,
    pub parts: Vec<Value>,
}

impl Events for Recorder<'_> {
    fn text(&mut self, text: &str) -> io::Result<()> {
        match self.parts.last_mut() {
            Some(part) if part["text"].is_string() => {
                part["text"] = format!("{}{text}", part["text"].as_str().unwrap_or_default()).into();
            }
            _ if text.is_empty() => {}
            _ => self.parts.push(json!({ "text": text })),
        }
        self.inner.text(text)
    }

    fn tool(&mut self, name: &str, arguments: &Value) -> io::Result<()> {
        self.parts.push(json!({ "tool": chat::describe(name, arguments) }));
        self.inner.tool(name, arguments)
    }

    fn reply_end(&mut self) -> io::Result<()> {
        self.inner.reply_end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_chat_becomes_messages_again() {
        let session = Session {
            id: "42".into(),
            turns: vec![
                json!({ "user": "What is in src?\nmore", "parts": [{ "tool": "list_dir src" }, { "text": "main.rs" }] }),
                json!({ "user": "And tests?", "parts": [{ "error": "no answer" }] }),
            ],
        };
        assert_eq!(session.title(), "What is in src?");
        let messages = session.messages("sys");
        let roles: Vec<&str> = messages.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "user"]);
        assert_eq!(messages[2]["content"], "main.rs");
        assert!(file("../escape").is_none());
        assert!(file("").is_none());
    }

    #[test]
    fn the_recorder_joins_streamed_text() {
        let mut quiet = chat::Quiet;
        let mut recorder = Recorder { inner: &mut quiet, parts: Vec::new() };
        recorder.text("Hel").unwrap();
        recorder.text("lo").unwrap();
        recorder.tool("read_file", &json!({ "path": "a.txt" })).unwrap();
        recorder.text("").unwrap();
        recorder.text("Done").unwrap();
        assert_eq!(recorder.parts, vec![json!({ "text": "Hello" }), json!({ "tool": "read_file a.txt" }), json!({ "text": "Done" })]);
    }
}
