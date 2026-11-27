---
name: herdr-core
description: "Use herdr-core (hc) for persistent terminals, background tasks and servers, or terminal and file operations on remote machines."
---

This skill is for herdr-core, whose command is `hc`. It is not `herdr`,
the older terminal multiplexer: that has its own `herdr` command and skill.
Use `hc` here, never `herdr`.

If `$HC_TERMINAL_ID` is set, you're running inside herdr-core: use hc.
If not, ask the user once this session whether to use hc for this work.

For delegation, use the host's native tools by default. Use hc to delegate
when the user requests Herdr or the child needs a persistent or remote terminal.

Before using hc, run `hc --skill` and follow it.
For the Herdr window (spaces, tabs, layouts, mods, what's shown),
run `herdr-app --skill`.
