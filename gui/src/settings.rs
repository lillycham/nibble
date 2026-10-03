//! Reading and writing nibble's config file for the settings page. The file
//! belongs to the CLI as much as to us, so a save only touches the keys shown
//! on the page and leaves every other key as it was.

use std::fs;
use std::path::PathBuf;

use serde_json::{Map, Value, json};

/// How a setting is edited on the page.
#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Text,
    Number,
    /// A list of strings, typed with commas between them.
    List,
    /// Stored in the file but never shown, only replaced.
    Secret,
    /// One of a few values, or unset. Clicking steps through them.
    Choice(&'static [&'static str]),
}

pub struct Field {
    pub key: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
    pub kind: Kind,
}

/// The settings worth a place on the page, in order. Anything else is still
/// honoured from the file; it just has no field here.
pub const FIELDS: [Field; 11] = [
    Field { key: "url", label: "Server address", hint: "default: http://127.0.0.1:8765", kind: Kind::Text },
    Field { key: "token", label: "Access token", hint: "not set, which is fine on this machine alone", kind: Kind::Secret },
    Field {
        key: "model",
        label: "Model folder",
        hint: "not set here: the server was given its model when it was started",
        kind: Kind::Text,
    },
    Field {
        key: "model_dir",
        label: "Models to choose from",
        hint: "default: the folder that holds the model folder",
        kind: Kind::Text,
    },
    Field {
        key: "roots",
        label: "Folders the model may read",
        hint: "default: the folder the server was started in",
        kind: Kind::List,
    },
    Field { key: "idle_seconds", label: "Unload the model after (seconds)", hint: "default: 600", kind: Kind::Number },
    Field { key: "max_tokens", label: "Longest reply (tokens)", hint: "default: 1024", kind: Kind::Number },
    Field { key: "input_chars", label: "Conversation kept (characters)", hint: "default: 24000", kind: Kind::Number },
    Field { key: "max_calls", label: "Tool calls per question", hint: "default: 12, or what suits the model", kind: Kind::Number },
    Field { key: "tools", label: "File tools", hint: "default: by model", kind: Kind::Choice(&["on", "off"]) },
    Field { key: "prompt", label: "Tool prompt", hint: "default: by model", kind: Kind::Choice(&["firm", "light"]) },
];

pub fn path() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    var("NIBBLE_CONFIG")
        .or_else(|| var("XDG_CONFIG_HOME").map(|dir| dir.join("nibble/config.json")))
        .or_else(|| var("HOME").map(|dir| dir.join(".config/nibble/config.json")))
}

/// Whether the file is written by something else. The Nix module links it
/// into the store, and there the place to change a setting is the module.
pub fn managed() -> bool {
    path().and_then(|path| fs::read_link(path).ok()).is_some_and(|target| target.starts_with("/nix/store"))
}

pub fn load() -> Map<String, Value> {
    path()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

/// What to put in a field for the value in the file.
pub fn show(field: &Field, settings: &Map<String, Value>) -> String {
    let Some(value) = settings.get(field.key) else { return String::new() };
    match field.kind {
        Kind::Secret => String::new(),
        Kind::List => {
            let items: Vec<&str> = value.as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            items.join(", ")
        }
        Kind::Choice(_) if field.key == "tools" => match value.as_bool() {
            Some(true) => "on".to_string(),
            Some(false) => "off".to_string(),
            None => String::new(),
        },
        _ => value.as_str().map(str::to_string).unwrap_or_else(|| value.to_string()),
    }
}

/// What the server says it uses for a field, as the hint for an empty one.
/// None when it doesn't say, or says nothing worth showing.
pub fn in_use(field: &Field, server: &Map<String, Value>) -> Option<String> {
    let value = show(field, server);
    (!value.is_empty()).then(|| format!("in use: {value}"))
}

/// Put what was typed into the settings. An empty field removes the key, so
/// the default (or the model's preset) applies again. An empty secret leaves
/// the stored one alone.
pub fn apply(field: &Field, typed: &str, settings: &mut Map<String, Value>) -> Result<(), String> {
    let typed = typed.trim();
    if typed.is_empty() {
        if field.kind != Kind::Secret {
            settings.remove(field.key);
        }
        return Ok(());
    }
    let value = match field.kind {
        Kind::Text | Kind::Secret => json!(typed),
        Kind::Number => json!(typed.parse::<u64>().map_err(|_| format!("{}: \"{typed}\" is not a whole number", field.label))?),
        Kind::List => json!(typed.split(',').map(str::trim).filter(|item| !item.is_empty()).collect::<Vec<_>>()),
        Kind::Choice(_) if field.key == "tools" => json!(typed == "on"),
        Kind::Choice(_) => json!(typed),
    };
    settings.insert(field.key.to_string(), value);
    Ok(())
}

pub fn save(settings: &Map<String, Value>) -> Result<(), String> {
    let path = path().ok_or("HOME is not set, so there is nowhere to save")?;
    if managed() {
        return Err("This file is managed by Nix. Change the settings in your Nix configuration.".into());
    }
    let dir = path.parent().ok_or("no config directory")?;
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let text = format!("{:#}\n", Value::Object(settings.clone()));
    fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(key: &str) -> &'static Field {
        FIELDS.iter().find(|field| field.key == key).unwrap()
    }

    #[test]
    fn typed_values_become_settings_and_back() {
        let mut settings: Map<String, Value> =
            serde_json::from_str(r#"{ "token": "secret", "server_args": ["--x"], "tools": false }"#).unwrap();

        apply(field("roots"), " ~/devel , ~/notes ", &mut settings).unwrap();
        apply(field("idle_seconds"), "300", &mut settings).unwrap();
        apply(field("tools"), "on", &mut settings).unwrap();
        apply(field("prompt"), "light", &mut settings).unwrap();
        assert_eq!(settings["roots"], json!(["~/devel", "~/notes"]));
        assert_eq!(settings["idle_seconds"], json!(300));
        assert_eq!(settings["tools"], json!(true));
        assert_eq!(show(field("roots"), &settings), "~/devel, ~/notes");
        assert_eq!(show(field("tools"), &settings), "on");
        assert_eq!(show(field("idle_seconds"), &settings), "300");

        // A key with no field on the page is kept.
        assert_eq!(settings["server_args"], json!(["--x"]));
        // A secret is never shown, and an empty field does not erase it.
        assert_eq!(show(field("token"), &settings), "");
        apply(field("token"), "", &mut settings).unwrap();
        assert_eq!(settings["token"], json!("secret"));
        // Emptying any other field removes the key.
        apply(field("prompt"), "", &mut settings).unwrap();
        assert!(!settings.contains_key("prompt"));
        assert!(apply(field("max_tokens"), "lots", &mut settings).is_err());
    }

    #[test]
    fn the_server_s_settings_become_hints() {
        let server: Map<String, Value> =
            serde_json::from_str(r#"{ "max_calls": 6, "tools": false, "roots": ["/a", "/b"], "prompt": "light", "token": "x" }"#)
                .unwrap();
        assert_eq!(in_use(field("max_calls"), &server).as_deref(), Some("in use: 6"));
        assert_eq!(in_use(field("tools"), &server).as_deref(), Some("in use: off"));
        assert_eq!(in_use(field("roots"), &server).as_deref(), Some("in use: /a, /b"));
        assert_eq!(in_use(field("prompt"), &server).as_deref(), Some("in use: light"));
        // Not said, or a secret: the field keeps its own hint.
        assert_eq!(in_use(field("idle_seconds"), &server), None);
        assert_eq!(in_use(field("token"), &server), None);
    }
}
