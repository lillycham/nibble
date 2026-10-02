# nibble

A small harness for a small local model. It is for the tasks that don't need
a big agent: summarise this log, name this commit, find where that setting
lives. It runs on Apple silicon with MLX, and it keeps its own weight low: the
command-line program is about 1 MB with two dependencies.

```bash
git diff | nibble "Write a commit message for this diff."
nibble "Which nixpkgs branch does the flake use?"
nibble            # a chat
```

## What is in it

- **`nibble`**: one-shot questions, piped input, and a chat. The model can read
  and search files in the current directory, and nothing else. It can't change
  anything.
- **`nibble serve`**: a proxy in front of the model server. It starts the model
  on the first request and unloads it after an idle period, so the weights only
  hold memory while something is using them. It also serves a chat page at `/`.
- **`nibble mcp`**: an MCP server, so that Claude can hand small reading tasks
  to the local model. `delegate` gives it one task; `map` asks one question of
  many files, each in a fresh context.
- **`ask_claude`**: the other direction. The local model can pass a question
  that is too hard for it to `claude -p`.
- **`nibble-gui`**: a native chat window, built with GPUI.

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
