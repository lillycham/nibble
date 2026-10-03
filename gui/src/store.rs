//! Saved chats: one JSON file each, under nibble's data directory.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

pub enum Part {
    Text(String),
    Tool(String),
    Error(String),
}

pub struct Turn {
    /// What the model was sent.
    pub user: String,
    /// What was typed, when it was a command such as "/summarise some text".
    pub typed: Option<String>,
    pub parts: Vec<Part>,
}

impl Turn {
    /// What the turn shows as its heading: what was typed.
    pub fn asked(&self) -> &str {
        self.typed.as_deref().unwrap_or(&self.user)
    }

    /// The model's words in this turn, without the tool lines.
    pub fn answer(&self) -> String {
        self.parts.iter().filter_map(|part| if let Part::Text(text) = part { Some(text.as_str()) } else { None }).collect()
    }
}

pub struct Chat {
    pub id: String,
    pub turns: Vec<Turn>,
    /// The folder the model works in, with the home directory as `~`. None
    /// means the server's own, its first root.
    pub dir: Option<String>,
}

/// A row in the list of chats.
pub struct Entry {
    pub id: String,
    pub title: String,
}

fn dir() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    var("XDG_DATA_HOME")
        .map(|data| data.join("nibble/chats"))
        .or_else(|| var("HOME").map(|home| home.join(".local/share/nibble/chats")))
}

fn file(id: &str) -> Option<PathBuf> {
    // Ids are ours, but never let one walk out of the directory.
    id.chars().all(|c| c.is_ascii_digit()).then(|| dir().map(|dir| dir.join(format!("{id}.json"))))?
}

impl Chat {
    /// A new, empty chat. Its id is the time, so ids sort by age.
    pub fn new() -> Self {
        let millis = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_millis());
        Chat { id: millis.to_string(), turns: Vec::new(), dir: None }
    }

    pub fn title(&self) -> String {
        let first = self.turns.first().map_or("", Turn::asked);
        let line = first.lines().next().unwrap_or_default().trim();
        let title: String = line.chars().take(60).collect();
        if title.is_empty() { "New chat".to_string() } else { title }
    }

    fn to_json(&self) -> Value {
        let turns: Vec<Value> = self
            .turns
            .iter()
            .map(|turn| {
                let parts: Vec<Value> = turn
                    .parts
                    .iter()
                    .map(|part| match part {
                        Part::Text(text) => json!({ "text": text }),
                        Part::Tool(tool) => json!({ "tool": tool }),
                        Part::Error(error) => json!({ "error": error }),
                    })
                    .collect();
                let mut saved = json!({ "user": turn.user, "parts": parts });
                if let Some(typed) = &turn.typed {
                    saved["typed"] = json!(typed);
                }
                saved
            })
            .collect();
        let mut saved = json!({ "title": self.title(), "turns": turns });
        if let Some(dir) = &self.dir {
            saved["dir"] = json!(dir);
        }
        saved
    }

    fn from_json(id: &str, saved: &Value) -> Self {
        let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
        let turns = saved["turns"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|turn| Turn {
                user: text(&turn["user"]),
                typed: turn["typed"].as_str().map(str::to_string),
                parts: turn["parts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|part| {
                        if part["text"].is_string() {
                            Some(Part::Text(text(&part["text"])))
                        } else if part["tool"].is_string() {
                            Some(Part::Tool(text(&part["tool"])))
                        } else if part["error"].is_string() {
                            Some(Part::Error(text(&part["error"])))
                        } else {
                            None
                        }
                    })
                    .collect(),
            })
            .collect();
        Chat { id: id.to_string(), turns, dir: saved["dir"].as_str().map(str::to_string) }
    }

    /// Write the chat to disk. An empty chat is not worth a file.
    pub fn save(&self) -> Result<(), String> {
        if self.turns.is_empty() {
            return Ok(());
        }
        let path = file(&self.id).ok_or("no data directory")?;
        let dir = path.parent().ok_or("no data directory")?;
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        fs::write(&path, format!("{:#}\n", self.to_json())).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn load(id: &str) -> Option<Self> {
        let saved: Value = serde_json::from_str(&fs::read_to_string(file(id)?).ok()?).ok()?;
        Some(Chat::from_json(id, &saved))
    }
}

/// Every saved chat, newest first.
pub fn list() -> Vec<Entry> {
    let Some(dir) = dir() else { return Vec::new() };
    let Ok(files) = fs::read_dir(dir) else { return Vec::new() };
    let mut entries: Vec<Entry> = files
        .flatten()
        .filter_map(|file| {
            let path = file.path();
            let id = path.file_stem()?.to_str()?.to_string();
            if path.extension()? != "json" || !id.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            let saved: Value = serde_json::from_str(&fs::read_to_string(&path).ok()?).ok()?;
            Some(Entry { id, title: saved["title"].as_str().unwrap_or("Chat").to_string() })
        })
        .collect();
    // Ids are times, but compare them as numbers: as text, "9" sorts after "10".
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.id.parse::<u128>().unwrap_or(0)));
    entries
}

pub fn delete(id: &str) {
    if let Some(path) = file(id) {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chat_survives_saving() {
        let chat = Chat {
            id: "42".into(),
            turns: vec![Turn {
                user: "What is in src?\nSecond line".into(),
                typed: None,
                parts: vec![Part::Tool("list_dir src".into()), Part::Text("main.rs".into()), Part::Error("oops".into())],
            }],
            dir: Some("~/devel/nibble".into()),
        };
        assert_eq!(chat.title(), "What is in src?");
        let back = Chat::from_json("42", &chat.to_json());
        assert_eq!(back.turns.len(), 1);
        assert_eq!(back.turns[0].answer(), "main.rs");
        assert_eq!(back.turns[0].parts.len(), 3);
        assert_eq!(back.dir.as_deref(), Some("~/devel/nibble"));
        assert!(file("../escape").is_none());

        // A command keeps what was typed, and shows it.
        let mut chat = back;
        chat.turns[0].typed = Some("/summarise src".into());
        let back = Chat::from_json("42", &chat.to_json());
        assert_eq!((back.title().as_str(), back.turns[0].user.as_str()), ("/summarise src", "What is in src?\nSecond line"));
    }
}
