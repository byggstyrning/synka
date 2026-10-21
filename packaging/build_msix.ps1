<#
.SYNOPSIS
    Builds Synka into a single-project MSIX package.

.DESCRIPTION
    The app is SINGLE-PROJECT MSIX: the manifest lives in the project
    (ui\Package.appxmanifest) and `dotnet build` produces the .msix directly.
    This script only does what MSBuild does not do by itself - tests, the Rust
    engine, the verification of the result, and signing.

    How the parts fit together:

      Synka.exe                   WinUI 3 window, framework-dependent Windows App SDK
      engine\synka-engine.exe     the Rust engine, baked in by the csproj's Content
                                    entry in Release (target\release\synka-engine.exe)
      Microsoft.WindowsAppRuntime   the framework, laid out next to the package under
                                    ui\AppPackages\...\Dependencies\x64\

    The order:
      0. Clean ui\AppPackages and dist\ (dist\store\ is kept).
      1. cargo test                       (skipped with -SkipTests)
      2. cargo build --release            -> target\release\synka-engine.exe
                                          (skipped with -SkipEngineBuild)
         -Sign: sign and timestamp the engine with the existing release certificate
      3. dotnet build of ui\Synka.csproj, Release x64, GenerateAppxPackageOnBuild
      4. Find the .msix and the x64 framework, verify the package contents
      5. signtool, with -Sign or -PfxPath only   (same certificate as the engine)
      6. -Install only: framework first, then the app, as the current user

    The same script builds on a developer machine and on GitHub Actions
    (.github\workflows\ci.yml unsigned, release.yml signed with -PfxPath).

    The version has ONE source: [package] version in Cargo.toml. The csproj's
    StampPackageVersion target writes it into the manifest, and this script checks
    that the manifest in the built package says the same.

    FRAMEWORK-DEPENDENT: the package declares Microsoft.WindowsAppRuntime as a
    dependency instead of bundling its DLLs. When sideloading, the framework is
    installed first; -Install does that automatically.

.PARAMETER Sign
    Sign the engine before packaging, then sign the MSIX, using the existing
    CN=FastADC private key in CurrentUser\My matching packaging\FastADC.cer.
    Both signatures require an RFC 3161 timestamp. Never creates a signing key.
    The release machine must already trust the committed certificate so Windows
    can verify both signatures as Valid; the build never adds certificate trust.

.PARAMETER Install
    Install the package after the build, framework dependency included. Requires
    -Sign. Windows closes the target app during an in-place version update. The certificate must
    already be trusted on this machine for signing verification and installation.
    Trust setup is an explicit operator step; the build never imports certificates.

.PARAMETER SkipTests
    Do not run cargo test.

.PARAMETER SkipEngineBuild
    Do not run cargo build: package the engine already in target\release (CI
    builds it in its own step or job). Stops when it is missing. A signed build
    still signs and timestamps that engine before packing it into the MSIX.

.PARAMETER PfxPath
    Sign with the key in this .pfx file instead of the certificate store. Implies
    -Sign, and never creates a key. The certificate must be issued to
    -PublisherCN and must be the one in packaging\FastADC.cer, the certificate
    recipients have trusted; a different key is refused before anything is built.
    The release workflow decodes the repository secret SIGNING_PFX_BASE64 to a
    file and passes it here.

.PARAMETER PfxPassword
    The password of -PfxPath, as a SecureString:
    -PfxPassword (Read-Host -AsSecureString)  or  (ConvertTo-SecureString $text -AsPlainText -Force)
    It reaches signtool as an argument. Windows PowerShell 5.1 mangles an argument
    with a double quote in it, so such a password works only from PowerShell 7 (CI).

.PARAMETER TimestampUrl
    RFC 3161 timestamp service used for the engine and MSIX signatures. The default
    is DigiCert's public timestamp endpoint. A failed timestamp stops the build.

.EXAMPLE
    powershell -NoProfile -File packaging\build_msix.ps1 -SkipTests
    powershell -NoProfile -File packaging\build_msix.ps1 -SkipTests -Sign
    powershell -NoProfile -File packaging\build_msix.ps1 -SkipTests -Sign -Install
    & .\packaging\build_msix.ps1 -SkipTests -PfxPath C:\keys\FastADC.pfx -PfxPassword (Read-Host -AsSecureString)
#>
[CmdletBinding()]
param(
    [switch]$Sign,
    [switch]$Install,
    [switch]$SkipTests,
    [switch]$SkipEngineBuild,
    [string]$PfxPath,
    [SecureString]$PfxPassword,
    [string]$PublisherCN = "CN=FastADC",
    [string]$TimestampUrl = "http://timestamp.digicert.com"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$uiDir = Join-Path $root "ui"
$csproj = Join-Path $uiDir "Synka.csproj"
$packagesDir = Join-Path $uiDir "AppPackages"
$dist = Join-Path $root "dist"
$cerPath = Join-Path $PSScriptRoot "FastADC.cer"
$engineExe = Join-Path $root "target\release\synka-engine.exe"
$packageName = "Byggstyrning.Synka"
. (Join-Path $PSScriptRoot "assert-package-engine.ps1")

if ($PfxPath) { $Sign = $true }
if ($Install -and -not $Sign) { throw "-Install needs -Sign: Windows refuses an unsigned package." }

function Invoke-Native {
    <#
      PowerShell 5.1 turns stderr from an external program into a terminating
      error when ErrorActionPreference is Stop - even on exit code 0. cargo, dotnet
      and signtool all write ordinary information to stderr. Only the exit code
      decides.
    #>
    param([string]$Exe, [string[]]$Arguments, [string]$What, [switch]$OnlyProblems)
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        if ($OnlyProblems) {
            # dotnet build writes over a thousand lines. Keep the lines that carry a
            # diagnostic code, and print everything on failure. A build may be quiet
            # about what went well, never about what creaked.
            $output = & $Exe @Arguments 2>&1
            if ($LASTEXITCODE -ne 0) {
                $output | ForEach-Object { Write-Host "   $_" }
                throw "$What failed (exit code $LASTEXITCODE)"
            }
            $problems = @($output | Where-Object { "$_" -match '(?i):\s+(warning|error)\s' } |
                ForEach-Object { "$_".Trim() } | Select-Object -Unique)
            if ($problems.Count -gt 0) {
                $problems | ForEach-Object { Write-Host "   $_" -ForegroundColor Yellow }
                Write-Host ("   {0} unique warning/error lines" -f $problems.Count) -ForegroundColor Yellow
            }
            else {
                Write-Host "   no warnings" -ForegroundColor DarkGray
            }
        }
        else {
            # Stringified first: stderr lines are ErrorRecords, and Write-Host would
            # print each one as a wrapper type name above the message.
            & $Exe @Arguments 2>&1 | ForEach-Object { "$_" } | ForEach-Object { Write-Host "   $_" }
            if ($LASTEXITCODE -ne 0) { throw "$What failed (exit code $LASTEXITCODE)" }
        }
    }
    finally { $ErrorActionPreference = $previous }
}

function Find-SdkTool([string]$name) {
    # The newest Windows SDK that has the tool: bin\<10.0.x.y>\x64\<name>. No SDK
    # version is assumed; a build machine or CI runner has whichever it has.
    $bases = @(
        "${env:ProgramFiles(x86)}\Windows Kits\10\bin",
        "$env:ProgramFiles\Windows Kits\10\bin"
    )
    foreach ($base in $bases) {
        if (-not (Test-Path $base)) { continue }
        $found = Get-ChildItem $base -Directory -ErrorAction SilentlyContinue |
            Sort-Object { $v = $null; if ([version]::TryParse($_.Name, [ref]$v)) { $v } else { [version]"0.0" } } -Descending |
            ForEach-Object { Join-Path $_.FullName "x64\$name" } |
            Where-Object { Test-Path $_ } |
            Select-Object -First 1
        if ($found) { return $found }
    }
    return $null
}

function Find-Tool([string]$name, [string]$fallback) {
    $resolved = Get-Command $name -ErrorAction SilentlyContinue
    if ($resolved) { return $resolved.Source }
    if (Test-Path $fallback) { return $fallback }
    throw "$name was not found."
}

function Get-CargoVersion {
    $raw = [IO.File]::ReadAllText((Join-Path $root "Cargo.toml"), [Text.Encoding]::UTF8)
    $at = $raw.IndexOf("[package]")
    $hit = [regex]::new('(?m)^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"').Match($raw, [Math]::Max($at, 0))
    if ($at -lt 0 -or -not $hit.Success) { throw "Cannot read the [package] version from Cargo.toml" }
    return $hit.Groups[1].Value
}

function Read-ZipEntries([string]$path) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [IO.Compression.ZipFile]::OpenRead($path)
    try {
        return @($archive.Entries | ForEach-Object { [pscustomobject]@{ Name = $_.FullName; Length = $_.Length } })
    }
    finally { $archive.Dispose() }
}

function Read-ZipText([string]$path, [string]$entryName) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [IO.Compression.ZipFile]::OpenRead($path)
    try {
        $entry = $archive.Entries | Where-Object { $_.FullName -eq $entryName } | Select-Object -First 1
        if (-not $entry) { return $null }
        $reader = New-Object IO.StreamReader($entry.Open())
        try { return $reader.ReadToEnd() } finally { $reader.Dispose() }
    }
    finally { $archive.Dispose() }
}

function Read-Pfx([string]$path, [SecureString]$password) {
    # EphemeralKeySet: the key is only looked at here, never written to a key container.
    $flags = [Security.Cryptography.X509Certificates.X509KeyStorageFlags]::EphemeralKeySet
    try {
        if ($password) {
            return [Security.Cryptography.X509Certificates.X509Certificate2]::new($path, $password, $flags)
        }
        return [Security.Cryptography.X509Certificates.X509Certificate2]::new($path, [string]"", $flags)
    }
    catch {
        throw "Cannot open $path. Wrong password, or not a .pfx: $($_.Exception.Message)"
    }
}

function Sign-ReleaseFile([string]$path) {
    $signArgs = @("sign", "/fd", "SHA256", "/tr", $TimestampUrl, "/td", "SHA256")
    if ($pfxCert) {
        $signArgs += @("/f", $PfxPath)
        if ($PfxPassword) { $signArgs += @("/p", [Net.NetworkCredential]::new("", $PfxPassword).Password) }
    }
    else {
        $signArgs += @("/sha1", $cert.Thumbprint)
    }
    # Invoke-Native logs output only: never print arguments containing the PFX password.
    Invoke-Native $signtool ($signArgs + @($path)) "signtool sign"
    $null = Assert-ReleaseFileSignature -Path $path -Certificate $cert
}

# Select the existing committed release identity BEFORE building or signing anything.
$pfxCert = $null
$cert = $null
$signtool = $null
if ($Sign) {
    Write-Host "== Checking the signing key ==" -ForegroundColor Cyan
    if (-not (Test-Path -LiteralPath $cerPath)) { throw "The committed release certificate is missing: $cerPath" }
    $trustedCer = New-Object Security.Cryptography.X509Certificates.X509Certificate2 $cerPath
    if ($trustedCer.Subject -ne $PublisherCN) {
        throw "The committed certificate is '$($trustedCer.Subject)', not the package publisher $PublisherCN."
    }
    if ($PfxPath) {
        if (-not (Test-Path -LiteralPath $PfxPath)) { throw "No .pfx at $PfxPath" }
        $PfxPath = (Resolve-Path -LiteralPath $PfxPath).Path
        $pfxCert = Read-Pfx $PfxPath $PfxPassword
        $cert = $pfxCert
    }
    else {
        $cert = Get-ChildItem Cert:\CurrentUser\My |
            Where-Object { $_.Thumbprint -eq $trustedCer.Thumbprint -and $_.HasPrivateKey } |
            Select-Object -First 1
        if (-not $cert) { throw "The existing release private key $($trustedCer.Thumbprint) is not in CurrentUser\My. Supply the matching -PfxPath; no new key will be created." }
    }
    if ($cert.Thumbprint -ne $trustedCer.Thumbprint -or $cert.Subject -ne $PublisherCN -or -not $cert.HasPrivateKey) {
        throw "The signing key must match the committed release certificate ($($trustedCer.Thumbprint)) and contain its private key."
    }
    if ($cert.NotAfter -le (Get-Date) -or $cert.NotBefore -gt (Get-Date)) {
        throw "The signing certificate $($cert.Thumbprint) is not currently valid."
    }
    if ($cert.NotAfter -lt (Get-Date).AddDays(30)) { Write-Warning "The signing certificate expires $($cert.NotAfter.ToString('yyyy-MM-dd'))." }
    $signtool = Find-SdkTool "signtool.exe"
    if (-not $signtool) { throw "signtool.exe is missing. Run: winget install Microsoft.WindowsSDK.10.0.26100" }
    Write-Host "   $($cert.Thumbprint), same as packaging\FastADC.cer, valid to $($cert.NotAfter.ToString('yyyy-MM-dd'))" -ForegroundColor Green
}

Write-Host "== 0. Cleaning up the previous build ==" -ForegroundColor Cyan
Write-Host "   running Synka and synka-engine are left alone during the build" -ForegroundColor DarkGray
if (Test-Path $packagesDir) { Remove-Item $packagesDir -Recurse -Force -ErrorAction SilentlyContinue }
if (Test-Path $dist) {
    Get-ChildItem $dist -Force | Where-Object { $_.Name -ne "store" } |
        ForEach-Object { Remove-Item $_.FullName -Recurse -Force -ErrorAction SilentlyContinue }
}
New-Item -ItemType Directory -Force -Path $dist | Out-Null

$version = Get-CargoVersion
Write-Host "   version $version (from Cargo.toml)" -ForegroundColor DarkGray

$cargo = $null
if (-not ($SkipTests -and $SkipEngineBuild)) {
    $cargo = Find-Tool "cargo" (Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe")
}
$dotnet = Find-Tool "dotnet" "$env:ProgramFiles\dotnet\dotnet.exe"

Push-Location $root
try {
    if ($SkipTests) {
        Write-Host "== 1. Tests: skipping (-SkipTests) ==" -ForegroundColor DarkGray
    }
    else {
        Write-Host "== 1. Running the engine's tests (cargo test) ==" -ForegroundColor Cyan
        Invoke-Native $cargo @("test", "--locked", "--", "--test-threads=1") "cargo test"
    }

    if ($SkipEngineBuild) {
        Write-Host "== 2. Engine: the one already in target\release (-SkipEngineBuild) ==" -ForegroundColor DarkGray
    }
    else {
        Write-Host "== 2. Building the engine (cargo build --release) ==" -ForegroundColor Cyan
        # Panic locations carry source paths, and the dependencies' paths start with
        # the builder's profile (C:\Users\<name>\.cargo\registry\...). The release gate
        # refuses a user name in a package, so the profile is written as "~".
        # CARGO_ENCODED_RUSTFLAGS takes the flag whole, spaces in the path included.
        $previousFlags = $env:CARGO_ENCODED_RUSTFLAGS
        $remap = "--remap-path-prefix=$($env:USERPROFILE)=~"
        $env:CARGO_ENCODED_RUSTFLAGS = if ($previousFlags) { "$previousFlags$([char]0x1f)$remap" } else { $remap }
        try { Invoke-Native $cargo @("build", "--release") "cargo build --release" }
        finally { $env:CARGO_ENCODED_RUSTFLAGS = $previousFlags }
    }
}
finally { Pop-Location }

if (-not (Test-Path $engineExe)) {
    if ($SkipEngineBuild) { throw "-SkipEngineBuild, but there is no engine at $engineExe. Build it first: cargo build --release" }
    throw "The build produced no engine at $engineExe"
}
Write-Host ("   engine: {0}  {1:N1} MB" -f $engineExe, ((Get-Item $engineExe).Length / 1MB)) -ForegroundColor DarkGray
if ($Sign) {
    Write-Host "   signing and timestamping the notification-area engine before packaging" -ForegroundColor DarkGray
    Sign-ReleaseFile $engineExe
}

Write-Host "== 3. Building the app and the package ==" -ForegroundColor Cyan
Invoke-Native $dotnet @(
    "build", $csproj,
    "-c", "Release", "-p:Platform=x64", "-p:GenerateAppxPackageOnBuild=true", "--nologo"
) "dotnet build" -OnlyProblems

$msix = Get-ChildItem $packagesDir -Recurse -Filter "Synka_*_x64.msix" -ErrorAction SilentlyContinue |
    Select-Object -First 1
if (-not $msix) { throw "The build produced no Synka_*_x64.msix under $packagesDir" }
Write-Host ("   {0}  {1:N1} MB" -f $msix.Name, ($msix.Length / 1MB)) -ForegroundColor Green

$dependency = Get-ChildItem $packagesDir -Recurse -Filter "Microsoft.WindowsAppRuntime.*.msix" -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -match '\\x64\\' } | Select-Object -First 1
if (-not $dependency) { throw "The x64 Microsoft.WindowsAppRuntime package was not laid out under $packagesDir" }
Write-Host ("   {0}  {1:N1} MB  (framework)" -f $dependency.Name, ($dependency.Length / 1MB)) -ForegroundColor Green

Write-Host "== 4. Checking the package contents ==" -ForegroundColor Cyan
# A package without the engine starts, shows its window and never syncs. Look
# inside before anything else is done with it.
$entries = Read-ZipEntries $msix.FullName
$engineEntry = $entries | Where-Object { $_.Name -match '(^|[\\/])engine[\\/]synka-engine\.exe$' } | Select-Object -First 1
if (-not $engineEntry) { throw "The package contains no engine\synka-engine.exe. Check the csproj's Release Content entry." }
if ($engineEntry.Length -ne (Get-Item $engineExe).Length) {
    throw "engine\synka-engine.exe in the package is $($engineEntry.Length) bytes, target\release has $((Get-Item $engineExe).Length). Not the same build."
}
Write-Host "   engine\synka-engine.exe  $($engineEntry.Length) bytes, same as target\release" -ForegroundColor Green
if ($Sign) {
    $embeddedEngine = Assert-PackageEngine -PackagePath $msix.FullName -Certificate $cert -ExpectedEnginePath $engineExe
    Write-Host "   embedded engine: matching SHA256, signer $($embeddedEngine.signer), timestamp present" -ForegroundColor Green
}
if (-not ($entries | Where-Object { $_.Name -match '(^|[\\/])Synka\.exe$' })) { throw "The package contains no Synka.exe" }
# Explorer's lock state icons, referenced by path from the engine (src\shell\states.rs).
foreach ($icon in @("LockedByMe.ico", "LockedByOther.ico")) {
    if (-not ($entries | Where-Object { $_.Name -match ('(^|[\\/])Assets[\\/]' + [regex]::Escape($icon) + '$') })) {
        throw "The package contains no Assets\$icon. Run: python -I packaging\make_icons.py --lock-icons ui\Assets"
    }
}

$manifestText = Read-ZipText $msix.FullName "AppxManifest.xml"
$identity = [regex]::Match($manifestText, '<Identity[^>]*Version="([0-9.]+)"')
$packageIdentity = [regex]::Match($manifestText, '<Identity[^>]*Name="([^"]+)"[^>]*Publisher="([^"]+)"')
if (-not $identity.Success -or $identity.Groups[1].Value -ne "$version.0") {
    throw "The manifest in the package says version '$($identity.Groups[1].Value)', Cargo.toml says $version. The stamp did not reach it."
}
if (-not $packageIdentity.Success -or $packageIdentity.Groups[1].Value -ne $packageName) {
    throw "The package identity is '$($packageIdentity.Groups[1].Value)', expected $packageName."
}
if ($packageIdentity.Groups[2].Value -ne $PublisherCN) {
    throw "The manifest Publisher is '$($packageIdentity.Groups[2].Value)', the signing certificate is $PublisherCN. They must match."
}
Write-Host "   manifest: $packageName $($identity.Groups[1].Value), publisher $PublisherCN" -ForegroundColor Green

if (-not $Sign) {
    Write-Host @"

Done. The package is UNSIGNED.
  $($msix.FullName)

  * Microsoft Store: upload it as it is - the Store signs it and solves the
    framework dependency for the user. Replace Identity/Publisher in
    ui\Package.appxmanifest with the values Partner Center assigns first.
  * Sideloading: run again with -Sign
"@ -ForegroundColor Green
    exit 0
}

Write-Host "== 5. Signing ==" -ForegroundColor Cyan
Write-Host "   $signtool" -ForegroundColor DarkGray
Sign-ReleaseFile $msix.FullName

# packaging\FastADC.cer is the certificate the recipient needs. It lives here, not
# only in AppPackages, because step 0 deletes AppPackages on every build.
# Keep the committed certificate unchanged; the selected signing key matched it.
Write-Host "   certificate: $cerPath"
# A copy beside the package, so the folder AppPackages is complete on its own.
Copy-Item $cerPath (Join-Path $msix.DirectoryName "FastADC.cer") -Force
Write-Host "   certificate: $(Join-Path $msix.DirectoryName 'FastADC.cer') (copy beside the package)"

if (-not $Install) {
    Write-Host @"

Done and signed.
  Package     : $($msix.FullName)
  Framework   : $($dependency.FullName)
  Certificate : $cerPath

Next: make_delivery.ps1 packs these into the zip for the recipient.
"@ -ForegroundColor Green
    exit 0
}

Write-Host "== 6. Installing ==" -ForegroundColor Cyan

# Windows refuses a package whose signer it does not trust. Checked before anything
# is installed, so the framework is not installed for nothing. The import needs
# admin, and this script does not elevate: it prints the command and stops.
$trusted = @(Get-ChildItem Cert:\LocalMachine\TrustedPeople, Cert:\LocalMachine\Root -ErrorAction SilentlyContinue |
    Where-Object { $_.Thumbprint -eq $cert.Thumbprint })
if ($trusted.Count -eq 0) {
    Write-Host ""
    Write-Host "The certificate $($cert.Thumbprint) is not trusted on this machine." -ForegroundColor Yellow
    Write-Host "Import it ONCE from an ELEVATED PowerShell (Run as administrator):" -ForegroundColor Yellow
    Write-Host ""
    Write-Host "  Import-Certificate -FilePath `"$cerPath`" -CertStoreLocation Cert:\LocalMachine\TrustedPeople" -ForegroundColor Cyan
    Write-Host ""
    Write-Host "Then run this script with -Install again. Nothing was installed." -ForegroundColor Yellow
    exit 1
}

# Framework first. 0x80073D06 = a newer framework is already installed, which
# satisfies the minimum the package declares, so it is not an error.
try {
    Add-AppxPackage -Path $dependency.FullName -ErrorAction Stop
    Write-Host "   framework installed"
}
catch {
    if ("$($_.Exception.Message)" -match "0x80073D06") {
        Write-Host "   framework: a newer version is already installed"
    }
    else {
        Write-Warning "The framework could not be installed: $($_.Exception.Message)"
        Write-Warning "Close the apps named above and run again."
        exit 1
    }
}

# An MSIX with a higher version updates the existing package in place and keeps
# LocalState. Do not uninstall/reinstall an equal-version development build: that
# would delete LocalState, and copying a live SQLite/WAL folder can lose writes.
$targetVersion = [version]"$version.0"
$existing = Get-AppxPackage -Name $packageName -ErrorAction SilentlyContinue | Select-Object -First 1
if ($existing) {
    if ($existing.Publisher -ne $PublisherCN) {
        throw "The installed $packageName publisher is '$($existing.Publisher)', expected $PublisherCN. Nothing changed."
    }
    if ([version]$existing.Version -ge $targetVersion) {
        throw "Installed version $($existing.Version) is not older than $targetVersion. Increase Cargo.toml's version for an in-place install; LocalState was not changed."
    }
    Write-Host "   updating $($existing.Version) to $targetVersion in place"
}
Add-AppxPackage -Path $msix.FullName -ForceTargetApplicationShutdown -ErrorAction Stop
$installed = Get-AppxPackage -Name $packageName -ErrorAction Stop | Select-Object -First 1
if (-not $installed -or [version]$installed.Version -ne $targetVersion) {
    throw "After installation, expected $targetVersion but found '$($installed.Version)'."
}
Write-Host "   $($installed.PackageFullName)" -ForegroundColor Green
Write-Host @"

Installed. Synka starts at sign-in. Start it from the Start menu, or:
  Start-Process "shell:AppsFolder\$($installed.PackageFamilyName)!App"
"@ -ForegroundColor Green
