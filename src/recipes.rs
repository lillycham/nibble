//! Recipes: named prompts with settings of their own, such as `/summarise` or
//! `/commit`. The chat and the window run them as slash commands, and the
//! command line as `nibble commit`. They are declared in the config file's
//! "recipes" section (which the Nix module writes), next to a few built in.

use std::process::Command;

use serde_json::{json, Map, Value};

/// Names a recipe may not take: the built-in slash commands, and the
/// command line's own subcommands, which `nibble NAME` would shadow.
pub const RESERVED: [&str; 13] =
    ["help", "new", "clear", "model", "settings", "quote", "serve", "mcp", "eval", "config", "chats", "recipes", "plugins"];

#[derive(Clone, Debug, PartialEq)]
pub struct Recipe {
    pub name: String,
    /// One line for the list of commands.
    pub description: String,
    /// What is sent. The text given with the command goes where `{input}`
    /// is, or after the prompt in an <input> block when there is no `{input}`.
    pub prompt: String,
    /// Replaces the system prompt for this one turn.
    pub system: Option<String>,
    pub max_tokens: Option<u32>,
    /// False takes the file tools away for this turn. True can't give them
    /// where they aren't allowed.
    pub tools: Option<bool>,
    /// Quote-your-evidence mode for this turn.
    pub quote: bool,
    /// A program to run, and its arguments, when the command is given no
    /// text: its output is the input. Only on the command line and in the
    /// chat there, which run in a directory; the window asks for the text.
    pub command: Vec<String>,
}

/// Built in, so that the two the plan names exist from the start. The config
/// file can change them, or remove one by setting it to null.
const BUILT_IN: &str = r#"{
    "summarise": {
        "description": "Summarise the text given",
        "prompt": "Summarise this in a few sentences. Keep names, numbers and decisions; leave out the rest.",
        "tools": false
    },
    "commit": {
        "description": "Write a commit message for the staged changes",
        "prompt": "Write a git commit message for this change: a summary line of at most 60 characters in the imperative, a blank line, then a short paragraph on what changed and why. Reply with the message only.",
        "command": ["git", "diff", "--staged"],
        "tools": false
    }
}"#;

pub fn built_in() -> Vec<Recipe> {
    let mut recipes = Vec::new();
    merge(&mut recipes, &serde_json::from_str(BUILT_IN).expect("built-in recipes are valid JSON")).expect("built-in recipes are valid");
    recipes
}

/// Lay the recipes in `value` (an object, name to recipe or null) over
/// `recipes`: a name already there is replaced, and null removes it.
pub fn merge(recipes: &mut Vec<Recipe>, value: &Value) -> Result<(), String> {
    let object = value.as_object().ok_or("\"recipes\" must be an object")?;
    for (name, recipe) in object {
        recipes.retain(|had| had.name != *name);
        if !recipe.is_null() {
            recipes.push(parse(name, recipe).map_err(|e| format!("recipes.{name}: {e}"))?);
        }
    }
    recipes.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(())
}

fn parse(name: &str, value: &Value) -> Result<Recipe, String> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("a name may hold only letters, digits, - and _".into());
    }
    if RESERVED.contains(&name) {
        return Err(format!("\"{name}\" is taken by a built-in command"));
    }
    let object = value.as_object().ok_or("must be an object")?;
    let text = |key: &str| match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("\"{key}\" must be text")),
    };
    let flag = |key: &str| match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_bool().map(Some).ok_or(format!("\"{key}\" must be true or false")),
    };
    let max_tokens = match object.get("max_tokens") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_u64().and_then(|n| u32::try_from(n).ok()).ok_or("\"max_tokens\" must be a number")?),
    };
    let command = match object.get("command") {
        None | Some(Value::Null) => Vec::new(),
        Some(value) => value
            .as_array()
            .and_then(|args| args.iter().map(|arg| arg.as_str().map(str::to_string)).collect::<Option<Vec<_>>>())
            .filter(|args| !args.is_empty())
            .ok_or("\"command\" must be a list of text: the program, then its arguments")?,
    };
    for key in object.keys() {
        if !["description", "prompt", "system", "max_tokens", "tools", "quote", "command"].contains(&key.as_str()) {
            return Err(format!("unknown setting \"{key}\""));
        }
    }
    Ok(Recipe {
        name: name.to_string(),
        description: text("description")?.unwrap_or_default(),
        prompt: text("prompt")?.filter(|p| !p.trim().is_empty()).ok_or("needs a \"prompt\"")?,
        system: text("system")?.filter(|s| !s.is_empty()),
        max_tokens,
        tools: flag("tools")?,
        quote: flag("quote")?.unwrap_or(false),
        command,
    })
}

impl Recipe {
    /// The message to send, with `input` in it.
    pub fn message(&self, input: &str) -> String {
        let input = input.trim();
        if self.prompt.contains("{input}") {
            return self.prompt.replace("{input}", input);
        }
        if input.is_empty() {
            return self.prompt.clone();
        }
        format!("{}\n\n<input>\n{input}\n</input>", self.prompt)
    }

    /// Run the recipe's command for its input. None when it has none.
    pub fn run_command(&self) -> Option<Result<String, String>> {
        let (program, args) = self.command.split_first()?;
        let output = match Command::new(program).args(args).output() {
            Ok(output) => output,
            Err(e) => return Some(Err(format!("can't run {program}: {e}"))),
        };
        if !output.status.success() {
            let said = String::from_utf8_lossy(&output.stderr);
            return Some(Err(format!("{program} failed: {}", said.trim())));
        }
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Some(if text.is_empty() { Err(format!("{} gave nothing to work on", self.command.join(" "))) } else { Ok(text) })
    }

    pub fn to_json(&self) -> Value {
        let mut object = Map::new();
        object.insert("description".into(), json!(self.description));
        object.insert("prompt".into(), json!(self.prompt));
        if let Some(system) = &self.system {
            object.insert("system".into(), json!(system));
        }
        if let Some(max_tokens) = self.max_tokens {
            object.insert("max_tokens".into(), json!(max_tokens));
        }
        if let Some(tools) = self.tools {
            object.insert("tools".into(), json!(tools));
        }
        if self.quote {
            object.insert("quote".into(), json!(true));
        }
        if !self.command.is_empty() {
            object.insert("command".into(), json!(self.command));
        }
        Value::Object(object)
    }
}

pub fn to_json(recipes: &[Recipe]) -> Value {
    Value::Object(recipes.iter().map(|recipe| (recipe.name.clone(), recipe.to_json())).collect())
}

/// Split a chat line into a command and the rest: "/sum some text" gives
/// ("sum", "some text"). None for a line that is not a command.
pub fn split(line: &str) -> Option<(&str, &str)> {
    let rest = line.trim_start().strip_prefix('/')?;
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let (name, text) = rest.split_at(end);
    // "/" alone, or a path such as "/etc/hosts", is a message, not a command.
    (!name.is_empty() && !name.contains('/')).then(|| (name, text.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipes_merge_over_the_built_in_ones() {
        let mut recipes = built_in();
        assert_eq!(recipes.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["commit", "summarise"]);
        assert_eq!(recipes[0].command, ["git", "diff", "--staged"]);

        let mine = json!({
            "commit": null,
            "review": { "prompt": "Review this diff:\n{input}\nList the bugs.", "max_tokens": 600, "quote": true },
            "summarise": { "prompt": "Summarise briefly.", "system": "You are terse." }
        });
        merge(&mut recipes, &mine).unwrap();
        let names: Vec<_> = recipes.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["review", "summarise"]);
        assert_eq!((recipes[0].max_tokens, recipes[0].quote, recipes[0].tools), (Some(600), true, None));
        assert_eq!(recipes[1].system.as_deref(), Some("You are terse."));
        assert_eq!(recipes[0].message("a diff"), "Review this diff:\na diff\nList the bugs.");
        assert_eq!(recipes[1].message(""), "Summarise briefly.");
        assert_eq!(recipes[1].message(" text "), "Summarise briefly.\n\n<input>\ntext\n</input>");

        // What it prints reads back the same.
        let mut again = Vec::new();
        merge(&mut again, &to_json(&recipes)).unwrap();
        assert_eq!(again, recipes);

        for bad in [
            json!({ "new": { "prompt": "x" } }),
            json!({ "a b": { "prompt": "x" } }),
            json!({ "x": { "description": "no prompt" } }),
            json!({ "x": { "prompt": "x", "max_tokens": "lots" } }),
            json!({ "x": { "prompt": "x", "command": "git diff" } }),
            json!({ "x": { "prompt": "x", "tool": false } }),
            json!(["not an object"]),
        ] {
            assert!(merge(&mut recipes, &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn commands_are_told_from_messages() {
        assert_eq!(split("/model"), Some(("model", "")));
        assert_eq!(split("  /summarise  some text\nmore "), Some(("summarise", "some text\nmore")));
        assert_eq!(split("/etc/hosts is odd"), None);
        assert_eq!(split("/"), None);
        assert_eq!(split("what is /model?"), None);
    }

    #[test]
    fn a_command_gives_the_input() {
        let recipe = |command: &[&str]| Recipe { command: command.iter().map(|s| s.to_string()).collect(), ..built_in().remove(0) };
        assert_eq!(recipe(&["echo", "a change"]).run_command(), Some(Ok("a change".to_string())));
        assert!(recipe(&["true"]).run_command().unwrap().is_err());
        assert!(recipe(&["false"]).run_command().unwrap().is_err());
        assert!(recipe(&["no-such-program-here"]).run_command().unwrap().is_err());
        assert_eq!(recipe(&[]).run_command(), None);
    }
}
