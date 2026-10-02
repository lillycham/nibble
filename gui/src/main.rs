//! A native chat window for nibble. It holds no model and runs no tools: it
//! talks to `nibble serve` over the same `/chat` endpoint as the web page.

mod input;

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{
    App, Application, Bounds, Context, Entity, Focusable, Hsla, KeyBinding, ScrollHandle, SharedString,
    TitlebarOptions, Window, WindowAppearance, WindowBounds, WindowOptions, actions, div, prelude::*, px, rgb, size,
};
use serde_json::{Value, json};

use input::TextInput;

actions!(nibble, [Submit, NewChat, Quit]);

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
        let file = var("NIBBLE_CONFIG")
            .map(PathBuf::from)
            .or_else(|| var("XDG_CONFIG_HOME").map(|dir| PathBuf::from(dir).join("nibble/config.json")))
            .or_else(|| var("HOME").map(|dir| PathBuf::from(dir).join(".config/nibble/config.json")));
        let settings: Value = file
            .and_then(|file| std::fs::read_to_string(file).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        let setting = |key: &str| settings[key].as_str().unwrap_or_default().to_string();

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
}

enum Event {
    Text(String),
    Tool(String),
    Error(String),
}

enum Part {
    Text(String),
    Tool(String),
    Error(String),
}

struct Turn {
    user: String,
    parts: Vec<Part>,
}

impl Turn {
    fn answer(&self) -> String {
        self.parts.iter().filter_map(|part| if let Part::Text(text) = part { Some(text.as_str()) } else { None }).collect()
    }
}

struct Chat {
    server: Server,
    input: Entity<TextInput>,
    turns: Vec<Turn>,
    scroll: ScrollHandle,
    /// Set while a reply is arriving. Raising the flag stops it.
    running: Option<Arc<AtomicBool>>,
}

/// One chat turn, on a plain thread: the request blocks, and GPUI's own
/// executors should not.
fn ask(server: &Server, body: &Value, stop: &AtomicBool, events: &mpsc::UnboundedSender<Event>) -> Result<(), String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        // Long, because a cold start has to load the model first.
        .timeout_recv_response(Some(Duration::from_secs(300)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut post = agent.post(format!("{}/chat", server.url));
    if !server.token.is_empty() {
        post = post.header("Authorization", format!("Bearer {}", server.token));
    }
    let mut response =
        post.send_json(body).map_err(|e| format!("No answer from nibble serve at {}: {e}", server.url))?;
    match response.status().as_u16() {
        200 => {}
        401 => return Err("The server needs a token. Set \"token\" or \"token_file\" in nibble's config file.".into()),
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

impl Chat {
    fn new(server: Server, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextInput::new("Ask something small", cx));
        window.focus(&input.focus_handle(cx));
        // Follow the system when it switches between light and dark.
        cx.observe_window_appearance(window, |_, _, cx| cx.notify()).detach();
        Chat { server, input, turns: Vec::new(), scroll: ScrollHandle::new(), running: None }
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        // Enter while a reply is arriving stops it, like the page's Stop button.
        if let Some(stop) = self.running.take() {
            stop.store(true, Ordering::Relaxed);
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
        for turn in &self.turns {
            messages.push(json!({ "role": "user", "content": turn.user }));
            if !turn.answer().is_empty() {
                messages.push(json!({ "role": "assistant", "content": turn.answer() }));
            }
        }
        messages.push(json!({ "role": "user", "content": text }));
        self.turns.push(Turn { user: text.to_string(), parts: Vec::new() });

        let stop = Arc::new(AtomicBool::new(false));
        self.running = Some(stop.clone());
        let (send, mut receive) = mpsc::unbounded();
        let server = self.server.clone();
        std::thread::spawn(move || {
            if let Err(error) = ask(&server, &json!({ "messages": messages }), &stop, &send) {
                let _ = send.unbounded_send(Event::Error(error));
            }
        });
        cx.spawn(async move |this, cx| {
            while let Some(event) = receive.next().await {
                if this.update(cx, |chat, cx| chat.apply(event, cx)).is_err() {
                    return;
                }
            }
            let _ = this.update(cx, |chat, cx| {
                chat.running = None;
                // For checking the app from a terminal, where nobody can see the window.
                if std::env::var_os("NIBBLE_GUI_TRACE").is_some() {
                    for part in chat.turns.last().map(|turn| turn.parts.as_slice()).unwrap_or_default() {
                        match part {
                            Part::Text(text) => eprintln!("reply: {text}"),
                            Part::Tool(tool) => eprintln!("tool: {tool}"),
                            Part::Error(error) => eprintln!("error: {error}"),
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn apply(&mut self, event: Event, cx: &mut Context<Self>) {
        let Some(turn) = self.turns.last_mut() else { return };
        match (event, turn.parts.last_mut()) {
            (Event::Text(more), Some(Part::Text(text))) => text.push_str(&more),
            (Event::Text(text), _) => turn.parts.push(Part::Text(text)),
            (Event::Tool(tool), _) => turn.parts.push(Part::Tool(tool)),
            (Event::Error(error), _) => turn.parts.push(Part::Error(error)),
        }
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn new_chat(&mut self, _: &NewChat, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(stop) = self.running.take() {
            stop.store(true, Ordering::Relaxed);
        }
        self.turns.clear();
        cx.notify();
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

fn button(id: &'static str, label: &'static str, theme: &Theme) -> gpui::Stateful<gpui::Div> {
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
        .child(label)
}

impl Render for Chat {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(window);
        self.input.update(cx, |input, _| {
            input.dim = theme.dim;
            input.accent = theme.accent;
        });

        let log = div().id("log").flex_1().overflow_y_scroll().track_scroll(&self.scroll).px_4().children(
            self.turns.iter().map(|turn| {
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .py_2()
                    .child(div().bg(theme.user).rounded_lg().px_3().py_2().child(SharedString::from(turn.user.clone())))
                    .children(turn.parts.iter().map(|part| match part {
                        Part::Text(text) => reply(text, &theme).into_any_element(),
                        Part::Tool(tool) => {
                            div().text_color(theme.dim).text_size(px(12.)).child(format!("· {tool}")).into_any_element()
                        }
                        Part::Error(error) => div()
                            .text_color(theme.accent)
                            .text_size(px(12.))
                            .child(SharedString::from(error.clone()))
                            .into_any_element(),
                    }))
            }),
        );

        div()
            .key_context("Chat")
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::new_chat))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg)
            .text_color(theme.fg)
            .text_size(px(14.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .py_2()
                    .child(div().font_weight(gpui::FontWeight::BOLD).child("nibble"))
                    .child(div().flex_1().text_color(theme.dim).text_size(px(12.)).child("a small local model"))
                    .child(button("new", "New chat", &theme).on_click(cx.listener(|chat, _, window, cx| {
                        chat.new_chat(&NewChat, window, cx)
                    }))),
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
                        button("send", if self.running.is_some() { "Stop" } else { "Send" }, &theme)
                            .on_click(cx.listener(|chat, _, window, cx| chat.submit(&Submit, window, cx))),
                    ),
            )
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

        let bounds = Bounds::centered(None, size(px(560.), px(720.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions { title: Some("nibble".into()), ..Default::default() }),
            ..Default::default()
        };
        cx.open_window(options, |window, cx| {
            cx.new(|cx| {
                let mut chat = Chat::new(Server::load(), window, cx);
                chat.say(first.trim(), cx);
                chat
            })
        })
        .unwrap();
        cx.activate(true);
    });
}
