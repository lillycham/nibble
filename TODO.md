# nibble: plan and notes

## Decided

- MLX backend through `mlx_lm.server`; Rust harness; Pi stays as a fallback.
- File tools are read-only and confined to the current directory. `--anywhere` lifts the limit.
- `ask_claude` is on by default, and off whenever Claude is the caller.
- No memory feature for now. Revisit on better hardware, since it is the one
  feature that spends context on every request.
- No web fetch tool: with file reading it would give a small model a way to leak files.
- Default overhead (system prompt plus tool schemas) stays under 10% of the window.
  Anything that adds to it is opt-in.
- Not a one-model harness. Nothing in the code may assume Qwen, a 4B model or
  mlx-lm: what suits one model is a setting with a default (`tools`, `system`,
  `system_tools`, the size limits, `server_command`, `server_args`).

## Done

- One-shot, chat and `serve` (lazy start, idle stop).
- Read-only tools, `ask_claude`, read confinement.
- Config file (`nibble config` shows it), with every limit a setting.
- `nibble mcp` with `delegate` and `map`, tested from Claude Code.
- Home-manager module (`homeModules.default`): package, config file, and
  `nibble serve` as a launchd agent or systemd user service.
- HTTP layer on `nibble serve`: chat page at `/`, `POST /chat` (server-sent
  events), Bearer token on everything except the page itself. Refuses to listen
  beyond loopback without a token. Chats get the file tools only inside `roots`.
- GPUI chat window in `gui/` (`nibble-gui`), built by Nix with `runtime_shaders`.
  4.9 MB binary, about 50 MB resident.
- In the window: a sidebar that lists saved chats (one JSON file each under
  `~/.local/share/nibble/chats`), and a settings page that edits the shared
  config file and leaves keys it has no field for alone.
- Published at github.com/lillycham/nibble (MIT), and packaged in
  lillycham/nur-packages as `nibble`, `nibble-gui` and `nibble-mlx-server`.
  The MLX wheel recipe is in both places, so bump both.
- In use on mirai: the module in `~/nixfiles`, and `nibble mcp` registered
  with Claude Code and the Claude desktop app.
- Per-model presets: built-in ones for the models tried, and a `models` section
  in the config file. The model's name comes from `--model`, `NIBBLE_MODEL`,
  `GET /info` on `nibble serve`, or the `model` setting.
- Model switch: `nibble serve` lists the entries of `model_dir` in `/info`, and
  `POST /model` changes to one of them (never to a path the client names).
  It refuses while a reply is in progress. The window and the page have a list.
  The switch is not kept: a restart goes back to `model`.
- `/v1/` requests through `nibble serve` are pinned to the model it runs: the
  body's `"model"` is replaced, so a client with the token can't make
  mlx_lm.server load another one (from Hugging Face or anywhere). Each
  connection carries one request, and chunked bodies are refused, so nothing
  passes unchecked.
- `nibble mcp` asks `nibble serve` for its model before each `tools/list` and
  `tools/call`, and loads that model's presets when it has changed, so a switch
  needs no restart. `NIBBLE_MODEL` pins it.
- `-f FILE` (repeatable) attaches a file to the prompt, or to a chat's first
  message, so the model needs no tool round to read it. Like piped input, it
  turns the tools off unless `--tools` is given. The user named the file, so
  it may be outside the current directory. Files over `input_chars` are
  refused, not cut; piped input gets what room is left.
- Stats line per turn on the command line: prompt size, reply tokens, speed and
  time to the first token, on stderr. On when stderr is a terminal; `--stats`
  and `--no-stats` override. Token counts need a server that honours
  `stream_options.include_usage`; without them it gives the prompt in characters.

## Next

- Look at the GPUI window: its layout has only been checked through a trace
  and its self-test (`NIBBLE_GUI_SELFTEST=1`, with the XDG directories pointed
  somewhere disposable), never seen.
- The GPUI input is one line and does not scroll sideways. It needs to grow
  into a multi-line field. It has the macOS editing keys (by word, to either
  end, the Control keys, undo, double-click), but not spell-check, autocorrect
  or Look Up, which belong to AppKit's text system and are out of reach.
- Replies have a Copy button, but their text can't be selected.
- The app bundle has no icon.
- A skill for Claude is not needed so far: the MCP server's `instructions`
  already say when delegation is worth it.

## Later

- Recipes: named presets (`nibble commit`, `nibble summarise`), declared in the Nix module.
- Sessions for the command line: save and resume chats (the window has them).
- In the window's settings page: a field per model preset, and masking for the token.
- Settings page: fill each empty field with the value really in use, not a
  grey hint that repeats a default written into the GUI. The server knows its
  effective settings (model, roots, limits, presets applied), so it should
  report them, for example from `/info`, and the page should show those.
- Write and shell tools behind `--write` and `--shell`.
- Quote-your-evidence mode: the model quotes the lines behind its answer, and
  nibble checks that each quote appears in the file. Flags invented answers.
- Point Pi at `nibble serve` (http://127.0.0.1:8765/v1) in place of oMLX.

## Frontend

Decided: a web page for remote use (done, served by `nibble serve`), and a
GPUI client for use on the machine itself. Both talk to `POST /chat`.

- Tailscale: listen on the tailnet address with a token set. Do not rely on
  "requests from 127.0.0.1 are local": `tailscale serve` makes remote requests
  arrive from loopback, which is why the token covers every request.
- `ask_claude` is off in web chats, so a remote user can't spend Claude usage.
- GPUI client lives in `gui/` as its own Cargo project, so the CLI keeps its
  two dependencies.
  - GPUI compiles Metal shaders at build time, which the Nix sandbox can't do.
    The `runtime_shaders` feature compiles them at start instead, and works.
  - crates.io has 0.2.2 (October 2025); Zed develops it in its own repository,
    so a git pin may be needed for anything newer.
  - GPUI has no text input widget of its own; ours is adapted from its example.
  - Linux is not packaged: GPUI there needs Wayland and X11 libraries wired in.
- GUI for the config file, later.

## Other models and servers

Tested on mlx_lm.server 0.32.0:

- Qwen3-4B-Instruct (JSON tool calls). Reluctant: without a firm prompt it
  refuses any question that names no file. Takes the first search hit as the
  answer. Does not follow "more lines" paging.
- LFM2.5-2.6B, Liquid's own MLX conversion (pythonic tool calls). The opposite:
  eager, up to 15 calls for one question, pages through files, and got right
  two answers Qwen got wrong. Slow as a result: one question ran past 90 s.
  It tried to read a file called `<input>` when input was piped, which is why
  piped input now gets no tools.
- gemma-3n E4B. No tool-call format; needs `"tools": false`, then works as a
  plain assistant.

What this means: one built-in prompt can't suit both a reluctant and an eager
model, hence the presets. With its preset (light prompt, six calls, shorter
results) LFM answers a project question in about 50 to 70 seconds.

Not tested: any model above 4B, and any server other than mlx_lm.server. Tool
calls streamed in pieces (OpenAI, llama.cpp) are handled and unit-tested, but
have never met a real server. `tools` must still be set by hand.

Use conversions made with mlx-lm. `mlx-community/LFM2.5-2.6B-4bit` was made
with mlx-vlm and does not load.

## Known problems

- mlx-lm 0.31.3 hangs on gemma-3n unless the request has a seed.
