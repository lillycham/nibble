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
    prelude::*, px, rgb, size,
};
use serde_json::{Map, Value, json};

use input::TextInput;
use settings::{FIELDS, Kind};
use store::{Chat, Entry, Part, Turn};

actions!(nibble, [Submit, NewChat, OpenSettings, Quit]);

/// The same colours as the web page.
struct Theme {
    bg: Hsla,
    fg: Hsla,
    dim: Hsla,
    line: Hsla,
    user: Hsla,
    code: Hsla,
    accent: Hsla,
}

impl Theme {
    fn of(window: &Window) -> Self {
        let colours = match window.appearance() {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => {
                [0x1c1a18, 0xe9e3da, 0x8f867c, 0x35302b, 0x2a2622, 0x26221f, 0xe08a5b]
            }
            _ => [0xfaf8f5, 0x2b2622, 0x8a8078, 0xe6e0d8, 0xefe9e0, 0xf1ece4, 0xb4572e],
        };
        let [bg, fg, dim, line, user, code, accent] = colours.map(|hex| Hsla::from(rgb(hex)));
        Theme { bg, fg, dim, line, user, code, accent }
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

/// Just enough Markdown for a chat: fenced code gets its own block.
fn reply(text: &str, theme: &Theme) -> impl IntoElement {
    div().flex().flex_col().gap_2().children(text.split("```").enumerate().filter(|(_, part)| !part.trim().is_empty()).map(
        |(n, part)| {
            if n % 2 == 1 {
                // The first line of a fence is its language tag.
                let code = part.split_once('\n').map_or(part, |(_, code)| code).trim_end();
                div()
                    .bg(theme.code)
                    .rounded_md()
                    .p_2()
                    .font_family("Menlo")
                    .text_size(px(12.5))
                    .child(SharedString::from(code.to_string()))
            } else {
                div().child(SharedString::from(part.trim().to_string()))
            }
        },
    ))
}

fn button(id: impl Into<gpui::ElementId>, label: impl Into<SharedString>, theme: &Theme) -> Stateful<Div> {
    let hover = theme.dim;
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(theme.line)
        .hover(move |style| style.border_color(hover))
        .cursor_pointer()
        .child(label.into())
}

impl Nibble {
    fn sidebar(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.chats.iter().enumerate().map(|(n, entry)| {
            let (open_id, delete_id) = (entry.id.clone(), entry.id.clone());
            let current = self.view == View::Chat && entry.id == self.chat.id;
            let hover = theme.user;
            div()
                .id(("chat", n))
                .flex()
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .when(current, |row| row.bg(theme.user))
                .hover(move |style| style.bg(hover))
                .on_click(cx.listener(move |nibble, _, window, cx| nibble.open_chat(&open_id, window, cx)))
                .child(div().flex_1().overflow_hidden().truncate().child(SharedString::from(entry.title.clone())))
                .child(
                    div()
                        .id(("delete", n))
                        .px_1()
                        .text_color(theme.dim)
                        .hover(|style| style.text_color(gpui::red()))
                        .on_click(cx.listener(move |nibble, _, window, cx| {
                            // Not also a click on the row, which would open the chat.
                            cx.stop_propagation();
                            nibble.delete_chat(&delete_id, window, cx);
                        }))
                        .child("×"),
                )
        });

        div()
            .w(px(200.))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .gap_2()
            .p_2()
            .border_r_1()
            .border_color(theme.line)
            .text_size(px(13.))
            .child(
                button("new", "New chat", theme)
                    .on_click(cx.listener(|nibble, _, window, cx| nibble.new_chat(&NewChat, window, cx))),
            )
            .child(div().id("chats").flex_1().overflow_y_scroll().flex().flex_col().gap_1().children(rows))
            .child(
                button("settings", "Settings", theme)
                    .when(self.view == View::Settings, |button| button.bg(theme.user))
                    .on_click(cx.listener(|nibble, _, window, cx| nibble.open_settings(&OpenSettings, window, cx))),
            )
    }

    fn chat_view(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let log = div().id("log").flex_1().overflow_y_scroll().track_scroll(&self.scroll).px_4().children(
            self.chat.turns.iter().enumerate().map(|(n, turn)| {
                let answer = turn.answer();
                // The text can't be selected, so offer the whole reply.
                let copy = (!answer.trim().is_empty()).then(|| {
                    div()
                        .id(("copy", n))
                        .text_color(theme.dim)
                        .text_size(px(12.))
                        .cursor_pointer()
                        .hover(|style| style.underline())
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(answer.trim().to_string()));
                        }))
                        .child("Copy")
                });
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .py_2()
                    .child(div().bg(theme.user).rounded_lg().px_3().py_2().child(SharedString::from(turn.user.clone())))
                    .children(turn.parts.iter().map(|part| match part {
                        Part::Text(text) => reply(text, theme).into_any_element(),
                        Part::Tool(tool) => {
                            div().text_color(theme.dim).text_size(px(12.)).child(format!("· {tool}")).into_any_element()
                        }
                        Part::Error(error) => div()
                            .text_color(theme.accent)
                            .text_size(px(12.))
                            .child(SharedString::from(error.clone()))
                            .into_any_element(),
                    }))
                    .children(copy.map(|copy| div().flex().child(copy)))
            }),
        );

        div()
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .py_2()
                    .child(div().font_weight(gpui::FontWeight::BOLD).child("nibble"))
                    .child(self.model_picker(theme, cx))
                    .when_some(self.model_error.clone(), |row, error| {
                        row.child(div().text_color(theme.accent).text_size(px(12.)).child(error))
                    })
                    .child(div().flex_1()),
            )
            .child(log)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_3()
                    .border_t_1()
                    .border_color(theme.line)
                    .child(
                        div()
                            .flex_1()
                            .px_3()
                            .py_2()
                            .rounded_lg()
                            .border_1()
                            .border_color(theme.line)
                            .child(self.input.clone()),
                    )
                    .child(
                        button("send", if self.running.is_some() { "Stop" } else { "Send" }, theme)
                            .on_click(cx.listener(|nibble, _, window, cx| nibble.submit(&Submit, window, cx))),
                    ),
            )
    }

    /// The model in use, and a list of the others to switch to when the
    /// server offers more than one.
    fn model_picker(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let current = &self.models.current;
        let label = if current.is_empty() { "a small local model".to_string() } else { current.clone() };
        if self.models.all.len() < 2 {
            return div().text_color(theme.dim).text_size(px(12.)).child(label).into_any_element();
        }
        let hover = theme.user;
        let rows = self.models.all.iter().enumerate().map(|(n, model)| {
            let picked = model.clone();
            div()
                .id(("model", n))
                .px_2()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .when(model == current, |row| row.font_weight(gpui::FontWeight::BOLD))
                .hover(move |style| style.bg(hover))
                .on_click(cx.listener(move |nibble, _, _, cx| nibble.pick_model(picked.clone(), cx)))
                .child(SharedString::from(model.clone()))
        });
        let menu = div()
            .id("models")
            .occlude()
            .mt_1()
            .p_1()
            .min_w(px(220.))
            .flex()
            .flex_col()
            .bg(theme.bg)
            .border_1()
            .border_color(theme.line)
            .rounded_lg()
            .shadow_md()
            .text_size(px(13.))
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
                    .gap_1()
                    .px_2()
                    .rounded_md()
                    .text_color(theme.dim)
                    .text_size(px(12.))
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover))
                    .on_hover(cx.listener(|nibble, hovered: &bool, _, _| nibble.on_picker = *hovered))
                    .on_click(cx.listener(|nibble, _, _, cx| nibble.toggle_models(cx)))
                    .child(label)
                    .child("▾"),
            )
            .when(self.picking, |picker| {
                picker.child(deferred(anchored().child(menu)))
            })
            .into_any_element()
    }

    fn settings_view(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let file = settings::path().map_or("no config file: HOME is not set".to_string(), |path| path.display().to_string());
        let rows = FIELDS.iter().enumerate().map(|(n, field)| {
            let control = match field.kind {
                Kind::Choice(_) => {
                    let picked = &self.choices[n];
                    let label = if picked.is_empty() { self.hints[n].clone() } else { picked.clone() };
                    button(("choice", n), label, theme)
                        .when(picked.is_empty(), |button| button.text_color(theme.dim))
                        .on_click(cx.listener(move |nibble, _, _, cx| nibble.cycle(n, cx)))
                        .into_any_element()
                }
                _ => div()
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.line)
                    .child(self.fields[n].clone())
                    .into_any_element(),
            };
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_color(theme.dim).text_size(px(12.)).child(field.label))
                .child(control)
        });

        div()
            .id("settings-page")
            .flex_1()
            .h_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3()
            .px_4()
            .py_3()
            .child(div().font_weight(gpui::FontWeight::BOLD).child("Settings"))
            .child(div().text_color(theme.dim).text_size(px(12.)).child(file))
            .child(div().text_color(theme.dim).text_size(px(12.)).child(if settings::load().is_empty() {
                "There is no config file yet, so everything is at its default. Each field shows that default in grey; \
                 fill in only what you want to change. These settings are shared with the nibble command."
            } else {
                "An empty field is at its default, shown in grey. These settings are shared with the nibble command."
            }))
            .children(rows)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        button("save", "Save", theme)
                            .on_click(cx.listener(|nibble, _, window, cx| nibble.save_settings(window, cx))),
                    )
                    .when_some(self.notice.clone(), |row, (is_error, text)| {
                        row.child(
                            div()
                                .flex_1()
                                .text_size(px(12.))
                                .text_color(if is_error { theme.accent } else { theme.dim })
                                .child(text),
                        )
                    }),
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
            View::Chat => self.chat_view(&theme, cx).into_any_element(),
            View::Settings => self.settings_view(&theme, cx).into_any_element(),
        };
        div()
            .key_context("Nibble")
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::new_chat))
            .on_action(cx.listener(Self::open_settings))
            .size_full()
            .flex()
            .bg(theme.bg)
            .text_color(theme.fg)
            .text_size(px(14.))
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

        let bounds = Bounds::centered(None, size(px(820.), px(720.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions { title: Some("nibble".into()), ..Default::default() }),
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
