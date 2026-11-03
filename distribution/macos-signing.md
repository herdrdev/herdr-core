# macOS release signing and notarization

In herdr-core's GitHub **release** environment, add these secrets with the
**same values as herdr-app's release environment**:

- `APPLE_CERTIFICATE_P12`: base64-encoded Developer ID Application certificate
  exported as a `.p12`, including its private key.
- `APPLE_CERTIFICATE_PASSWORD`: the password for that `.p12`.
- `APPLE_API_KEY`: App Store Connect API private key (the contents of the `.p8`).
- `APPLE_API_KEY_ID`: that API key's ID.
- `APPLE_API_ISSUER`: that API key's issuer ID.

Tag pushes require all five secrets. Both macOS architectures are signed with
hardened runtime and notarized before packaging; rejection fails the release.
Manual/untagged release builds use ad-hoc signing without production credentials.

The signature uses the stable identifier `dev.herdr.core`. The executable has
a bound Info.plist naming **Herdr Core** and explaining privacy requests made
by terminal programs. Existing Herdr.app grants do not transfer. Grants to
ad-hoc builds may need to be granted again on the first Developer ID release.
Keep the signing team and identifier stable across updates. This does not
guarantee that macOS's session-scoped permissions stop prompting.

The hardened-runtime entitlement
`com.apple.security.cs.allow-dyld-environment-variables` preserves `DYLD_*`
for terminal programs. It does not disable library validation for core itself,
nor override restrictions in Apple-protected child programs. Core needs no JIT,
debugging or unsigned-memory exceptions. Camera, audio-input and Apple Events
entitlements permit user-approved terminal workloads; they do not grant access
without consent. There is no App Sandbox.

`scripts/mac-bundle` assembles **Herdr Core.app** with the same Info.plist as
its executable. `scripts/mac-sign` signs the executable and then the bundle.
`scripts/mac-notarize` submits the bundle zip, waits for acceptance, and staples
and validates its ticket. The existing macOS archive names contain the complete
bundle plus a root executable for released updaters. The root and bundled
executables are independently signed and notarized.

The installer and new `hc update` stage and verify the complete bundle, check
that it runs, then atomically exchange it with the installed bundle. They do
not re-sign, strip, or add quarantine. Entry points in `HC_INSTALL_DIR` (default
`~/.local/bin`) resolve to the newest bundle at the fixed physical path
`~/.local/share/herdr-core/active/Herdr Core.app`, via the `current` symlink.
Previous bundles are kept aside while their cores or holders run; later installs
reclaim unused previous bundles.
Old updaters that copy only the executable remain compatible. Run `hc update`
to complete a bare install with the bundle, even if the version is already
current; the app completes this during its update. Startup never downloads
updates. Updates keep running terminals.

After launchd detachment the core uses a SETEXEC responsibility disclaimer;
each holder does the same. Terminal programs never disclaim: the live holder
must remain their privacy subject across core restarts. Missing SPI falls back
to inherited responsibility with a diagnostic. Existing pre-upgrade holders
cannot be repaired by restarting the core: users must recreate those terminals
if privacy access is broken. Updates never kill them automatically.

## Verification

The tag release is the Developer ID/notarization check. If signing,
notarization or Gatekeeper assessment fails, the build fails and nothing
publishes. No separate manual workflow receives Apple credentials.

For native scratch checks, `tests/macos.rs` verifies launchd isolation and the
embedded identity. Compile `crates/herdr-core/tests/macos-dyld.c` with `cc`
for a non-Apple probe: with no arguments it checks that DYLD_LIBRARY_PATH equals
`/herdr-identity-test`; with core PID and caller PID arguments it checks their
coalitions differ. Start only an isolated core with scratch XDG paths and
HC_SESSION, set that DYLD value on startup, then run the probe using both
`hc exec` and `hc run`. Reject a smoke if its core.log contains
`launchd startup failed`, even if child commands succeed.

These checks do not attempt LAN access or trigger privacy prompts. A GUI Mac
still needs a separately approved Local Network test; CI success cannot prove
prompt wording, System Settings visibility or user-grant persistence.
