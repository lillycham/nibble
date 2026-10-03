//! Slash commands in the window: the built-in ones, which act on the window,
//! and recipes, which are prompts with settings of their own kept in the
//! config file. The server says which recipes there are, and applies their
//! settings when a message names one; the window puts the prompt together,
//! so that the chat's history reads the same as what the model was sent.

use serde_json::{Map, Value};

/// The built-in commands: name, what follows it, and what it does.
pub const BUILT_IN: [(&str, &str, &str); 7] = [
    ("new", "", "Start a new chat"),
    ("cd", "folder", "Choose the folder this chat works in"),
    ("clear", "", "Clear this chat and forget it"),
    ("model", "name", "Switch to another model"),
    ("settings", "", "Open the settings"),
    ("quote", "", "Have answers quote the lines they rest on"),
    ("help", "", "List the commands"),
];

/// One line of the list of commands shown as one is typed.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub name: String,
    pub takes: String,
    pub about: String,
}

/// A recipe, as the server describes it.
pub struct Recipe {
    pub name: String,
    pub prompt: String,
    /// Whether it reads its input from a program when given none, which the
    /// window can't do.
    pub command: bool,
}

pub fn recipes(in_use: &Map<String, Value>) -> Vec<Recipe> {
    let Some(recipes) = in_use.get("recipes").and_then(Value::as_object) else { return Vec::new() };
    let mut recipes: Vec<Recipe> = recipes
        .iter()
        .filter_map(|(name, recipe)| {
            let prompt = recipe["prompt"].as_str()?.to_string();
            Some(Recipe { name: name.clone(), prompt, command: recipe["command"].is_array() })
        })
        .collect();
    recipes.sort_by(|a, b| a.name.cmp(&b.name));
    recipes
}

/// Split a message into a command and the rest: "/sum some text" gives
/// ("sum", "some text"). None for a message that is not a command.
pub fn split(text: &str) -> Option<(&str, &str)> {
    let rest = text.trim_start().strip_prefix('/')?;
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let (name, rest) = rest.split_at(end);
    // A path such as "/etc/hosts" starts a message, not a command.
    (!name.is_empty() && !name.contains('/')).then(|| (name, rest.trim()))
}

/// Every command, built-in ones first, then the recipes.
pub fn all(in_use: &Map<String, Value>) -> Vec<Item> {
    let built_in = BUILT_IN.iter().map(|(name, takes, about)| Item {
        name: name.to_string(),
        takes: takes.to_string(),
        about: about.to_string(),
    });
    let mut recipes: Vec<Item> = in_use
        .get("recipes")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(name, recipe)| Item {
            name: name.clone(),
            takes: "text".to_string(),
            about: recipe["description"].as_str().unwrap_or_default().to_string(),
        })
        .collect();
    recipes.sort_by(|a, b| a.name.cmp(&b.name));
    built_in.chain(recipes).collect()
}

/// The commands to offer while `text` is a command's name being typed:
/// those that start with what is there so far. None once it isn't.
pub fn offer(text: &str, in_use: &Map<String, Value>) -> Option<Vec<Item>> {
    let typed = text.strip_prefix('/')?;
    if typed.contains(char::is_whitespace) || typed.contains('/') {
        return None;
    }
    let items: Vec<Item> = all(in_use).into_iter().filter(|item| item.name.starts_with(typed)).collect();
    (!items.is_empty()).then_some(items)
}

/// The command a name stands for: itself, or the one command it begins.
pub fn resolve(name: &str, in_use: &Map<String, Value>) -> Option<String> {
    let all = all(in_use);
    if all.iter().any(|item| item.name == name) {
        return Some(name.to_string());
    }
    match all.iter().filter(|item| item.name.starts_with(name)).collect::<Vec<_>>().as_slice() {
        [one] => Some(one.name.clone()),
        _ => None,
    }
}

/// The message a recipe sends: the text given goes where `{input}` is, or
/// after the prompt in an <input> block. The same as on the command line.
pub fn message(prompt: &str, input: &str) -> String {
    let input = input.trim();
    if prompt.contains("{input}") {
        return prompt.replace("{input}", input);
    }
    if input.is_empty() {
        return prompt.to_string();
    }
    format!("{prompt}\n\n<input>\n{input}\n</input>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn in_use() -> Map<String, Value> {
        json!({ "recipes": {
            "summarise": { "description": "Summarise the text given", "prompt": "Summarise this." },
            "commit": { "description": "Write a commit message", "prompt": "Write one:\n{input}", "command": ["git", "diff"] },
        } })
        .as_object()
        .unwrap()
        .clone()
    }

    #[test]
    fn commands_are_offered_as_they_are_typed() {
        let names = |items: Vec<Item>| items.into_iter().map(|item| item.name).collect::<Vec<_>>();
        assert_eq!(names(offer("/", &in_use()).unwrap()).len(), 9);
        assert_eq!(names(offer("/s", &in_use()).unwrap()), ["settings", "summarise"]);
        assert_eq!(names(offer("/co", &in_use()).unwrap()), ["commit"]);
        assert!(offer("/sum some text", &in_use()).is_none());
        assert!(offer("/x", &in_use()).is_none());
        assert!(offer("hello", &in_use()).is_none());

        assert_eq!(resolve("sum", &in_use()).as_deref(), Some("summarise"));
        assert_eq!(resolve("new", &in_use()).as_deref(), Some("new"));
        assert_eq!(resolve("s", &in_use()), None);
        assert_eq!(resolve("nope", &in_use()), None);

        assert_eq!(split("/model lfm"), Some(("model", "lfm")));
        assert_eq!(split("/cd ~/devel/nibble"), Some(("cd", "~/devel/nibble")));
        assert_eq!(split("/usr/bin is where"), None);

        let recipes = recipes(&in_use());
        assert_eq!(recipes.iter().map(|r| (r.name.as_str(), r.command)).collect::<Vec<_>>(), [("commit", true), ("summarise", false)]);
        assert_eq!(message(&recipes[0].prompt, "a diff"), "Write one:\na diff");
        assert_eq!(message(&recipes[1].prompt, "text"), "Summarise this.\n\n<input>\ntext\n</input>");
    }
}
