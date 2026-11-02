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

`scripts/mac-notarize` submits a zip and waits for acceptance. A bare executable
cannot be stapled; Gatekeeper obtains its notarization ticket online. The
distributed tarball contains the exact signed executable that was submitted.

The installer downloads, extracts, copies, chmods and renames the binary (or
delegates to `hc update --from`). `hc update` stages a copy, chmods it, checks
it runs, then renames it into place. Neither re-signs, strips, nor adds
quarantine. Herdr's core installer delegates to these same paths. The embedded
plist and signature survive these operations unchanged.

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
