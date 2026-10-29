# Security and trust

Core runs commands with your account's access. It is not a sandbox or a permission broker for agents.
Anyone who can connect to its API can run commands, read and change files, and control its terminals with that access.

[Back to the README](../README.md) · [API](../schema/README.md) · [Report a vulnerability](../SECURITY.md)

## What runs

The CLI starts a background core when it needs one.
Holder processes own the terminals so a core restart or update can leave programs running.
Programs run as the account that runs core, with its filesystem and network access.
Templates are shell commands and opening instructions, not isolation rules.

Do not run core as root or as an administrator unless you intend to give its clients that access.
A status of blocked can prevent a normal send, but it is not an access boundary against another client.

## Who can connect

On Unix, core listens on a local Unix socket, not a public TCP port.
It creates its state and socket directories with mode 0700.
It does not repair permissions on directories that already exist.
Keep custom socket paths in directories only your account can enter.
There is no separate API password or per-client permission check.
Other programs running as your account are part of the trust boundary.

On Windows, core uses named pipes. Their access rules allow the current account and SYSTEM.
The integrity label prevents a lower-integrity process from controlling an elevated core.
State files use the filesystem's directory permissions; core does not install a separate Windows state-directory ACL.

Do not expose the socket or forward the raw API to people you would not trust with a shell on that machine.

## Sessions

The default session is named default.
Setting `HC_SESSION` selects a separate core, terminals, and state directory:

```sh
HC_SESSION=scratch hc ls
```

Sessions are useful for tests and separate sets of work.
They are not security boundaries: another process under the same account can select the same session.
Config and templates are shared across sessions in the same config home.
Tests should also use scratch XDG directories so they do not touch your real state or config.

## Remote trust

SSH handles login, host keys, and encryption. Core carries its protocol through that connection.
A custom connection command must provide its own trust and transport guarantees.
Core does not add another encryption layer or authenticate a remote user itself.

Adding a machine gives local clients access to its hc account's commands, files, and terminals.
Trust the destination, the SSH target, and any custom connection command.
Machine installation or update copies executable code there and restarts that session's core.
Forgetting a machine removes the connection here, not its remote programs or state.
See [machines](machines.md).

## State and endpoints

| Item | Default |
| --- | --- |
| Unix state | `~/.local/state/herdr-core/<session>/` |
| Unix config | `~/.config/herdr-core/config.toml` |
| Windows state | `%LOCALAPPDATA%\herdr-core\<session>\` |
| Windows config | `%APPDATA%\herdr-core\config.toml` |
| Templates | A templates directory beside the config file |

`XDG_STATE_HOME` and `XDG_CONFIG_HOME` override the state and config homes on both platforms.
On Unix, sockets live under `$XDG_RUNTIME_DIR/herdr-core/` when set.
Otherwise core uses `/run/user/<uid>/herdr-core/` if that runtime directory exists, then falls back to the session's state directory.
The client socket is named `<session>.sock`; holder sockets sit beside it.
On Windows, pipe names include the account name and session.

`HC_SOCKET` overrides the client endpoint. Otherwise the socket setting in config can override it.
A custom endpoint does not move the state directory.
Keep Unix socket paths short; the OS limits their length.

State includes terminal records, saved screens and history, machine connections, peer records, and API documents.
Core and holder logs also live there. Treat the whole directory as private: terminal output and saved screens may contain secrets.
State is not encrypted by core. Removing state is destructive, not a way to stop running programs safely.

## Updates and signing

```sh
hc update
hc update --version 0.1.5
```

The version form means at least that version, not an exact downgrade.
Updates restart the core while holders keep the terminals running.
An update may also restart an older running core when the installed binary is already current.

Release installers and updates download a manifest and check the archive's SHA-256 against it.
The default release service is HTTPS at releases.herdr.dev.
This trusts the release service and HTTPS; a checksum is not an independently signed release manifest.
The manifest is not verified with a separate signing key.

Windows release signing is conditional in the release workflow.
When the signing profile is configured, the workflow signs and verifies the executable with Azure Artifact Signing.
If the profile is unset, it publishes unsigned builds. Windows may block those builds.
Do not assume every downloaded build is signed.

`hc update --from /path/to/herdr-core` installs a local build instead of downloading one.
Only use a build you trust. The updater's compatibility checks are not a review of its code.
