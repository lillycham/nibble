mod chat;
mod config;
mod eval;
mod mcp;
mod plugins;
mod quotes;
mod recipes;
mod serve;
mod sessions;
mod tools;
mod web;

use std::error::Error;
use std::fs::File;
use std::io::{self, IsTerminal, Read};
use std::os::fd::AsFd;
use std::os::unix::fs::FileTypeExt;
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::Value;

use chat::message;
use quotes::Sources;
use recipes::Recipe;
use sessions::{Recorder, Session};

const USAGE: &str = "usage: nibble [options] [PROMPT...]
       nibble serve --model PATH [options]
       nibble mcp [--root DIR]...
       nibble eval [--model NAME]...
       nibble RECIPE [options] [TEXT...]
       nibble config
       nibble chats
       nibble plugins
       nibble recipes

Text piped on stdin is given to the model as input for the prompt.
With no prompt and no pipe, nibble starts a chat, which is saved as it goes.

  -f, --file FILE       attach a file to the prompt (or the chat's first
                        message); repeat for more. Its whole text is sent, so
                        the model needs no tool to read it
  -s, --system TEXT     replace the system prompt
  -n, --max-tokens N    reply length limit
      --no-tools        don't let the model read files
      --tools           let it read files even when input is piped in or
                        files are attached
      --no-claude       don't let the model ask Claude for help
  -p, --plugin NAME     offer the model a plugin's tools too; repeat for more.
                        Plugins are set up in the config file
  -q, --quote           have the model quote the lines its answer rests on,
                        and check that each quote is really in the file
      --anywhere        let the model read files outside the current directory
      --stats           after each answer, show its prompt size, tokens and speed
      --no-stats        don't (the default when stderr is not a terminal)
  -c, --continue        go on with the newest saved chat
  -r, --resume ID       go on with a saved chat; `nibble chats` lists them
      --no-save         don't save this chat

In a chat, lines that start with / are commands: /help lists them.

A recipe is a prompt kept under a name, with settings of its own, such as
`nibble summarise` or `nibble commit`; `nibble recipes` lists them. The text
after its name, piped input and attached files are what it works on.

Saved chats are shared with the window. A one-shot prompt is saved only when
it continues a chat.

On its own, the model can read and search files but can't change anything.

`nibble config` prints the settings in use and where the config file belongs.
`nibble plugins` lists the plugins that are set up, and what each one costs.
`nibble eval` tries the model on a set of questions with known answers.";

#[derive(Clone)]
struct Args {
    /// The system prompt, without the request for quotes.
    system: String,
    max_tokens: u32,
    prompt: String,
    /// The attached files, each in a <file> block, ready to append to a message.
    files: String,
    attached: Vec<(String, String)>,
    tools: Vec<Value>,
    /// Whether `tools` holds the file tools, which quotes may be checked against.
    file_tools: bool,
    /// The tools of the plugins asked for, also in `tools`.
    plugin_tools: Vec<Value>,
    stats: bool,
    resume: Option<Resume>,
    save: bool,
    /// What quotes are checked against, in quote mode.
    quotes: Option<Sources>,
    piped: bool,
}

#[derive(Clone)]
enum Resume {
    Latest,
    Id(String),
}

impl Args {
    /// The system prompt to send, with the request for quotes in quote mode.
    fn system(&self) -> String {
        match &self.quotes {
            None => self.system.clone(),
            Some(_) => {
                let input = if self.piped { " For the <input>, write input as the path." } else { "" };
                format!("{}{}{input}", self.system, quotes::SYSTEM_QUOTE)
            }
        }
    }

    /// Quote mode's sources for these settings: the attached files, and the
    /// files the tools can read.
    fn sources(&self) -> Sources {
        let mut sources = Sources::new(self.file_tools);
        for (path, text) in &self.attached {
            sources.give(path, text);
        }
        sources
    }

    /// The settings for one turn of `recipe`.
    fn with(&self, recipe: &Recipe) -> Result<Args, String> {
        let mut args = self.clone();
        if recipe.tools == Some(false) {
            // Plugins were asked for by name, so they stay.
            args.tools.clone_from(&args.plugin_tools);
            args.file_tools = false;
        }
        let more = plugins::start(&recipe.plugins)?;
        plugins::add(&mut args.plugin_tools, more.clone());
        plugins::add(&mut args.tools, more);
        if let Some(max_tokens) = recipe.max_tokens {
            args.max_tokens = max_tokens;
        }
        if let Some(system) = &recipe.system {
            args.system.clone_from(system);
        }
        if recipe.quote && args.quotes.is_none() {
            args.quotes = Some(args.sources());
        }
        Ok(args)
    }
}

/// `recipe` gives the defaults, for `nibble RECIPE`; flags still win.
fn parse_args(
    mut args: impl Iterator<Item = String>,
    piped: bool,
    recipe: Option<&Recipe>,
) -> Result<Option<Args>, Box<dyn Error>> {
    let mut system = recipe.and_then(|recipe| recipe.system.clone());
    let mut max_tokens = recipe.and_then(|recipe| recipe.max_tokens).unwrap_or(config::get().max_tokens);
    let (mut tools, mut claude, mut confined) = (recipe.and_then(|recipe| recipe.tools), tools::claude_allowed(), true);
    let mut stats = io::stderr().is_terminal();
    let (mut resume, mut save) = (None, true);
    let mut words = Vec::new();
    let mut files = String::new();
    let mut attached = Vec::new();
    let mut quote = recipe.is_some_and(|recipe| recipe.quote);
    let mut plugins = recipe.map(|recipe| recipe.plugins.clone()).unwrap_or_default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "-f" | "--file" => {
                // The user named the file, so it may be anywhere: confinement
                // is for the model's own reads.
                let path = args.next().ok_or("-f needs a file")?;
                let text = tools::read_text(&tools::expand(&path))?;
                files += &mcp::attach(&path, &text);
                attached.push((path, text));
            }
            "-s" | "--system" => system = Some(args.next().ok_or("-s needs a value")?),
            "-n" | "--max-tokens" => max_tokens = args.next().ok_or("-n needs a value")?.parse()?,
            "--no-tools" => tools = Some(false),
            "--tools" => tools = Some(true),
            "--no-claude" => claude = false,
            "-p" | "--plugin" => plugins.push(args.next().ok_or("--plugin needs a name")?),
            "-q" | "--quote" => quote = true,
            "--anywhere" => confined = false,
            "--stats" => stats = true,
            "--no-stats" => stats = false,
            "-c" | "--continue" => resume = Some(Resume::Latest),
            "-r" | "--resume" => resume = Some(Resume::Id(args.next().ok_or("-r needs a chat id")?)),
            "--no-save" => save = false,
            _ => words.push(arg),
        }
    }
    // Piped input and attached files are the whole task, so the model gets no
    // tools with them: an eager model otherwise goes looking for files, and
    // the schemas cost tokens.
    let tools = tools.unwrap_or(config::get().tools && !piped && files.is_empty());
    if tools && confined {
        tools::confine(&[std::env::current_dir()?])?;
    }
    let system = system.unwrap_or_else(|| {
        let mut system = chat::system().to_string();
        if tools {
            system += &chat::system_tools();
        }
        if tools && claude {
            system += chat::SYSTEM_CLAUDE;
        }
        if tools {
            system += &tools::context();
        }
        system
    });
    let file_tools = tools;
    let mut tools = if tools { tools::schemas(claude) } else { Vec::new() };
    // Asked for by name, so offered even with piped input or attached files.
    let mut plugin_tools = Vec::new();
    plugins::add(&mut plugin_tools, plugins::start(&plugins)?);
    plugins::add(&mut tools, plugin_tools.clone());
    let mut args = Args {
        system,
        max_tokens,
        prompt: words.join(" "),
        files,
        attached,
        tools,
        file_tools,
        plugin_tools,
        stats,
        resume,
        save,
        quotes: None,
        piped,
    };
    if quote {
        args.quotes = Some(args.sources());
    }
    Ok(Some(args))
}

/// The saved chat to go on with, if one was asked for.
fn resumed(args: &Args) -> Result<Option<Session>, Box<dyn Error>> {
    let session = match &args.resume {
        None => return Ok(None),
        Some(Resume::Latest) => Session::latest()?,
        Some(Resume::Id(id)) => Session::load(id)?,
    };
    let turns = session.turns.len();
    eprintln!("nibble: going on with \"{}\" ({turns} turn{})", session.title(), if turns == 1 { "" } else { "s" });
    Ok(Some(session))
}

/// Run one user turn, and keep it in the saved chat if there is one. A turn
/// that fails is forgotten, so that it can be asked again. `typed` is the
/// command the turn came from, if it did.
fn turn(
    messages: &mut Vec<Value>,
    user: &str,
    typed: Option<&str>,
    args: &Args,
    session: Option<&mut Session>,
) -> Result<(), Box<dyn Error>> {
    let before = messages.len();
    messages.push(message("user", user));
    let mut terminal = chat::Terminal::default();
    let mut recorder = Recorder { inner: &mut terminal, parts: Vec::new() };
    let outcome = match chat::run(messages, &args.tools, args.max_tokens, &mut recorder) {
        Ok(outcome) => outcome,
        Err(e) => {
            messages.truncate(before);
            return Err(e);
        }
    };
    if let Some(sources) = &args.quotes {
        eprintln!("nibble: {}", quotes::check(&outcome.reply, sources).report());
    }
    show_stats(args, &outcome);
    if let Some(session) = session.filter(|_| args.save) {
        let mut saved = serde_json::json!({ "user": user, "parts": recorder.parts });
        if let Some(typed) = typed {
            saved["typed"] = typed.into();
        }
        session.turns.push(saved);
        if let Err(e) = session.save() {
            eprintln!("nibble: the chat was not saved: {e}");
        }
    }
    Ok(())
}

/// What a chat in the terminal keeps from one line to the next.
struct Repl {
    args: Args,
    session: Session,
    messages: Vec<Value>,
    /// Attached files go with the first message, and wait for one that gets an answer.
    files: String,
}

const COMMANDS: [(&str, &str); 7] = [
    ("/new", "start a new chat; this one stays saved"),
    ("/clear", "start over, and forget this chat"),
    ("/model [NAME]", "list the models, or switch to one"),
    ("/settings", "show the settings in use"),
    ("/quote [on|off]", "have answers quote the lines they rest on, and check them"),
    ("/plugin NAME", "offer a plugin's tools in this chat; /plugin off NAME stops"),
    ("/help", "this list"),
];

fn help() {
    eprintln!("Commands:");
    for (command, what) in COMMANDS {
        eprintln!("  {command:<18}{what}");
    }
    let recipes = &config::get().recipes;
    if !recipes.is_empty() {
        eprintln!("Recipes, each with the text after it:");
        for recipe in recipes {
            eprintln!("  {:<18}{}", format!("/{}", recipe.name), recipe.description);
        }
    }
}

impl Repl {
    /// Run one turn, with `args` in place of the chat's own settings.
    fn ask(&mut self, user: &str, typed: Option<&str>, args: Option<Args>) {
        let args = args.unwrap_or_else(|| self.args.clone());
        self.messages[0] = message("system", &args.system());
        let typed = typed.map(|typed| format!("{typed}{}", self.files));
        match turn(&mut self.messages, &format!("{user}{}", self.files), typed.as_deref(), &args, Some(&mut self.session)) {
            Ok(()) => self.files.clear(),
            Err(e) => eprintln!("nibble: {e}"),
        }
        self.messages[0] = message("system", &self.args.system());
        chat::trim(&mut self.messages);
    }

    /// `/plugin NAME` adds a plugin's tools to this chat, `/plugin off NAME`
    /// takes them away, and `/plugin` alone says which there are.
    fn plugin(&mut self, text: &str) -> Result<(), Box<dyn Error>> {
        let on = |args: &Args, name: &str| {
            let names = plugins::tool_names(name);
            !names.is_empty() && args.tools.iter().any(|t| t["function"]["name"].as_str().is_some_and(|n| names.iter().any(|m| m == n)))
        };
        match text.split_whitespace().collect::<Vec<_>>()[..] {
            [] => {
                let all = plugins::configured();
                if all.is_empty() {
                    eprintln!("nibble: no plugins are set up; `nibble plugins` says how");
                }
                for (name, _) in all {
                    eprintln!("  {name:<18}{}", if on(&self.args, &name) { "on" } else { "off" });
                }
            }
            ["off", name] => {
                let names = plugins::tool_names(name);
                let theirs = |t: &Value| t["function"]["name"].as_str().is_some_and(|n| names.iter().any(|m| m == n));
                self.args.tools.retain(|t| !theirs(t));
                self.args.plugin_tools.retain(|t| !theirs(t));
                eprintln!("nibble: {name} off");
            }
            [name] => {
                let schemas = plugins::start(&[name.to_string()])?;
                let count = schemas.len();
                plugins::add(&mut self.args.plugin_tools, schemas.clone());
                plugins::add(&mut self.args.tools, schemas);
                eprintln!("nibble: {name} on, with {count} tool{}", if count == 1 { "" } else { "s" });
            }
            _ => return Err("/plugin takes a name, or off and a name".into()),
        }
        Ok(())
    }

    fn start_over(&mut self) {
        self.session = Session::new();
        self.messages = self.session.messages(&self.args.system());
    }

    fn command(&mut self, name: &str, text: &str) -> Result<(), Box<dyn Error>> {
        match name {
            "help" => help(),
            "new" => {
                self.start_over();
                eprintln!("nibble: new chat");
            }
            "clear" => {
                self.session.forget();
                self.start_over();
                eprintln!("nibble: cleared");
            }
            "model" => model(text)?,
            "settings" => show_config(),
            "quote" => {
                let on = match text {
                    "" => self.args.quotes.is_none(),
                    "on" => true,
                    "off" => false,
                    _ => return Err("/quote takes on or off".into()),
                };
                self.args.quotes = on.then(|| self.args.sources());
                self.messages[0] = message("system", &self.args.system());
                eprintln!("nibble: {}", if on { "answers quote their evidence" } else { "quotes off" });
            }
            "plugin" => self.plugin(text)?,
            _ => {
                let recipe = config::get().recipe(name).ok_or_else(|| format!("no command /{name}; /help lists them"))?;
                let mut args = self.args.with(recipe)?;
                // The text given, else the attached files alone, else the command's output.
                let input = match recipe.run_command().filter(|_| text.is_empty() && self.files.is_empty()) {
                    Some(output) => output?,
                    None => text.to_string(),
                };
                let budget = config::get().input_chars.saturating_sub(self.files.len());
                let input = chat::clip(&input, budget);
                if let Some(sources) = args.quotes.as_mut().filter(|_| !input.is_empty()) {
                    sources.give("input", &input);
                    args.piped = true;
                }
                let typed = format!("/{name} {text}");
                self.ask(&recipe.message(&input), Some(typed.trim_end()), Some(args));
            }
        }
        Ok(())
    }
}

/// Ask `nibble serve` for its models, or to switch: `body` makes it a POST.
fn ask_server(path: &str, body: Option<Value>) -> Result<Value, String> {
    let config = config::get();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .http_status_as_error(false)
        .build()
        .into();
    let url = format!("{}{path}", config.url);
    let token = (!config.token.is_empty()).then(|| format!("Bearer {}", config.token));
    let response = match body {
        Some(body) => {
            let post = agent.post(&url);
            let post = match &token { Some(token) => post.header("Authorization", token), None => post };
            post.send_json(body)
        }
        None => {
            let get = agent.get(&url);
            let get = match &token { Some(token) => get.header("Authorization", token), None => get };
            get.call()
        }
    };
    let mut response = response.map_err(|_| format!("no answer from {}", config.url))?;
    let text = response.body_mut().read_to_string().unwrap_or_default();
    match response.status().as_u16() {
        200 => serde_json::from_str(&text).map_err(|_| "the server gave a bad answer".to_string()),
        401 => Err("the server needs a token".into()),
        404 => Err("this server can't list or switch models".into()),
        _ => Err(text.trim().to_string()),
    }
}

/// `/model`: list the models, or switch to the one whose name holds `wanted`.
fn model(wanted: &str) -> Result<(), Box<dyn Error>> {
    let info = ask_server("/info", None)?;
    let current = info["model"].as_str().unwrap_or_default();
    let all: Vec<&str> = info["models"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if wanted.is_empty() {
        for name in if all.is_empty() { vec![current] } else { all } {
            eprintln!("{} {name}", if name == current { "*" } else { " " });
        }
        return Ok(());
    }
    let lower = wanted.to_lowercase();
    let found: Vec<&str> = match all.iter().find(|name| **name == wanted) {
        Some(name) => vec![name],
        None => all.iter().copied().filter(|name| name.to_lowercase().contains(&lower)).collect(),
    };
    let name = match found.as_slice() {
        [name] => *name,
        [] => return Err(format!("no model called \"{wanted}\"; /model lists them").into()),
        many => return Err(format!("\"{wanted}\" could be any of {}", many.join(", ")).into()),
    };
    if name == current {
        eprintln!("nibble: {name} is in use already");
        return Ok(());
    }
    ask_server("/model", Some(serde_json::json!({ "model": name })))?;
    // Its presets apply from the next message on.
    config::switch(name)?;
    eprintln!("nibble: switched to {name}");
    Ok(())
}

fn repl(args: Args) -> Result<(), Box<dyn Error>> {
    let session = resumed(&args)?.unwrap_or_else(Session::new);
    let mut messages = session.messages(&args.system());
    chat::trim(&mut messages);
    let files = args.files.clone();
    let mut repl = Repl { args, session, messages, files };
    let mut line = String::new();
    loop {
        eprint!("> ");
        line.clear();
        if io::stdin().read_line(&mut line)? == 0 {
            eprintln!();
            if repl.args.save && !repl.session.turns.is_empty() {
                eprintln!("nibble: saved; `nibble -r {}` goes on with it", repl.session.id);
            }
            return Ok(());
        }
        if line.trim().is_empty() {
            continue;
        }
        match recipes::split(&line) {
            Some((name, text)) => {
                if let Err(e) = repl.command(name, text) {
                    eprintln!("nibble: {e}");
                }
            }
            None => repl.ask(line.trim(), None, None),
        }
    }
}

fn show_stats(args: &Args, outcome: &chat::Outcome) {
    if args.stats {
        eprintln!("nibble: {}", outcome.stats);
    }
}

/// Whatever was piped or redirected into stdin, or nothing.
fn piped_input() -> io::Result<String> {
    let stdin = io::stdin();
    if stdin.is_terminal() {
        return Ok(String::new());
    }
    let kind = File::from(stdin.as_fd().try_clone_to_owned()?).metadata()?.file_type();
    let mut input = String::new();
    if kind.is_fifo() || kind.is_file() {
        // A shell pipe or a redirect. It will end, however slow the writer is.
        stdin.lock().read_to_string(&mut input)?;
        return Ok(input);
    }
    // Anything else was handed to us by the program that started us: often
    // /dev/null, but agent harnesses pass a socket that stays open and silent,
    // and reading that to its end would wait for ever. Give it a moment.
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let mut input = String::new();
        let _ = send.send(io::stdin().read_to_string(&mut input).map(|_| input));
    });
    receive.recv_timeout(Duration::from_millis(500)).unwrap_or(Ok(input))
}

fn show_config() {
    match config::path() {
        Some(path) if path.exists() => eprintln!("config file: {}", path.display()),
        Some(path) => eprintln!("config file: {} (not there; these are the defaults)", path.display()),
        None => eprintln!("config file: none, because HOME is not set"),
    }
    match config::get().model_name.as_str() {
        "" => eprintln!("model: unknown, so no per-model presets apply"),
        name => eprintln!("model: {name}"),
    }
    println!("{:#}", config::get().to_json());
}

fn run() -> Result<(), Box<dyn Error>> {
    // The model decides which presets apply, so find it before the settings
    // are read: `serve` may name it with a flag, and the others ask the server.
    let all: Vec<String> = std::env::args().skip(1).collect();
    let serving = all.first().is_some_and(|mode| mode == "serve");
    let flag_model = all.iter().position(|arg| arg == "--model").and_then(|at| all.get(at + 1)).cloned();
    config::init(flag_model.filter(|_| serving), !serving)?;
    let mut argv = all.into_iter().peekable();
    match argv.peek().map(String::as_str) {
        Some("serve") => return serve::run(argv.skip(1)),
        Some("mcp") => return mcp::run(argv.skip(1)),
        Some("eval") => return eval::run(argv.skip(1)),
        Some("config") => {
            show_config();
            return Ok(());
        }
        Some("chats") => {
            sessions::show_list();
            return Ok(());
        }
        Some("plugins") => {
            plugins::show();
            return Ok(());
        }
        Some("recipes") => {
            for recipe in &config::get().recipes {
                println!("{:<14}{}", recipe.name, recipe.description);
            }
            return Ok(());
        }
        _ => {}
    }
    // `nibble NAME` runs the recipe of that name.
    let recipe = argv.peek().and_then(|name| config::get().recipe(name)).cloned();
    if recipe.is_some() {
        argv.next();
    }
    let input = piped_input()?;
    let Some(mut args) = parse_args(argv, !input.trim().is_empty(), recipe.as_ref())? else {
        println!("{USAGE}");
        return Ok(());
    };

    // Refuse rather than cut an attached file: an answer from half a file
    // would look just as confident as one from the whole of it. Piped input
    // gets what room is left.
    let budget = config::get().input_chars;
    if args.files.len() > budget {
        return Err(format!(
            "the attached files are {} characters, over the limit of {budget} (input_chars)",
            args.files.len()
        )
        .into());
    }
    let budget = budget - args.files.len();
    let mut input = input;
    if let Some(recipe) = &recipe {
        // The text after its name and any piped input are what it works on;
        // with neither, nor files, its command gives the input.
        input = match (args.prompt.is_empty(), input.trim().is_empty()) {
            (true, true) if args.files.is_empty() => recipe.run_command().transpose()?.unwrap_or_default(),
            (_, true) => std::mem::take(&mut args.prompt),
            (true, false) => input,
            (false, false) => format!("{}\n\n{input}", std::mem::take(&mut args.prompt)),
        };
        args.piped = !input.trim().is_empty();
    }
    if input.trim().len() > budget {
        eprintln!("nibble: input is {} characters, so the middle is cut to fit {budget}", input.trim().len());
    }
    let input = chat::clip(input.trim(), budget);
    if let Some(sources) = &mut args.quotes {
        if !input.is_empty() {
            sources.give("input", &input);
        }
    }

    let user = match (args.prompt.is_empty(), input.is_empty()) {
        _ if recipe.is_some() => recipe.as_ref().map(|recipe| recipe.message(&input)).unwrap_or_default(),
        (true, true) if io::stdin().is_terminal() => return repl(args),
        (true, true) => return Err("no prompt".into()),
        (true, false) => input.into_owned(),
        (false, true) => args.prompt.clone(),
        (false, false) => format!("{}\n\n<input>\n{input}\n</input>", args.prompt),
    } + &args.files;
    let mut session = resumed(&args)?;
    let mut messages = match &session {
        Some(session) => session.messages(&args.system()),
        None => vec![message("system", &args.system())],
    };
    chat::trim(&mut messages);
    turn(&mut messages, &user, None, &args, session.as_mut())
}

fn main() -> ExitCode {
    let result = run();
    plugins::stop_all();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("nibble: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attached_files_go_whole_and_turn_tools_off() {
        let dir = std::env::temp_dir().join(format!("nibble-attach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
        std::fs::write(&a, "first\n").unwrap();
        std::fs::write(&b, "second\n").unwrap();
        let argv = |extra: &[&str]| {
            let mut argv = vec!["-f", a.to_str().unwrap(), "what", "is", "this", "--file", b.to_str().unwrap()];
            argv.extend(extra);
            argv.into_iter().map(String::from).collect::<Vec<_>>()
        };

        let args = parse_args(argv(&[]).into_iter(), false, None).unwrap().unwrap();
        assert_eq!(args.prompt, "what is this");
        let expected = format!(
            "\n\n<file path=\"{}\">\nfirst\n</file>\n\n<file path=\"{}\">\nsecond\n</file>",
            a.display(),
            b.display()
        );
        assert_eq!(args.files, expected);
        assert!(args.tools.is_empty());

        let missing = ["-f".to_string(), dir.join("missing").display().to_string()];
        assert!(parse_args(missing.into_iter(), false, None).is_err());
        assert!(parse_args(["-f".to_string()].into_iter(), false, None).is_err());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
