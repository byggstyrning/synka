# Resilience laboratory

The laboratory attacks Synka's real HTTP client, sync/outbox operations, SQLite state,
and UI/engine protocol with repeatable failures. It uses loopback HTTP, private temporary
folders, dummy credentials, and synthetic project data. No Autodesk account is needed.
It never starts the installed app, connects to its named pipe, or changes system networking.

## Run it

Requirements: Windows, stable Rust with the MSVC toolchain, the .NET 8 SDK, and PowerShell 7 (`pwsh`).
Run from the repository:

```powershell
# Regression suite plus a fast pass through every ordinary resilience scenario.
pwsh -NoProfile -File tools/test-resilience.ps1

# Repeat fault injection and run the larger stress scenarios with 10 different seeds.
pwsh -NoProfile -File tools/test-resilience.ps1 -Profile Stress -Seed 42

# Keep testing for roughly 30 minutes (the current pass is allowed to finish).
pwsh -NoProfile -File tools/test-resilience.ps1 -Profile Soak -DurationMinutes 30 -Seed 42

# Reproduce a particular failing seed with the same operation count.
pwsh -NoProfile -File tools/test-resilience.ps1 -Profile Stress -Seed 47 -Iterations 1 -Operations 1000
```

Quick uses 100 configurable operations and one pass. Stress uses 1,000 operations and
10 passes. Soak uses 1,000 operations per pass, up to 10,000 passes within the time budget.
`-Iterations` overrides the pass limit. `-Operations` overrides the configurable workload;
the runner accepts up to 10,000 operations and individual scenarios can impose lower caps.
Some boundary scenarios intentionally have fixed sizes. Each iteration increments the seed.
All test execution is serial because the engine has process-wide synchronization state.
`-SkipBaseline` omits the existing regression suite when narrowing down a fault.
`-SkipUiProtocol` omits the linked-source .NET client audit for a narrower Rust-only run.
The UI protocol audit runs once per invocation and includes the real 15-second command timeout.

Each test process has a 180-second watchdog. `-PassTimeoutSeconds` changes that limit
for larger workloads. The watchdog terminates a stuck process and records a failure.
The build has a separate 10-minute timeout. A test filter that runs zero tests is a failure.
Failure stops the run immediately, preserving its seed and logs.

## Fault coverage

Network scenarios use a small scripted TCP server bound to `127.0.0.1` on an ephemeral
port. Requests pass through the production request/JSON code. Faults include transient
HTTP errors, rate limits, permanent client errors, dropped connections, connection
refusal, timeouts, malformed JSON, incomplete responses, and concurrent requests.
Retry counts, recovered responses, request bodies, and transfer headers are asserted.

Sync scenarios use the existing engine fixture with real files and a real SQLite
database. They exercise repeated local changes, save bursts, duplicate work, locked
files, failed/offline jobs, restart persistence, and mass-delete safeguards. Assertions
check queue state and file bytes, so a crash-free run alone does not count as success.
Seeded scenarios print the seed and operation count for reproduction.

IPC scenarios exercise the actual client handler over bounded in-memory streams.
They send rapid commands, fragmented/coalesced frames, malformed JSON, invalid UTF-8,
oversized lines, disconnects, and slow-reader event bursts. Assertions check acknowledgments,
recovery, cache limits, and current-state snapshots after missed events.

The linked-source C# audit also tests the real `EngineClient.cs` and `Protocol.cs` over
unique Windows named pipes. It covers fragmented Unicode, malformed messages, out-of-order
ACKs, mid-frame disconnect, a stalled engine resuming after command timeouts, and sign-in
binding notifications. The dispatcher runs inline and package/log/language services are
stubbed; the transport, serialization, pending commands, semaphore and timeout use production
code. Each audit writes source hashes and scenario evidence under `target/ui-ipc-audit/`.

Additional reality-based regressions exercise these boundaries:

- A server commits a new file/version and then loses the response. The client must return
  uncertainty for reconciliation rather than automatically replaying a non-idempotent write.
- An opaque stored object advertises gzip encoding. Download and range bytes must match
  the stored file exactly; metadata JSON can still use transparent compression.
- An HTTP 200 error envelope or a repeated/cyclic next-page link must fail the listing,
  rather than treating incomplete data as an empty/complete remote folder.
- An actual Windows directory junction replaces a registered sync folder. External files
  must not be registered or uploaded through it, including jobs queued before replacement.
- A locked source or blocked rescue destination must preserve every unsynced byte until
  a successful retry. Opening a corrupt SQLite database must not silently reset its bytes.
- Ordinary files and directories must report no placeholder metadata even before a cloud
  provider is active on their volume. The helper classifies the same open handle first;
  errors querying actual placeholders and missing files remain errors.

The original failures are preserved in `target/resilience/discoveries/*-before-fix.log`.
These tests were observed failing against the previous implementation before the fixes.
Passing the same seed repeatedly verifies regressions; it does not add new failure models.

Only ignored tests whose names contain `resilience_` run in Stress and Soak. The older
ignored tests that register a real Cloud Files sync root or open the user's real pipe
are deliberately outside this laboratory.

## Results and CI

Every invocation creates `target/resilience/<UTC timestamp>-<process>-<unique suffix>/`:

- `summary.json`: profile, seed, operation count, stage results, timing, test counts, commands,
  and the private test scratch path and filesystem.
- `junit.xml`: one test case per build/baseline/fault pass, suitable for CI report readers.
- `<stage>.log`: exact invocation, seed, captured output and Rust backtrace on panic.
- `<stage>.stdout.log` / `<stage>.stderr.log`: streamed output, including the last test
  reached before a hang or external interruption.
- `dotnet-home/`: private .NET CLI initialization state, so the audit does not need to
  write its first-run files to the user's profile.
- `runner-error.log`: runner/build/setup errors, when present.

Test processes use a separate private `synka-resilience/<run ID>/work/` directory under
a writable NTFS location. The runner checks the user's profile directory, then the checkout;
it fails if neither is writable NTFS outside system folders. Cloud Files and USN fixtures
require NTFS, while relocation fixtures reject system folders such as AppData and Windows
Temp. CI checkouts can be on a different filesystem.
The chosen path is printed and saved in `summary.json`. Fixtures clean up where possible;
leftovers are synthetic test data and may help investigate a failure. Build output,
.NET CLI state, and reports stay under the repository's `target/` directory.

The runner exits nonzero on a build error, assertion failure, crash, hang, or empty test
selection. Reports are updated after each completed stage. An external hard kill of the
runner can leave its last report marked `running`.

Pull requests and pushes to main run Quick as part of the existing CI engine gate.
The **Resilience laboratory** workflow can be dispatched manually for Stress or Soak;
it uploads logs and JSON/JUnit reports even when a test fails. CI caps requested soak
duration at 60 minutes. Local soak runs can be longer.

## Extending the laboratory

Name new fault tests `resilience_<behavior>` so the runner selects them. Use `#[ignore]`
for larger stress scenarios. Read `SYNKA_TEST_SEED` and `SYNKA_STRESS_OPS` for generated
workloads; retain a fixed default so direct `cargo test` runs work too. Keep test services
on loopback and keep account/token and endpoint overrides under `#[cfg(test)]`.
Add assertions for the intended recovery state, conserved bytes, bounded retries,
and queued work. Turn every reproducible issue into a regression test before fixing it.

These tests do not launch the WinUI window or register a real Cloud Files provider.
They cannot verify Explorer hydration, packaged startup, visual UI behavior, or actual
Autodesk behavior. Before releasing, use a separate Windows VM and disposable ACC project
to exercise those paths: rapid clicks during reconnect, edit/save while a transfer is
interrupted, kill/restart during upload, placeholder hydration/free-space cycles, and
disk-full recovery. Keep that manual work separate from normal user projects.

## Security scan notes

Aikido scans the files added or edited for this work.
Its workflow findings were fixed by disabling persisted checkout credentials and pinning
third-party actions to verified commits. The final scan retains these Rust findings
(scores below are the scanner's numeric severity):

- Unsafe operation, score 40, `src/sync/mod.rs:2750`: `LASTINPUTINFO.cbSize` is the exact
  initialized struct size passed by mutable reference to the Windows API. Reviewed as a false positive.
- Path traversal, score 85, `src/sync/mod.rs:3141`: directory enumeration follows component
  checks rejecting reparse points and checks Synka placeholder ownership. Direct traversal is guarded.
- Path traversal, score 85, `src/sync/mod.rs:4149`: rescue copying opens a source path;
  component-level reparse protection remains a hardening gap in this existing code.
- Path traversal, score 85, `src/sync/upload.rs:256`: completion uses the current item path
  or resolves the saved path through the enabled project's registered parent chain before
  queuing work. The raw path-to-file-write implication is overstated; the reparse caveat below remains.
- Path traversal, score 85, `src/sync/upload.rs:868`: the saved path is tested for existence;
  finalization uses the current item path derived from sanitized database names.
- Path traversal, score 85, upload/mkdir: `locate()` now also rejects symlink/junction
  components, with a regression using a real Windows directory junction. These are path
  checks before I/O; they are not a complete handle-based defense against an active
  same-user filesystem replacement between validation and use.
- Path traversal, score 85, `tools/ui-ipc-audit/Program.cs:82`: source hashing joins the
  discovered repository root with fixed source-file paths supplied by the audit itself.
  No user-provided path reaches this helper; reviewed as a false positive.

The remaining rescue-copy reparse concerns require local filesystem/database manipulation; these scans
do not demonstrate an escape through an Autodesk filename. They are not an all-clear result.
Aikido's IaC stage could not run because its configured Checkov executable is missing.
