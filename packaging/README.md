# Synka packaging

Synka ships as a sideloaded, self-signed MSIX, plus a zip that a recipient double-clicks. It is
internal only: there is no public download and no public update channel. Colleagues install and
update it through BoomerHubb, from the private release channel `VilhelmH-git/Synka-releases`. The package has two
processes:

| Process | What | Lifetime |
|---|---|---|
| `Synka.exe` (WinUI 3, `ui\`) | the window | only while open |
| `engine\synka-engine.exe` (Rust, `engine\` in the package) | sync, tray icon | always, starts at sign-in |

The package is **single-project MSIX**: the manifest is `ui\Package.appxmanifest`,
and `dotnet build` of `ui\Synka.csproj` produces the `.msix` directly. It is
framework-dependent (Windows App SDK). The `Microsoft.WindowsAppRuntime` package
is laid out next to it and installed first.

## Files

| File | Purpose |
|---|---|
| `build_msix.ps1` | Tests, `cargo build --release`, the Release MSIX build, verification of the package contents, optional signing of the engine and MSIX (`-Sign` from the existing certificate store key, `-PfxPath` from a file) and install (`-Install`). CI calls it too. |
| `make_delivery.ps1` | Packs the signed package, the framework, `FastADC.cer` and `delivery\` into `dist\Synka-<ver>-sidladdning.zip`. CI calls it too. |
| `assert-package-engine.ps1` | Checks the embedded engine's signing certificate and timestamp; the build also compares its SHA-256 with `target\release\synka-engine.exe`. Delivery and resumed releases use the same checks. |
| `version.ps1` | Reads the version, or bumps it in `Cargo.toml` and `Cargo.lock` (`-Bump patch`). The one place that writes it, locally and in CI. |
| `share_release.ps1` | Copies the sideload zip into the shared OneDrive folder `Synka` (or `-TargetFolder`). Keeps the newest 5 versions and `Synka-latest.zip`. |
| `new_release.ps1` | Current local release route: serial tests, version bump, tag, locally timestamped signing, ZIP, release gate, private release, the same release on `VilhelmH-git/Synka-releases`, the BoomerHubb catalog entry, and optional OneDrive copy. |
| `make_icons.py` | Draws the Synka icon. `--assets` regenerates both ICOs (engine and window) plus every MSIX tile, splash and title-bar PNG in `ui\Assets`. |
| `FastADC.cer` | The public certificate, `CN=FastADC`. Not secret. Recipients trust it; CI refuses to sign with any other key. |
| `delivery\` | Handwritten recipient files: `Install.cmd`, `Install.ps1`, `README.txt` (Swedish). Checked out with CRLF (`.gitattributes`). |

## The version

`[package] version` in `Cargo.toml` is the only version source. The csproj's
`StampPackageVersion` target writes it into the manifest as `X.Y.Z.0`, and
`build_msix.ps1` checks that the built package says the same. The tag is made
from it, never the reverse. `version.ps1 -Bump patch` sets it in `Cargo.toml`
and refreshes `Cargo.lock` with `cargo update --workspace` (online), so the tag
builds with `--locked`.

## Build

Unsigned, for a quick check. Tests are skipped here:

    powershell -NoProfile -File packaging\build_msix.ps1 -SkipTests

Output: `ui\AppPackages\Synka_<ver>.0_x64_*\Synka_<ver>.0_x64.msix`, unsigned.
Store upload needs the Identity and Publisher that Partner Center assigns; then the
Store signs it.

Signed, for sideloading:

    powershell -NoProfile -File packaging\build_msix.ps1 -SkipTests -Sign

This requires the existing `CN=FastADC` private key in `Cert:\CurrentUser\My`
matching the committed `packaging\FastADC.cer`. It signs and timestamps the engine
before packaging, then signs and timestamps the MSIX with the same key. A missing
or different key stops the build; the script creates no key and leaves the
committed certificate unchanged.

Signed builds, delivery creation and resumed releases require Windows to report
both signatures as `Valid`, with the expected signer and a timestamp. The release
machine must already trust the existing public certificate. `UnknownError` is a
failure, including when trust has not been configured. No packaging script adds
trust automatically. If that setup is needed, first inspect the thumbprint of the
committed `packaging\FastADC.cer`, then explicitly import that same certificate
from an elevated PowerShell on the release machine:

    Import-Certificate -FilePath "C:\apps\synka\packaging\FastADC.cer" -CertStoreLocation Cert:\LocalMachine\TrustedPeople

Signed with a key from a file, the way the Release workflow does it (never
creates a key; refuses one whose certificate is not `packaging\FastADC.cer`):

    & .\packaging\build_msix.ps1 -SkipTests -PfxPath C:\keys\FastADC.pfx -PfxPassword (Read-Host -AsSecureString)

`-SkipEngineBuild` packs the engine already in `target\release` instead of
running `cargo build --release` (CI builds the engine in its own step). With
`-Sign` or `-PfxPath`, that reused engine is still signed before packaging.
`make_delivery.ps1 -SkipBuild` requires exactly one MSIX for the version in
`Cargo.toml`, verifies its manifest and both the MSIX and embedded engine
signatures and timestamps, and takes the matching x64
Windows App Runtime from that MSIX's own `Dependencies\x64` folder. It stops
if the package or framework is missing or ambiguous.

A build leaves the running `Synka.exe` and `synka-engine.exe` alone.
With `-Install`, Windows closes the target app during an in-place update.

Signed and installed on this machine:

    powershell -NoProfile -File packaging\build_msix.ps1 -SkipTests -Sign -Install

`-Install` also needs the certificate trusted for sideloading. Use the existing
certificate and an explicit admin import when that installation trust is missing:

    Import-Certificate -FilePath "C:\apps\synka\packaging\FastADC.cer" -CertStoreLocation Cert:\LocalMachine\TrustedPeople

`-Install` installs the framework first, then a **newer** app version in place.
It refuses an equal or older installed version; increase `Cargo.toml`'s version
before using it to deploy another build. The user's `LocalState` stays in place.
The recipient's `Install.cmd` skips an already installed equal version.

## Signing certificate

Since 2026-10-21 releases are signed with the key `CN=FastADC` `F1085485D4950C413AFE6A6C1E1F9F63A16D54FE`
(valid to 2031-10-20), committed as `packaging\FastADC.cer`. It replaced the earlier `CN=FastADC`
key `03F6B767F3CFC690AB55333AAE6093AC0C8CF233`. The Publisher is unchanged, so the package
identity is the same; a machine needs the new certificate trusted (`Install.cmd`, or BoomerHubb)
before it accepts a package signed with it, and a build with the old `.cer` embedded will not
accept an update signed with the new key.

> **Later, if wanted: a certificate with another name.** The package Publisher and the signing
> certificate stay `CN=FastADC` for now. Create a new trusted certificate (for example
> `CN=Byggstyrning`), distribute it to every recipient, then change the Publisher in
> `ui/Package.appxmanifest`, `PACKAGE_PUBLISHER` / `SIGNING_CERT_DER` in the updater,
> `-PublisherCN` in `build_msix.ps1`, the checks in `release.yml` and `new_release.ps1`, and the
> `.cer` file. A Publisher change is another new package identity: uninstall and reinstall
> again on every machine.

## The signing key (read before the first release)

The key is the identity of the app. A release signed with a new key installs on no
machine that trusts the old one, and every recipient would have to import the new
certificate again.

- When configured, GitHub Actions signs with the key in the `SIGNING_PFX_BASE64` secret, and
  `build_msix.ps1 -PfxPath` refuses it unless its certificate is the committed
  `packaging\FastADC.cer`. A new key is introduced by committing its certificate
  there first.
- `new_release.ps1` signs on this machine and refuses to start without a `CN=FastADC` certificate with a
  private key in `Cert:\CurrentUser\My`. It also refuses a key other than the one
  this machine trusts, unless `-NewCertificate` is given.
- **First setup on a release machine:** restore the existing release key from
  its password-protected `.pfx` backup. The public `.cer` alone contains no
  private key. Import the backup into `Cert:\CurrentUser\My` for `-Sign` and
  `new_release.ps1`, or use `-PfxPath` for a direct build without importing it.
  Check that its certificate thumbprint matches `packaging\FastADC.cer`.

      Import-PfxCertificate -FilePath C:\keys\FastADC.pfx -CertStoreLocation Cert:\CurrentUser\My -Password (Read-Host -AsSecureString "PFX password")

  Keep the backup outside git and outside the shared folder. If the existing key
  is unavailable, recover it from the release machine or backup before building;
  running `-Sign` does not generate a replacement.
- The release machine needs the public certificate trusted for signature
  verification, even when not installing the app. The explicit trust imports
  described above are separate from restoring the private key; neither happens
  automatically during packaging.
- Check the existing certificate's expiry. `new_release.ps1` and signed builds
  warn 30 days ahead.

## Recipient install

### Notification icon identity across updates

The engine uses one permanent Windows notification-icon GUID,
`6e1b0f02-70be-4768-a154-f30c67ca1912`, for packaged Release runs. Keep it unchanged
between releases. Debug and unpackaged runs do not register this production GUID.
Unsigned CI packages are build artifacts; sign the engine and rebuild with `-Sign`
before installing a Release package for a tray-identity test.

WindowsApps installation paths include the package version. Windows preserves an
icon's saved visibility preference across that path change only when its GUID stays
the same and both executable versions carry the same Authenticode publisher identity.
The release build therefore signs and timestamps `synka-engine.exe` before packaging,
then signs the MSIX. It checks the actual engine inside the package as well.
Signing the MSIX container alone does not sign its embedded executable.
See [Microsoft's notification-icon identity rules](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/ns-shellapi-notifyicondataw#troubleshooting).

Older releases used a path-based icon identity. Windows may require one final
visibility choice when moving to the permanent GUID. The app does not rewrite the
user's notification-area preferences or force icons out of overflow; Windows owns
the final arrangement.

To validate a release upgrade, use a disposable Windows account or VM: show the
packaged icon beside the clock, place it among the other icons, then update through
the Info page to another signed version. Check that the icon remains visible, both
click actions work, and the tooltip updates. Also check recovery after an Explorer
restart. Test with the icon hidden too, so an update respects that choice.

### Install from the ZIP

The recipient unpacks the whole zip and double-clicks `Install.cmd`. Only the
certificate import needs admin, and it runs in its own elevated window. The
framework and the app are installed as the recipient, so the app lands in the
right user profile. The app is started at the end. See `delivery\README.txt`.

### Upgrading from the earlier names

Earlier packages (Better Desktop Connector, FastADC) have different package identities. `Install.cmd`
asks to remove the old package first, then installs Synka. The user's sync folder and
database are not part of the package; see the migration steps in `docs/ROADMAP.md`.

## Local release (current route)

Pushing a tag does **not** start `release.yml`; that workflow is dispatch-only.
The signing key stays on this machine. `new_release.ps1` signs and timestamps the
engine and MSIX locally, then publishes the same signed package, certificate and ZIP
on the private source repository and on the private release channel
`VilhelmH-git/Synka-releases`, then points the `synka` entry of the BoomerHubb catalog
(`VilhelmH-git/catalog`) at it. There is no public mirror. The release gate and the catalog step
are in claude-kit (`toolselease-scan`, `tools\catalog`), cloned beside this repository or named
by `CLAUDE_KIT`; the script stops before the version bump when either is missing, or when `gh`
cannot write to the channel.

1. Commit the finished source on `main`, push it and wait for CI to pass. Run the
   serial tests and Clippy locally if changes landed after that CI run.
2. Write Swedish change notes to a UTF-8 Markdown file outside the checkout
   (or commit it first, since the release needs a clean Git tree). Preview the
   next version and signing key with:

        powershell -NoProfile -ExecutionPolicy Bypass -File .\packaging\new_release.ps1 -Bump patch -Notes "$env:TEMP\synka-whats-new.md" -DryRun

3. Cut the patch release:

        powershell -NoProfile -ExecutionPolicy Bypass -File .\packaging\new_release.ps1 -Bump patch -Notes "$env:TEMP\synka-whats-new.md"

   The script runs `cargo test --locked -- --test-threads=1`, bumps Cargo.toml
   and Cargo.lock, commits and tags the version, pushes `main` and the tag,
   builds the signed MSIX, packs the ZIP, publishes the private release, and
   copies the ZIP to OneDrive unless `-NoShare` is set. It never uploads an
   unsigned MSIX. If packaging fails after the tag is pushed and before the
   private release is created, use `-OnlyPublish` from that same tagged source
   commit after fixing the cause. Reused packages must still have a correctly
   signed and timestamped engine inside the signed MSIX.
4. Compare the ZIP, MSIX and certificate hashes with the local artifacts. Install
   the ZIP on a test machine and confirm an update preserves `LocalState`.

Local upgrade validation used versions after `v0.1.0`. The patch version is
computed from `Cargo.toml`; for example, `0.1.2` bumps to `v0.1.3`. Confirm
the actual release version with `-DryRun`.

## Release from GitHub Actions

This path is available for future releases when signing secrets are configured.
It needs no developer machine, no local certificate and no `gh` login.

| Workflow | When | What |
|---|---|---|
| `.github/workflows/ci.yml` | pull requests, pushes to main, by hand | `cargo test`, `cargo clippy`, `cargo build --release`, then the unsigned MSIX via `build_msix.ps1 -SkipEngineBuild`. Engine and package are kept as run artifacts for 7 days. |
| `.github/workflows/release.yml` | **Actions -> Release -> Run workflow** | bump, tag, build, sign, zip, publish to the private repository |

### Once: secrets

1. **Export the signing key.** On the machine that holds the `CN=FastADC` key
   (the one whose certificate is `packaging\FastADC.cer`), in PowerShell:

        $expected = [Security.Cryptography.X509Certificates.X509Certificate2]::new((Resolve-Path packaging\FastADC.cer).Path)
        $cert = Get-ChildItem Cert:\CurrentUser\My | Where-Object { $_.Thumbprint -eq $expected.Thumbprint -and $_.HasPrivateKey } | Select-Object -First 1
        if (-not $cert) { throw "The existing release private key is missing." }
        Export-PfxCertificate -Cert $cert -FilePath .\FastADC.pfx -Password (Read-Host -AsSecureString "PFX password")
        [Convert]::ToBase64String([IO.File]::ReadAllBytes('FastADC.pfx')) | Set-Clipboard

   Keep `FastADC.pfx` somewhere safe, outside git and outside OneDrive, and
   delete the copy in the current folder once the secret is saved.
2. **Repository secrets** of `byggstyrning/synka` (Settings -> Secrets and
   variables -> Actions -> New repository secret):

   | Secret | Value | Needed |
   |---|---|---|
   | `SIGNING_PFX_BASE64` | the base64 text from step 1 (the clipboard) | yes |
   | `SIGNING_PFX_PASSWORD` | the PFX password from step 1 | yes |

   Without the two signing secrets the release stops before anything is tagged.
   Remove any `RELEASES_TOKEN` secret left from earlier setups.
3. If `main` is protected, allow GitHub Actions to push to it: the release
   pushes its `chore: version X.Y.Z` commit and the tag with `GITHUB_TOKEN`.

### Cut a release

**Actions -> Release -> Run workflow**, branch `main`, choose `bump` (patch,
minor, major) and optionally write `notes` (Swedish, what is new; empty = the
list of commits since the previous tag). Then:

1. **bump** (Linux): refuses unless `ci.yml` passed on that commit (waits while
   it is still running), and refuses a commit that is already released. Then
   `version.ps1` bumps `Cargo.toml` and `Cargo.lock`, commits
   `chore: version X.Y.Z`, creates the annotated tag `vX.Y.Z` (the notes go in
   its message) and pushes both in one atomic push.
2. **build** (Windows): checks out the tag, checks it against `Cargo.toml`,
   `cargo test`, `cargo build --release`, then `build_msix.ps1 -PfxPath` with the
   key from `SIGNING_PFX_BASE64` (refused unless it is `CN=FastADC` and the same
   certificate as `packaging\FastADC.cer`), `signtool verify`, and
   `make_delivery.ps1 -SkipBuild`. The `.pfx` exists only in the runner's temp
   folder during that step.
3. **publish** (Linux): a GitHub release `vX.Y.Z` on this repository with
   `Synka-X.Y.Z-sidladdning.zip`, `Synka_X.Y.Z.0_x64.msix` (signed) and
   `FastADC.cer`, marked latest.

The OneDrive copy is not part of it (a runner has no OneDrive). On a machine
with the shared folder, afterwards: `powershell -NoProfile -File packaging\share_release.ps1`
(it downloads the newest release's zip).

A run that failed **after the bump** has spent its version (the tag is on
GitHub). When the cause is outside the code (a secret or the runner), fix it and use
**Re-run failed jobs**: that builds and publishes the same tag, and publishing is idempotent
(a re-run replaces the release's files instead of failing on the existing release).
*Re-run all jobs* stops at the bump, because the tag exists. When the code at the tag is
broken, push the fix and release again; the tag stays behind as a version that was never published.

A tag pushed by hand does not start this workflow. Use the local signed release
steps above while the signing key is kept on this machine.

## Release rule

Every push to `main` that changes the app (`src/`, `ui/`, `packaging/`, `Cargo.toml`,
`Cargo.lock`) is followed by a release in the same session: the local signed
route above, or the GitHub Actions dispatch when signing secrets are configured.
A push that only changes docs or tooling does not release. The
release's own `chore: version` commit does not trigger another release (it is
pushed with `GITHUB_TOKEN`, which starts no workflow). Several commits that
belong together are pushed together and released once. Every release spends a
version number, and the same version cannot be installed twice.

## Known gaps

- A release from a developer machine needs `git`, `gh` and `cargo`, and without
  the Release workflow also a signing key. None of that is installed by these
  scripts. A release from GitHub Actions needs only the signing secrets above.
- The release gate runs in `new_release.ps1` only; the GitHub Actions route has none yet.
  `new_release.ps1` has a marked hook for it; in CI it belongs in the publish job,
  before the first `gh release` call.
- The existing `v0.1.0` package has no timestamp. `build_msix.ps1` now requests
  and verifies an RFC 3161 timestamp while signing future releases; if the
  timestamp service is unavailable the build stops. Plan the new certificate
  before the current one expires on 2031-10-20: see the TODO above, commit it as
  `packaging\FastADC.cer`, use the matching local signing key (and replace the
  workflow secrets if that route is enabled), and have every recipient run
  `Install.cmd` once more to trust it. Existing installs pin the old key.
- The in-app update installs the app package only. A release that needs a newer
  Windows App SDK framework than the recipient has must be installed from the
  zip (`Install.cmd`), which carries the framework.
- `.gitignore` lists `dist/` and `*.pfx`. `packaging/FastADC.cer` is safe to commit.
