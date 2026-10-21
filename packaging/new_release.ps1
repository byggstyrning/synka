<#
.SYNOPSIS
    Cuts a Synka release from this machine: tests, bumps the version,
    tags, signs locally, and publishes the signed package and delivery zip.

.DESCRIPTION
    The version has ONE source: [package] version in Cargo.toml. The tag is made
    from it, never the other way round. version.ps1 writes it, here and in CI.
    The GitHub release workflow is workflow_dispatch-only: pushing a tag never
    starts it. This script always builds and signs locally:

      1. Preconditions as above, plus the signing key CN=FastADC in
         CurrentUser\My with its private key; the certificate this machine trusts
         is the same key (unless -NewCertificate).
      2. cargo test - before any tag exists, so a failure costs nothing.
      3. Bump Cargo.toml and Cargo.lock (version.ps1), commit "chore: version X.Y.Z",
         annotated tag vX.Y.Z, push main, then push the tag.
      4. build_msix.ps1 -Sign -SkipTests     timestamped engine and MSIX, same key
         (build_msix.ps1 checks
         the engine is inside the package and the manifest carries the version)
      5. make_delivery.ps1 -SkipBuild        the sideload zip
      6. The release gate (claude-kit tools\release-scan\scan_package.py) over the
         zip and the signed package; a finding stops with nothing published.
      7. Create or resume a draft release on THIS repository; upload missing
         assets, verify their bytes, then publish it.
      8. The same, signed, on the release channel VilhelmH-git/Synka-releases
         (private), which BoomerHubb reads through its GitHub App, then point
         the BoomerHubb catalog's "synka" entry at it (claude-kit
         tools\catalog\set_entry.py). The catalog step never sinks a release.
      9. share_release.ps1: the zip into the shared OneDrive folder, after GitHub.

    THE SIGNING KEY. The engine and MSIX use the existing CN=FastADC private key
    matching packaging\FastADC.cer; build_msix.ps1 never creates a new key.
    This script also refuses a key that differs from the one the machine trusts
    unless -NewCertificate says a new key is intended. -DryRun reports the state
    instead of failing.

    -OnlyPublish starts at step 4 for a tag that is already on origin, for a
    release whose publishing failed. It reuses a signed local package and zip
    when available, checks the embedded engine's signer and timestamp too, and
    resumes a partial GitHub release without replacing assets that differ from
    the local files.

.PARAMETER Bump
    patch (default), minor or major. Ignored with -Version.

.PARAMETER Version
    An explicit version, "1.2.0".

.PARAMETER Notes
    A Markdown file with what is new (Swedish), appended to the release notes.

.PARAMETER OnlyPublish
    Skip the bump, commit and tag: build and publish, on this machine, the version
    Cargo.toml already says, whose tag must be on origin.

.PARAMETER ShareFolder
    Where the zip is copied for colleagues. Default: share_release.ps1's own, $env:OneDriveCommercial\Synka.

.PARAMETER NoShare
    Publish on GitHub only; do not copy into the shared folder.

.PARAMETER NewCertificate
    Sign with a CN=FastADC key that is not the one this machine trusts. Every
    recipient must then import the new certificate; say so in -Notes.

.PARAMETER DryRun
    Says what it would do and changes nothing.

.EXAMPLE
    powershell -NoProfile -File packaging\new_release.ps1 -DryRun
    powershell -NoProfile -File packaging\new_release.ps1
    powershell -NoProfile -File packaging\new_release.ps1 -Bump minor -Notes $env:TEMP\synka-whats-new.md
    powershell -NoProfile -File packaging\new_release.ps1 -OnlyPublish
#>
[CmdletBinding()]
param(
    [ValidateSet("patch", "minor", "major")]
    [string]$Bump = "patch",
    [string]$Version,
    [string]$Notes,
    [switch]$OnlyPublish,
    [string]$ShareFolder,
    [switch]$NoShare,
    [switch]$NewCertificate,
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$dist = Join-Path $root "dist"
$publisher = "CN=FastADC"
# The release channel BoomerHubb installs from. Private, like every releases
# repository in the catalog: BoomerHubb reads it through the GitHub App
# "Byggstyrning Releases", which must be installed on it.
$channel = "VilhelmH-git/Synka-releases"
$catalogId = "synka"
# The release gate and the catalog script live in the private claude-kit, cloned
# beside this repository. CLAUDE_KIT overrides the place.
$kit = if ($env:CLAUDE_KIT) { $env:CLAUDE_KIT } else { Join-Path (Split-Path $root -Parent) "claude-kit" }
$gate = Join-Path $kit "tools\release-scan\scan_package.py"
$setEntry = Join-Path $kit "tools\catalog\set_entry.py"
$python = (Get-Command python -ErrorAction SilentlyContinue).Source
$versionScript = Join-Path $PSScriptRoot "version.ps1"
$cargo = (Get-Command cargo -ErrorAction SilentlyContinue).Source
if (-not $cargo) { $cargo = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe" }

function Invoke-Git {
    # git writes ordinary information to stderr; PowerShell 5.1 would turn it into
    # errors. Lines are stringified inside the pipeline; only the exit code decides.
    param([string[]]$Arguments, [switch]$Quiet)
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $output = & git -C $root @Arguments 2>&1 | ForEach-Object { "$_" }
        if ($LASTEXITCODE -ne 0) {
            $output | ForEach-Object { Write-Host "   $_" }
            throw "git $($Arguments -join ' ') failed (exit code $LASTEXITCODE)"
        }
        if ($Quiet) { return $output }
        $output | ForEach-Object { Write-Host "   $_" }
    }
    finally { $ErrorActionPreference = $previous }
}

function Invoke-Gh {
    # From the repository root, whatever the current directory: gh reads the
    # repository off the folder it runs in.
    param([string[]]$Arguments)
    Push-Location $root
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $output = & gh @Arguments 2>&1 | ForEach-Object { "$_" }
        if ($LASTEXITCODE -ne 0) { throw "gh $($Arguments -join ' ') failed: $($output -join ' ')" }
        return $output
    }
    finally {
        $ErrorActionPreference = $previous
        Pop-Location
    }
}

function Invoke-Step {
    # One step: a script or a program. Stops on a non-zero exit code.
    param([string]$What, [scriptblock]$Body)
    Write-Host ""
    Write-Host "== $What ==" -ForegroundColor Cyan
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    $global:LASTEXITCODE = 0
    try {
        & $Body
        if ($LASTEXITCODE -ne 0) { throw "$What failed (exit code $LASTEXITCODE)" }
    }
    finally { $ErrorActionPreference = $previous }
}

function Get-ReleaseInfo([string]$Repository, [string]$Tag) {
    try {
        $json = Invoke-Gh @("release", "view", $Tag, "--repo", $Repository, "--json", "url,isDraft,assets")
        return (($json -join "`n") | ConvertFrom-Json)
    }
    catch {
        # Only a missing tag/release is an invitation to create it. Authentication,
        # rate-limit and transport errors must stop before any publication.
        if ($_.Exception.Message -match '(?i)(release not found|HTTP 404|Not Found \(HTTP 404\))') {
            return $null
        }
        throw
    }
}

function Assert-ReleaseAssets([string]$Repository, [string]$Tag, $Release, [System.IO.FileInfo[]]$Files) {
    $expected = @($Files | ForEach-Object { $_.Name })
    foreach ($asset in @($Release.assets)) {
        if ($expected -notcontains $asset.name) {
            throw "$Repository $Tag contains unexpected asset '$($asset.name)'; inspect it before publishing."
        }
    }
    foreach ($file in $Files) {
        $assetMatches = @($Release.assets | Where-Object { $_.name -eq $file.Name })
        if ($assetMatches.Count -ne 1) {
            throw "$Repository $Tag must contain exactly one $($file.Name) asset; found $($assetMatches.Count)."
        }
        if ($null -ne $assetMatches[0].size -and [long]$assetMatches[0].size -ne $file.Length) {
            throw "$Repository $Tag has a different-sized $($file.Name) asset. Existing assets are never replaced."
        }
        if ($assetMatches[0].state -and $assetMatches[0].state -ne "uploaded") {
            throw "$Repository $Tag asset $($file.Name) is not fully uploaded (state $($assetMatches[0].state))."
        }
        $localHash = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash
        $digest = [string]$assetMatches[0].digest
        if ($digest -match '^sha256:([0-9a-fA-F]{64})$') {
            if ($localHash -ne $Matches[1]) {
                throw "$Repository $Tag has a different $($file.Name) asset (local SHA256 $localHash, remote $digest). Existing assets are never replaced."
            }
            continue
        }
        # Older GitHub responses may omit digest. Download to a unique empty
        # folder and hash the actual release asset instead of relying on size.
        $downloadDir = Join-Path ([IO.Path]::GetTempPath()) ("synka-release-verify-" + [guid]::NewGuid().ToString("N"))
        New-Item -ItemType Directory -Path $downloadDir | Out-Null
        $downloaded = Join-Path $downloadDir $file.Name
        try {
            Invoke-Gh @("release", "download", $Tag, "--repo", $Repository,
                "--pattern", $file.Name, "--dir", $downloadDir) | Out-Null
            if (-not (Test-Path -LiteralPath $downloaded)) {
                throw "Could not download $($file.Name) from $Repository $Tag for verification."
            }
            $remoteHash = (Get-FileHash -LiteralPath $downloaded -Algorithm SHA256).Hash
            if ($localHash -ne $remoteHash) {
                throw "$Repository $Tag has a different $($file.Name) asset (local SHA256 $localHash, remote SHA256 $remoteHash). Existing assets are never replaced."
            }
        }
        finally {
            Remove-Item -LiteralPath $downloaded -Force -ErrorAction SilentlyContinue
            Remove-Item -LiteralPath $downloadDir -Force -ErrorAction SilentlyContinue
        }
    }
}

function Publish-Release([string]$Repository, [string]$Tag, [string]$Title,
    [string]$NotesFile, [System.IO.FileInfo[]]$Files, [switch]$VerifyTag, [switch]$Latest) {
    $release = Get-ReleaseInfo $Repository $Tag
    if (-not $release) {
        # Drafts stay out of /releases/latest until every asset is uploaded and
        # verified. A failure leaves a release that -OnlyPublish can resume.
        $create = @("release", "create", $Tag, "--repo", $Repository,
            "--title", $Title, "--notes-file", $NotesFile, "--draft")
        if ($VerifyTag) { $create += "--verify-tag" }
        Invoke-Gh $create | ForEach-Object { Write-Host "   $_" }
        $release = Get-ReleaseInfo $Repository $Tag
        if (-not $release) { throw "$Repository $Tag was created but cannot be viewed." }
    }
    $expected = @($Files | ForEach-Object { $_.Name })
    foreach ($asset in @($release.assets)) {
        if ($expected -notcontains $asset.name) {
            throw "$Repository $Tag contains unexpected asset '$($asset.name)'; inspect it before publishing."
        }
    }
    # Verify all assets already there before uploading a missing one. This
    # catches a rebuild with the same version but different signed bytes.
    $existing = @($Files | Where-Object { $name = $_.Name; @($release.assets | Where-Object { $_.name -eq $name }).Count -gt 0 })
    if ($existing.Count -gt 0) { Assert-ReleaseAssets $Repository $Tag $release $existing }
    foreach ($file in $Files) {
        $name = $file.Name
        if (@($release.assets | Where-Object { $_.name -eq $name }).Count -gt 0) { continue }
        # No --clobber: a concurrent upload is verified on the next run rather
        # than silently replacing an asset in an existing release.
        Invoke-Gh @("release", "upload", $Tag, $file.FullName, "--repo", $Repository) |
            ForEach-Object { Write-Host "   $_" }
        $release = Get-ReleaseInfo $Repository $Tag
        if (-not $release) { throw "$Repository $Tag disappeared after uploading $name." }
    }
    Assert-ReleaseAssets $Repository $Tag $release $Files
    if ($release.isDraft -or $Latest) {
        $edit = @("release", "edit", $Tag, "--repo", $Repository, "--draft=false")
        if ($Latest) { $edit += "--latest" }
        Invoke-Gh $edit | ForEach-Object { Write-Host "   $_" }
    }
    $release = Get-ReleaseInfo $Repository $Tag
    if (-not $release -or $release.isDraft) { throw "$Repository $Tag is not published." }
    Assert-ReleaseAssets $Repository $Tag $release $Files
    if ($Latest) {
        $latestTag = (Invoke-Gh @("api", "repos/$Repository/releases/latest", "--jq", ".tag_name") | Select-Object -First 1)
        if ($latestTag -ne $Tag) { throw "$Repository latest release is $latestTag, not $Tag." }
    }
    return $release.url
}

# ------------------------------------------------------------ 1. preconditions

if (-not (Test-Path (Join-Path $root ".git"))) {
    throw "$root is not a git repository yet. Run git init, add the GitHub remote 'origin', and commit once before releasing."
}

$dirty = Invoke-Git @("status", "--porcelain") -Quiet
if ($dirty) {
    Write-Host "Uncommitted changes:" -ForegroundColor Yellow
    $dirty | ForEach-Object { Write-Host "   $_" }
    throw "Commit or stash first. What is not committed does not go into the release."
}

$branch = (Invoke-Git @("rev-parse", "--abbrev-ref", "HEAD") -Quiet) | Select-Object -First 1
if ($branch -ne "main" -and (-not $OnlyPublish -or $branch -ne "HEAD")) {
    throw "You are on '$branch'. A release is cut from main, or published from its exact tag in a detached worktree."
}

Invoke-Git @("fetch", "--tags", "origin", "main") -Quiet | Out-Null
if ($branch -eq "HEAD") {
    $headCommit = ((Invoke-Git @("rev-parse", "HEAD") -Quiet) | Select-Object -First 1).Trim()
    $mainCommit = ((Invoke-Git @("rev-parse", "origin/main") -Quiet) | Select-Object -First 1).Trim()
    if ($headCommit -ne $mainCommit) {
        throw "Detached release commit $headCommit differs from origin/main $mainCommit. Publish only the tagged main commit."
    }
}
$behind = ((Invoke-Git @("rev-list", "--count", "HEAD..origin/main") -Quiet) | Select-Object -First 1).Trim()
if ($behind -ne "0") { throw "main is $behind commit(s) behind origin. Pull first." }

if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
    throw "gh (GitHub CLI) is missing; it publishes the release and follows the Release run. winget install GitHub.cli, then gh auth login."
}
$privateRepo = (Invoke-Gh @("repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner") | Select-Object -First 1)
if ($privateRepo -ne "byggstyrning/synka") { throw "The checkout resolves to '$privateRepo', not byggstyrning/synka. Check origin before releasing." }
if (-not (Test-Path $cargo)) {
    throw "cargo was not found. Install Rust: winget install Rustlang.Rustup"
}

# The signing key. Checked before anything is bumped, tagged or built - but only
# when this machine signs. In a dry run a missing key is reported rather than fatal.
$signer = $null
$keyProblem = $null
# The key whose certificate is committed, when this machine has it: a machine can
# hold more than one CN=FastADC key, and the newest is not necessarily the release key.
$committedPrint = $null
$releaseCertificatePath = Join-Path $PSScriptRoot "FastADC.cer"
if (Test-Path $releaseCertificatePath) {
    $committedPrint = (New-Object Security.Cryptography.X509Certificates.X509Certificate2 $releaseCertificatePath).Thumbprint
}
$candidates = @(Get-ChildItem Cert:\CurrentUser\My |
    Where-Object { $_.Subject -eq $publisher -and $_.HasPrivateKey })
$signer = $candidates | Where-Object { $_.Thumbprint -eq $committedPrint } | Select-Object -First 1
if (-not $signer) { $signer = $candidates | Sort-Object NotAfter -Descending | Select-Object -First 1 }
if (-not $signer) {
    $keyProblem = "No signing key: CurrentUser\My holds no $publisher certificate with a private key. build_msix.ps1 -Sign requires the existing key matching packaging\FastADC.cer and never creates a replacement. Import the .pfx from the machine that cut the previous release: Import-PfxCertificate -FilePath <file>.pfx -CertStoreLocation Cert:\CurrentUser\My"
}
else {
    if ($signer.NotAfter -le (Get-Date) -or $signer.NotBefore -gt (Get-Date)) {
        $keyProblem = "The signing certificate $($signer.Thumbprint) is not currently valid ($($signer.NotBefore) to $($signer.NotAfter))."
    }
    $releaseCertificate = Join-Path $PSScriptRoot "FastADC.cer"
    if (-not (Test-Path $releaseCertificate)) {
        $keyProblem = "packaging\FastADC.cer is missing. Commit the public certificate for the signing key before tagging; the updater embeds it to verify future releases."
    }
    else {
        $committedCertificate = New-Object Security.Cryptography.X509Certificates.X509Certificate2 $releaseCertificate
        if ($committedCertificate.Thumbprint -ne $signer.Thumbprint) {
            $keyProblem = "The signing key $($signer.Thumbprint) differs from packaging\FastADC.cer $($committedCertificate.Thumbprint). Commit the matching public certificate before tagging; the updater pins this key."
        }
    }
    $trusted = @(Get-ChildItem Cert:\LocalMachine\TrustedPeople, Cert:\LocalMachine\Root -ErrorAction SilentlyContinue |
        Where-Object { $_.Subject -eq $publisher })
    $trustedPrints = @($trusted | ForEach-Object { $_.Thumbprint } | Select-Object -Unique)
    if (-not $keyProblem -and $trustedPrints.Count -gt 0 -and $trustedPrints -notcontains $signer.Thumbprint -and -not $NewCertificate) {
        $keyProblem = "The signing key ($($signer.Thumbprint)) is not the $publisher certificate this machine trusts ($($trustedPrints -join ', ')), so recipients would not trust it either. Pass -NewCertificate if a new key is intended, and say so in -Notes."
    }
    elseif ($signer.NotAfter -lt (Get-Date).AddDays(30)) {
        Write-Warning "The signing certificate expires $($signer.NotAfter.ToString('yyyy-MM-dd'))."
    }
}
if ($keyProblem -and -not $DryRun) { throw $keyProblem }

# The release gate and the release channel, checked before a version number is
# spent. The gate itself runs over the built files, before the first gh call that
# publishes anything.
$gateProblem = $null
if (-not (Test-Path $gate)) {
    $gateProblem = "The release gate is not at $gate. Nothing is published unchecked: clone claude-kit beside this repository, or set CLAUDE_KIT."
}
elseif (-not $python) {
    $gateProblem = "python is not on PATH; the release gate and the catalog step need it."
}
$channelProblem = $null
try {
    $channelState = (Invoke-Gh @("repo", "view", $channel, "--json", "visibility,viewerPermission")) -join "`n" | ConvertFrom-Json
    if ($channelState.viewerPermission -notin @("ADMIN", "MAINTAIN", "WRITE")) {
        $channelProblem = "gh cannot write to the release channel $channel (permission $($channelState.viewerPermission))."
    }
}
catch {
    $channelProblem = "The release channel $channel cannot be reached with gh: $($_.Exception.Message)"
}
foreach ($problem in @($gateProblem, $channelProblem)) {
    if ($problem -and -not $DryRun) { throw $problem }
}

# ------------------------------------------------------------ 2. the version

# version.ps1 reads it, and with -Plan says what the bump would set (it refuses a
# version that does not go forwards). Nothing is written yet.
$current = & $versionScript
if ($current -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$') { throw "version.ps1 read '$current' from Cargo.toml, not x.y.z" }

if ($OnlyPublish) {
    $next = $current
    $tag = "v$next"
    $onOrigin = Invoke-Git @("ls-remote", "--tags", "origin", "refs/tags/$tag") -Quiet
    if (-not $onOrigin) { throw "The tag $tag is not on origin. Run without -OnlyPublish to cut $next." }
    $tagCommit = ((Invoke-Git @("rev-parse", "$tag^{commit}") -Quiet) | Select-Object -First 1).Trim()
    $headCommit = ((Invoke-Git @("rev-parse", "HEAD") -Quiet) | Select-Object -First 1).Trim()
    if ($tagCommit -ne $headCommit) {
        throw "-OnlyPublish must build the exact $tag source commit ($tagCommit), but HEAD is $headCommit. Check out the tagged source before publishing."
    }
}
else {
    $next = & $versionScript -Bump $Bump -Version $Version -Plan
    if ($next -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$') { throw "version.ps1 planned '$next', not x.y.z" }
    # Koordinatverktyget refuses a first digit of 0, because the Microsoft Store rejects
    # it. Synka is sideloaded, not sold through a store, so 0.x releases are
    # allowed here; a Store upload later would need a 1.0.0 first.
    if ($next.StartsWith("0.")) { Write-Host "  note: $next is a 0.x version - fine for sideloading, the Store rejects a first digit of 0." -ForegroundColor DarkGray }
    $tag = "v$next"
    $existingTag = Invoke-Git @("tag", "--list", $tag) -Quiet
    if ($existingTag) { throw "The tag $tag already exists. Pick another version, or -OnlyPublish to release it." }
}

if (-not $OnlyPublish -and (Get-ReleaseInfo $privateRepo $tag)) {
    throw "A release $tag already exists on GitHub. Choose another version, or use -OnlyPublish to resume it."
}

if ($Notes) {
    if (-not (Test-Path $Notes)) { throw "No such notes file: $Notes" }
    $Notes = (Resolve-Path $Notes).Path
}

Write-Host ""
if ($OnlyPublish) { Write-Host "  $next   (tag $tag, already on origin)" -ForegroundColor Cyan }
else { Write-Host "  $current  ->  $next   (tag $tag)" -ForegroundColor Cyan }
Write-Host ""

if ($DryRun) {
    Write-Host "-DryRun: nothing changed. It would have:" -ForegroundColor DarkGray
    Write-Host "   run cargo test"
    if (-not $OnlyPublish) {
        Write-Host "   set [package] version in Cargo.toml and Cargo.lock to $next (version.ps1), committed 'chore: version $next', tagged $tag (annotated), pushed main and the tag"
    }
    if ($keyProblem) { Write-Host "   FAILED before that: $keyProblem" -ForegroundColor Yellow }
    else { Write-Host "   signed it with $($signer.Thumbprint) (valid to $($signer.NotAfter.ToString('yyyy-MM-dd'))) via build_msix.ps1 -Sign" }
    Write-Host "   packed dist\Synka-$next-sidladdning.zip with make_delivery.ps1 -SkipBuild"
    if ($gateProblem) { Write-Host "   FAILED before publishing: $gateProblem" -ForegroundColor Yellow }
    else { Write-Host "   ran the release gate over the zip and the signed package: $gate" }
    $withNotes = if ($Notes) { " and the notes in $Notes" } else { "" }
    Write-Host "   published $tag on this repository with the zip and the signed package$withNotes"
    if ($channelProblem) { Write-Host "   FAILED at the channel: $channelProblem" -ForegroundColor Yellow }
    else {
        Write-Host "   published $tag on $channel ($($channelState.visibility)) with the zip, the signed package and the certificate"
        Write-Host "   pointed the BoomerHubb catalog entry '$catalogId' at it ($setEntry)"
    }
    if ($NoShare) { Write-Host "   not copied into the shared folder (-NoShare)" }
    else { Write-Host "   copied the zip into the shared OneDrive folder Synka (share_release.ps1)" }
    exit 0
}

# ------------------------------------------------------------ 3. tests, bump, tag, push

Invoke-Step "Running the tests (cargo test)" {
    Push-Location $root
    try { & $cargo test --locked -- --test-threads=1 }
    finally { Pop-Location }
}

if (-not $OnlyPublish) {
    # Cargo.toml and Cargo.lock, the same way the Release workflow bumps.
    Invoke-Step "Setting the version to $next" {
        $written = & $versionScript -Version $next
        if ($written -ne $next) { throw "version.ps1 set '$written', not $next" }
    }

    Invoke-Git @("add", "--", "Cargo.toml", "Cargo.lock")
    # The subject is the release's commit message; the trailer is the attribution line.
    Invoke-Git @("commit", "-m", "chore: version $next")
    # Annotated: it records who cut the release and when. Notes are included on
    # the GitHub release page below.
    $tagMessage = "Synka $next`n"
    $tagMessageFile = Join-Path ([IO.Path]::GetTempPath()) "synka-tag-$next.txt"
    [IO.File]::WriteAllText($tagMessageFile, $tagMessage, (New-Object System.Text.UTF8Encoding $false))
    try { Invoke-Git @("tag", "-a", $tag, "-F", $tagMessageFile, "--cleanup=verbatim") }
    finally { Remove-Item $tagMessageFile -ErrorAction SilentlyContinue }
    Invoke-Git @("push", "origin", "main")
    # The tag last, and as its own push: it must not exist before its commit is on the server.
    Invoke-Git @("push", "origin", $tag)
}
else {
    Write-Host ""
    Write-Host "-OnlyPublish: no bump, commit or tag; publishing $tag as it is." -ForegroundColor DarkGray
}

# ------------------------------------------------------------ 4. signed package, built here

$buildScript = Join-Path $PSScriptRoot "build_msix.ps1"
$packagesDir = Join-Path $root "ui\AppPackages"
# dotnet names the package with the four-part version: 0.1.1 -> Synka_0.1.1.0_x64.msix
$msixName = "Synka_${next}.0_x64.msix"
$zipPath = Join-Path $dist "Synka-$next-sidladdning.zip"
$prebuilt = @(Get-ChildItem $packagesDir -Recurse -File -Filter $msixName -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -eq $msixName })
$reuseBuild = $OnlyPublish -and $prebuilt.Count -eq 1

if ($reuseBuild) {
    Write-Host "-OnlyPublish: reusing signed $($prebuilt[0].FullName); its signature is checked below." -ForegroundColor DarkGray
}
else {
    Invoke-Step "Building and signing $next" {
        & $buildScript -Sign -SkipTests
    }
}
$packages = @(Get-ChildItem $packagesDir -Recurse -File -Filter $msixName -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -eq $msixName })
if ($packages.Count -ne 1) { throw "Expected exactly one signed $msixName under $packagesDir, found $($packages.Count)." }
$signedItem = $packages[0]
. (Join-Path $PSScriptRoot "assert-package-engine.ps1")
$null = Assert-ReleaseFileSignature -Path $signedItem.FullName -Certificate $signer
$embeddedEngine = Assert-PackageEngine -PackagePath $signedItem.FullName -Certificate $signer
Write-Host "   embedded engine: signer $($embeddedEngine.signer), timestamp present" -ForegroundColor Green
Add-Type -AssemblyName System.IO.Compression.FileSystem
$packageArchive = [IO.Compression.ZipFile]::OpenRead($signedItem.FullName)
try {
    $manifestEntry = $packageArchive.GetEntry("AppxManifest.xml")
    if (-not $manifestEntry) { throw "$msixName has no AppxManifest.xml." }
    $reader = New-Object IO.StreamReader($manifestEntry.Open())
    try { [xml]$manifest = $reader.ReadToEnd() }
    finally { $reader.Dispose() }
}
finally { $packageArchive.Dispose() }
$identity = $manifest.SelectSingleNode("/*[local-name()='Package']/*[local-name()='Identity']")
if (-not $identity -or $identity.GetAttribute("Name") -ne "Byggstyrning.Synka" -or
    $identity.GetAttribute("Publisher") -ne $publisher -or
    $identity.GetAttribute("ProcessorArchitecture") -ne "x64" -or
    $identity.GetAttribute("Version") -ne "$next.0") {
    throw "$msixName contains the wrong package identity, publisher, architecture, or version."
}

# ------------------------------------------------------------ 6. the zip

if ($reuseBuild -and (Test-Path -LiteralPath $zipPath)) {
    Write-Host "-OnlyPublish: reusing $zipPath; its embedded MSIX is checked below." -ForegroundColor DarkGray
}
else {
    Invoke-Step "Packing the sideload zip" {
        & (Join-Path $PSScriptRoot "make_delivery.ps1") -SkipBuild -OutputDir $dist
    }
}
if (-not (Test-Path -LiteralPath $zipPath)) { throw "No $zipPath after packing." }
$zip = Get-Item $zipPath
$signed = $signedItem
$certificate = Join-Path $PSScriptRoot "FastADC.cer"
if (-not (Test-Path $certificate)) { throw "No $certificate after signing." }

# A cached zip must contain exactly the signed package and certificate beside
# it. If it was built for the same version from different bytes, stop before
# any GitHub mutation rather than publish a mixed delivery.
$archive = [IO.Compression.ZipFile]::OpenRead($zip.FullName)
try {
    foreach ($pair in @(@($msixName, $signed.FullName), @("FastADC.cer", $certificate))) {
        $entries = @($archive.Entries | Where-Object { $_.Name -eq $pair[0] })
        if ($entries.Count -ne 1) { throw "$($zip.Name) must contain exactly one $($pair[0]); found $($entries.Count)." }
        $sha = [Security.Cryptography.SHA256]::Create()
        $stream = $entries[0].Open()
        try { $embeddedHash = [BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-', '') }
        finally { $stream.Dispose(); $sha.Dispose() }
        $fileHash = (Get-FileHash -LiteralPath $pair[1] -Algorithm SHA256).Hash
        if ($embeddedHash -ne $fileHash) { throw "$($zip.Name) contains a different $($pair[0]) than the file being published." }
    }
    foreach ($required in @("Install.cmd", "Install.ps1", "README.txt")) {
        if (@($archive.Entries | Where-Object { $_.Name -eq $required }).Count -ne 1) {
            throw "$($zip.Name) is missing $required."
        }
    }
    if (@($archive.Entries | Where-Object { $_.FullName -match '/Dependencies/x64/Microsoft\.WindowsAppRuntime\..*\.msix$' }).Count -ne 1) {
        throw "$($zip.Name) must contain one x64 Windows App Runtime dependency."
    }
}
finally { $archive.Dispose() }

# ------------------------------------------------------------ the release gate

Invoke-Step "Release gate" {
    $env:PYTHONIOENCODING = "utf-8"
    & $python $gate $zip.FullName $signedItem.FullName | ForEach-Object { Write-Host "   $_" }
    if ($LASTEXITCODE -ne 0) {
        throw "The release gate stopped the release (see above). Nothing was published. Fix the cause, then -OnlyPublish."
    }
}

# ------------------------------------------------------------ 7. GitHub

$standard = @"
**Sideloading** - download ``Synka-$next-sidladdning.zip``, unpack the WHOLE zip and double-click ``Install.cmd``. It trusts the certificate and installs Synka, with one UAC prompt. Administrator rights are needed once per machine.

**Requirements** - Windows 10 version 2004 or later, x64.

The ``.msix`` beside it is signed with the included ``FastADC.cer`` certificate.

Colleagues install and update Synka through BoomerHubb. New versions are also shared through the internal OneDrive folder ``Synka``; there is no public download and no in-app update check.

**Varumärken / Trademarks** - Autodesk, Autodesk Construction Cloud, Autodesk Docs, Autodesk Forma och Revit är registrerade varumärken eller varumärken som tillhör Autodesk, Inc. och/eller dess dotter- eller närstående bolag. Synka är en fristående produkt från Byggstyrning och är inte ansluten till, godkänd av eller sponsrad av Autodesk.

Autodesk, Autodesk Construction Cloud, Autodesk Docs, Autodesk Forma and Revit are registered trademarks or trademarks of Autodesk, Inc., and/or its subsidiaries and/or affiliates. Synka is an independent product of Byggstyrning and is not affiliated with, endorsed by or sponsored by Autodesk.
"@
if ($Notes) { $standard += "`n" + (Get-Content -Raw -Encoding UTF8 $Notes) }
$notesFile = Join-Path ([IO.Path]::GetTempPath()) "synka-release-$next.md"
[IO.File]::WriteAllText($notesFile, $standard, (New-Object System.Text.UTF8Encoding $false))

$url = $null
try {
    $assets = [System.IO.FileInfo[]]@($zip, $signed, (Get-Item $certificate))
    $url = Invoke-Step "Publishing or resuming $tag on $privateRepo" {
        Publish-Release $privateRepo $tag "Synka $next" $notesFile $assets -VerifyTag
    }
    # The release channel: the same signed files. Its tag is made on the channel's
    # own default branch; no source is involved. Publish-Release resumes a partial
    # release there too, so -OnlyPublish finishes a failed mirror.
    $channelUrl = Invoke-Step "Publishing or resuming $tag on $channel" {
        Publish-Release $channel $tag "Synka $next" $notesFile $assets -Latest
    }
}
finally {
    Remove-Item $notesFile -ErrorAction SilentlyContinue
}

# ------------------------------------------------------------ the BoomerHubb catalog

# BoomerHubb installs and updates the team's apps from VilhelmH-git/catalog. Point
# the "synka" entry at the release just published on the channel. Never sinks the
# release: it stands, and the command to run again is printed.
$catalogArgs = @($setEntry, "--id", $catalogId, "--repo", $channel, "--tag", $tag, "--cert-thumbprint", $signer.Thumbprint)
Write-Host ""
Write-Host "== The BoomerHubb catalog ==" -ForegroundColor Cyan
$previous = $ErrorActionPreference
$ErrorActionPreference = "Continue"
& $python @catalogArgs 2>&1 | ForEach-Object { Write-Host "   $_" }
$catalogOk = $LASTEXITCODE -eq 0
$ErrorActionPreference = $previous
if (-not $catalogOk) {
    Write-Host "   The catalog was not updated - BoomerHubb keeps offering the previous version." -ForegroundColor Yellow
    Write-Host "   When fixed: python $($catalogArgs -join ' ')" -ForegroundColor Yellow
}

Write-Host "$tag is published." -ForegroundColor Green
Write-Host "  $url"
Write-Host "  $channelUrl  (BoomerHubb's channel)"
Write-Host ("  {0}  {1:N1} MB" -f $zip.Name, ($zip.Length / 1MB))
Write-Host ("  {0}  {1:N1} MB  (signed; verified on GitHub)" -f $signed.Name, ($signed.Length / 1MB))

# ------------------------------------------------------------ 8. shared folder

if ($NoShare) {
    Write-Host ""
    Write-Host "Not copied into the shared folder (-NoShare). Later: .\packaging\share_release.ps1 -Zip `"$($zip.FullName)`"" -ForegroundColor Yellow
}
else {
    # After GitHub, never before: the folder is a copy of what is published.
    try {
        Invoke-Step "Copying into the shared folder" {
            $shareArgs = @{ Zip = $zip.FullName }
            if ($ShareFolder) { $shareArgs.TargetFolder = $ShareFolder }
            & (Join-Path $PSScriptRoot "share_release.ps1") @shareArgs
        }
    }
    catch {
        throw "$tag is published on GitHub, but the copy into the shared folder failed: $($_.Exception.Message)`nWhen that is fixed: .\packaging\share_release.ps1 -Zip `"$($zip.FullName)`""
    }
}

Write-Host ""
Write-Host "Done. This machine keeps whatever Synka it had installed until you install $next (Install.cmd from the zip)." -ForegroundColor Green
