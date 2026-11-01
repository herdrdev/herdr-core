# herdr-core API

`hc.openrpc.json` is the whole request/response API as an
[OpenRPC](https://open-rpc.org) document: every method with its params,
result and errors in JSON Schema. It is generated from the Rust types (a
test fails when it is stale), and `hc schema` or the `rpc.discover` method
returns the same document from a running core.

This file covers what OpenRPC does not describe: the two connection modes,
the attach stream, and event notifications.

Status: preview (protocol generation 1, version 0.x).

The API only grows. `released.openrpc.json` is the API as last released;
a test fails on any change that would break a client of it: a removed
method, param or result field, a new required input, an input that accepts
less, or an answer that can carry a new kind of value or loses its type.
At each release, copy `hc.openrpc.json` over it. A real break needs a new
protocol generation, decided on purpose.

## Connecting

On Unix, core listens on a Unix socket; on Windows, it uses a named pipe.
See [endpoint rules](../docs/security.md#state-and-endpoints) for default paths,
fallbacks, and overrides. `hc connect` speaks the same protocol over
stdin/stdout, so `ssh box hc connect` is a remote core.

A connection starts in **plain mode**: JSON-RPC 2.0, one message per line.
Any language or `socat` works.

```
→ {"jsonrpc":"2.0","id":1,"method":"terminal.list","params":{}}
← {"jsonrpc":"2.0","id":1,"result":{"terminals":[...],"hidden_finished":0}}
```

Errors are JSON-RPC errors with a stable kind and a hint:

```
{"code":-32000,"message":"no terminal t9","data":{"kind":"not_found","hint":"..."}}
```

`hello` with `{"framed": true}` switches the connection to **framed mode**
after its reply. Attach needs it; everything else works in both modes.

## Framed mode

Every frame: `[channel u32 LE][kind u8][len u32 LE][payload]`, payload at
most 16 MiB.

| kind | name | payload |
|---|---|---|
| 0 | DATA | channel 0: one JSON-RPC message; other channels: stream bytes |
| 1 | CREDIT | u32 LE: bytes the receiver consumed |
| 2 | CLOSE | the channel is over; from the core, a JSON reason |
| 3 | RESIZE | u16 LE cols, u16 LE rows: viewer → core, core → observer |
| 4 | FOCUS | viewer → core: 1 gained focus, 0 lost it |
| 5 | FACTS | viewer → core: JSON display facts (`Display` in the schema) |
| 6 | AUTOMATIC_INPUT | viewer → core: terminal-generated reply bytes, not human activity |

Channel 0 carries JSON-RPC both ways. Calls on it run side by side.

### Attach streams

`terminal.attach` returns a `channel`. On it the core sends the first paint
(the current screen, optional history, the latest status report and title,
and `CSI ?9001h` or `l`: whether the program asked for win32-input-mode,
as Windows' ConPTY does when it starts), then the program's raw output,
minus terminal queries the core already answered. The core never encodes
keys: a viewer that honors 9001 sends key events in that format itself.
The viewer sends keys as DATA on the same channel, and RESIZE,
FOCUS and FACTS as it changes. The last active viewer sets the size
(active: it attached, typed, resized or gained focus); `observe` viewers never type or resize.
Automatic query answers (including mode 2048 reports, kitty graphics and XTGETTCAP
replies) use AUTOMATIC_INPUT, not DATA. Their bytes reach the program unchanged,
including duplicates from multiple viewers, but do not take size/display ownership
or reset the human quiet period. They follow the same input serialization as DATA.
Tag replies at their source, never by inspecting their bytes; arbitrary splits are valid.
Cores advertise `automatic_input` in `hello.features`; `terminal.attach` confirms
`automatic_input: true` only for a supporting path. Clients must require both before
sending kind 6; absent/false means send DATA (legacy activity behavior). Old cores
ignore unknown local frame kinds, so sending kind 6 without negotiation loses input.
Relays preserve the tag and close a previously capable stream if support is lost
on reconnect; attach again to negotiate the fallback. Untagged DATA is unchanged.
Observers may opt in with `resize_notifications: true` on `terminal.attach`:
the client must apply incoming RESIZE before consuming subsequent DATA.
The reply confirms `resize_notifications: true` only when every core on the path
supports it. False or an absent field means the client must track size through
size events instead; no replacement paints are injected for this request.
Cores advertise `resize_notifications` in both hello requests and replies, so an
older intermediate relay cannot silently drop the grid announcements. If a
previously capable path loses support on reconnect, its stream closes; attach
again to negotiate the fallback.
These confirmed observers receive the terminal's actual grid before their first paint.
When that grid changes, pending output is superseded by RESIZE followed by an
authoritative paint (starting with CAN to cancel any unfinished sequence).
Size/paint pairs share stream ordering and wait for DATA credit; RESIZE itself
does not consume credit. Intermediate unsent grids are coalesced, and a paint is
serialized only when the observer has credit. The attach reply precedes stream
frames, including on core-to-core relays and reconnects.
Without this opt-in, existing event-driven observers keep their previous output
behavior: no additional resize paints or RESIZE frames.

Flow control: a stream starts with 256 KiB of credit. The viewer sends
CREDIT for the bytes it has written out. A viewer that falls more than
4 MiB behind gets a fresh repaint instead of the backlog. A stalled stream
never blocks channel 0 or other streams.

A viewer detaches by sending CLOSE; the core answers with an empty CLOSE.
When the program ends, the core sends CLOSE with
`{"reason":"exited","exit_code":0,"signal":null}` (`reason` as in the
terminal record: exited, killed, server_stop, ...).

## Events

`events.subscribe` replies with a snapshot (`seq`, matching terminals) and,
in framed mode, a channel; in plain mode the connection then carries only
notifications. Notifications are JSON-RPC messages without an id:

```
{"jsonrpc":"2.0","method":"event","params":{"seq":42,"type":"status","terminal":"t7","data":{...}}}
{"jsonrpc":"2.0","method":"heartbeat","params":{"seq":42}}
{"jsonrpc":"2.0","method":"missed","params":{"seq":57,"terminals":[...]}}
```

Each event's `data`, and how to apply it to a snapshot:

| type | data | apply |
|---|---|---|
| `created` | the whole `Terminal` record | add it |
| `lost` | `{reason, exit_code, signal, finished_ms: null}` | kept terminal remains `live`; set these, clear `pid` and drop `foreground` |
| `restarted` | the whole `Terminal` record | replace it; reattach to the new shell |
| `keep` | `{keep}` | set `keep` |
| `finished` | `{reason, exit_code, signal, finished_ms}` | set `state: "finished"` and these, and drop `foreground`; status is cleared just before (a `status` event) |
| `removed` | `{}` | drop it |
| `status` | `{status, records, reported}`: the root status (null when cleared), every record by key, whether it ever reported | replace all three |
| `turn` | `{turn, state}`: the turn counter after a turn ended, and the word it ended on | set `turn` |
| `title` | `{title}` | set `title` |
| `labels` | `{labels}`: all labels now; plus `record` (the whole `Terminal`) when it newly matches your filter | replace `labels`; with `record`, add it; with a label filter, a terminal that no longer matches leaves your view (you still get this event) |
| `size` | `{cols, rows}` | set both |
| `foreground` | `{pid, name, argv, cwd}`, or null: what runs in it now (the foreground process group's leader) and its folder; read about twice a second | set `foreground` |

Terminals on machines this core added are in the same stream, addressed as
seen from here (`rohan/t7`), in the same order and filters (`terminal`,
`children_of`, labels). When a machine's line comes back, its terminals that
changed meanwhile arrive as `created` again (replace the record) and those
that went as `removed`; forgetting the machine removes its terminals.

`run --keep` or `terminal.keep {terminal, keep}` keeps a lost terminal live
without a program until this machine's shell can start in its last observed
folder (then its start folder, then home). The original command is never
re-run. Loss closes existing attach streams with its reason; wait for
`restarted` and attach again. An exit with a code and explicit kill stay final.
Death by signal without an exit code is loss (`signaled`) for kept terminals;
non-kept behavior is unchanged. Windows reports no signals, so `signaled` never
fires there: native termination normally reports an exit code. A replacement
killed by a shutdown signal is lost again instead of closing its record.
Repeated failures back off by 1.5, 3, 6, 12, 30, then 60 seconds forever; a
program that lived at least 10 seconds resets the delay. Recovery
runs on every OS after a grace delay, but not while the core stops/restarts
or Windows reports session shutdown. During recovery, send/keys/interrupt/attach
return `unavailable`: "lost, waiting for its new shell"; retry after `restarted`.
Server stop parks kept terminals; the next core start gives them fresh shells.
Waits end on `lost` as on `finished`: they follow the program, not the kept record.

An initial spawn that times out remains claimed with `reason: "spawn_pending"`;
input/attach are unavailable until the holder answers. It may already be running,
so core does not clear the claim or start another program on a guess. Explicit
kill cancels it; a core restart reconciles the holder. A late spawn failure clears
the claim and retries if kept, or finishes if keep was disabled.

`reason` on a finished terminal: `exited`, `killed`, `server_stop`,
`host_shutdown`, `holder_lost`, `stopped_by_restart`. Kept live records may
also have `reason: "signaled"` while waiting for a new shell. `seq` rises by one per event, in order. A `heartbeat`
arrives every 15 s with the current `seq` (when no events are queued for you). A subscriber that falls behind
gets `missed` with a fresh snapshot: drop events whose `seq` is not above
it. `seq` restarts with each core process; take a fresh snapshot after
reconnecting.

## Documents

`doc.get {name, on?}` reads opaque JSON and its version. An unwritten name
returns `{value: null, version: 0}`. `doc.set {name, value, version, on?}`
writes only if the version matches, returning the new version. A stale write
fails with kind `conflict` and the current version in its message; read again
before retrying. Writing null deletes the value but retains the version, so
an old writer cannot recreate it unnoticed.

Names follow the terminal name rules. Each value is limited to 1 MiB of
serialized JSON. Documents persist atomically in `docs.json` in core state,
survive restart and handover, and disappear when that state is wiped. Core
never interprets their contents. A broken document file is warned about and
renamed to `docs.json.bad`; the core starts with empty documents rather than
making terminals unavailable. These methods are API-only, with no CLI verb.

Unfiltered event subscribers receive changes as:

```
{"jsonrpc":"2.0","method":"event","params":{"seq":43,"type":"doc","data":{"name":"herdr-app","version":2}}}
```

There is no `terminal` on a doc event. Changes on an added machine arrive in
the same stream with `machine: "rohan"` (this core's name for it). Terminal,
parent and label filters exclude doc events. The event carries no value;
call `doc.get` to read it, using `on` for another machine.

Subscribe before reading. Read documents after subscribing or reconnecting,
a `missed` notification, or a remote machine reconnect: snapshots contain
terminals only, not documents, and changes while disconnected are not replayed.
A remote stream's `missed`, and each successful establishment of its event
subscription (including the first connection), are reported locally as `missed`
to unfiltered subscribers, using the local sequence. Refetch documents on all
watched machines after it.

## Status

A terminal's status is whatever its program reported through OSC 7501, the
Program Status Protocol (`idle`, `working`,
`done`, `blocked`, `error`, ...), stored as reported. hc never infers status
from the screen. Programs that don't speak 7501 have no status.

`terminal.report` (`hc report`) applies one 7501 body to a live terminal as if
its program had written it, for a process with no terminal to write to, such
as an agent's hook. It goes through the same parser and the same records, and
attached viewers get it in their stream, so a report sent this way and one
written to the screen are indistinguishable. It is saved at once. One exception:
after a core crash, the reports in the screen's recent output are applied
again, so they win over an earlier `terminal.report` of the same record. A hook
should report a record of its own (`id=bg`), not one its program reports.
