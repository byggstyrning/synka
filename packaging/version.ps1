<#
.SYNOPSIS
    Reads or sets Synka's version: [package] version in Cargo.toml and the
    root package's entry in Cargo.lock.

.DESCRIPTION
    The version has ONE source, [package] version in Cargo.toml. This script is
    the one place that writes it. new_release.ps1 (on a developer machine) and the
    Release workflow (.github\workflows\release.yml, on GitHub Actions) both call
    it, so a version is bumped the same way wherever a release is cut.

    Only the version goes to the output, as one line "X.Y.Z". Everything else is
    written to the host, so a caller can do:  $next = & .\version.ps1 -Bump patch

    Cargo.lock is refreshed with `cargo update --workspace`, which rewrites the
    root package's entry and moves no dependency. (`cargo metadata --no-deps` does
    NOT touch the lock file: a release commit made with it kept the old version in
    Cargo.lock, and every `--locked` build of the tag failed.) cargo reads the
    crates.io index for it, so the machine must be online.

    Without -Bump or -Version the script prints the current version and changes
    nothing.

.PARAMETER Bump
    patch, minor or major: the next version, counted from the current one.

.PARAMETER Version
    An explicit version, x.y.z. Wins over -Bump. It must be later than the current one.

.PARAMETER Plan
    Print the version that -Bump or -Version would set, and change nothing.

.EXAMPLE
    pwsh -NoProfile -File packaging/version.ps1                     # prints 0.1.0
    pwsh -NoProfile -File packaging/version.ps1 -Bump patch         # sets and prints 0.1.1
    pwsh -NoProfile -File packaging/version.ps1 -Bump minor -Plan   # prints 0.2.0, changes nothing
#>
[CmdletBinding()]
param(
    [ValidateSet("patch", "minor", "major")]
    [string]$Bump,
    [string]$Version,
    [switch]$Plan
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$cargoToml = Join-Path $root "Cargo.toml"
$cargoLock = Join-Path $root "Cargo.lock"
$utf8 = New-Object System.Text.UTF8Encoding $false

function Get-PackageMatch([string]$text, [string]$key) {
    # A key inside [package] only - the first `key = "..."` at a line start after
    # the [package] header. [package] is the first table in Cargo.toml.
    $at = $text.IndexOf("[package]")
    if ($at -lt 0) { return $null }
    $match = [regex]::new('(?m)^' + $key + '\s*=\s*"([^"]+)"').Match($text, $at)
    if (-not $match.Success) { return $null }
    return $match
}

function Get-LockVersion([string]$text, [string]$name) {
    # The [[package]] entry of the root package. CRLF on a Windows checkout.
    $match = [regex]::Match($text, '(?m)^name = "' + [regex]::Escape($name) + '"\r?\nversion = "([^"]+)"')
    if (-not $match.Success) { return $null }
    return $match.Groups[1].Value
}

$raw = [IO.File]::ReadAllText($cargoToml, $utf8)
$hit = Get-PackageMatch $raw "version"
$nameHit = Get-PackageMatch $raw "name"
if (-not $hit -or $hit.Groups[1].Value -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$') {
    throw "Cannot read the [package] version (x.y.z) from $cargoToml"
}
if (-not $nameHit) { throw "Cannot read the [package] name from $cargoToml" }
$current = $hit.Groups[1].Value
$name = $nameHit.Groups[1].Value

if (-not $Bump -and -not $Version) {
    if ($Plan) { throw "-Plan needs -Bump or -Version." }
    return $current
}

if ($Version) {
    if ($Version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$') { throw "-Version must read x.y.z, not '$Version'" }
    $next = $Version
}
else {
    $parts = $current.Split(".") | ForEach-Object { [int]$_ }
    switch ($Bump) {
        "major" { $next = "$($parts[0] + 1).0.0" }
        "minor" { $next = "$($parts[0]).$($parts[1] + 1).0" }
        "patch" { $next = "$($parts[0]).$($parts[1]).$($parts[2] + 1)" }
    }
}
if ([version]$next -le [version]$current) {
    throw "'$next' is not later than the current $current. A version number only goes forwards."
}
if ($Plan) { return $next }

# cargo is found BEFORE anything is written: half a bump is worse than none.
$cargo = (Get-Command cargo -ErrorAction SilentlyContinue).Source
if (-not $cargo -and $env:USERPROFILE) {
    $candidate = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
    if (Test-Path $candidate) { $cargo = $candidate }
}
if (-not $cargo) { throw "cargo was not found; it refreshes Cargo.lock. Install Rust: winget install Rustlang.Rustup" }

$updated = $raw.Substring(0, $hit.Index) + ('version = "' + $next + '"') + $raw.Substring($hit.Index + $hit.Length)
if ((Get-PackageMatch $updated "version").Groups[1].Value -ne $next) { throw "The version line was not rewritten in $cargoToml" }
[IO.File]::WriteAllText($cargoToml, $updated, $utf8)
Write-Host "   Cargo.toml: $current -> $next"

# cargo writes ordinary progress to stderr; PowerShell 5.1 would turn it into
# errors under Stop. Lines are stringified; only the exit code decides.
$previous = $ErrorActionPreference
$ErrorActionPreference = "Continue"
try {
    & $cargo update --workspace --manifest-path $cargoToml 2>&1 | ForEach-Object { Write-Host "   $_" }
    $exitCode = $LASTEXITCODE
}
finally { $ErrorActionPreference = $previous }
if ($exitCode -ne 0) {
    throw "cargo update --workspace failed (exit code $exitCode). Cargo.toml already says $next; undo with: git checkout -- Cargo.toml Cargo.lock"
}

$locked = Get-LockVersion ([IO.File]::ReadAllText($cargoLock, $utf8)) $name
if ($locked -ne $next) { throw "Cargo.lock says '$locked' for $name after cargo update, not $next." }
Write-Host "   Cargo.lock: $name $next"

return $next
