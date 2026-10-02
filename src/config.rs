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
    /// The name of the model in use, when it could be found out. Not a setting:
    /// it comes from `model`, or from asking the server.
    pub model_name: String,
    /// Which built-in tools prompt to use: "firm" for a model that is slow
    /// to use its tools, "light" for one that is quick to.
    pub prompt: String,
    /// The most tool calls in one turn, however many rounds they take.
    pub max_calls: usize,
    /// Whether to offer the model tools at all. Turn it off for a model with
    /// no tool-call format: told to use tools it can't call, it writes
    /// make-believe calls as its answer.
    pub tools: bool,
    /// Replacements for the built-in system prompts. Empty means built-in.
    /// The built-in ones are firm, to suit small models.
    pub system: String,
    pub system_tools: String,
    /// Directories `nibble mcp` may read inside. Empty means its working directory.
    pub roots: Vec<String>,
    // `nibble serve`
    pub model: String,
    pub listen: String,
    pub backend_port: u16,
    pub idle_seconds: u64,
    pub server_command: String,
    /// Extra arguments for the model server. If any of them holds {model} or
    /// {port}, they are the whole command line instead, with those filled
    /// in, for servers that don't take mlx_lm.server's flags.
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
            model_name: String::new(),
            prompt: "firm".into(),
            max_calls: 12,
            tools: true,
            system: String::new(),
            system_tools: String::new(),
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
            "tools" => value.as_bool().map(|v| self.tools = v),
            "prompt" => match value.as_str() {
                Some("firm" | "light") => text(value).map(|v| self.prompt = v),
                Some(other) => return Err(format!("\"prompt\" must be \"firm\" or \"light\", not \"{other}\"")),
                None => None,
            },
            "max_calls" => number(value).map(|v| self.max_calls = v),
            "system" => text(value).map(|v| self.system = v),
            "system_tools" => text(value).map(|v| self.system_tools = v),
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
            "tools": self.tools,
            "prompt": self.prompt,
            "max_calls": self.max_calls,
            "system": self.system,
            "system_tools": self.system_tools,
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

/// What we know about particular models: only the settings that should differ
/// from the defaults, and only for models someone has actually run. The
/// pattern is looked for in the model's name, ignoring case. A "models"
/// section in the config file has the same form, is applied later, and wins.
const KNOWN_MODELS: [(&str, &str); 2] = [
    // No tool-call format. Offered tools, it writes make-believe calls.
    ("gemma-3n", r#"{ "tools": false }"#),
    // Eager with tools, where the default prompt is written for the reluctant.
    // It also reads far more than it needs, and each read slows the next
    // round, so keep its results short and its calls few.
    ("lfm2", r#"{ "prompt": "light", "max_calls": 6, "result_chars": 4000 }"#),
];

/// Ask `nibble serve` which model it runs. Quietly gives up on any other
/// server, and on none at all.
fn served_model(url: &str) -> Option<String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(2)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut response = agent.get(format!("{url}/info")).call().ok()?;
    let info: Value = serde_json::from_str(&response.body_mut().read_to_string().ok()?).ok()?;
    info["model"].as_str().map(str::to_string)
}

/// `flag_model` is a model named on the command line. `ask_server` says
/// whether to ask the server for its model when nothing else names one.
fn load(flag_model: Option<String>, ask_server: bool) -> Result<Config, String> {
    let mut file = serde_json::Map::new();
    let mut source = String::new();
    if let Some(path) = path() {
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                source = format!("{}: ", path.display());
                let parsed: Value = serde_json::from_str(&text).map_err(|e| format!("{source}{e}"))?;
                file = parsed.as_object().ok_or(format!("{source}expected a JSON object"))?.clone();
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let setting = |key: &str| file.get(key).and_then(Value::as_str).filter(|value| !value.is_empty()).map(str::to_string);

    // The model has to be known first, because it decides which presets apply.
    let mut model = flag_model.clone().or_else(|| var("NIBBLE_MODEL")).or_else(|| setting("model")).unwrap_or_default();
    if model.is_empty() && ask_server {
        let url = var("NIBBLE_URL").or_else(|| setting("url")).unwrap_or_else(|| Config::default().url);
        model = served_model(&url).unwrap_or_default();
    }
    let name = model.trim_end_matches('/').rsplit('/').next().unwrap_or_default().to_string();
    let matches = |pattern: &str| !name.is_empty() && name.to_lowercase().contains(&pattern.to_lowercase());

    let mut config = Config { model_name: name.clone(), ..Config::default() };
    for (pattern, preset) in KNOWN_MODELS {
        if matches(pattern) {
            let preset: Value = serde_json::from_str(preset).expect("built-in preset is valid JSON");
            for (key, value) in preset.as_object().into_iter().flatten() {
                config.set(key, value)?;
            }
        }
    }
    for (key, value) in file.iter().filter(|(key, _)| *key != "models") {
        config.set(key, value).map_err(|e| format!("{source}{e}"))?;
    }
    if let Some(models) = file.get("models") {
        let models = models.as_object().ok_or(format!("{source}\"models\" must be an object"))?;
        for (pattern, settings) in models.iter().filter(|(pattern, _)| matches(pattern)) {
            let settings = settings.as_object().ok_or(format!("{source}models.{pattern} must be an object"))?;
            for (key, value) in settings {
                config.set(key, value).map_err(|e| format!("{source}models.{pattern}: {e}"))?;
            }
        }
    }

    let vars = [("NIBBLE_URL", "url"), ("NIBBLE_CLAUDE", "claude_command"), ("NIBBLE_TOKEN", "token")];
    for (name, key) in vars {
        if let Some(value) = var(name) {
            config.set(key, &value.into())?;
        }
    }
    if let Some(model) = flag_model.or_else(|| var("NIBBLE_MODEL")) {
        config.model = model;
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
pub fn init(flag_model: Option<String>, ask_server: bool) -> Result<(), String> {
    let config = load(flag_model, ask_server)?;
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
        assert!(config.set("prompt", &json!("shouty")).is_err());
        for (_, preset) in KNOWN_MODELS {
            let preset: Value = serde_json::from_str(preset).unwrap();
            for (key, value) in preset.as_object().unwrap() {
                config.set(key, value).unwrap();
            }
        }
    }
}
