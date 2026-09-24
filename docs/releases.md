# Release process

Tagging `vX.Y.Z`, `vX.Y.Z-alpha.N`, `vX.Y.Z-beta.N` or `vX.Y.Z-rc.N` runs the release workflow.
Tags are immutable and must be published in increasing version order.
The workflow stamps the tag version into Cargo metadata before building; `ballast --version` and the running daemon report that exact version.
Debian prereleases replace the hyphen with `~`, so stable versions upgrade correctly.

CI must pass formatting, strict clippy and all default tests on macOS and Linux before builds start.
Native builds produce four tarballs, two Debian packages, a Homebrew formula, a signed APT repository archive and `SHA256SUMS`.
Linux builds use Ubuntu 22.04 for glibc compatibility.
Rust and GitHub Actions are pinned; hosted OS images and system package security updates still evolve, so this is a repeatable procedure rather than a claim of byte-identical binaries across dates.

The formula installs prebuilt macOS binaries without a Homebrew service declaration.
Debian packages contain no maintainer scripts and never edit user homes.
Every user runs `ballast install` separately.
Package upgrades rely on stable executable paths and the daemon's replacement detection.

## Repository configuration

| Setting | Purpose |
| --- | --- |
| `TAP_DEPLOY_KEY` Actions secret | Dedicated SSH private key, with write access only to `anur4ag/homebrew-tap`. |
| `APT_SIGNING_KEY` Actions secret | Dedicated ASCII-armored APT signing private key. |
| `APT_SIGNING_FINGERPRINT` Actions variable | Public key fingerprint used to sign the repository. |
| `PUBLISH_PACKAGES=true` Actions variable | Explicitly permits Pages deployment for non-test tags. |

Public key: [packaging/key.gpg](../packaging/key.gpg).
Fingerprint: `8E6D CDA1 BA85 507B B7F6 2B8F 707B F744 6DB5 1F5E`.
The file is ASCII-armored and is installed with an `.asc` suffix in APT's keyring directory.
Keep private key material only in Actions secrets, never in a worktree, release asset or workflow log.
If replacing the key, publish and distribute the new public key before switching the signing fingerprint.

A release first uploads assets to a draft, updates the tap, then publishes the release.
The APT archive carries previous published packages forward so pinned older versions remain available.
Pages publishes the same signed directory when enabled.
If publication fails after creating a draft, inspect the failing step, delete that incomplete draft, and rerun the workflow; do not replace a published release's assets.
A Pages-only failure can be retried by rerunning the failed job.

## First public release

Complete package and service verification and the remaining desktop/agent checks before publishing.
The repositories stay private during preparation.

After explicit approval to publish:

1. Make `anur4ag/ballast` and `anur4ag/homebrew-tap` public.
2. Configure Ballast's GitHub Pages source as GitHub Actions and allow the release tag in the `github-pages` environment.
3. Set `PUBLISH_PACKAGES=true` and confirm the secrets and public fingerprint are present.
4. Tag the reviewed commit `v0.1.0-alpha.1` and push that tag.
5. Confirm release assets, tap installation, APT signature verification and Pages availability before announcing the release.

## Private dry runs

Use throwaway `v0.0.0-test.N` tags.
They produce the same assets and a prerelease, and update the private tap, but cannot deploy Pages.
Download assets through authenticated `gh release download` while the repository is private.
For Homebrew verification, substitute only the generated formula's archive URL with a local downloaded archive; retain its published SHA-256.
Test first install, upgrade and removal through Homebrew and through a signed `file://` APT repository in a disposable Linux VM.
Always isolate agent configs, Ballast home and service labels, and disable notifications.

After verification, remove test releases and tags and restore the private tap to its pre-test commit.
Keep the test evidence outside the repository, including run URLs, package hashes, signatures, version transitions and cleanup results.

## README captures

The committed `docs/images/top-*.ansi` files are actual terminal output from the synthetic fleet fixture.
They retain the fixture's labels and simulated pressure readings.
Regenerate the SVGs with:

```sh
python3 scripts/release/render-capture.py docs/images/top-dark.ansi docs/images/top-dark.svg dark
python3 scripts/release/render-capture.py docs/images/top-light.ansi docs/images/top-light.svg light
```
