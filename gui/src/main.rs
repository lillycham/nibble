//! A native chat window for nibble. It holds no model and runs no tools: it
//! talks to `nibble serve` over the same `/chat` endpoint as the web page.
//! It does keep the chats, as files, and it can edit nibble's config file.

mod input;
mod settings;
mod store;

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{
    App, Application, Bounds, Context, Div, Entity, Focusable, Hsla, KeyBinding, ScrollHandle, SharedString, Stateful,
    TitlebarOptions, Window, WindowAppearance, WindowBounds, WindowOptions, actions, anchored, deferred, div,
    point, prelude::*, px, rgb, size,
};
use serde_json::{Map, Value, json};

use input::TextInput;
use settings::{FIELDS, Kind};
use store::{Chat, Entry, Part, Turn};

actions!(nibble, [Submit, NewChat, OpenSettings, Quit]);

/// Code, here and in replies. SF Mono is not one of the fonts macOS offers
/// to apps, so Menlo.
const MONO: &str = "Menlo";

/// The column that the chat and the settings are set in, and how far the
/// sidebar and the bar along the top reach.
const COLUMN: f32 = 680.;
const SIDEBAR: f32 = 224.;
const BAR: f32 = 46.;

/// After macOS's own look: a grey source list beside the page, one indigo
/// accent, and your own messages tinted with it.
struct Theme {
    page: Hsla,
    side: Hsla,
    fg: Hsla,
    side_fg: Hsla,
    dim: Hsla,
    line: Hsla,
    user: Hsla,
    user_fg: Hsla,
    code: Hsla,
    selected: Hsla,
    accent: Hsla,
    on_accent: Hsla,
    error: Hsla,
    ok: Hsla,
    shadow: Hsla,
}

impl Theme {
    fn of(window: &Window) -> Self {
        let dark = matches!(window.appearance(), WindowAppearance::Dark | WindowAppearance::VibrantDark);
        let colours = if dark {
            [
                0x1e1f22, 0x26272b, 0xe8e9ec, 0xc9cad0, 0x8b8e95, 0x303136, 0x2b3260, 0xdfe4ff, 0x17181b,
                0x37383e, 0x8d9bff, 0x14162b, 0xff7b72, 0x32d74b,
            ]
        } else {
            [
                0xffffff, 0xecedf0, 0x1c1d20, 0x3a3d44, 0x83868d, 0xe3e4e8, 0xe6ebff, 0x1d2a5c, 0xf5f6f8,
                0xdcdfe7, 0x4a5bd8, 0xffffff, 0xc4413a, 0x34c759,
            ]
        };
        let [page, side, fg, side_fg, dim, line, user, user_fg, code, selected, accent, on_accent, error, ok] =
            colours.map(|hex| Hsla::from(rgb(hex)));
        let shadow = gpui::black().opacity(if dark { 0.5 } else { 0.12 });
        Theme { page, side, fg, side_fg, dim, line, user, user_fg, code, selected, accent, on_accent, error, ok, shadow }
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
    chats: Vec<Entry>,
    scroll: ScrollHandle,
    /// Set while a reply is arriving. Raising the flag stops it.
    running: Option<Arc<AtomicBool>>,
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
        let input = cx.new(|cx| TextInput::new("Ask something small", cx));
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
            chats: store::list(),
            scroll: ScrollHandle::new(),
            running: None,
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
        let text = self.input.update(cx, |input, cx| input.take(cx));
        self.say(text.trim(), cx);
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

        let stop = Arc::new(AtomicBool::new(false));
        self.running = Some(stop.clone());
        let epoch = self.epoch;
        let (send, mut receive) = mpsc::unbounded();
        let server = self.server.clone();
        std::thread::spawn(move || {
            if let Err(error) = ask(&server, &json!({ "messages": messages }), &stop, &send) {
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

/// Just enough Markdown for a chat: fenced code gets its own block, with its
/// language and a button that copies it.
fn reply(text: &str, id: &str, theme: &Theme) -> impl IntoElement {
    div().flex().flex_col().gap_3().children(text.split("```").enumerate().filter(|(_, part)| !part.trim().is_empty()).map(
        |(n, part)| {
            if n % 2 == 1 {
                // The first line of a fence is its language tag.
                let (language, code) = part.split_once('\n').unwrap_or(("", part));
                let code = code.trim_end().to_string();
                let shown = SharedString::from(code.clone());
                let language = if language.trim().is_empty() { "code" } else { language.trim() };
                div()
                    .flex()
                    .flex_col()
                    .rounded(px(10.))
                    .border_1()
                    .border_color(theme.line)
                    .bg(theme.code)
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .px_3()
                            .py(px(5.))
                            .border_b_1()
                            .border_color(theme.line)
                            .text_size(px(11.5))
                            .text_color(theme.dim)
                            .child(SharedString::from(language.to_string()))
                            .child(link(SharedString::from(format!("{id}-code-{n}")), "Copy", theme).on_click(
                                move |_, _, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(code.clone())),
                            )),
                    )
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .font_family(MONO)
                            .text_size(px(12.))
                            .line_height(px(19.))
                            .child(shown),
                    )
                    .into_any_element()
            } else {
                div().line_height(px(21.)).child(SharedString::from(part.trim().to_string())).into_any_element()
            }
        },
    ))
}

/// A tool call as a chip: what the model did, and to what.
fn tool_chip(tool: &str, theme: &Theme) -> Div {
    let (name, about) = tool.split_once(' ').unwrap_or((tool, ""));
    let verb = match name {
        "read_file" => "Read",
        "list_dir" => "Listed",
        "search" => "Searched",
        "ask_claude" => "Asked Claude",
        other => other,
    };
    let about: String = match about.chars().count() {
        0..=48 => about.to_string(),
        _ => about.chars().take(47).chain(['…']).collect(),
    };
    div()
        .flex()
        .items_center()
        .gap_1()
        .max_w_full()
        .px(px(9.))
        .py(px(2.))
        .rounded_full()
        .bg(theme.code)
        .border_1()
        .border_color(theme.line)
        .text_size(px(12.))
        .text_color(theme.dim)
        .child(SharedString::from(verb.to_string()))
        .when(!about.is_empty() && about != ".", |chip| {
            chip.child(
                div().min_w_0().truncate().font_family(MONO).text_size(px(11.)).text_color(theme.fg).child(about),
            )
        })
}

fn capital(word: &str) -> String {
    let mut chars = word.chars();
    chars.next().map_or(String::new(), |first| first.to_uppercase().chain(chars).collect())
}

/// Small grey text that acts when clicked.
fn link(id: impl Into<gpui::ElementId>, label: impl Into<SharedString>, theme: &Theme) -> Stateful<Div> {
    let hover = theme.fg;
    div()
        .id(id)
        .text_size(px(11.5))
        .text_color(theme.dim)
        .cursor_pointer()
        .hover(move |style| style.text_color(hover))
        .child(label.into())
}

/// A quiet button: no border until the pointer is on it.
fn button(id: impl Into<gpui::ElementId>, label: impl Into<SharedString>, theme: &Theme) -> Stateful<Div> {
    let hover = theme.selected;
    div()
        .id(id)
        .px(px(9.))
        .py(px(3.))
        .rounded(px(6.))
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .child(label.into())
}

/// The strip along the top of a pane, under the window's own buttons, which
/// zooms the window when double-clicked like any title bar.
fn title_bar(id: &'static str, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(BAR))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .border_b_1()
        .border_color(theme.line)
        .on_click(|event, window, _| {
            if event.click_count() == 2 {
                window.titlebar_double_click();
            }
        })
}

/// Centre the content of a pane in a column of readable width.
fn column(content: impl IntoElement) -> Div {
    div().w_full().flex().flex_col().items_center().child(div().w_full().max_w(px(COLUMN)).child(content))
}

impl Nibble {
    fn sidebar(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.chats.iter().enumerate().map(|(n, entry)| {
            let (open_id, delete_id) = (entry.id.clone(), entry.id.clone());
            let current = self.view == View::Chat && entry.id == self.chat.id;
            let (hover, dim, fg, line) = (theme.selected.opacity(0.6), theme.dim, theme.fg, theme.line);
            div()
                .id(("chat", n))
                .group("chat-row")
                .flex()
                .items_center()
                .gap_1()
                .pl_2()
                .pr_1()
                .py(px(4.))
                .rounded(px(7.))
                .cursor_pointer()
                .when(current, |row| row.bg(theme.selected).text_color(theme.fg).font_weight(gpui::FontWeight::MEDIUM))
                .when(!current, |row| row.hover(move |style| style.bg(hover)))
                .on_click(cx.listener(move |nibble, _, window, cx| nibble.open_chat(&open_id, window, cx)))
                .child(div().flex_1().min_w_0().truncate().child(SharedString::from(entry.title.clone())))
                .child(
                    // Only there while the pointer is on the row.
                    div()
                        .id(("delete", n))
                        .size(px(18.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(5.))
                        .text_color(gpui::transparent_black())
                        .group_hover("chat-row", move |style| style.text_color(dim))
                        .hover(move |style| style.bg(line).text_color(fg))
                        .on_click(cx.listener(move |nibble, _, window, cx| {
                            // Not also a click on the row, which would open the chat.
                            cx.stop_propagation();
                            nibble.delete_chat(&delete_id, window, cx);
                        }))
                        .child("×"),
                )
        });

        let reached = !self.models.current.is_empty();
        let address = self.server.url.trim_start_matches("http://").trim_start_matches("https://").to_string();
        let new_hover = theme.selected;

        div()
            .w(px(SIDEBAR))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(theme.side)
            .text_color(theme.side_fg)
            .border_r_1()
            .border_color(theme.line)
            .text_size(px(13.))
            .child(
                // The window's own three buttons sit at the left of this strip.
                div()
                    .id("side-bar")
                    .h(px(BAR))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_end()
                    .px_2()
                    .on_click(|event, window, _| {
                        if event.click_count() == 2 {
                            window.titlebar_double_click();
                        }
                    })
                    .child(
                        div()
                            .id("new")
                            .size(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(7.))
                            .text_size(px(19.))
                            .text_color(theme.dim)
                            .cursor_pointer()
                            .hover(move |style| style.bg(new_hover))
                            .on_click(cx.listener(|nibble, _, window, cx| {
                                cx.stop_propagation();
                                nibble.new_chat(&NewChat, window, cx)
                            }))
                            .child("+"),
                    ),
            )
            .child(
                div()
                    .px_4()
                    .pt_1()
                    .pb_1()
                    .text_size(px(11.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.dim)
                    .child("Chats"),
            )
            .child(
                div()
                    .id("chats")
                    .flex_1()
                    .overflow_y_scroll()
                    .px_2()
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .children(rows)
                    .when(self.chats.is_empty(), |list| {
                        list.child(div().px_2().py_1().text_color(theme.dim).child("Saved chats appear here."))
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .pl_4()
                    .pr_2()
                    .py_2()
                    .border_t_1()
                    .border_color(theme.line)
                    .text_size(px(12.))
                    .text_color(theme.dim)
                    .child(div().size(px(7.)).flex_shrink_0().rounded_full().bg(if reached { theme.ok } else { theme.dim }))
                    .child(div().flex_1().min_w_0().truncate().child(address))
                    .child(
                        button("settings", "Settings", theme)
                            .when(self.view == View::Settings, |button| button.bg(theme.selected).text_color(theme.fg))
                            .on_click(cx.listener(|nibble, _, window, cx| nibble.open_settings(&OpenSettings, window, cx))),
                    ),
            )
    }

    /// One turn: your message, then what the model did and said.
    fn turn(&self, n: usize, turn: &Turn, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let last = n + 1 == self.chat.turns.len();
        let waiting = last && self.running.is_some();

        // Tool calls in a row share a line of chips.
        let mut parts: Vec<gpui::AnyElement> = Vec::new();
        let mut chips: Vec<Div> = Vec::new();
        let flush = |chips: &mut Vec<Div>, parts: &mut Vec<gpui::AnyElement>| {
            if !chips.is_empty() {
                parts.push(div().flex().flex_wrap().gap(px(6.)).children(chips.drain(..)).into_any_element());
            }
        };
        for (k, part) in turn.parts.iter().enumerate() {
            match part {
                Part::Tool(tool) => chips.push(tool_chip(tool, theme)),
                Part::Text(text) => {
                    flush(&mut chips, &mut parts);
                    parts.push(reply(text, &format!("turn-{n}-{k}"), theme).into_any_element());
                }
                Part::Error(error) => {
                    flush(&mut chips, &mut parts);
                    parts.push(
                        div()
                            .px_3()
                            .py_2()
                            .rounded(px(8.))
                            .bg(theme.error.opacity(0.1))
                            .text_color(theme.error)
                            .text_size(px(12.5))
                            .child(SharedString::from(error.clone()))
                            .into_any_element(),
                    );
                }
            }
        }
        flush(&mut chips, &mut parts);
        if waiting && turn.answer().is_empty() {
            parts.push(div().text_color(theme.dim).child("Thinking…").into_any_element());
        }

        let answer = turn.answer();
        // The text can't be selected, so offer the whole reply.
        let copy = (!waiting && !answer.trim().is_empty()).then(|| {
            link(("copy", n), "Copy reply", theme).on_click(cx.listener(move |_, _, _, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(answer.trim().to_string()));
            }))
        });

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div().flex().justify_end().pl_12().child(
                    div()
                        .bg(theme.user)
                        .text_color(theme.user_fg)
                        .px(px(13.))
                        .py(px(8.))
                        .rounded(px(16.))
                        .rounded_br(px(5.))
                        .line_height(px(20.))
                        .child(SharedString::from(turn.user.clone())),
                ),
            )
            .children(parts)
            .children(copy.map(|copy| div().flex().child(copy)))
    }

    fn chat_view(&self, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let model = if self.models.current.is_empty() { "a small local model" } else { self.models.current.as_str() };
        let log = if self.chat.turns.is_empty() {
            // Nothing said yet: a quiet welcome in the middle of the pane.
            div()
                .id("log")
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .px_6()
                .child(
                    div()
                        .text_size(px(28.))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.fg)
                        .child("nibble"),
                )
                .child(
                    div()
                        .text_color(theme.dim)
                        .child(SharedString::from(format!("Ask something small. {model} answers through nibble serve."))),
                )
        } else {
            let turns: Vec<_> =
                self.chat.turns.iter().enumerate().map(|(n, turn)| self.turn(n, turn, theme, cx).into_any_element()).collect();
            div()
                .id("log")
                .flex_1()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .child(column(div().px_6().pt_5().pb_4().flex().flex_col().gap_6().children(turns)))
        };

        let focused = self.input.focus_handle(cx).is_focused(window);
        let running = self.running.is_some();
        let ready = running || !self.input.read(cx).text().trim().is_empty();
        let (accent, dim) = (theme.accent, theme.dim);
        let composer = div()
            .id("composer")
            .flex()
            .flex_col()
            .gap_2()
            .pl(px(14.))
            .pr(px(9.))
            .pt(px(10.))
            .pb(px(9.))
            .rounded(px(14.))
            .bg(theme.page)
            .border_1()
            .border_color(if focused { theme.accent.opacity(0.55) } else { theme.line })
            .shadow(vec![gpui::BoxShadow {
                color: theme.shadow,
                offset: gpui::point(px(0.), px(4.)),
                blur_radius: px(18.),
                spread_radius: px(-6.),
            }])
            .cursor_text()
            .on_click(cx.listener(|nibble, _, window, cx| window.focus(&nibble.input.focus_handle(cx))))
            .child(self.input.clone())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(px(11.5))
                    .text_color(theme.dim)
                    .child(if running { "↩ stops the reply" } else { "↩ sends" })
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("send")
                            .size(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(if ready { accent } else { dim.opacity(0.35) })
                            .text_color(theme.on_accent)
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_size(px(if running { 10. } else { 15. }))
                            .cursor_pointer()
                            .on_click(cx.listener(|nibble, _, window, cx| {
                                cx.stop_propagation();
                                nibble.submit(&Submit, window, cx)
                            }))
                            .child(if running { "■" } else { "↑" }),
                    ),
            );

        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(title_bar("chat-bar", theme).child(self.model_picker(theme, cx)))
            .when_some(self.model_error.clone(), |pane, error| {
                pane.child(
                    div()
                        .px_4()
                        .py_2()
                        .bg(theme.error.opacity(0.1))
                        .text_color(theme.error)
                        .text_size(px(12.))
                        .child(error),
                )
            })
            .child(log)
            .child(column(div().px_6().pt_1().pb_4().child(composer)))
    }

    /// The model in use, and a list of the others to switch to when the
    /// server offers more than one.
    fn model_picker(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let current = &self.models.current;
        let label = if current.is_empty() { "a small local model".to_string() } else { current.clone() };
        if self.models.all.len() < 2 {
            return div().font_weight(gpui::FontWeight::SEMIBOLD).child(label).into_any_element();
        }
        let hover = theme.selected;
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
                .child(div().w(px(12.)).text_color(theme.accent).child(if on { "✓" } else { "" }))
                .child(SharedString::from(model.clone()))
        });
        let menu = div()
            .id("models")
            .occlude()
            .mt_1()
            .p_1()
            .min_w(px(260.))
            .flex()
            .flex_col()
            .bg(theme.page)
            .border_1()
            .border_color(theme.line)
            .rounded(px(10.))
            .shadow_lg()
            .text_size(px(13.))
            .font_weight(gpui::FontWeight::NORMAL)
            .on_mouse_down_out(cx.listener(|nibble, _, _, cx| {
                if !nibble.on_picker {
                    nibble.picking = false;
                    cx.notify();
                }
            }))
            .children(rows);

        div()
            .child(
                div()
                    .id("model")
                    .flex()
                    .items_center()
                    .gap_1()
                    .px(px(10.))
                    .py(px(4.))
                    .rounded(px(7.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover))
                    .when(self.picking, |button| button.bg(hover))
                    .on_hover(cx.listener(|nibble, hovered: &bool, _, _| nibble.on_picker = *hovered))
                    .on_click(cx.listener(|nibble, _, _, cx| {
                        cx.stop_propagation();
                        nibble.toggle_models(cx)
                    }))
                    .child(label)
                    .child(div().text_size(px(10.)).text_color(theme.dim).child("▾")),
            )
            .when(self.picking, |picker| picker.child(deferred(anchored().child(menu))))
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
                    let (page, shadow, hover) = (theme.page, theme.shadow, theme.fg);
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(
                            div().flex().p(px(2.)).gap(px(2.)).rounded(px(8.)).bg(theme.code).border_1().border_color(theme.line).children(
                                segments.enumerate().map(|(k, (value, label))| {
                                    let on = picked == value;
                                    div()
                                        .id(("choice", n * 8 + k))
                                        .px_3()
                                        .py(px(3.))
                                        .rounded(px(6.))
                                        .cursor_pointer()
                                        .text_color(if on { theme.fg } else { theme.dim })
                                        .when(on, |segment| {
                                            segment.bg(page).shadow(vec![gpui::BoxShadow {
                                                color: shadow,
                                                offset: gpui::point(px(0.), px(1.)),
                                                blur_radius: px(2.),
                                                spread_radius: px(0.),
                                            }])
                                        })
                                        .when(!on, |segment| segment.hover(move |style| style.text_color(hover)))
                                        .on_click(cx.listener(move |nibble, _, _, cx| nibble.choose(n, value, cx)))
                                        .child(SharedString::from(capital(label)))
                                }),
                            ),
                        )
                        .when(picked.is_empty(), |row| {
                            row.child(div().text_size(px(11.5)).text_color(theme.dim).child(self.hints[n].clone()))
                        })
                        .into_any_element()
                }
                _ => div()
                    .px(px(10.))
                    .py(px(6.))
                    .rounded(px(8.))
                    .bg(theme.page)
                    .border_1()
                    .border_color(theme.line)
                    .child(self.fields[n].clone())
                    .into_any_element(),
            };
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .px_4()
                .py_3()
                .when(n > 0, |row| row.border_t_1().border_color(theme.line))
                .child(div().font_weight(gpui::FontWeight::MEDIUM).child(field.label))
                .child(control)
        });

        let about = if settings::load().is_empty() {
            "There is no config file yet, so everything is at its default. Each field shows that default in grey; \
             fill in only what you want to change. These settings are shared with the nibble command."
        } else {
            "An empty field is at its default, shown in grey. These settings are shared with the nibble command."
        };
        let (accent, on_accent) = (theme.accent, theme.on_accent);

        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(title_bar("settings-bar", theme).font_weight(gpui::FontWeight::SEMIBOLD).child("Settings"))
            .child(
                div().id("settings-page").flex_1().overflow_y_scroll().child(column(
                    div()
                        .px_6()
                        .py_5()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .text_size(px(12.))
                                .text_color(theme.dim)
                                .child(div().font_family(MONO).text_size(px(11.5)).child(file))
                                .child(about),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .rounded(px(12.))
                                .border_1()
                                .border_color(theme.line)
                                .bg(theme.code)
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
                                        .bg(accent)
                                        .text_color(on_accent)
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .cursor_pointer()
                                        .hover(move |style| style.bg(accent.opacity(0.85)))
                                        .on_click(cx.listener(|nibble, _, window, cx| nibble.save_settings(window, cx)))
                                        .child("Save"),
                                )
                                .when_some(self.notice.clone(), |row, (is_error, text)| {
                                    row.child(
                                        div()
                                            .flex_1()
                                            .text_size(px(12.))
                                            .text_color(if is_error { theme.error } else { theme.dim })
                                            .child(text),
                                    )
                                }),
                        ),
                )),
            )
    }
}

impl Render for Nibble {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(window);
        for input in self.fields.iter().chain([&self.input]) {
            input.update(cx, |input, _| {
                input.dim = theme.dim;
                input.accent = theme.accent;
            });
        }
        let main = match self.view {
            View::Chat => self.chat_view(&theme, window, cx).into_any_element(),
            View::Settings => self.settings_view(&theme, cx).into_any_element(),
        };
        div()
            .key_context("Nibble")
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::new_chat))
            .on_action(cx.listener(Self::open_settings))
            .size_full()
            .flex()
            .bg(theme.page)
            .text_color(theme.fg)
            .text_size(px(13.5))
            .child(self.sidebar(&theme, cx))
            .child(main)
    }
}

fn main() {
    // `nibble-gui some question` opens the window and asks straight away.
    let first: Vec<String> = std::env::args().skip(1).collect();
    let first = first.join(" ");

    Application::new().run(move |cx: &mut App| {
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
            // buttons sit in the sidebar's top strip.
            titlebar: Some(TitlebarOptions {
                title: Some("nibble".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(16.), px(17.))),
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
