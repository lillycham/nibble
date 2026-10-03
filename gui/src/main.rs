//! A native chat window for nibble. It holds no model and runs no tools: it
//! talks to `nibble serve` over the same `/chat` endpoint as the web page.
//! It does keep the chats, as files, and it can edit nibble's config file.

mod attach;
mod input;
mod settings;
mod store;

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{
    App, Application, Bounds, Context, Div, Entity, ExternalPaths, Focusable, Hsla, KeyBinding, ScrollHandle, SharedString,
    Stateful, TitlebarOptions, Window, WindowAppearance, WindowBounds, WindowOptions, actions, anchored, deferred, div,
    point, prelude::*, px, rgb, size,
};
use serde_json::{Map, Value, json};

use attach::Attached;
use input::TextInput;
use settings::{FIELDS, Kind};
use store::{Chat, Entry, Part, Turn};

actions!(nibble, [Submit, NewChat, OpenSettings, Quit]);

/// Headings are set in Charter, which comes with macOS; everything else in
/// Inter, which comes with the app (see `fonts/`); code in Menlo.
const SERIF: &str = "Charter";
const SANS: &str = "Inter";
const MONO: &str = "Menlo";

/// The column the chat and the settings are set in, and the strip along the
/// top that holds the window's buttons and the chats.
const COLUMN: f32 = 760.;
const BAR: f32 = 40.;

/// The colours of the tiny-lm palette: warm greys on a plain ground, and a
/// soft hue for each kind of thing. Slate blue is you and the controls, teal
/// the model's lookups (tool calls), ochre its thinking, sage a live server,
/// rose what went wrong.
struct Theme {
    ground: Hsla,
    panel: Hsla,
    panel_line: Hsla,
    card: Hsla,
    box_fill: Hsla,
    box_line: Hsla,
    ink: Hsla,
    ink_soft: Hsla,
    muted: Hsla,
    blue: Hsla,
    blue_fill: Hsla,
    teal: Hsla,
    ochre: Hsla,
    sage: Hsla,
    rose: Hsla,
    rose_fill: Hsla,
    shadow: Hsla,
}

impl Theme {
    fn of(window: &Window) -> Self {
        let dark = matches!(window.appearance(), WindowAppearance::Dark | WindowAppearance::VibrantDark);
        let colours = if dark {
            [
                0x191918, 0x20201e, 0x3a3a37, 0x242422, 0x262624, 0x4a4a46, 0xecebe6, 0xc4c4bd, 0xa3a39d, 0xa9b8d6,
                0x232a38, 0x9ac2c5, 0xd6b87a, 0x7aa887, 0xd6a6b1, 0x33252a,
            ]
        } else {
            [
                0xffffff, 0xfafaf8, 0xdcdcd6, 0xffffff, 0xf4f4f2, 0xc4c4bf, 0x2f2f2f, 0x555555, 0x6b6b6b, 0x4f6285,
                0xeef1f7, 0x4a7276, 0x8a6b30, 0x6f9a7c, 0x8a5561, 0xf4e7ea,
            ]
        };
        let [ground, panel, panel_line, card, box_fill, box_line, ink, ink_soft, muted, blue, blue_fill, teal, ochre, sage, rose, rose_fill] =
            colours.map(|hex| Hsla::from(rgb(hex)));
        let shadow = gpui::black().opacity(if dark { 0.4 } else { 0.08 });
        Theme {
            ground,
            panel,
            panel_line,
            card,
            box_fill,
            box_line,
            ink,
            ink_soft,
            muted,
            blue,
            blue_fill,
            teal,
            ochre,
            sage,
            rose,
            rose_fill,
            shadow,
        }
    }
}

/// Where `nibble serve` is, from the same config file the CLI reads.
#[derive(Clone)]
struct Server {
    url: String,
    token: String,
}

impl Server {
    fn load() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let settings = settings::load();
        let setting = |key: &str| settings.get(key).and_then(Value::as_str).unwrap_or_default().to_string();

        let mut token = var("NIBBLE_TOKEN").unwrap_or_else(|| setting("token"));
        if token.is_empty() && !setting("token_file").is_empty() {
            let file = setting("token_file");
            let file = match (file.strip_prefix("~/"), var("HOME")) {
                (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
                _ => PathBuf::from(&file),
            };
            token = std::fs::read_to_string(file).unwrap_or_default().trim().to_string();
        }
        let url = var("NIBBLE_URL").unwrap_or_else(|| setting("url"));
        let url = if url.is_empty() { "http://127.0.0.1:8765".to_string() } else { url };
        Server { url, token }
    }

    fn agent(&self, seconds: u64) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_recv_response(Some(Duration::from_secs(seconds)))
            .http_status_as_error(false)
            .build()
            .into()
    }

    /// The model the server runs and the others it offers, if it will say.
    /// With the token it also says which settings it uses.
    fn models(&self) -> Option<Models> {
        let mut get = self.agent(3).get(format!("{}/info", self.url));
        if !self.token.is_empty() {
            get = get.header("Authorization", format!("Bearer {}", self.token));
        }
        let mut response = get.call().ok()?;
        Models::from(&serde_json::from_str(&response.body_mut().read_to_string().ok()?).ok()?)
    }

    /// Ask the server to run another model from the next message on.
    fn switch(&self, model: &str) -> Result<Models, String> {
        let mut post = self.agent(10).post(format!("{}/model", self.url));
        if !self.token.is_empty() {
            post = post.header("Authorization", format!("Bearer {}", self.token));
        }
        let mut response = post.send_json(json!({ "model": model })).map_err(|e| e.to_string())?;
        let body = response.body_mut().read_to_string().unwrap_or_default();
        match response.status().as_u16() {
            200 => serde_json::from_str(&body).ok().as_ref().and_then(Models::from).ok_or("a bad answer".into()),
            401 => Err("the server needs a token; set it in Settings".into()),
            _ => Err(body.trim().to_string()),
        }
    }
}

/// From the server's /info.
#[derive(Default)]
struct Models {
    current: String,
    all: Vec<String>,
    /// The settings the server really uses, for the settings page to show
    /// in place of its default hints.
    in_use: Map<String, Value>,
}

impl Models {
    fn from(info: &Value) -> Option<Self> {
        let current = info["model"].as_str().filter(|name| !name.is_empty())?.to_string();
        let all = info["models"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
        let in_use = info["settings"].as_object().cloned().unwrap_or_default();
        Some(Models { current, all, in_use })
    }
}

enum Event {
    Text(String),
    Tool(String),
    Error(String),
}

/// One chat turn, on a plain thread: the request blocks, and GPUI's own
/// executors should not.
fn ask(server: &Server, body: &Value, stop: &AtomicBool, events: &mpsc::UnboundedSender<Event>) -> Result<(), String> {
    // Long, because a cold start has to load the model first.
    let mut post = server.agent(300).post(format!("{}/chat", server.url));
    if !server.token.is_empty() {
        post = post.header("Authorization", format!("Bearer {}", server.token));
    }
    let mut response =
        post.send_json(body).map_err(|e| format!("No answer from nibble serve at {}: {e}", server.url))?;
    match response.status().as_u16() {
        200 => {}
        401 => return Err("The server needs a token. Set the access token in Settings.".into()),
        _ => return Err(response.body_mut().read_to_string().unwrap_or_default().trim().to_string()),
    }
    for line in BufReader::new(response.body_mut().as_reader()).lines() {
        // Dropping the response closes the connection, which is what stops the server.
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let line = line.map_err(|e| e.to_string())?;
        let Some(data) = line.strip_prefix("data: ") else { continue };
        let data: Value = serde_json::from_str(data).map_err(|e| e.to_string())?;
        let event = if let Some(text) = data["text"].as_str() {
            Event::Text(text.to_string())
        } else if let Some(tool) = data["tool"].as_str() {
            Event::Tool(tool.to_string())
        } else if let Some(error) = data["error"].as_str() {
            Event::Error(error.to_string())
        } else {
            continue;
        };
        if events.unbounded_send(event).is_err() {
            break;
        }
    }
    Ok(())
}

#[derive(PartialEq)]
enum View {
    Chat,
    Settings,
}

struct Nibble {
    server: Server,
    models: Models,
    /// Whether the model list is open, and what went wrong with the last switch.
    picking: bool,
    model_error: Option<String>,
    /// Whether the pointer is on the model button, whose own click opens and
    /// closes the list, so that a click there doesn't also count as outside.
    on_picker: bool,
    view: View,
    input: Entity<TextInput>,
    chat: Chat,
    /// The open chat's text, read-only so that it can be selected, by turn,
    /// part and block. A turn's own message is part `usize::MAX`.
    texts: HashMap<(usize, usize, usize), Entity<TextInput>>,
    chats: Vec<Entry>,
    scroll: ScrollHandle,
    /// Set while a reply is arriving. Raising the flag stops it.
    running: Option<Arc<AtomicBool>>,
    /// Files dropped on the window, to go with the next message, and why
    /// the last one dropped could not.
    attached: Vec<Attached>,
    attach_error: Option<String>,
    /// Goes up whenever the open chat changes, so a reply that is still
    /// arriving for the old one is dropped instead of landing in the new one.
    epoch: usize,

    // The settings page: one text field per setting, and for a choice the
    // value picked. A message from the last save, and whether it is an error.
    fields: Vec<Entity<TextInput>>,
    choices: Vec<String>,
    /// What an empty field shows: the value the server uses, or a hint.
    hints: Vec<String>,
    notice: Option<(bool, String)>,
}

impl Nibble {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextInput::multiline("Ask something small", 10, cx));
        window.focus(&input.focus_handle(cx));
        // Follow the system when it switches between light and dark.
        cx.observe_window_appearance(window, |_, _, cx| cx.notify()).detach();
        let fields = FIELDS.iter().map(|field| cx.new(|cx| TextInput::new(field.hint, cx))).collect();
        let mut nibble = Nibble {
            server: Server::load(),
            models: Models::default(),
            picking: false,
            model_error: None,
            on_picker: false,
            view: View::Chat,
            input,
            chat: Chat::new(),
            texts: HashMap::new(),
            chats: store::list(),
            scroll: ScrollHandle::new(),
            running: None,
            attached: Vec::new(),
            attach_error: None,
            epoch: 0,
            fields,
            choices: vec![String::new(); FIELDS.len()],
            hints: FIELDS.iter().map(|field| field.hint.to_string()).collect(),
            notice: None,
        };
        nibble.find_model(cx);
        nibble
    }

    /// Ask the server which models it has, off the main thread, for the header.
    fn find_model(&mut self, cx: &mut Context<Self>) {
        let server = self.server.clone();
        self.update_models(cx, move || Ok(server.models().unwrap_or_default()));
    }

    /// Run `get` on a plain thread, and show the models it returns.
    fn update_models(&mut self, cx: &mut Context<Self>, get: impl FnOnce() -> Result<Models, String> + Send + 'static) {
        let (send, mut receive) = mpsc::unbounded();
        std::thread::spawn(move || {
            let _ = send.unbounded_send(get());
        });
        cx.spawn(async move |this, cx| {
            if let Some(outcome) = receive.next().await {
                let _ = this.update(cx, |nibble, cx| {
                    match outcome {
                        Ok(models) => nibble.models = models,
                        Err(error) => nibble.model_error = Some(error),
                    }
                    if nibble.view == View::Settings {
                        nibble.show_hints(cx);
                    }
                    nibble.trace();
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn toggle_models(&mut self, cx: &mut Context<Self>) {
        self.picking = !self.picking;
        if self.picking {
            // Look again, for a model downloaded since the window opened.
            self.find_model(cx);
        }
        cx.notify();
    }

    fn pick_model(&mut self, model: String, cx: &mut Context<Self>) {
        self.picking = false;
        self.model_error = None;
        if model == self.models.current {
            return cx.notify();
        }
        let server = self.server.clone();
        self.update_models(cx, move || server.switch(&model));
        cx.notify();
    }

    fn stop(&mut self) {
        if let Some(stop) = self.running.take() {
            stop.store(true, Ordering::Relaxed);
        }
        self.epoch += 1;
    }

    fn submit(&mut self, _: &Submit, window: &mut Window, cx: &mut Context<Self>) {
        if self.view == View::Settings {
            return self.save_settings(window, cx);
        }
        // Enter while a reply is arriving stops it, like the page's Stop button.
        if self.running.is_some() {
            self.stop();
            self.keep();
            cx.notify();
            return;
        }
        if self.input.read(cx).text().trim().is_empty() {
            return;
        }
        let text = self.input.update(cx, |input, cx| input.take(cx));
        let files: String = self.attached.drain(..).map(|file| file.block).collect();
        self.attach_error = None;
        self.say(&format!("{}{files}", text.trim()), cx);
    }

    /// Attach files dropped on the window to the next message, each whole,
    /// as long as they fit in what the server keeps of a conversation.
    fn attach(&mut self, paths: &[PathBuf], window: &mut Window, cx: &mut Context<Self>) {
        let budget = self
            .models
            .in_use
            .get("input_chars")
            .or(settings::load().get("input_chars"))
            .and_then(Value::as_u64)
            .map_or(24_000, |n| n as usize);
        self.attach_error = None;
        for path in paths {
            if self.attached.iter().any(|file| file.path == attach::shown(path)) {
                continue;
            }
            let used: usize = self.attached.iter().map(|file| file.block.len()).sum();
            match attach::read(path, budget.saturating_sub(used)) {
                Ok(file) => self.attached.push(file),
                Err(error) => self.attach_error = Some(error),
            }
        }
        if self.view == View::Settings {
            self.view = View::Chat;
        }
        window.focus(&self.input.focus_handle(cx));
        cx.notify();
    }

    fn detach(&mut self, n: usize, cx: &mut Context<Self>) {
        if n < self.attached.len() {
            self.attached.remove(n);
        }
        self.attach_error = None;
        cx.notify();
    }

    fn say(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        // The server keeps no history, so send all of it each time.
        let mut messages = Vec::new();
        for turn in &self.chat.turns {
            messages.push(json!({ "role": "user", "content": turn.user }));
            if !turn.answer().is_empty() {
                messages.push(json!({ "role": "assistant", "content": turn.answer() }));
            }
        }
        messages.push(json!({ "role": "user", "content": text }));
        self.chat.turns.push(Turn { user: text.to_string(), parts: Vec::new() });
        // Attached files are the whole task, so a chat with any gets no file
        // tools, as with `nibble -f`: an eager model goes looking otherwise.
        let mut body = json!({ "messages": messages });
        if self.chat.turns.iter().any(|turn| !attach::split(&turn.user).1.is_empty()) {
            body["tools"] = json!(false);
        }

        let stop = Arc::new(AtomicBool::new(false));
        self.running = Some(stop.clone());
        let epoch = self.epoch;
        let (send, mut receive) = mpsc::unbounded();
        let server = self.server.clone();
        std::thread::spawn(move || {
            if let Err(error) = ask(&server, &body, &stop, &send) {
                let _ = send.unbounded_send(Event::Error(error));
            }
        });
        cx.spawn(async move |this, cx| {
            while let Some(event) = receive.next().await {
                let live = this.update(cx, |nibble, cx| nibble.epoch == epoch && nibble.apply(event, cx));
                if !matches!(live, Ok(true)) {
                    return;
                }
            }
            let _ = this.update(cx, |nibble, cx| {
                if nibble.epoch == epoch {
                    nibble.running = None;
                    nibble.keep();
                    nibble.trace();
                    cx.notify();
                }
            });
        })
        .detach();
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn apply(&mut self, event: Event, cx: &mut Context<Self>) -> bool {
        let Some(turn) = self.chat.turns.last_mut() else { return false };
        match (event, turn.parts.last_mut()) {
            (Event::Text(more), Some(Part::Text(text))) => text.push_str(&more),
            (Event::Text(text), _) => turn.parts.push(Part::Text(text)),
            (Event::Tool(tool), _) => turn.parts.push(Part::Tool(tool)),
            (Event::Error(error), _) => turn.parts.push(Part::Error(error)),
        }
        self.scroll.scroll_to_bottom();
        cx.notify();
        true
    }

    /// Save the open chat and bring the list up to date.
    fn keep(&mut self) {
        if let Err(error) = self.chat.save() {
            eprintln!("nibble-gui: can't save the chat: {error}");
        }
        self.chats = store::list();
    }

    /// For checking the app from a terminal, where nobody can see the window.
    fn trace(&self) {
        if std::env::var_os("NIBBLE_GUI_TRACE").is_none() {
            return;
        }
        eprintln!("chats saved: {}, model: {} of {:?}", self.chats.len(), self.models.current, self.models.all);
        if let Some(error) = &self.model_error {
            eprintln!("model error: {error}");
        }
        for part in self.chat.turns.last().map(|turn| turn.parts.as_slice()).unwrap_or_default() {
            match part {
                Part::Text(text) => eprintln!("reply: {text}"),
                Part::Tool(tool) => eprintln!("tool: {tool}"),
                Part::Error(error) => eprintln!("error: {error}"),
            }
        }
    }

    fn show_chat(&mut self, chat: Chat, window: &mut Window, cx: &mut Context<Self>) {
        self.stop();
        self.keep();
        self.chat = chat;
        self.texts.clear();
        self.view = View::Chat;
        self.scroll.scroll_to_bottom();
        window.focus(&self.input.focus_handle(cx));
        cx.notify();
    }

    fn new_chat(&mut self, _: &NewChat, window: &mut Window, cx: &mut Context<Self>) {
        self.show_chat(Chat::new(), window, cx);
    }

    fn open_chat(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(chat) = Chat::load(id) {
            self.show_chat(chat, window, cx);
        }
    }

    fn delete_chat(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        store::delete(id);
        if self.chat.id == id {
            self.stop();
            // Empty it first, or closing it would save it straight back.
            self.chat = Chat::new();
            self.new_chat(&NewChat, window, cx);
        }
        self.chats = store::list();
        cx.notify();
    }

    fn open_settings(&mut self, _: &OpenSettings, _: &mut Window, cx: &mut Context<Self>) {
        let saved = settings::load();
        for (n, field) in FIELDS.iter().enumerate() {
            let shown = settings::show(field, &saved);
            self.fields[n].update(cx, |input, cx| input.set_text(&shown, cx));
            self.choices[n] = shown;
        }
        self.notice = settings::managed()
            .then(|| (false, "This file is managed by Nix, so it can't be changed from here.".to_string()));
        self.view = View::Settings;
        self.show_hints(cx);
        // Ask again: the server may have switched models, and with them presets.
        self.find_model(cx);
        cx.notify();
    }

    /// Show in each empty field what is used in its place: the server's own
    /// value when it says, else the hint written here.
    fn show_hints(&mut self, cx: &mut Context<Self>) {
        let saved = settings::load();
        for (n, field) in FIELDS.iter().enumerate() {
            let hint = match field.kind {
                Kind::Secret if saved.contains_key(field.key) => "set; type to replace it".to_string(),
                _ => settings::in_use(field, &self.models.in_use).unwrap_or_else(|| field.hint.to_string()),
            };
            self.fields[n].update(cx, |input, _| input.set_placeholder(&hint));
            self.hints[n] = hint;
        }
        cx.notify();
    }

    fn save_settings(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        // Start from the file as it is now, so keys with no field here survive.
        let mut saved = settings::load();
        let mut outcome = Ok(());
        for (n, field) in FIELDS.iter().enumerate() {
            let typed = match field.kind {
                Kind::Choice(_) => self.choices[n].clone(),
                _ => self.fields[n].read(cx).text(),
            };
            outcome = outcome.and(settings::apply(field, &typed, &mut saved));
        }
        self.notice = Some(match outcome.and_then(|()| settings::save(&saved)) {
            Ok(()) => (false, "Saved. Restart nibble serve for the server's own settings to take effect.".to_string()),
            Err(error) => (true, error),
        });
        // The address and token are ours too, so use them at once.
        self.server = Server::load();
        self.find_model(cx);
        cx.notify();
    }

    /// Step a choice to its next value: unset, then each option in turn.
    fn cycle(&mut self, n: usize, cx: &mut Context<Self>) {
        let Kind::Choice(options) = FIELDS[n].kind else { return };
        let at = options.iter().position(|option| *option == self.choices[n]);
        self.choices[n] = match at {
            None => options[0].to_string(),
            Some(at) if at + 1 < options.len() => options[at + 1].to_string(),
            Some(_) => String::new(),
        };
        cx.notify();
    }

    /// Set a choice to one value, or to unset with an empty one.
    fn choose(&mut self, n: usize, value: &str, cx: &mut Context<Self>) {
        self.choices[n] = value.to_string();
        cx.notify();
    }
}

impl Nibble {
    /// Walk through what the buttons do, with pauses so that each state is
    /// drawn, then quit. Nobody can click a window from a terminal or in CI.
    /// It saves settings and deletes a chat, so run it with XDG_CONFIG_HOME
    /// and XDG_DATA_HOME pointing somewhere disposable.
    fn self_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let pause = Duration::from_millis(400);
            let field = |key: &str| FIELDS.iter().position(|field| field.key == key).unwrap();

            cx.background_executor().timer(pause).await;
            this.update_in(cx, |nibble, window, cx| {
                nibble.open_settings(&OpenSettings, window, cx);
                nibble.fields[field("idle_seconds")].update(cx, |input, cx| input.set_text("123", cx));
                nibble.fields[field("roots")].update(cx, |input, cx| input.set_text("~/a, ~/b", cx));
                nibble.cycle(field("tools"), cx);
                nibble.cycle(field("tools"), cx);
            })?;
            cx.background_executor().timer(pause).await;
            this.update_in(cx, |nibble, window, cx| {
                nibble.submit(&Submit, window, cx);
                eprintln!("selftest save: {:?}", nibble.notice);
            })?;
            cx.background_executor().timer(pause).await;
            this.update_in(cx, |nibble, window, cx| {
                let Some(id) = nibble.chats.first().map(|entry| entry.id.clone()) else {
                    return eprintln!("selftest: no saved chats to open");
                };
                nibble.open_chat(&id, window, cx);
                eprintln!("selftest open: {} turn(s), view is chat: {}", nibble.chat.turns.len(), nibble.view == View::Chat);
            })?;
            cx.background_executor().timer(pause).await;
            this.update_in(cx, |nibble, window, cx| {
                let (before, id) = (nibble.chats.len(), nibble.chat.id.clone());
                nibble.delete_chat(&id, window, cx);
                eprintln!("selftest delete: {before} -> {} chats, open chat is empty: {}", nibble.chats.len(), nibble.chat.turns.is_empty());
            })?;
            cx.background_executor().timer(pause).await;
            this.update_in(cx, |nibble, _, cx| {
                nibble.toggle_models(cx);
                eprintln!("selftest models open: {}", nibble.picking);
                let other = nibble.models.all.iter().find(|model| **model != nibble.models.current).cloned();
                match other {
                    Some(other) => nibble.pick_model(other, cx),
                    None => eprintln!("selftest: the server offers no other model"),
                }
            })?;
            cx.background_executor().timer(Duration::from_secs(2)).await;
            this.update_in(cx, |_, _, cx| {
                eprintln!("selftest done");
                cx.quit();
            })
        })
        .detach();
    }
}

/// Text that can be selected, kept from one frame to the next so that the
/// selection is too, and brought up to date as a reply grows.
fn selectable(
    texts: &mut HashMap<(usize, usize, usize), Entity<TextInput>>,
    key: (usize, usize, usize),
    text: &str,
    cx: &mut App,
) -> Entity<TextInput> {
    let entity = texts.entry(key).or_insert_with(|| cx.new(TextInput::read_only)).clone();
    entity.update(cx, |input, cx| input.show(text, cx));
    entity
}

/// Just enough Markdown for a chat: fenced code gets its own block, with its
/// language and a button that copies it.
fn reply(
    texts: &mut HashMap<(usize, usize, usize), Entity<TextInput>>,
    (turn, part): (usize, usize),
    text: &str,
    theme: &Theme,
    cx: &mut App,
) -> impl IntoElement {
    let blocks = text.split("```").enumerate().filter(|(_, block)| !block.trim().is_empty()).map(|(n, block)| {
        if n % 2 == 1 {
            // The first line of a fence is its language tag.
            let (language, code) = block.split_once('\n').unwrap_or(("", block));
            let code = code.trim_end().to_string();
            let language = if language.trim().is_empty() { "code" } else { language.trim() };
            let shown = selectable(texts, (turn, part, n), &code, cx);
            div()
                .flex()
                .flex_col()
                .rounded(px(8.))
                .border_1()
                .border_color(theme.box_line)
                .bg(theme.box_fill)
                .overflow_hidden()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px_3()
                        .py(px(4.))
                        .border_b_1()
                        .border_color(theme.box_line)
                        .text_size(px(11.))
                        .text_color(theme.muted)
                        .child(div().font_family(MONO).child(SharedString::from(language.to_string())))
                        .child(link(SharedString::from(format!("code-{turn}-{part}-{n}")), "Copy", theme).on_click(
                            move |_, _, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(code.clone())),
                        )),
                )
                .child(div().px_3().py_2().font_family(MONO).text_size(px(12.)).line_height(px(19.)).child(shown))
        } else {
            div().line_height(px(22.)).child(selectable(texts, (turn, part, n), block.trim(), cx))
        }
    });
    div().flex().flex_col().gap_3().children(blocks)
}

/// A tool call as a line of the log: what the model did, and to what.
fn tool_line(tool: &str, theme: &Theme) -> Div {
    let (name, about) = tool.split_once(' ').unwrap_or((tool, ""));
    let verb = match name {
        "read_file" => "Read",
        "list_dir" => "Listed",
        "search" => "Searched",
        "ask_claude" => "Asked Claude",
        other => other,
    };
    div()
        .flex()
        .items_center()
        .gap_2()
        .text_size(px(12.5))
        .child(div().w(px(12.)).flex_shrink_0().text_color(theme.teal).child("→"))
        .child(div().flex_shrink_0().text_color(theme.muted).child(SharedString::from(verb.to_string())))
        .when(!about.is_empty() && about != ".", |line| {
            line.child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_family(MONO)
                    .text_size(px(11.5))
                    .text_color(theme.teal)
                    .child(SharedString::from(about.to_string())),
            )
        })
}

fn capital(word: &str) -> String {
    let mut chars = word.chars();
    chars.next().map_or(String::new(), |first| first.to_uppercase().chain(chars).collect())
}

/// Small grey text that acts when clicked.
fn link(id: impl Into<gpui::ElementId>, label: impl Into<SharedString>, theme: &Theme) -> Stateful<Div> {
    let hover = theme.ink;
    div()
        .id(id)
        .text_size(px(11.5))
        .text_color(theme.muted)
        .cursor_pointer()
        .hover(move |style| style.text_color(hover))
        .child(label.into())
}

/// A file that goes with a message, by name.
fn chip(path: &str, theme: &Theme) -> Div {
    div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap_1()
        .max_w(px(240.))
        .h(px(22.))
        .px_2()
        .rounded(px(6.))
        .border_1()
        .border_color(theme.box_line)
        .bg(theme.box_fill)
        .font_family(MONO)
        .text_size(px(11.5))
        .text_color(theme.ink_soft)
        .child(div().min_w_0().truncate().child(SharedString::from(attach::name(path).to_string())))
}

/// Centre the content of a pane in a column of readable width.
fn column(content: impl IntoElement) -> Div {
    div().w_full().flex().flex_col().items_center().child(div().w_full().max_w(px(COLUMN)).child(content))
}

impl Nibble {
    /// The strip along the top: room for the window's own buttons, then a tab
    /// for each saved chat, newest first, and one for a new chat.
    fn tabs(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let on_chat = self.view == View::Chat;
        let unsaved = !self.chats.iter().any(|entry| entry.id == self.chat.id);
        let tab = |id: gpui::ElementId, title: SharedString, current: bool| {
            let (hover, ground, line) = (theme.ink, theme.ground, theme.panel_line);
            div()
                .id(id)
                .group("tab")
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap_1()
                .max_w(px(200.))
                .h(px(28.))
                .pl_3()
                .pr_1()
                .rounded(px(6.))
                .border_1()
                .cursor_pointer()
                .text_size(px(12.5))
                .when(current, |tab| tab.bg(ground).border_color(line).text_color(theme.ink))
                .when(!current, |tab| {
                    tab.border_color(gpui::transparent_black()).text_color(theme.muted).hover(move |style| style.text_color(hover))
                })
                .child(div().min_w_0().truncate().child(title))
        };

        let mut tabs: Vec<gpui::AnyElement> = Vec::new();
        if unsaved {
            tabs.push(tab("tab-new".into(), "New chat".into(), on_chat).pr_3().into_any_element());
        }
        for (n, entry) in self.chats.iter().enumerate() {
            let (open_id, delete_id) = (entry.id.clone(), entry.id.clone());
            let current = on_chat && entry.id == self.chat.id;
            let (muted, ink, fill) = (theme.muted, theme.ink, theme.box_fill);
            tabs.push(
                tab(("tab", n).into(), SharedString::from(entry.title.clone()), current)
                    .on_click(cx.listener(move |nibble, _, window, cx| nibble.open_chat(&open_id, window, cx)))
                    .child(
                        // Only there while the pointer is on the tab.
                        div()
                            .id(("delete", n))
                            .size(px(16.))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.))
                            .text_color(gpui::transparent_black())
                            .group_hover("tab", move |style| style.text_color(muted))
                            .hover(move |style| style.bg(fill).text_color(ink))
                            .on_click(cx.listener(move |nibble, _, window, cx| {
                                // Not also a click on the tab, which would open the chat.
                                cx.stop_propagation();
                                nibble.delete_chat(&delete_id, window, cx);
                            }))
                            .child("×"),
                    )
                    .into_any_element(),
            );
        }

        let (hover, ink) = (theme.box_fill, theme.ink);
        div()
            .id("tabs-bar")
            .h(px(BAR))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_1()
            // The window's three buttons sit at the left of this strip.
            .pl(px(84.))
            .pr_2()
            .bg(theme.panel)
            .border_b_1()
            .border_color(theme.panel_line)
            .on_click(|event, window, _| {
                if event.click_count() == 2 {
                    window.titlebar_double_click();
                }
            })
            .child(div().id("tabs").min_w_0().flex().items_center().gap_1().overflow_x_scroll().children(tabs))
            .child(
                div()
                    .id("new")
                    .flex_shrink_0()
                    .size(px(26.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.))
                    .text_size(px(17.))
                    .text_color(theme.muted)
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover).text_color(ink))
                    .on_click(cx.listener(|nibble, _, window, cx| {
                        cx.stop_propagation();
                        nibble.new_chat(&NewChat, window, cx)
                    }))
                    .child("+"),
            )
    }

    /// The strip along the bottom: the server and its model, and the way
    /// to the settings.
    fn status_bar(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let reached = !self.models.current.is_empty();
        let address = self.server.url.trim_start_matches("http://").trim_start_matches("https://").to_string();
        let ink = theme.ink;
        div()
            .h(px(28.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .bg(theme.panel)
            .border_t_1()
            .border_color(theme.panel_line)
            .text_size(px(11.5))
            .text_color(theme.muted)
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap(px(6.))
                    .child(div().size(px(7.)).rounded_full().bg(if reached { theme.sage } else { theme.box_line }))
                    .child(SharedString::from(if reached {
                        format!("nibble serve · {address}")
                    } else {
                        format!("no answer from {address}")
                    })),
            )
            .child(self.model_picker(theme, cx))
            .when_some(self.model_error.clone(), |bar, error| {
                bar.child(div().min_w_0().truncate().text_color(theme.rose).child(error))
            })
            .child(div().flex_1())
            .child(div().flex_shrink_0().child("⌘N new chat"))
            .child(
                div()
                    .id("settings")
                    .flex_shrink_0()
                    .cursor_pointer()
                    .hover(move |style| style.text_color(ink))
                    .when(self.view == View::Settings, |button| button.text_color(theme.ink).font_weight(gpui::FontWeight::MEDIUM))
                    .on_click(cx.listener(|nibble, _, window, cx| nibble.open_settings(&OpenSettings, window, cx)))
                    .child("Settings ⌘,"),
            )
    }

    /// One turn: your message after a prompt mark, then what the model did,
    /// one line per tool call, and what it said, set in under your message.
    fn turn(
        texts: &mut HashMap<(usize, usize, usize), Entity<TextInput>>,
        n: usize,
        turn: &Turn,
        waiting: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let parts: Vec<gpui::AnyElement> = turn
            .parts
            .iter()
            .enumerate()
            .map(|(k, part)| match part {
                Part::Tool(tool) => tool_line(tool, theme).into_any_element(),
                Part::Text(text) => div().pt_1().child(reply(texts, (n, k), text, theme, cx)).into_any_element(),
                Part::Error(error) => div()
                    .px_3()
                    .py_2()
                    .rounded(px(8.))
                    .bg(theme.rose_fill)
                    .text_color(theme.rose)
                    .text_size(px(12.5))
                    .child(SharedString::from(error.clone()))
                    .into_any_element(),
            })
            .collect();

        // The files that went with the message show by name, not whole.
        let (asked, files) = attach::split(&turn.user);
        let files: Vec<Div> = files.into_iter().map(|path| chip(path, theme)).collect();
        let answer = turn.answer();
        // The whole reply at once, code blocks and all.
        let copy = (!waiting && !answer.trim().is_empty()).then(|| {
            link(("copy", n), "Copy reply", theme).on_click(cx.listener(move |_, _, _, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(answer.trim().to_string()));
            }))
        });

        div()
            .flex()
            .flex_col()
            .gap_2()
            .when(n > 0, |turn| turn.pt_6().border_t_1().border_color(theme.panel_line))
            .child(
                // Your message is the turn's heading, set in the serif.
                div()
                    .flex()
                    .gap(px(8.))
                    .line_height(px(24.))
                    .child(div().w(px(12.)).flex_shrink_0().text_color(theme.blue).font_family(MONO).child("›"))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_family(SERIF)
                            .text_size(px(17.))
                            .font_weight(gpui::FontWeight::BOLD)
                            .child(selectable(texts, (n, usize::MAX, 0), asked, cx)),
                    ),
            )
            .when(!files.is_empty(), |turn| turn.child(div().pl(px(20.)).flex().flex_wrap().gap_1().children(files)))
            .child(
                div()
                    .pl(px(20.))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(parts)
                    .when(waiting && turn.answer().is_empty(), |log| log.child(div().text_color(theme.ochre).child("Thinking…")))
                    .children(copy.map(|copy| div().pt_2().flex().child(copy))),
            )
    }

    fn chat_view(&mut self, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let log = if self.chat.turns.is_empty() {
            // Nothing said yet: a title page.
            div().id("log").flex_1().flex().flex_col().justify_center().child(column(
                div()
                    .px_8()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .font_family(SERIF)
                            .text_size(px(34.))
                            .line_height(px(40.))
                            .font_weight(gpui::FontWeight::BOLD)
                            .child("nibble"),
                    )
                    // The model loaded, once the server has said.
                    .when(!self.models.current.is_empty(), |page| {
                        page.child(
                            div()
                                .font_family(SERIF)
                                .text_size(px(18.))
                                .line_height(px(24.))
                                .text_color(theme.muted)
                                .child(SharedString::from(self.models.current.clone())),
                        )
                    }),
            ))
        } else {
            let (texts, count, running) = (&mut self.texts, self.chat.turns.len(), self.running.is_some());
            let turns: Vec<_> = self
                .chat
                .turns
                .iter()
                .enumerate()
                .map(|(n, turn)| Self::turn(texts, n, turn, running && n + 1 == count, theme, cx).into_any_element())
                .collect();
            div()
                .id("log")
                .flex_1()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .child(column(div().px_8().pt_6().pb_4().flex().flex_col().gap_6().children(turns)))
        };

        let focused = self.input.focus_handle(cx).is_focused(window);
        let running = self.running.is_some();
        let ready = running || !self.input.read(cx).text().trim().is_empty();
        let (blue, blue_fill) = (theme.blue, theme.blue_fill);
        let (muted, ink) = (theme.muted, theme.ink);
        let files: Vec<_> = self
            .attached
            .iter()
            .enumerate()
            .map(|(n, file)| {
                chip(&file.path, theme).pr(px(2.)).child(
                    div()
                        .id(("detach", n))
                        .size(px(16.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.))
                        .font_family(SANS)
                        .text_size(px(13.))
                        .text_color(muted)
                        .cursor_pointer()
                        .hover(move |style| style.text_color(ink))
                        .on_click(cx.listener(move |nibble, _, _, cx| {
                            cx.stop_propagation();
                            nibble.detach(n, cx);
                        }))
                        .child("×"),
                )
            })
            .collect();
        let composer = div()
            .id("composer")
            .flex()
            .flex_col()
            .gap_2()
            .px(px(14.))
            .pt(px(10.))
            .pb(px(8.))
            .rounded(px(8.))
            .bg(theme.card)
            .border_1()
            .border_color(if focused { theme.blue.opacity(0.6) } else { theme.box_line })
            .shadow(vec![gpui::BoxShadow {
                color: theme.shadow,
                offset: gpui::point(px(0.), px(2.)),
                blur_radius: px(10.),
                spread_radius: px(-2.),
            }])
            .cursor_text()
            .on_click(cx.listener(|nibble, _, window, cx| window.focus(&nibble.input.focus_handle(cx))))
            .when(!files.is_empty(), |composer| composer.child(div().pl(px(20.)).flex().flex_wrap().gap_1().children(files)))
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .line_height(px(22.))
                    .child(div().w(px(12.)).flex_shrink_0().text_color(theme.blue).font_family(MONO).child("›"))
                    .child(div().flex_1().min_w_0().child(self.input.clone())),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .pl(px(20.))
                    .text_size(px(11.5))
                    .text_color(theme.muted)
                    .child(match (running, &self.attach_error) {
                        (true, _) => div().child("↩ stops the reply"),
                        (false, Some(error)) => div().min_w_0().truncate().text_color(theme.rose).child(SharedString::from(error.clone())),
                        (false, None) => div().child("↩ sends · ⇧↩ starts a new line · drop files to attach"),
                    })
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("send")
                            .px_2()
                            .py(px(2.))
                            .rounded(px(6.))
                            .text_size(px(12.5))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(if ready { theme.blue } else { theme.muted })
                            .cursor_pointer()
                            .hover(move |style| style.bg(blue_fill).text_color(blue))
                            .on_click(cx.listener(|nibble, _, window, cx| {
                                cx.stop_propagation();
                                nibble.submit(&Submit, window, cx)
                            }))
                            .child(if running { "Stop" } else { "Send" }),
                    ),
            );

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(log)
            .child(column(div().px_8().pt_1().pb_4().child(composer)))
    }

    /// The model in use, and a list of the others to switch to when the
    /// server offers more than one. The list opens upwards, from the status bar.
    fn model_picker(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let current = &self.models.current;
        if current.is_empty() {
            return div().into_any_element();
        }
        let label = SharedString::from(current.clone());
        if self.models.all.len() < 2 {
            return div().flex_shrink_0().text_color(theme.ink_soft).child(label).into_any_element();
        }
        let hover = theme.box_fill;
        let rows = self.models.all.iter().enumerate().map(|(n, model)| {
            let picked = model.clone();
            let on = model == current;
            div()
                .id(("model", n))
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py(px(5.))
                .rounded(px(6.))
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .on_click(cx.listener(move |nibble, _, _, cx| nibble.pick_model(picked.clone(), cx)))
                .child(div().w(px(12.)).text_color(theme.blue).child(if on { "✓" } else { "" }))
                .child(SharedString::from(model.clone()))
        });
        let menu = div()
            .id("models")
            .occlude()
            .mb_1()
            .p_1()
            .min_w(px(280.))
            .flex()
            .flex_col()
            .bg(theme.card)
            .border_1()
            .border_color(theme.panel_line)
            .rounded(px(8.))
            .shadow_lg()
            .text_size(px(13.))
            .text_color(theme.ink)
            .on_mouse_down_out(cx.listener(|nibble, _, _, cx| {
                if !nibble.on_picker {
                    nibble.picking = false;
                    cx.notify();
                }
            }))
            .children(rows);

        let ink = theme.ink;
        div()
            .flex_shrink_0()
            .when(self.picking, |picker| {
                picker.child(deferred(anchored().anchor(gpui::Corner::BottomLeft).child(menu)))
            })
            .child(
                div()
                    .id("model")
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_color(theme.ink_soft)
                    .cursor_pointer()
                    .hover(move |style| style.text_color(ink))
                    .on_hover(cx.listener(|nibble, hovered: &bool, _, _| nibble.on_picker = *hovered))
                    .on_click(cx.listener(|nibble, _, _, cx| nibble.toggle_models(cx)))
                    .child(label)
                    .child(div().text_size(px(9.)).child("▴")),
            )
            .into_any_element()
    }

    fn settings_view(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let file = settings::path().map_or("no config file: HOME is not set".to_string(), |path| path.display().to_string());
        let rows = FIELDS.iter().enumerate().map(|(n, field)| {
            let control = match field.kind {
                Kind::Choice(options) => {
                    // A segmented control: the default, then each option.
                    let picked = self.choices[n].clone();
                    let segments = [("", "Default")].into_iter().chain(options.iter().map(|option| (*option, *option)));
                    let (card, shadow, hover) = (theme.card, theme.shadow, theme.ink);
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(
                            div()
                                .flex()
                                .p(px(2.))
                                .gap(px(2.))
                                .rounded(px(8.))
                                .bg(theme.box_fill)
                                .border_1()
                                .border_color(theme.panel_line)
                                .children(segments.enumerate().map(|(k, (value, label))| {
                                    let on = picked == value;
                                    div()
                                        .id(("choice", n * 8 + k))
                                        .px_3()
                                        .py(px(3.))
                                        .rounded(px(6.))
                                        .cursor_pointer()
                                        .text_color(if on { theme.ink } else { theme.muted })
                                        .when(on, |segment| {
                                            segment.bg(card).shadow(vec![gpui::BoxShadow {
                                                color: shadow,
                                                offset: gpui::point(px(0.), px(1.)),
                                                blur_radius: px(2.),
                                                spread_radius: px(0.),
                                            }])
                                        })
                                        .when(!on, |segment| segment.hover(move |style| style.text_color(hover)))
                                        .on_click(cx.listener(move |nibble, _, _, cx| nibble.choose(n, value, cx)))
                                        .child(SharedString::from(capital(label)))
                                })),
                        )
                        .when(picked.is_empty(), |row| {
                            row.child(div().text_size(px(12.)).text_color(theme.muted).child(self.hints[n].clone()))
                        })
                        .into_any_element()
                }
                _ => div()
                    .px(px(10.))
                    .py(px(6.))
                    .rounded(px(8.))
                    .bg(theme.card)
                    .border_1()
                    .border_color(theme.box_line)
                    .child(self.fields[n].clone())
                    .into_any_element(),
            };
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .px_5()
                .py_3()
                .when(n > 0, |row| row.border_t_1().border_color(theme.panel_line))
                .child(div().font_weight(gpui::FontWeight::MEDIUM).child(field.label))
                .child(control)
        });

        let about = if settings::load().is_empty() {
            "There is no config file yet, so everything is at its default. Each field shows that default in grey; \
             fill in only what you want to change. These settings are shared with the nibble command."
        } else {
            "An empty field is at its default, shown in grey. These settings are shared with the nibble command."
        };
        let (blue, ground) = (theme.blue, theme.ground);

        div().id("settings-page").flex_1().min_h_0().overflow_y_scroll().child(column(
            div()
                .px_8()
                .pt_8()
                .pb_6()
                .flex()
                .flex_col()
                .gap_5()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .font_family(SERIF)
                                .text_size(px(28.))
                                .line_height(px(34.))
                                .font_weight(gpui::FontWeight::BOLD)
                                .child("Settings"),
                        )
                        .child(
                            div()
                                .font_family(SERIF)
                                .text_size(px(16.))
                                .line_height(px(22.))
                                .text_color(theme.muted)
                                .child(about),
                        )
                        .child(div().font_family(MONO).text_size(px(11.5)).text_color(theme.muted).child(file)),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .rounded(px(12.))
                        .border_1()
                        .border_color(theme.panel_line)
                        .bg(theme.panel)
                        .children(rows),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(
                            div()
                                .id("save")
                                .px_4()
                                .py(px(6.))
                                .rounded(px(8.))
                                .bg(blue)
                                .text_color(ground)
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .cursor_pointer()
                                .hover(move |style| style.bg(blue.opacity(0.88)))
                                .on_click(cx.listener(|nibble, _, window, cx| nibble.save_settings(window, cx)))
                                .child("Save"),
                        )
                        .when_some(self.notice.clone(), |row, (is_error, text)| {
                            row.child(
                                div()
                                    .flex_1()
                                    .text_size(px(12.))
                                    .text_color(if is_error { theme.rose } else { theme.muted })
                                    .child(text),
                            )
                        }),
                ),
        ))
    }
}

impl Render for Nibble {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(window);
        let main = match self.view {
            View::Chat => self.chat_view(&theme, window, cx).into_any_element(),
            View::Settings => self.settings_view(&theme, cx).into_any_element(),
        };
        // After the views, which may have just made some of these.
        for input in self.fields.iter().chain([&self.input]).chain(self.texts.values()) {
            input.update(cx, |input, _| {
                input.dim = theme.muted;
                input.accent = theme.blue;
            });
        }
        div()
            .key_context("Nibble")
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::new_chat))
            .on_action(cx.listener(Self::open_settings))
            .on_drop(cx.listener(|nibble, paths: &ExternalPaths, window, cx| nibble.attach(paths.paths(), window, cx)))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.ground)
            .text_color(theme.ink)
            .font_family(SANS)
            .text_size(px(14.))
            .child(self.tabs(&theme, cx))
            .child(main)
            .child(self.status_bar(&theme, cx))
            // Files from elsewhere are the only thing dragged over the window.
            .when(cx.has_active_drag(), |root| root.child(Self::drop_here(&theme)))
    }
}

impl Nibble {
    /// What the window shows while files are dragged over it.
    fn drop_here(theme: &Theme) -> impl IntoElement {
        div().absolute().top_0().left_0().size_full().p_3().child(
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_1()
                .rounded(px(12.))
                .border_2()
                .border_color(theme.blue.opacity(0.7))
                .bg(theme.blue_fill.opacity(0.92))
                .child(
                    div()
                        .font_family(SERIF)
                        .text_size(px(22.))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(theme.blue)
                        .child("Drop to attach"),
                )
                .child(div().text_size(px(12.5)).text_color(theme.muted).child("Each file goes whole with your next message.")),
        )
    }
}

fn main() {
    // `nibble-gui some question` opens the window and asks straight away.
    let first: Vec<String> = std::env::args().skip(1).collect();
    let first = first.join(" ");

    Application::new().run(move |cx: &mut App| {
        // Inter is not part of macOS, so it comes with the app.
        let fonts = [
            include_bytes!("../fonts/Inter-Regular.ttf").as_slice(),
            include_bytes!("../fonts/Inter-Medium.ttf"),
            include_bytes!("../fonts/Inter-SemiBold.ttf"),
        ];
        if let Err(error) = cx.text_system().add_fonts(fonts.map(Cow::Borrowed).to_vec()) {
            eprintln!("nibble-gui: can't load Inter: {error}");
        }
        input::bind_keys(cx);
        cx.bind_keys([
            KeyBinding::new("enter", Submit, None),
            KeyBinding::new("cmd-n", NewChat, None),
            KeyBinding::new("cmd-,", OpenSettings, None),
            KeyBinding::new("cmd-q", Quit, None),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        // One window is the whole app, so closing it quits.
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(940.), px(740.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            // The page runs up under the title bar, and the window's three
            // buttons sit at the left of the strip of chats.
            titlebar: Some(TitlebarOptions {
                title: Some("nibble".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(14.), px(14.))),
            }),
            window_min_size: Some(size(px(600.), px(420.))),
            ..Default::default()
        };
        cx.open_window(options, |window, cx| {
            cx.new(|cx| {
                let mut nibble = Nibble::new(window, cx);
                if std::env::var_os("NIBBLE_GUI_SELFTEST").is_some() {
                    nibble.self_test(window, cx);
                } else {
                    nibble.say(first.trim(), cx);
                }
                nibble
            })
        })
        .unwrap();
        cx.activate(true);
    });
}
