# Machines

Run a program on another machine and come back to it from here.
The program lives there. Losing your connection does not end it.

[Back to the README](../README.md) · [Command reference](commands.md#hc-machine)

## Add a machine over SSH

Use the same target you would give SSH, including a Host from your SSH config:

```sh
hc machine add user@host --name box
hc machine ls
hc run --on box --cwd /srv/app cargo test
```

The other machine needs hc. In an interactive terminal, add asks before copying hc there if it is missing or too old to talk to.
For a script, `hc machine add user@host --name box --install` allows that copy without asking.
When the OS and CPU match, it copies your current build. Otherwise it uses this version's released build for that target.

SSH passwords, codes, and host-key prompts need a person at a terminal.
On Windows, use SSH keys or an SSH agent; hc does not share interactive password logins there.

### Reconnect without typing a password

A password login is shared while its SSH connection stays alive. Sleep, a network
change, or the server closing it can end that login. To reconnect without a person,
SSH needs a key it can use without a prompt.

Create a key if you do not already have one (`ssh-keygen -t ed25519`), and configure
SSH to select it. For example, in `~/.ssh/config`:

```sshconfig
Host box
    HostName server.example.com
    User alice
    IdentityFile ~/.ssh/id_ed25519
    AddKeysToAgent yes
```

Use that Host alias when adding the machine (`hc machine add box`). For an existing
machine, the Host block must match its saved SSH target, not just its local hc name.

Load a passphrase-protected key into your SSH agent with `ssh-add ~/.ssh/id_ed25519`.
On macOS, Apple's `/usr/bin/ssh-add --apple-use-keychain ~/.ssh/id_ed25519` saves its
passphrase in Keychain; add `UseKeychain yes` to that Host block for Apple's SSH.
Keep the key's passphrase; hc never stores your SSH password or private key.

For an already added machine, macOS and Linux can install its public key from a
terminal using `ssh-copy-id` (install that tool first if it is missing):

```sh
hc machine login box --copy-id ~/.ssh/id_ed25519.pub
```

This explicitly adds the public key to the server. Enter the server password or
code when SSH asks. The destination must support `ssh-copy-id` (normally a Unix
SSH server); custom `--via` connections are not supported. hc then checks a fresh,
noninteractive SSH login, without reusing the shared password login, before
reconnecting. If that check fails, inspect your SSH `IdentityFile` and agent, the
server's authorized keys, and its home/`.ssh` permissions. Copying a key alone does
not prove that automatic authentication works.
For a Unix OpenSSH server, check the home directory too: a group-writable home can
cause the server to refuse a key even when `.ssh` is 700 and `authorized_keys` is
600. Remove group/other write access (`chmod go-w ~`) if appropriate for that account.

On Windows, install the public key using the server's normal procedure. To test
automatic authentication on any platform, run:

```sh
ssh -o BatchMode=yes -o ControlPath=none box exit
```

Keys do not keep the network alive during sleep. On macOS, core listens for the
system's completed-wake notification and immediately checks existing connections.
A connection that answers the check within three seconds stays open. Otherwise,
core replaces it with a fresh SSH connection, bypassing any stale shared login;
viewers reattach and the programs on the remote machine keep running.

Wake also interrupts a pending connection attempt or retry delay. For the next
30 seconds, connection attempts get three seconds and retry delays grow from
250 ms to at most one second, so Wi-Fi returning a little later does not leave
core waiting through its usual backoff. After that window, the usual timeouts
and backoff (up to 30 seconds) apply. Automatic authentication still needs working
keys or an agent. The OS must finish waking and the network must become reachable;
this is not a promise of a fixed time from opening the lid.

Other platforms retain their existing connection detection and retry behavior.

To reach a separate core session on the same host:

```sh
hc machine add user@host --name sandbox --session scratch
```

Sessions separate terminals and state, not permissions. See [security](security.md).

## Use another way in

A custom connection command receives a shell line as its last argument.
It must carry stdin and stdout unchanged and ask no questions:

```sh
hc machine add --name box --via 'ssh -p 2222 -i /home/me/.ssh/key user@host'
hc machine add --name sandbox --temp --via 'fly ssh console -a my-app --pty=false -C'
```

The command runs on your local machine. The shell line runs on the destination and connects to hc there.
Log in with that tool first if it needs credentials.
The temporary marker only helps clients show short-lived machines apart; it does not delete anything for you.

### Vercel Sandbox

The `sandbox` CLI does not carry stdin as a plain pipe, so it needs a small helper:
[`via/vercel-sandbox.py`](via/vercel-sandbox.py) (Python 3, macOS or Linux).
Log in with `sandbox login` first, and give the sandbox a timeout long enough for the work:
when it stops, its terminals stop with it.

```sh
sandbox create --name my-box --timeout 1h
hc machine add --name my-box --temp --install --via '/path/to/vercel-sandbox.py my-box'
hc run --bg --on my-box --cwd /vercel/app npm run build
```

## What runs where

Your local core keeps the machine connection and combines terminal lists and events.
The remote core owns its programs, terminal history, and files.
A local name such as `box` is your name for that connection, not a remote hostname requirement.

```sh
hc run --bg --on box --cwd /srv/app --name build cargo build
hc ls
hc read box/build --lines 100
hc attach box/build
```

A returned address such as `box/t3` points to a terminal on box.
Use the full address when sending, reading, waiting, or attaching from here.
Ctrl+\ leaves an attached terminal without ending its program.

Remote paths start at the remote user's home unless you set a working directory.
Local paths start at your current folder. Prefer explicit remote paths.

## Commands and files without a terminal

Use exec for a short command whose exact output you need:

```sh
hc exec --on box --cwd /srv/app git diff
printf 'hello\n' | hc exec -i --on box cat
```

Use file for remote reads and exact edits:

```sh
hc file read --on box /srv/app/src/main.rs --offset 1 --limit 100
hc file edit --on box /srv/app/config.txt --old 'port=3000' --new 'port=4000'
hc file write --on box /srv/app/note.txt < note.txt
```

Write replaces the whole file and creates parent folders.
Edit requires the old text exactly once. Add context if it is missing or repeated.
Repeat old/new pairs for several edits; all succeed or none do.
`hc put box/build ./report.txt` sends a local file beside that terminal and prints where it landed.

## When something goes wrong

- Check `hc machine ls` first. It shows the connection state and the last observed hc version.
- If SSH needs a password or code again, run `hc machine login box` in a terminal. On Windows, fix the keys first and use login to reconnect.
- If hc is missing or incompatible, use `hc machine update box` to copy this hc there and restart that core. Its terminals keep running.
- If the connection closes at once, check that the remote login prints no extra text to stdout. The connection carries a byte protocol, not a terminal session.
- With a custom connection, check that it passes bytes unchanged and does not prompt. Do not request a remote PTY.
- If a socket path is too long, use a shorter session name or socket path. See [state and endpoints](security.md#state-and-endpoints).

Once a machine is added, core retries a lost connection. Reattach after it returns.
To forget the connection here, use `hc machine rm box`. That does not stop its remote terminals.
