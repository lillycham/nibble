//! Plugins: MCP servers that nibble starts, and whose tools it offers the
//! model next to its own. Each is declared once in the "plugins" setting and
//! stays off until a chat asks for it by name, because every tool's schema is
//! sent with every request, and a small window has little room to spare.
//!
//! Only what a plugin needs from MCP is here: stdio servers, and their tools.
//! Messages are one JSON object per line, as in `nibble mcp`.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::{chat, config};

/// Long enough for `npx` or `uvx` to fetch a server on its first run.
const START_TIMEOUT: Duration = Duration::from_secs(60);
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
const PROTOCOL: &str = "2025-06-18";
/// The tools nibble has itself. A plugin tool of the same name is renamed.
const OWN_TOOLS: [&str; 4] = ["read_file", "list_dir", "search", "ask_claude"];

/// One entry of the "plugins" setting.
#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    /// The program and its arguments.
    pub command: Vec<String>,
    /// Added to nibble's own environment.
    pub env: Vec<(String, String)>,
    /// Only these of its tools, to save room. None means all of them.
    pub tools: Option<Vec<String>>,
}

fn strings(value: &Value) -> Option<Vec<String>> {
    value.as_array()?.iter().map(|v| v.as_str().map(str::to_string)).collect()
}

/// Check one entry, so that a mistake shows when the config is read rather
/// than when the plugin is first asked for.
pub fn parse(name: &str, value: &Value) -> Result<Spec, String> {
    let entry = value.as_object().ok_or(format!("plugins.{name} must be an object"))?;
    for key in entry.keys() {
        if !["command", "env", "tools"].contains(&key.as_str()) {
            return Err(format!("plugins.{name}: unknown setting \"{key}\""));
        }
    }
    let command = entry.get("command").and_then(strings).filter(|c| !c.is_empty());
    let command = command.ok_or(format!("plugins.{name}.command must be a list of strings, the program first"))?;
    let env = match entry.get("env") {
        None => Vec::new(),
        Some(env) => env
            .as_object()
            .and_then(|env| env.iter().map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
            .ok_or(format!("plugins.{name}.env must be an object of strings"))?,
    };
    let tools = match entry.get("tools") {
        None => None,
        Some(tools) => Some(strings(tools).ok_or(format!("plugins.{name}.tools must be a list of tool names"))?),
    };
    Ok(Spec { command, env, tools })
}

/// A plugin that is running.
struct Server {
    name: String,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    /// The last lines it wrote to stderr, to explain a failure.
    said: Arc<Mutex<VecDeque<String>>>,
    next_id: u64,
}

impl Server {
    fn start(name: &str, spec: &Spec) -> Result<Server, String> {
        let mut child = Command::new(&spec.command[0])
            .args(&spec.command[1..])
            .envs(spec.env.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("plugin {name}: can't run {}: {e}", spec.command[0]))?;
        let (send, lines) = mpsc::channel();
        let stdout = child.stdout.take().expect("stdout is piped");
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        let said = Arc::new(Mutex::new(VecDeque::new()));
        let kept = said.clone();
        let stderr = child.stderr.take().expect("stderr is piped");
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                let mut kept = kept.lock().unwrap();
                if kept.len() == 5 {
                    kept.pop_front();
                }
                kept.push_back(line);
            }
        });
        let stdin = child.stdin.take();
        let mut server = Server { name: name.to_string(), child, stdin, lines, said, next_id: 1 };
        let hello = json!({
            "protocolVersion": PROTOCOL,
            "capabilities": {},
            "clientInfo": { "name": "nibble", "version": env!("CARGO_PKG_VERSION") },
        });
        server.request("initialize", hello, START_TIMEOUT)?;
        server.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))?;
        Ok(server)
    }

    fn send(&mut self, message: &Value) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("stopped")?;
        writeln!(stdin, "{message}").and_then(|()| stdin.flush()).map_err(|_| self.gone())
    }

    /// Why it stopped answering, with what it last said.
    fn gone(&self) -> String {
        let said = self.said.lock().unwrap();
        let said = said.iter().map(String::as_str).collect::<Vec<_>>().join(" / ");
        let said = if said.is_empty() { String::new() } else { format!(": {}", chat::clip(&said, 400)) };
        format!("plugin {} stopped{said}", self.name)
    }

    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        let deadline = Instant::now() + timeout;
        loop {
            let line = match self.lines.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(format!("plugin {} gave no answer in {} s", self.name, timeout.as_secs()))
                }
                Err(RecvTimeoutError::Disconnected) => return Err(self.gone()),
            };
            // Some servers print a banner or a log line on stdout. Skip it.
            let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
            if message["method"].is_string() {
                // The server asks something of us. We offer nothing beyond
                // being alive, and a notification needs no answer.
                if let Some(their_id) = message.get("id").cloned() {
                    let reply = if message["method"] == "ping" {
                        json!({ "jsonrpc": "2.0", "id": their_id, "result": {} })
                    } else {
                        json!({ "jsonrpc": "2.0", "id": their_id, "error": { "code": -32601, "message": "not supported" } })
                    };
                    self.send(&reply)?;
                }
                continue;
            }
            if message["id"].as_u64() != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                let said = error["message"].as_str().unwrap_or("an error");
                return Err(format!("plugin {}: {said}", self.name));
            }
            return Ok(message["result"].clone());
        }
    }

    fn tools(&mut self) -> Result<Vec<Value>, String> {
        let mut tools = Vec::new();
        let mut cursor = Value::Null;
        loop {
            let params = if cursor.is_null() { json!({}) } else { json!({ "cursor": cursor }) };
            let mut page = self.request("tools/list", params, START_TIMEOUT)?;
            tools.extend(page["tools"].as_array_mut().map(std::mem::take).unwrap_or_default());
            cursor = page["nextCursor"].take();
            if !cursor.is_string() {
                return Ok(tools);
            }
        }
    }

    /// Close its input, which is how MCP asks a stdio server to stop, and
    /// make sure it does.
    fn stop(&mut self) {
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            if !matches!(self.child.try_wait(), Ok(None)) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A plugin tool as the model sees it, and where it really is.
struct Tool {
    offered: String,
    real: String,
    server: usize,
    schema: Value,
}

struct Running {
    servers: Vec<Server>,
    tools: Vec<Tool>,
}

static RUNNING: Mutex<Running> = Mutex::new(Running { servers: Vec::new(), tools: Vec::new() });

/// The plugins in the "plugins" setting, by name.
pub fn configured() -> Vec<(String, Spec)> {
    let plugins = &config::get().plugins;
    // The config was checked when it was read.
    plugins.iter().filter_map(|(name, value)| Some((name.clone(), parse(name, value).ok()?))).collect()
}

/// An MCP tool in the shape a chat request wants.
fn schema(name: &str, tool: &Value) -> Value {
    let mut parameters = tool["inputSchema"].clone();
    if !parameters.is_object() {
        parameters = json!({ "type": "object", "properties": {} });
    }
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": tool["description"].as_str().unwrap_or_default(),
            "parameters": parameters,
        },
    })
}

/// Start the named plugins, those not already running, and return the tool
/// schemas of all of them, to offer the model.
pub fn start(names: &[String]) -> Result<Vec<Value>, String> {
    let all = configured();
    let mut running = RUNNING.lock().unwrap();
    let mut schemas = Vec::new();
    for name in names {
        let at = match running.servers.iter().position(|server| &server.name == name) {
            Some(at) => at,
            None => {
                let Some((_, spec)) = all.iter().find(|(known, _)| known == name) else {
                    let known: Vec<&str> = all.iter().map(|(name, _)| name.as_str()).collect();
                    let known = if known.is_empty() { "none are set up".to_string() } else { format!("there are {}", known.join(", ")) };
                    return Err(format!("no plugin called {name}: {known}"));
                };
                let mut server = Server::start(name, spec)?;
                let listed = server.tools();
                let listed = match listed {
                    Ok(listed) => listed,
                    Err(e) => {
                        server.stop();
                        return Err(e);
                    }
                };
                let at = running.servers.len();
                running.servers.push(server);
                for tool in listed {
                    let Some(real) = tool["name"].as_str().map(str::to_string) else { continue };
                    if spec.tools.as_ref().is_some_and(|wanted| !wanted.contains(&real)) {
                        continue;
                    }
                    let taken = |offered: &str| {
                        OWN_TOOLS.contains(&offered) || running.tools.iter().any(|tool| tool.offered == offered)
                    };
                    let offered = if taken(&real) { format!("{name}_{real}") } else { real.clone() };
                    let schema = schema(&offered, &tool);
                    running.tools.push(Tool { offered, real, server: at, schema });
                }
                at
            }
        };
        schemas.extend(running.tools.iter().filter(|tool| tool.server == at).map(|tool| tool.schema.clone()));
    }
    Ok(schemas)
}

/// Add `more` to `tools`, leaving out any already there.
pub fn add(tools: &mut Vec<Value>, more: Vec<Value>) {
    for schema in more {
        if !tools.iter().any(|had| had["function"]["name"] == schema["function"]["name"]) {
            tools.push(schema);
        }
    }
}

/// The names the model knows a running plugin's tools by.
pub fn tool_names(name: &str) -> Vec<String> {
    let running = RUNNING.lock().unwrap();
    let Some(at) = running.servers.iter().position(|server| server.name == name) else { return Vec::new() };
    running.tools.iter().filter(|tool| tool.server == at).map(|tool| tool.offered.clone()).collect()
}

/// The text of a tool result. Pictures and other things a text model can't
/// use are named, not sent.
fn result_text(result: &Value) -> String {
    let mut parts = Vec::new();
    for item in result["content"].as_array().into_iter().flatten() {
        let part = match item["type"].as_str().unwrap_or_default() {
            "text" => item["text"].as_str().unwrap_or_default().to_string(),
            "resource" => match item["resource"]["text"].as_str() {
                Some(text) => text.to_string(),
                None => format!("[{}]", item["resource"]["uri"].as_str().unwrap_or("a file")),
            },
            "resource_link" => format!("[{}]", item["uri"].as_str().unwrap_or("a link")),
            other => format!("[{} not shown]", if other.is_empty() { "something" } else { other }),
        };
        parts.push(part);
    }
    if parts.is_empty() && !result["structuredContent"].is_null() {
        parts.push(result["structuredContent"].to_string());
    }
    let text = parts.join("\n");
    let text = chat::clip(text.trim(), config::get().result_chars);
    if result["isError"] == true {
        format!("error: {text}")
    } else {
        text.into_owned()
    }
}

/// Run a plugin tool. None when no running plugin has a tool of that name.
pub fn call(name: &str, arguments: &Value) -> Option<String> {
    let mut running = RUNNING.lock().unwrap();
    let Running { servers, tools } = &mut *running;
    let tool = tools.iter().find(|tool| tool.offered == name)?;
    let arguments = if arguments.is_object() { arguments.clone() } else { Value::Object(Map::new()) };
    let params = json!({ "name": tool.real, "arguments": arguments });
    Some(match servers[tool.server].request("tools/call", params, CALL_TIMEOUT) {
        Ok(result) => result_text(&result),
        Err(e) => format!("error: {e}"),
    })
}

/// Stop every plugin. Called on the way out.
pub fn stop_all() {
    let mut running = RUNNING.lock().unwrap();
    for server in &mut running.servers {
        server.stop();
    }
    running.servers.clear();
    running.tools.clear();
}

/// `nibble plugins`: start each one, and say what it offers and what that
/// costs on every request.
pub fn show() {
    let all = configured();
    match config::path() {
        _ if !all.is_empty() => {}
        Some(path) => {
            eprintln!("No plugins are set up. Add a \"plugins\" section to {}, for example:", path.display());
            eprintln!("{EXAMPLE}");
            return;
        }
        None => {
            eprintln!("No plugins are set up, and there is no config file because HOME is not set.");
            return;
        }
    }
    for (name, _) in &all {
        match start(std::slice::from_ref(name)) {
            Ok(schemas) => {
                let names: Vec<&str> = schemas.iter().filter_map(|s| s["function"]["name"].as_str()).collect();
                let size: usize = schemas.iter().map(|s| s.to_string().len()).sum();
                println!("{name}: {} tools, {size} characters on every request", names.len());
                if !names.is_empty() {
                    println!("  {}", names.join(", "));
                }
            }
            Err(e) => println!("{name}: {e}"),
        }
    }
    stop_all();
}

const EXAMPLE: &str = r#"  "plugins": {
    "git": { "command": ["uvx", "mcp-server-git"], "tools": ["git_status", "git_log", "git_diff"] }
  }
Then `nibble --plugin git` offers its tools in that chat."#;

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in MCP server in a few lines of shell. It counts our requests
    /// to know their ids, as nibble numbers them from 1, and logs to stderr
    /// and stdout the way some real servers do.
    const FAKE_SERVER: &str = r#"
echo "starting up" >&2
echo "a banner that is not JSON"
n=0
while read -r line; do
  # Notifications, and our answer to its ping, aren't requests.
  case "$line" in *'"method":"notifications/'*) continue ;; *'"method"'*) ;; *) continue ;; esac
  n=$((n+1))
  case "$line" in
    *'"initialize"'*)
      echo '{"jsonrpc":"2.0","method":"notifications/message","params":{}}'
      echo '{"jsonrpc":"2.0","id":"x","method":"ping"}'
      printf '{"jsonrpc":"2.0","id":%d,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"1"}}}\n' $n ;;
    *'"tools/list"'*'"cursor"'*)
      printf '{"jsonrpc":"2.0","id":%d,"result":{"tools":[{"name":"search","description":"Search the web","inputSchema":{"type":"object","properties":{"q":{"type":"string"}}}}]}}\n' $n ;;
    *'"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%d,"result":{"tools":[{"name":"shout","description":"Say it loud","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}},{"name":"whisper"}],"nextCursor":"2"}}\n' $n ;;
    *'"text":"die"'*)
      echo "shout: fatal" >&2; exit 1 ;;
    *'"name":"shout"'*)
      printf '{"jsonrpc":"2.0","id":%d,"result":{"content":[{"type":"text","text":"HELLO"},{"type":"image","data":"","mimeType":"image/png"}]}}\n' $n ;;
    *)
      printf '{"jsonrpc":"2.0","id":%d,"result":{"content":[{"type":"text","text":"no such tool"}],"isError":true}}\n' $n ;;
  esac
done
"#;

    fn spec() -> Spec {
        Spec { command: vec!["sh".into(), "-c".into(), FAKE_SERVER.into()], env: Vec::new(), tools: None }
    }

    #[test]
    fn entries_are_checked() {
        let good = json!({ "command": ["uvx", "mcp-server-git"], "env": { "A": "b" }, "tools": ["git_log"] });
        let spec = parse("git", &good).unwrap();
        assert_eq!(spec.command, ["uvx", "mcp-server-git"]);
        assert_eq!(spec.env, [("A".to_string(), "b".to_string())]);
        assert_eq!(spec.tools, Some(vec!["git_log".to_string()]));
        assert_eq!(parse("git", &json!({ "command": ["x"] })).unwrap().tools, None);
        for bad in [
            json!(["uvx"]),
            json!({}),
            json!({ "command": [] }),
            json!({ "command": "uvx mcp-server-git" }),
            json!({ "command": ["x"], "env": { "A": 1 } }),
            json!({ "command": ["x"], "tools": "git_log" }),
            json!({ "command": ["x"], "comand": ["x"] }),
        ] {
            assert!(parse("git", &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn speaks_mcp_to_a_server() {
        let mut server = Server::start("fake", &spec()).unwrap();
        // Two pages, put together.
        let tools = server.tools().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(names, ["shout", "whisper", "search"]);
        // A tool with no schema still gets one a chat request accepts.
        assert_eq!(schema("whisper", &tools[1])["function"]["parameters"], json!({ "type": "object", "properties": {} }));

        let result = server.request("tools/call", json!({ "name": "shout", "arguments": { "text": "hello" } }), CALL_TIMEOUT);
        assert_eq!(result_text(&result.unwrap()), "HELLO\n[image not shown]");
        let result = server.request("tools/call", json!({ "name": "nothing", "arguments": {} }), CALL_TIMEOUT);
        assert_eq!(result_text(&result.unwrap()), "error: no such tool");

        // A crash is reported with what the server last said.
        let crash = server.request("tools/call", json!({ "name": "shout", "arguments": { "text": "die" } }), CALL_TIMEOUT);
        let crash = crash.unwrap_err();
        assert!(crash.starts_with("plugin fake stopped"), "{crash}");
        // Its stderr may still be on the way when stdout closes.
        thread::sleep(Duration::from_millis(100));
        assert!(server.gone().contains("shout: fatal"), "{}", server.gone());
        server.stop();

        let missing = Spec { command: vec!["/nonexistent/plugin".into()], env: Vec::new(), tools: None };
        let error = Server::start("missing", &missing).err().unwrap();
        assert!(error.starts_with("plugin missing: can't run"), "{error}");
    }

    #[test]
    fn a_silent_server_times_out() {
        let script = r#"read -r line; echo '{"jsonrpc":"2.0","id":1,"result":{}}'; cat > /dev/null"#;
        let silent = Spec { command: vec!["sh".into(), "-c".into(), script.into()], env: Vec::new(), tools: None };
        let mut server = Server::start("silent", &silent).unwrap();
        let error = server.request("tools/list", json!({}), Duration::from_millis(200)).unwrap_err();
        assert!(error.starts_with("plugin silent gave no answer"), "{error}");
        server.stop();
    }

    #[test]
    fn results_name_what_they_leave_out() {
        let result = json!({ "content": [
            { "type": "resource", "resource": { "uri": "file:///a.txt", "text": "apples" } },
            { "type": "resource", "resource": { "uri": "file:///b.png", "blob": "" } },
            { "type": "resource_link", "uri": "file:///c.txt" },
            { "type": "audio" },
        ] });
        assert_eq!(result_text(&result), "apples\n[file:///b.png]\n[file:///c.txt]\n[audio not shown]");
        assert_eq!(result_text(&json!({ "content": [], "structuredContent": { "n": 1 } })), "{\"n\":1}");
    }
}
