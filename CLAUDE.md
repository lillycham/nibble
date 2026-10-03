# Notes for agents

## Keep it portable across models

nibble is not a one-model harness. It must work with any model and any
OpenAI-style server, not only the ones it has been tried with (Qwen3-4B,
LFM2.5, gemma-3n on mlx_lm.server). See "Decided" in TODO.md.

- Don't special-case a model in the code. No `if name.contains("qwen")`
  branches, and no fixes that only make sense for one model's quirks.
- When a model needs different behaviour, make it a setting with a
  sensible default (`prompt`, `max_calls`, `tools`, `system`,
  `system_tools`, the size limits). The model presets are data that sets
  those settings, and users can add their own in the `models` section.
- Don't assume a model size, a tool-call format, a context length or
  mlx-lm. Handle the general case, such as tool calls that arrive whole
  or in pieces.
- Check a prompt or tool change against more than one model when you
  can (`nibble eval --model A --model B`). A fix that helps one model
  often hurts another: Qwen is reluctant to use its tools, and LFM is
  too eager.
