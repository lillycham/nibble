# nibble

A small harness for a small local model. It is for the tasks that don't need
a big agent: summarise this log, name this commit, find where that setting
lives. It runs on Apple silicon with MLX, and it keeps its own weight low: the
command-line program is about 1 MB with two dependencies.

```bash
git diff | nibble "Write a commit message for this diff."
nibble "Which nixpkgs branch does the flake use?"
nibble -f flake.nix "What does this flake build?"
nibble -q "Where is the idle timeout set?"   # with quotes that are checked
nibble            # a chat
nibble -c         # go on with the last chat
```

## What is in it

- **`nibble`**: one-shot questions, piped input, and a chat. The model can read
  and search files in the current directory, and nothing else. It can't change
  anything. Chats are saved and shared with the window: `nibble chats` lists
  them, and `-c` or `-r ID` goes on with one. With `-q`, the model quotes the
  lines its answer rests on and nibble checks that each one is in the file, so
  an invented answer shows up as a quote that isn't there.
- **`nibble serve`**: a proxy in front of the model server. It starts the model
  on the first request and unloads it after an idle period, so the weights only
  hold memory while something is using them. It also serves a chat page at `/`.
- **`nibble mcp`**: an MCP server, so that Claude can hand small reading tasks
  to the local model. `delegate` gives it one task; `map` asks one question of
  many files, each in a fresh context. `delegate` takes `quote` too.
- **`ask_claude`**: the other direction. The local model can pass a question
  that is too hard for it to `claude -p`.
- **Plugins**: MCP servers whose tools a chat can ask for with `--plugin`.
- **`nibble-gui`**: a native chat window, built with GPUI. Drop files on it to
  attach them to your next message, as `nibble -f` does.

## Run it

You need Nix, an Apple silicon Mac, and a model in MLX format that was
converted with mlx-lm, for example
[Qwen3-4B-Instruct-2507-4bit](https://huggingface.co/mlx-community/Qwen3-4B-Instruct-2507-4bit).

```bash
nix run github:lillycham/nibble -- serve --model /path/to/model
```

```bash
nix run github:lillycham/nibble -- "What is in this directory?"
```

```bash
nix run github:lillycham/nibble#nibble-gui
```

The flake also has a home-manager module, `homeModules.default`, which runs
`nibble serve` under launchd or systemd:

```nix
services.nibble = {
  enable = true;
  model = "/path/to/model";
  settings.roots = [ "~/projects" ];
};
```

## Settings

`nibble config` prints every setting and where the config file belongs
(`~/.config/nibble/config.json`). Environment variables and flags override
the file.

Models differ. One needs a firm prompt before it will use its tools, another
uses them too freely, and some can't call tools at all. nibble has presets for
the models it has been tried with, and a `models` section in the config file
adds your own:

```json
{
  "models": {
    "my-model": { "prompt": "light", "max_calls": 6 }
  }
}
```

The key is looked for in the model's name.

## More than one model

The chat page and the window have a list to switch models. It holds every
entry in `model_dir`, which defaults to the directory that holds `model`. The
switch applies from the next message, and each model's presets come with it.
A restart of `nibble serve` goes back to `model`.

## Trying a model

`nibble eval` asks the model 20 questions about a small sample project and
reports how many it got right, how many tool calls it made and how long it
took. `--model NAME`, repeated, compares several models from the list.

## Plugins

A plugin is an MCP server, so the ones already written for other tools work
here too. Set one up in the config file, and it stays off until a chat asks for
it with `--plugin NAME` (or `-p`):

```json
{
  "plugins": {
    "git": {
      "command": ["uvx", "mcp-server-git"],
      "tools": ["git_status", "git_log", "git_diff"]
    }
  }
}
```

Every tool's description is sent with every request, and a small model has
little room, so plugins are never on by default. `tools` keeps only the ones
you name; `nibble plugins` shows what each plugin offers and how many
characters that adds to a request. `env` sets environment variables for it.

A plugin runs in the current directory with your permissions, and nibble's own
limits (read only, inside this directory) don't apply to it. Pick ones whose
tools you'd let the model use.

## Reach

`nibble serve` listens on this machine only. To listen on another address, set
`token` or `token_file` first: it refuses to start without one, because anyone
who can reach it can use the model and read files through it.

## State

Early. It has been run with Qwen3-4B, LFM2.5-2.6B and gemma-3n, on
mlx_lm.server only. See [TODO.md](TODO.md) for what is decided, what is done
and what is known to be wrong.

## Licence

MIT. See [LICENSE](LICENSE). One file, `gui/src/input.rs`, is adapted from an
example in [GPUI](https://www.gpui.rs) and stays under the Apache License 2.0;
see [gui/LICENSE-APACHE](gui/LICENSE-APACHE).
