# Commands

Generated from the built binary's help. Do not edit by hand.
Run `just docs` to refresh it and `just docs-check` to check it.

[Start here](../README.md) for a short tour.

## hc

```text
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
```

## hc attach

```text
Put this terminal on a core terminal (detach: ctrl+\)

Usage: hc attach [OPTIONS] [TERMINAL]

Arguments:
  [TERMINAL]  The terminal: `t7`, a name, or `machine/t7`

Options:
      --json               JSON output even in a terminal
      --observe            Watch only: never type or resize
      --history <HISTORY>  Scrollback lines to show first (main screen only) [default: 0]
  -h, --help               Print help
```

## hc connect

```text
Speak the protocol over stdin/stdout (`ssh box hc connect`)

Usage: hc connect [OPTIONS]

Options:
      --json               JSON output even in a terminal
      --session <SESSION>  The hc session to reach, like HC_SESSION (one command that reads the same in sh, cmd and PowerShell)
  -h, --help               Print help
```

## hc events

```text
Follow fact changes as they happen (one JSON line each when piped)

Usage: hc events [OPTIONS]

Options:
      --json                       JSON output even in a terminal
      --terminal <TERMINAL>        Only this terminal
      --children-of <CHILDREN_OF>  Only children of this terminal
      --label <K=V>                Only terminals carrying all of these labels (repeatable)
  -h, --help                       Print help
```

## hc exec

```text
Run one program without a terminal, here or on a machine: exactly what it wrote, and its exit code

Usage: hc exec [OPTIONS] <ARGV>...

Arguments:
  <ARGV>...  Program and arguments, run as given (no shell)

Options:
      --json               JSON output even in a terminal
      --on <MACHINE>       Run it on another machine (see `hc machine ls`)
      --cwd <CWD>          Working directory (default: where hc runs; through the API, the core's; with --on, that machine's home)
      --env <K=V>          Set on top of the core's environment, key=value (repeatable)
      --timeout <TIMEOUT>  Seconds before it and everything it started are ended (default 60, at most 600)
  -i, --stdin              Feed what is piped into hc to its stdin
  -h, --help               Print help
```

## hc file

```text
Read, write or edit a file here or on a machine: an agent's hands there

Usage: hc file [OPTIONS] <COMMAND>

Commands:
  read   Print a text file, or lines of it (--offset, --limit)
  write  Write what is piped in as the file, making missing folders
  edit   Replace exact text found once (--old X --new Y, repeatable); all or none
  help   Print this message or the help of the given subcommand(s)

Options:
      --json  JSON output even in a terminal
  -h, --help  Print help
```

## hc file edit

```text
Replace exact text found once (--old X --new Y, repeatable); all or none

Usage: hc file edit [OPTIONS] --old <OLD> --new <NEW> <PATH>

Arguments:
  <PATH>  The file: absolute, or from `cwd`

Options:
      --json          JSON output even in a terminal
      --on <MACHINE>  On another machine (see `hc machine ls`)
      --cwd <CWD>     Where a relative path starts (default: where hc runs; through the API, the core's; with --on, that machine's home)
      --old <OLD>     Exact text found once in the file (repeatable, paired with --new in order)
      --new <NEW>     What replaces it
  -h, --help          Print help
```

## hc file read

```text
Print a text file, or lines of it (--offset, --limit)

Usage: hc file read [OPTIONS] <PATH>

Arguments:
  <PATH>  The file: absolute, or from `cwd`

Options:
      --json             JSON output even in a terminal
      --on <MACHINE>     On another machine (see `hc machine ls`)
      --cwd <CWD>        Where a relative path starts (default: where hc runs; through the API, the core's; with --on, that machine's home)
      --offset <OFFSET>  The first line to give, from 1
      --limit <LIMIT>    At most this many lines (default: all, up to 8 MB)
  -h, --help             Print help
```

## hc file write

```text
Write what is piped in as the file, making missing folders

Usage: hc file write [OPTIONS] <PATH>

Arguments:
  <PATH>  The file: absolute, or from `cwd`

Options:
      --json          JSON output even in a terminal
      --on <MACHINE>  On another machine (see `hc machine ls`)
      --cwd <CWD>     Where a relative path starts (default: where hc runs; through the API, the core's; with --on, that machine's home)
  -h, --help          Print help
```

## hc get

```text
Show one terminal

Usage: hc get [OPTIONS] [TERMINAL]

Arguments:
  [TERMINAL]  The terminal: `t7`, a name, or `machine/t7`

Options:
      --json  JSON output even in a terminal
  -h, --help  Print help
```

## hc interrupt

```text
Press the stop key and report what happened

Usage: hc interrupt [OPTIONS] [TERMINAL]

Arguments:
  [TERMINAL]  The terminal: `t7`, a name, or `machine/t7`

Options:
      --json               JSON output even in a terminal
      --timeout <TIMEOUT>  Seconds to watch for an effect (default 10)
  -h, --help               Print help
```

## hc keep

```text
Keep a live terminal after loss (--off disables)

Usage: hc keep [OPTIONS] <TERMINAL>

Arguments:
  <TERMINAL>  The terminal: `t7`, a name, or `machine/t7`

Options:
      --json  JSON output even in a terminal
      --off   Keep this live terminal after loss. Existing streams end with the loss reason; reattach after `restarted`
  -h, --help  Print help
```

## hc keys

```text
Press named keys (enter, esc, up, ctrl+c, alt+x, f5, a...)

Usage: hc keys [OPTIONS] <TERMINAL> <KEYS>...

Arguments:
  <TERMINAL>  The terminal: `t7`, a name, or `machine/t7`
  <KEYS>...   Key names: enter, esc, tab, up, ctrl+c, alt+x, f5, a, ...

Options:
      --json  JSON output even in a terminal
  -h, --help  Print help
```

## hc kill

```text
End a terminal's program (hangup, then escalate)

Usage: hc kill [OPTIONS] [TERMINAL]

Arguments:
  [TERMINAL]  The terminal: `t7`, a name, or `machine/t7`

Options:
      --json  JSON output even in a terminal
  -h, --help  Print help
```

## hc label

```text
Set labels (key=value; key= removes)

Usage: hc label [OPTIONS] <TERMINAL> <K=V>...

Arguments:
  <TERMINAL>  The terminal: `t7`, a name, or `machine/t7`
  <K=V>...    Labels to set, key=value; `key=` removes one

Options:
      --json  JSON output even in a terminal
  -h, --help  Print help
```

## hc ls

```text
List live terminals

Usage: hc ls [OPTIONS]

Options:
      --all          Include finished terminals
      --json         JSON output even in a terminal
      --label <K=V>  Only terminals carrying all of these labels (repeatable)
  -h, --help         Print help
```

## hc machine

```text
Other machines, reached over ssh

Usage: hc machine [OPTIONS] <COMMAND>

Commands:
  add     Add a machine you reach with ssh: hc machine add vps
  ls      List the machines added here
  login   Log in again (a password or code) after the machine says it needs one. On Windows: reconnect once its SSH keys work
  update  Copy this hc there and restart hc there (its terminals keep running)
  rename  Give a machine a new name here
  rm      Forget a machine (its terminals keep running there)
  help    Print this message or the help of the given subcommand(s)

Options:
      --json  JSON output even in a terminal
  -h, --help  Print help
```

## hc machine add

```text
Add a machine you reach with ssh: hc machine add vps

Usage: hc machine add [OPTIONS] [TARGET]

Arguments:
  [TARGET]  How ssh reaches it: `vps`, `can@10.0.0.7`, a Host from ~/.ssh/config

Options:
      --json               JSON output even in a terminal
      --via <COMMAND>      Reach it with this command instead of ssh: it is given a shell line to run there as its last argument, like ssh (`ssh -p 2222 -i key user@ip`, `fly ssh console -a app --pty=false -C`). It must pass bytes both ways untouched and ask nothing. Needs --name
      --temp               Mark it short-lived (a sandbox for a task), so clients can show it apart from machines that stay. hc treats it like any other; remove it with `hc machine rm`
      --install            Copy this hc there when it has none or one too old, without asking (for scripts; in a terminal you are asked)
      --name <NAME>        What to call it here (default: the target's host)
      --session <SESSION>  The hc session on that machine (default: its default)
  -h, --help               Print help
```

## hc machine login

```text
Log in again (a password or code) after the machine says it needs one. On Windows: reconnect once its SSH keys work

Usage: hc machine login [OPTIONS] <MACHINE>

Arguments:
  <MACHINE>  The machine's name

Options:
      --json  JSON output even in a terminal
  -h, --help  Print help
```

## hc machine ls

```text
List the machines added here

Usage: hc machine ls [OPTIONS]

Options:
      --json  JSON output even in a terminal
  -h, --help  Print help
```

## hc machine rename

```text
Give a machine a new name here

Usage: hc machine rename [OPTIONS] <MACHINE> <NEW_NAME>

Arguments:
  <MACHINE>   The machine's name now
  <NEW_NAME>  Its new name. Addresses with the old one stop working

Options:
      --json  JSON output even in a terminal
  -h, --help  Print help
```

## hc machine rm

```text
Forget a machine (its terminals keep running there)

Usage: hc machine rm [OPTIONS] <MACHINE>

Arguments:
  <MACHINE>  The machine's name

Options:
      --json  JSON output even in a terminal
  -h, --help  Print help
```

## hc machine update

```text
Copy this hc there and restart hc there (its terminals keep running)

Usage: hc machine update [OPTIONS] <MACHINE>

Arguments:
  <MACHINE>  The machine's name

Options:
      --json  JSON output even in a terminal
  -h, --help  Print help
```

## hc put

```text
Send a local file beside a terminal (on its machine); prints where it landed

Usage: hc put [OPTIONS] <TERMINAL> <FILE>

Arguments:
  <TERMINAL>  The terminal: `t7`, a name, or `machine/t7`
  <FILE>      The file's name, kept so `.png` stays `.png` (CLI: the local file to send)

Options:
      --json  JSON output even in a terminal
  -h, --help  Print help
```

## hc read

```text
Print a terminal's screen and history

Usage: hc read [OPTIONS] [TERMINAL]

Arguments:
  [TERMINAL]  The terminal: `t7`, a name, or `machine/t7`

Options:
      --json           JSON output even in a terminal
      --lines <LINES>  Lines of history above the screen [default: 0]
  -h, --help           Print help
```

## hc rm

```text
Remove a finished terminal

Usage: hc rm [OPTIONS] [TERMINAL]

Arguments:
  [TERMINAL]  The terminal: `t7`, a name, or `machine/t7`

Options:
      --json  JSON output even in a terminal
  -h, --help  Print help
```

## hc run

```text
Start a program, or a template by name with its task: hc run reviewer "review src/x.rs"

Usage: hc run [OPTIONS] [ARGV]...

Arguments:
  [ARGV]...  Program and arguments, or a template name and its task. With neither argv nor line: that machine's own shell

Options:
      --json               JSON output even in a terminal
      --name <NAME>        Name, unique among live terminals: 1-32 of a-z 0-9 - _ 
      --cwd <CWD>          Working directory (default: where hc runs; through the API, the core's; with --on, that machine's home)
      --on <MACHINE>       Run it on another machine (see `hc machine ls`); its address is `machine/t7`
      --label <K=V>        Labels, key=value (repeatable)
      --parent <PARENT>    Parent terminal (default: the terminal it is started from)
      --no-parent          No parent, even when started from a terminal
      --no-notify          Don't type notes about it into its parent
      --outlive-parent     Keep it running when its parent's program ends (by default it ends too)
      --rm                 Remove it right after it exits (once a waiting parent has the exit code)
      --keep               Replace lost programs with a shell in place, never re-run the original command
      --bg                 Start it and come back at once (a server, several workers)
      --timeout <TIMEOUT>  Seconds before a wait checks in, leaving it running (default: config `wait_timeout`, else 180)
  -h, --help               Print help
```

## hc schema

```text
Every API method with its JSON Schema, for app builders

Usage: hc schema [OPTIONS]

Options:
      --json  JSON output even in a terminal
  -h, --help  Print help
```

## hc send

```text
Paste text into a terminal and press Enter (waits for a person to pause)

Usage: hc send [OPTIONS] <TERMINAL> <TEXT>

Arguments:
  <TERMINAL>  The terminal: `t7`, a name, or `machine/t7`
  <TEXT>      Pasted, then submitted with Enter

Options:
      --force              Type even if the program is blocked (it may answer its dialog)
      --json               JSON output even in a terminal
      --if <STATUS>        Only type if the status is exactly this [possible values: idle, working]
      --wait               Then wait for the turn this send started
      --timeout <TIMEOUT>  With wait: seconds before it checks in (default: config, else 180)
  -h, --help               Print help
```

## hc server

```text
Start, stop or inspect the core

Usage: hc server [OPTIONS] <COMMAND>

Commands:
  start    
  stop     
  status   
  restart  Restart the core (e.g. after an update); programs keep running
  help     Print this message or the help of the given subcommand(s)

Options:
      --json  JSON output even in a terminal
  -h, --help  Print help
```

## hc server restart

```text
Restart the core (e.g. after an update); programs keep running

Usage: hc server restart [OPTIONS]

Options:
      --json  JSON output even in a terminal
  -h, --help  Print help
```

## hc server start

```text
Usage: hc server start [OPTIONS]

Options:
      --json  JSON output even in a terminal
  -h, --help  Print help
```

## hc server status

```text
Usage: hc server status [OPTIONS]

Options:
      --json  JSON output even in a terminal
  -h, --help  Print help
```

## hc server stop

```text
Usage: hc server stop [OPTIONS]

Options:
      --json  JSON output even in a terminal
  -h, --help  Print help
```

## hc templates

```text
Templates you can run by name (~/.config/herdr-core/templates)

Usage: hc templates [OPTIONS]

Options:
      --json  JSON output even in a terminal
  -h, --help  Print help
```

## hc update

```text
Swap this hc for another build and restart (terminals keep running)

Usage: hc update [OPTIONS]

Options:
      --from <FILE>      A herdr-core binary to use (default: download the latest release)
      --json             JSON output even in a terminal
      --version <X.Y.Z>  At least this release (`0.1.4`): nothing changes when this hc is already that or newer
      --force            Allow an older version (saved state only moves forward), or a release of another protocol generation
  -h, --help             Print help
```

## hc wait

```text
Wait for your children (--children any) or one terminal; a message to you also ends it

Usage: hc wait [OPTIONS] [TERMINAL]

Arguments:
  [TERMINAL]
          The terminal; with `children`, whose children (default: the caller)

Options:
      --children <CHILDREN>
          Wait on children instead: all = until none is working, any = next news or a message to you.
          
          `all`: until none of the children is working (one that hasn't reported yet counts as working). `any`: the parent's unseen news (a turn ended, it blocked, it hit an error, it exited), else the next child to stop working; for the parent itself, also a message typed into it while it worked (reason `message`).
          
          [possible values: any, all]

      --json
          JSON output even in a terminal

      --output <OUTPUT>
          For programs, not agents: end when output printed after the wait started contains this text (plain text, escape sequences stripped). Blocked, error or exit end it too

      --timeout <TIMEOUT>
          Seconds before it checks in (default: config `wait_timeout`, else 180)

  -h, --help
          Print help (see a summary with '-h')
```
