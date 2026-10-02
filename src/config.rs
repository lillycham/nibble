//! Settings. Each one takes its value from, in rising order: the built-in
//! default, the config file, an environment variable, a command-line flag.
//!
//! Sizes are in characters, not tokens, because nibble has no tokenizer.
//! Four characters is roughly one token of English, three of code.

use std::path::PathBuf;
use std::sync::OnceLock;

use serde_json::{json, Value};

pub struct Config {
    /// Where the model server (or `nibble serve`) listens.
    pub url: String,
    /// Reply length limit, in tokens.
    pub max_tokens: u32,
    /// How much conversation to send. About 6k tokens, which leaves room for
    /// the reply in an 8k window.
    pub input_chars: usize,
    /// The most one tool result may hold: about a quarter of the window.
    pub result_chars: usize,
    /// Rounds of tool calls before the tools are taken away to force an answer.
    pub max_steps: usize,
    pub map_max_files: usize,
    pub map_file_chars: usize,
    pub map_max_tokens: u32,
    pub claude_command: String,
    /// Directories `nibble mcp` may read inside. Empty means its working directory.
    pub roots: Vec<String>,
    // `nibble serve`
    pub model: String,
    pub listen: String,
    pub backend_port: u16,
    pub idle_seconds: u64,
    pub server_command: String,
    pub server_args: Vec<String>,
    /// When set, every request to `nibble serve` must carry it as a Bearer
    /// token, the same way an OpenAI-style API key is sent.
    pub token: String,
    /// A file that holds the token, for setups where the config file is public.
    pub token_file: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            url: "http://127.0.0.1:8765".into(),
            max_tokens: 1024,
            input_chars: 24_000,
            result_chars: 6_000,
            max_steps: 8,
            map_max_files: 20,
            map_file_chars: 8_000,
            map_max_tokens: 100,
            claude_command: "claude".into(),
            roots: Vec::new(),
            model: String::new(),
            listen: "127.0.0.1:8765".into(),
            backend_port: 8766,
            idle_seconds: 600,
            server_command: "nibble-mlx-server".into(),
            server_args: Vec::new(),
            token: String::new(),
            token_file: String::new(),
        }
    }
}

fn text(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

fn number<T: TryFrom<u64>>(value: &Value) -> Option<T> {
    T::try_from(value.as_u64()?).ok()
}

fn list(value: &Value) -> Option<Vec<String>> {
    value.as_array()?.iter().map(text).collect()
}

impl Config {
    fn set(&mut self, key: &str, value: &Value) -> Result<(), String> {
        let done = match key {
            "url" => text(value).map(|v| self.url = v),
            "max_tokens" => number(value).map(|v| self.max_tokens = v),
            "input_chars" => number(value).map(|v| self.input_chars = v),
            "result_chars" => number(value).map(|v| self.result_chars = v),
            "max_steps" => number(value).map(|v| self.max_steps = v),
            "map_max_files" => number(value).map(|v| self.map_max_files = v),
            "map_file_chars" => number(value).map(|v| self.map_file_chars = v),
            "map_max_tokens" => number(value).map(|v| self.map_max_tokens = v),
            "claude_command" => text(value).map(|v| self.claude_command = v),
            "roots" => list(value).map(|v| self.roots = v),
            "model" => text(value).map(|v| self.model = v),
            "listen" => text(value).map(|v| self.listen = v),
            "backend_port" => number(value).map(|v| self.backend_port = v),
            "idle_seconds" => number(value).map(|v| self.idle_seconds = v),
            "server_command" => text(value).map(|v| self.server_command = v),
            "server_args" => list(value).map(|v| self.server_args = v),
            "token" => text(value).map(|v| self.token = v),
            "token_file" => text(value).map(|v| self.token_file = v),
            // A typo should be loud, not a setting that quietly does nothing.
            _ => return Err(format!("unknown setting \"{key}\"")),
        };
        done.ok_or_else(|| format!("\"{key}\" has the wrong type of value"))
    }

    pub fn to_json(&self) -> Value {
        json!({
            "url": self.url,
            "max_tokens": self.max_tokens,
            "input_chars": self.input_chars,
            "result_chars": self.result_chars,
            "max_steps": self.max_steps,
            "map_max_files": self.map_max_files,
            "map_file_chars": self.map_file_chars,
            "map_max_tokens": self.map_max_tokens,
            "claude_command": self.claude_command,
            "roots": self.roots,
            "model": self.model,
            "listen": self.listen,
            "backend_port": self.backend_port,
            "idle_seconds": self.idle_seconds,
            "server_command": self.server_command,
            "server_args": self.server_args,
            // Never print the secret itself.
            "token": if self.token.is_empty() { "" } else { "(set)" },
            "token_file": self.token_file,
        })
    }
}

pub fn path() -> Option<PathBuf> {
    let var = |name| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    var("NIBBLE_CONFIG")
        .or_else(|| var("XDG_CONFIG_HOME").map(|dir| dir.join("nibble/config.json")))
        .or_else(|| var("HOME").map(|dir| dir.join(".config/nibble/config.json")))
}

fn load() -> Result<Config, String> {
    let mut config = Config::default();
    if let Some(path) = path() {
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let name = path.display();
                let file: Value = serde_json::from_str(&text).map_err(|e| format!("{name}: {e}"))?;
                let settings = file.as_object().ok_or(format!("{name}: expected a JSON object"))?;
                for (key, value) in settings {
                    config.set(key, value).map_err(|e| format!("{name}: {e}"))?;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    let vars =
        [("NIBBLE_URL", "url"), ("NIBBLE_MODEL", "model"), ("NIBBLE_CLAUDE", "claude_command"), ("NIBBLE_TOKEN", "token")];
    for (name, key) in vars {
        if let Ok(value) = std::env::var(name) {
            config.set(key, &value.into())?;
        }
    }
    if config.token.is_empty() && !config.token_file.is_empty() {
        let file = crate::tools::expand(&config.token_file);
        let token = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        config.token = token.trim().to_string();
    }
    Ok(config)
}

static CONFIG: OnceLock<Config> = OnceLock::new();

/// Read the config file. Call once, before anything uses `get`.
pub fn init() -> Result<(), String> {
    let config = load()?;
    CONFIG.set(config).map_err(|_| "config loaded twice".to_string())
}

/// The settings. Tests never call `init`, so they get the defaults.
pub fn get() -> &'static Config {
    CONFIG.get_or_init(Config::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_reject_mistakes() {
        let mut config = Config::default();
        for (key, value) in Config::default().to_json().as_object().unwrap() {
            config.set(key, value).unwrap();
        }
        config.set("map_max_files", &json!(5)).unwrap();
        assert_eq!(config.map_max_files, 5);
        assert!(config.set("map_max_file", &json!(5)).is_err());
        assert!(config.set("max_tokens", &json!("many")).is_err());
        assert!(config.set("backend_port", &json!(70000)).is_err());
    }
}
