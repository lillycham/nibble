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
    /// Shown as dots, and never copied out of the field.
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
    if field.kind == Kind::Secret {
        return None;
    }
    let value = show(field, server);
    (!value.is_empty()).then(|| format!("in use: {value}"))
}

/// Put what was typed into the settings. An empty field removes the key, so
/// the default (or the model's preset) applies again.
pub fn apply(field: &Field, typed: &str, settings: &mut Map<String, Value>) -> Result<(), String> {
    let typed = typed.trim();
    if typed.is_empty() {
        settings.remove(field.key);
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

/// The settings a model can have of its own, and what kind of value each takes.
/// The rest (the address, the token, which model to start with) belong to the
/// server as a whole. Mirrors `Config::set` in the nibble crate.
const PER_MODEL: [(&str, Kind); 10] = [
    ("tools", Kind::Choice(&["on", "off"])),
    ("prompt", Kind::Choice(&["firm", "light"])),
    ("max_calls", Kind::Number),
    ("max_tokens", Kind::Number),
    ("input_chars", Kind::Number),
    ("result_chars", Kind::Number),
    ("max_steps", Kind::Number),
    ("system", Kind::Text),
    ("system_tools", Kind::Text),
    ("map_max_tokens", Kind::Number),
];

/// The presets nibble has built in, as in `KNOWN_MODELS` in the nibble crate,
/// to say what a model gets when the file gives it nothing of its own.
const BUILT_IN: [(&str, &str); 2] =
    [("gemma-3n", "tools: off"), ("lfm2", "prompt: light, max_calls: 6, result_chars: 4000")];

/// Whether a preset's name picks out a model: it is looked for in the model's
/// name, ignoring case, as nibble does.
fn matches(pattern: &str, model: &str) -> bool {
    !model.is_empty() && model.to_lowercase().contains(&pattern.to_lowercase())
}

/// The presets to show, one field each: those in the file's "models" section,
/// then each model there is to choose from that none of them covers yet.
pub fn preset_names(settings: &Map<String, Value>, models: &[String]) -> Vec<String> {
    let section = settings.get("models").and_then(Value::as_object);
    let mut names: Vec<String> = section.into_iter().flatten().map(|(name, _)| name.clone()).collect();
    for model in models {
        if !names.iter().any(|name| matches(name, model)) {
            names.push(model.clone());
        }
    }
    names
}

/// What an empty preset field says: the built-in preset if there is one.
pub fn preset_hint(name: &str) -> String {
    match BUILT_IN.iter().find(|(pattern, _)| matches(pattern, name)) {
        Some((_, preset)) => format!("built in: {preset}"),
        None => "nothing of its own: the settings above apply".to_string(),
    }
}

/// A model's preset as typed on the page: `prompt: light, max_calls: 6`.
pub fn show_preset(name: &str, settings: &Map<String, Value>) -> String {
    let Some(preset) = settings.get("models").and_then(|models| models.get(name)).and_then(Value::as_object) else {
        return String::new();
    };
    let value = |key: &str, value: &Value| match value {
        Value::Bool(on) if key == "tools" => if *on { "on" } else { "off" }.to_string(),
        // Quoted only when it would otherwise be read back as something else.
        Value::String(text) if text.contains([',', '"']) || text != text.trim() => value.to_string(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    preset.iter().map(|(key, v)| format!("{key}: {}", value(key, v))).collect::<Vec<_>>().join(", ")
}

/// Split at the commas that are not inside double quotes.
fn split_pairs(typed: &str) -> Vec<&str> {
    let (mut pieces, mut start, mut quoted, mut escaped) = (Vec::new(), 0, false, false);
    for (at, c) in typed.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ',' if !quoted => {
                pieces.push(&typed[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    pieces.push(&typed[start..]);
    pieces.into_iter().map(str::trim).filter(|piece| !piece.is_empty()).collect()
}

/// Read a typed preset. Nothing typed is no preset.
fn parse_preset(name: &str, typed: &str) -> Result<Map<String, Value>, String> {
    let mut preset = Map::new();
    for pair in split_pairs(typed) {
        let fail = |why: String| format!("{name}: {why}");
        let (key, raw) = pair
            .split_once(':')
            .ok_or_else(|| fail(format!("\"{pair}\" needs a name and a value, as in max_calls: 6")))?;
        let (key, raw) = (key.trim(), raw.trim());
        let kind = PER_MODEL.iter().find(|(known, _)| *known == key).map(|(_, kind)| *kind).ok_or_else(|| {
            let known: Vec<&str> = PER_MODEL.iter().map(|(known, _)| *known).collect();
            fail(format!("\"{key}\" can't be set per model; these can: {}", known.join(", ")))
        })?;
        let text = if raw.starts_with('"') {
            serde_json::from_str::<String>(raw).map_err(|_| fail(format!("{key}: the quotes don't close")))?
        } else {
            raw.to_string()
        };
        let value = match kind {
            Kind::Number => {
                json!(text.parse::<u64>().map_err(|_| fail(format!("{key}: \"{text}\" is not a whole number")))?)
            }
            Kind::Choice(options) if !options.contains(&text.as_str()) => {
                return Err(fail(format!("{key} is {}, not \"{text}\"", options.join(" or "))));
            }
            Kind::Choice(_) if key == "tools" => json!(text == "on"),
            _ => json!(text),
        };
        preset.insert(key.to_string(), value);
    }
    Ok(preset)
}

/// Put the typed presets, (name, text) pairs, into the settings' "models"
/// section. An empty field removes that model's preset, and an empty section
/// goes too.
pub fn apply_presets(typed: &[(String, String)], settings: &mut Map<String, Value>) -> Result<(), String> {
    let mut models = settings.get("models").and_then(Value::as_object).cloned().unwrap_or_default();
    for (name, text) in typed {
        let preset = parse_preset(name, text)?;
        if preset.is_empty() {
            models.remove(name);
        } else {
            models.insert(name.clone(), Value::Object(preset));
        }
    }
    if models.is_empty() {
        settings.remove("models");
    } else {
        settings.insert("models".to_string(), Value::Object(models));
    }
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
        // A secret goes into its (masked) field, and emptying it removes it.
        assert_eq!(show(field("token"), &settings), "secret");
        apply(field("token"), "", &mut settings).unwrap();
        assert!(!settings.contains_key("token"));
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

    #[test]
    fn presets_are_typed_as_names_and_values() {
        let mut settings: Map<String, Value> = serde_json::from_str(
            r#"{ "max_tokens": 500, "models": { "qwen3": { "max_calls": 3, "tools": false }, "old": { "prompt": "firm" } } }"#,
        )
        .unwrap();
        let models = ["Qwen3-4B-Instruct".to_string(), "LFM2.5-2.6B".to_string()];

        // The file's presets, then each model none of them covers.
        assert_eq!(preset_names(&settings, &models), ["qwen3", "old", "LFM2.5-2.6B"]);
        assert_eq!(show_preset("qwen3", &settings), "max_calls: 3, tools: off");
        assert_eq!(show_preset("LFM2.5-2.6B", &settings), "");
        assert_eq!(preset_hint("LFM2.5-2.6B"), "built in: prompt: light, max_calls: 6, result_chars: 4000");

        let typed = [
            ("qwen3".to_string(), "max_calls: 4, tools: on, system: \"Be brief, and quote.\"".to_string()),
            ("old".to_string(), "  ".to_string()),
            ("LFM2.5-2.6B".to_string(), "max_tokens: 800".to_string()),
        ];
        apply_presets(&typed, &mut settings).unwrap();
        assert_eq!(
            settings["models"],
            json!({
                "qwen3": { "max_calls": 4, "tools": true, "system": "Be brief, and quote." },
                "LFM2.5-2.6B": { "max_tokens": 800 },
            })
        );
        assert_eq!(show_preset("qwen3", &settings), "max_calls: 4, tools: on, system: \"Be brief, and quote.\"");
        assert_eq!(settings["max_tokens"], json!(500));

        let bad = |text: &str| apply_presets(&[("qwen3".to_string(), text.to_string())], &mut settings.clone()).is_err();
        assert!(bad("url: http://x"));
        assert!(bad("max_calls: lots"));
        assert!(bad("prompt: soft"));
        assert!(bad("max_calls"));

        // Emptying every preset removes the section.
        let empty: Vec<(String, String)> =
            preset_names(&settings, &[]).into_iter().map(|name| (name, String::new())).collect();
        apply_presets(&empty, &mut settings).unwrap();
        assert!(!settings.contains_key("models"));
    }
}
