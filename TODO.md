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

## Next

- Use the module from `~/nixfiles`, then register `nibble mcp` with Claude Code
  and the Claude desktop app.
- Look at the GPUI window: its layout has only been checked through a trace,
  never seen. Then give it an .app bundle, so it has a Dock icon and Spotlight
  can find it.
- The GPUI input is one line and does not scroll sideways. It needs to grow
  into a multi-line field. Replies can't be selected or copied yet.
- A licence. `gui/src/input.rs` is adapted from GPUI's Apache-2.0 example.
- A skill for Claude is not needed so far: the MCP server's `instructions`
  already say when delegation is worth it.

## Later

- Stats line per turn: prompt size, tokens, speed.
- `-f FILE` to attach files without a tool round.
- No tool schemas when input is piped.
- Recipes: named presets (`nibble commit`, `nibble summarise`), declared in the Nix module.
- Sessions: save and resume chats.
- Write and shell tools behind `--write` and `--shell`.
- Quote-your-evidence mode: the model quotes the lines behind its answer, and
  nibble checks that each quote appears in the file. Flags invented answers.
- Remove Homebrew oMLX and the Hermes install once the module is in.

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

- Tested: Qwen3-4B-Instruct (tools work) and gemma-3n E4B (no tool-call format;
  needs `"tools": false`, then works as a plain assistant).
- Not tested: any larger model, and any server other than mlx_lm.server.
  Tool calls streamed in pieces (OpenAI, llama.cpp) are handled and unit-tested,
  but have never met a real server.
- To do: find out whether a model can call tools without being told, so
  `tools` need not be set by hand. And try llama.cpp's server through
  `server_args` with `{model}` and `{port}`.

## Known problems

- The 4B model does not follow "more lines" paging hints in `read_file`.
- mlx-lm 0.31.3 hangs on gemma-3n unless the request has a seed.
