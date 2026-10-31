hc - run programs and coding agents in terminals that stay alive

Usage: hc <command> [options]
       hc               pick a terminal and attach (new… on top)

Terminals
  run <cmd|template> [task]   run it and wait like a shell (--bg: don't wait)
  ls                          list live terminals (--all: finished too)
  get <t>                     show one terminal
  attach [t]                  put this terminal on it (ctrl+\ detaches)
  read <t>                    print its screen and history
  kill <t>                    end its program
  rm <t>                      remove a finished terminal
  label <t> k=v...            set labels
  keep <t> [--off]            replace lost programs with a shell (also run --keep)
                              signal death recovers too; streams end on loss, reattach

Talk to programs
  send <t> <text>             paste text and press enter
  keys <t> <keys...>          press keys: enter, esc, up, ctrl+c
  interrupt <t>               press the stop key, report what happened
  wait <t>                    wait until it stops working (--output X: until it prints X,
                              --children: your helpers). Use it instead of sleep.
  put <t> <file>              send a file beside it, print where it landed

No terminal
  exec [--on m] <cmd>         run it, print exactly what it wrote, exit with its code
                              (-i: feed it what is piped in)
  file read|write|edit [--on m] <path>
                              a file here or there: print it, write what is piped
                              in, or replace exact text (--old X --new Y)

More
  templates                   list templates
  events                      follow changes live
  server start|stop|status|restart
                              stop parks kept terminals; next core start gives them shells
  update [--version X|--from <file>]   latest release, at least X, or a build
  machine add|ls|login|update|rename|rm   other machines, over ssh or a command
                              (--via 'fly ssh console -a x --pty=false -C')
  connect                     the API over stdin/stdout (ssh box hc connect)
  schema                      the API as JSON Schema, for app builders

<t> is an address like t7 or t1b, or a name. In a terminal, attach, read, get,
kill, rm, wait and interrupt pick one when it's left out.
Options: --json (default when piped; run and read print text), -V --version,
--licenses (what hc includes, and their licenses).
Per command: hc <command> --help
Settings: ~/.config/herdr-core/config.toml. notes = false: hc types nothing
on its own (no notes to parent agents, no <hc> tags on messages).
shell = ["nu"] overrides the shell for new terminals and keep recovery.

Examples
  hc run --on vps "cd app && npm test"     like ssh, in a terminal that stays
  hc run --bg claude                       Claude Code in a new terminal
  hc attach t3                             take it over
  hc run reviewer "review src/x.rs"        a template with a task

Agent? Run: hc --skill
