<#
.SYNOPSIS
    Packs a FINISHED, signed Synka build into the sideload zip for recipients.

.DESCRIPTION
    The zip holds:
      Synka_<ver>.0_x64.msix                 the signed app (window + engine)
      Dependencies\x64\Microsoft.WindowsAppRuntime.*.msix
                                               the framework the app declares
      FastADC.cer                              the certificate the package is signed with
      Install.cmd / Install.ps1                one double-click, one UAC prompt
      README.txt                               instructions (Swedish)

    packaging\delivery\ is handwritten and under version control - this script
    copies it, it does not generate it.

    The version comes from Cargo.toml, the one version source. The zip is named
    Synka-<version>-sidladdning.zip, the name share_release.ps1 expects.

.PARAMETER SkipBuild
    Reuse the signed package in ui\AppPackages. Without the switch everything is
    built and signed first. Reused packages must contain a signed, timestamped
    engine matching the committed release certificate too.

.PARAMETER OutputDir
    Where the zip ends up. Default: dist\.

.EXAMPLE
    powershell -NoProfile -File packaging\make_delivery.ps1
    powershell -NoProfile -File packaging\make_delivery.ps1 -SkipBuild
#>
[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [string]$OutputDir
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$packagesDir = Join-Path $root "ui\AppPackages"
$sourceDir = Join-Path $PSScriptRoot "delivery"
$cerPath = Join-Path $PSScriptRoot "FastADC.cer"
. (Join-Path $PSScriptRoot "assert-package-engine.ps1")
if (-not $OutputDir) { $OutputDir = Join-Path $root "dist" }

# ---------------------------------------------------------------- build

if ($SkipBuild) {
    Write-Host "== Build: skipping (-SkipBuild) ==" -ForegroundColor DarkGray
}
else {
    Write-Host "== Building and signing ==" -ForegroundColor Cyan
    & (Join-Path $PSScriptRoot "build_msix.ps1") -Sign
    if ($LASTEXITCODE -ne 0) { throw "build_msix.ps1 failed (exit code $LASTEXITCODE)" }
}

# ---------------------------------------------------------------- version

$raw = [IO.File]::ReadAllText((Join-Path $root "Cargo.toml"), [Text.Encoding]::UTF8)
$packageAt = $raw.IndexOf("[package]")
$hit = [regex]::new('(?m)^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"').Match($raw, [Math]::Max($packageAt, 0))
if ($packageAt -lt 0 -or -not $hit.Success) { throw "Cannot read the version from Cargo.toml" }
$version = $hit.Groups[1].Value
$name = "Synka-$version-sidladdning"
$msixName = "Synka_${version}.0_x64.msix"

# ---------------------------------------------------------------- the parts

$packages = @(Get-ChildItem $packagesDir -Recurse -File -Filter $msixName -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -eq $msixName })
if ($packages.Count -ne 1) {
    throw "Expected exactly one $msixName under $packagesDir, found $($packages.Count). Remove ambiguous build outputs or run without -SkipBuild."
}
$msix = $packages[0]

function Read-PackageManifest([string]$path) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [IO.Compression.ZipFile]::OpenRead($path)
    try {
        $entry = $archive.GetEntry("AppxManifest.xml")
        if (-not $entry) { throw "$path has no AppxManifest.xml" }
        $reader = New-Object IO.StreamReader($entry.Open())
        try { return [xml]$reader.ReadToEnd() }
        finally { $reader.Dispose() }
    }
    finally { $archive.Dispose() }
}

$manifest = Read-PackageManifest $msix.FullName
$identity = $manifest.SelectSingleNode("/*[local-name()='Package']/*[local-name()='Identity']")
if (-not $identity -or $identity.GetAttribute("Name") -ne "Byggstyrning.Synka" -or
    $identity.GetAttribute("Version") -ne "$version.0" -or
    $identity.GetAttribute("ProcessorArchitecture") -ne "x64") {
    throw "$msixName contains the wrong app identity, version or architecture. Build the current Cargo version again."
}

if (-not (Test-Path $cerPath)) { throw "Cannot find the committed release certificate: $cerPath" }

# Validate both signatures against the committed certificate before making a delivery.
# The release machine must already trust that certificate; no trust is added here.
$releaseCertificate = New-Object Security.Cryptography.X509Certificates.X509Certificate2 $cerPath
$cerPublisher = $releaseCertificate.Subject
if ($identity.GetAttribute("Publisher") -ne $cerPublisher) {
    throw "$msixName has publisher '$($identity.GetAttribute('Publisher'))', not $cerPublisher."
}
$null = Assert-ReleaseFileSignature -Path $msix.FullName -Certificate $releaseCertificate
$embeddedEngine = Assert-PackageEngine -PackagePath $msix.FullName -Certificate $releaseCertificate
Write-Host "   embedded engine: signer $($embeddedEngine.signer), timestamp present" -ForegroundColor Green

$runtimeDir = Join-Path $msix.DirectoryName "Dependencies\x64"
$runtimes = @(Get-ChildItem $runtimeDir -File -Filter "Microsoft.WindowsAppRuntime.*.msix" -ErrorAction SilentlyContinue)
if ($runtimes.Count -ne 1) {
    throw "Expected exactly one x64 Windows App Runtime beside $msixName in $runtimeDir, found $($runtimes.Count)."
}
$runtime = $runtimes[0]
$dependencies = @($manifest.SelectNodes("/*[local-name()='Package']/*[local-name()='Dependencies']/*[local-name()='PackageDependency']") |
    Where-Object { $_.GetAttribute("Name") -like "Microsoft.WindowsAppRuntime.*" })
if ($dependencies.Count -ne 1) { throw "$msixName does not declare exactly one Windows App Runtime dependency." }
$dependency = $dependencies[0]
$runtimeManifest = Read-PackageManifest $runtime.FullName
$runtimeIdentity = $runtimeManifest.SelectSingleNode("/*[local-name()='Package']/*[local-name()='Identity']")
if (-not $runtimeIdentity -or $runtimeIdentity.GetAttribute("Name") -ne $dependency.GetAttribute("Name") -or
    $runtimeIdentity.GetAttribute("Publisher") -ne $dependency.GetAttribute("Publisher") -or
    $runtimeIdentity.GetAttribute("ProcessorArchitecture") -ne "x64" -or
    [version]$runtimeIdentity.GetAttribute("Version") -lt [version]$dependency.GetAttribute("MinVersion")) {
    throw "The x64 Windows App Runtime beside $msixName does not satisfy its declared dependency."
}

foreach ($required in @("Install.cmd", "Install.ps1", "README.txt")) {
    if (-not (Test-Path (Join-Path $sourceDir $required))) { throw "$required is missing from $sourceDir" }
}

# ---------------------------------------------------------------- the zip

Write-Host "== Assembling the delivery ==" -ForegroundColor Cyan
$tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd([char[]]@('\', '/'))
$staging = [IO.Path]::GetFullPath((Join-Path $tempRoot ("synka-delivery-" + [guid]::NewGuid().ToString("N").Substring(0, 8))))
if (-not $staging.StartsWith($tempRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
    throw "Delivery staging escaped the temporary directory: $staging"
}
$stage = Join-Path $staging $name

try {
    New-Item -ItemType Directory -Force -Path (Join-Path $stage "Dependencies\x64") | Out-Null
    Copy-Item $msix.FullName $stage
    Copy-Item $runtime.FullName (Join-Path $stage "Dependencies\x64")
    Copy-Item $cerPath $stage
    Get-ChildItem $sourceDir -File | ForEach-Object { Copy-Item $_.FullName $stage }

    New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
    $zip = Join-Path $OutputDir "$name.zip"
    if (Test-Path $zip) { Remove-Item $zip -Force }
    # Compress-Archive on Windows writes backslashes into ZIP entry names. ZIP paths use
    # forward slashes, including on Windows, and the release validator relies on them.
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [IO.Compression.ZipFile]::Open($zip, [IO.Compression.ZipArchiveMode]::Create)
    try {
        $stagePrefix = $stage.TrimEnd([char[]]@('\', '/')) + [IO.Path]::DirectorySeparatorChar
        foreach ($file in (Get-ChildItem -LiteralPath $stage -Recurse -File | Sort-Object FullName)) {
            if (-not $file.FullName.StartsWith($stagePrefix, [StringComparison]::OrdinalIgnoreCase)) {
                throw "Delivery file escaped staging: $($file.FullName)"
            }
            $relative = $file.FullName.Substring($stagePrefix.Length).Replace('\', '/')
            $entryName = "$name/$relative"
            [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
                $archive, $file.FullName, $entryName, [IO.Compression.CompressionLevel]::Optimal)
        }
    }
    finally { $archive.Dispose() }

    # Regression check: ensure the actual archive can be read by standard ZIP tools and
    # contains portable entry names, especially the runtime dependency subdirectory.
    $archive = [IO.Compression.ZipFile]::OpenRead($zip)
    try {
        $entryNames = @($archive.Entries | ForEach-Object { $_.FullName })
        if (@($entryNames | Where-Object { $_.Contains('\') }).Count -gt 0) {
            throw "$zip contains Windows-style backslashes in ZIP entry names."
        }
        foreach ($required in @(
            "$name/$msixName",
            "$name/Dependencies/x64/$($runtime.Name)",
            "$name/FastADC.cer",
            "$name/Install.cmd",
            "$name/Install.ps1",
            "$name/README.txt"
        )) {
            if ($entryNames -notcontains $required) { throw "$zip is missing ZIP entry $required" }
        }
    }
    finally { $archive.Dispose() }
}
finally {
    $stagingItem = Get-Item -LiteralPath $staging -Force -ErrorAction SilentlyContinue
    if ($stagingItem -and -not ($stagingItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        Remove-Item -LiteralPath $staging -Recurse -Force -ErrorAction SilentlyContinue
    }
}

$zipItem = Get-Item $zip
Write-Host ""
Write-Host "  $($zipItem.FullName)" -ForegroundColor Green
Write-Host ("  {0:N1} MB" -f ($zipItem.Length / 1MB)) -ForegroundColor Green
Write-Host @"

Contents:
  $($msix.Name)   the app
  Dependencies\x64\...   the framework (Microsoft.WindowsAppRuntime)
  FastADC.cer                 the certificate
  Install.cmd / Install.ps1   the installer
  README.txt                  instructions (Swedish)

The recipient unpacks the WHOLE zip, double-clicks Install.cmd and answers yes to
the UAC prompt. Administrator rights are needed once per machine, for the certificate.
"@
