<#
.SYNOPSIS
    Puts a release's sideload zip into the shared OneDrive folder, under its own
    name and under the fixed name Synka-latest.zip.

.DESCRIPTION
    The last step of a release. new_release.ps1 calls it after the release is
    published on GitHub; run on its own it fills the folder from GitHub.

    What ends up in the folder:
      Synka-latest.zip               the fixed name, overwritten every release
      Synka-1.2.3-sidladdning.zip    the versioned copy; the newest 5 are kept

    The folder is one the OneDrive client syncs, so the client does the uploading.
    The zip is staged under TEMP and MOVED into place, so the sync client sees a
    finished file appear. The size is checked after the move.

    Only the sideload zip belongs in the folder. The .msix is never copied here.

.PARAMETER Zip
    The sideload zip, dist\Synka-<version>-sidladdning.zip. Without it, the
    newest release's zip is downloaded from GitHub (gh must be logged in).

.PARAMETER TargetFolder
    Default: $env:OneDriveCommercial\Synka.

.PARAMETER StableName
    The fixed name. Default Synka-latest.zip.

.PARAMETER KeepVersions
    How many versioned copies to keep beside the fixed name. Default 5.

.PARAMETER Force
    Write even when the folder already holds this exact file.

.EXAMPLE
    powershell -NoProfile -File packaging\share_release.ps1
    powershell -NoProfile -File packaging\share_release.ps1 -Zip dist\Synka-0.2.0-sidladdning.zip
#>
[CmdletBinding()]
param(
    [string]$Zip,
    [string]$TargetFolder,
    [string]$StableName = "Synka-latest.zip",
    [ValidateRange(1, 100)]
    [int]$KeepVersions = 5,
    [switch]$Force
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
# Only the sideload zip belongs in the folder.
$pattern = '^Synka-([0-9]+\.[0-9]+\.[0-9]+)-sidladdning\.zip$'

# ---------------------------------------------------------------- the folder

if (-not $TargetFolder) {
    if ([string]::IsNullOrWhiteSpace($env:OneDriveCommercial)) {
        throw "No business OneDrive is signed in on this machine (OneDriveCommercial is not set). Pass -TargetFolder."
    }
    $TargetFolder = Join-Path $env:OneDriveCommercial "Synka"
}
if (-not (Test-Path -LiteralPath $TargetFolder)) {
    New-Item -ItemType Directory -Force -Path $TargetFolder | Out-Null
    Write-Host "Created $TargetFolder"
}
$TargetFolder = (Resolve-Path -LiteralPath $TargetFolder).Path

if (-not (Get-Process OneDrive -ErrorAction SilentlyContinue)) {
    Write-Warning "OneDrive is not running. The file is written, but it reaches nobody until the client starts."
}

# ---------------------------------------------------------------- helpers

function Invoke-Gh {
    param([string[]]$Arguments)
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $output = & gh @Arguments 2>&1 | ForEach-Object { "$_" }
        if ($LASTEXITCODE -ne 0) { throw "gh $($Arguments -join ' ') failed: $($output -join ' ')" }
        return $output
    }
    finally { $ErrorActionPreference = $previous }
}

function Test-InPlace([string]$Name, [long]$Size) {
    # Both files, by SIZE. A copy that failed halfway has the right name and the
    # wrong contents. The fixed name is checked on its own too.
    foreach ($leaf in @($Name, $StableName)) {
        $path = Join-Path $TargetFolder $leaf
        if (-not (Test-Path -LiteralPath $path)) { return $false }
        if ((Get-Item -LiteralPath $path).Length -ne $Size) { return $false }
    }
    return $true
}

function Remove-OldVersions {
    # Sorted as a VERSION, not as text: as text 0.1.10 comes before 0.1.9.
    $copies = Get-ChildItem -LiteralPath $TargetFolder -File |
        ForEach-Object {
            $hit = [regex]::Match($_.Name, $pattern)
            if ($hit.Success) { [PSCustomObject]@{ File = $_; Version = [version]$hit.Groups[1].Value } }
        } |
        Sort-Object Version -Descending

    foreach ($old in @($copies | Select-Object -Skip $KeepVersions)) {
        try {
            Remove-Item -LiteralPath $old.File.FullName -Force
            Write-Host "   removed the old copy $($old.File.Name)" -ForegroundColor DarkGray
        }
        catch {
            Write-Warning "Could not remove $($old.File.Name): $($_.Exception.Message)"
        }
    }
}

$staging = Join-Path ([IO.Path]::GetTempPath()) ("synka-share-" + [guid]::NewGuid().ToString("N").Substring(0, 8))

function Publish-File {
    param([string]$Source, [string]$Destination, [long]$Expected)
    $staged = Join-Path $staging ([IO.Path]::GetRandomFileName())
    Copy-Item -LiteralPath $Source -Destination $staged
    for ($attempt = 1; ; $attempt++) {
        try {
            if (Test-Path -LiteralPath $Destination) { Remove-Item -LiteralPath $Destination -Force }
            Move-Item -LiteralPath $staged -Destination $Destination
            break
        }
        catch {
            # The sync client may hold the file for a moment. Three tries, two seconds apart.
            if ($attempt -ge 3) { throw "Cannot replace $Destination after $attempt attempts: $($_.Exception.Message)" }
            Start-Sleep -Seconds 2
        }
    }
    $written = Get-Item -LiteralPath $Destination
    if ($written.Length -ne $Expected) {
        throw "$($written.Name) is $($written.Length) bytes in the folder, the zip is $Expected. Not the same file."
    }
    Write-Host "   $($written.Name)" -ForegroundColor Green
}

# ---------------------------------------------------------------- the zip

if ($Zip) {
    $source = Get-Item -LiteralPath $Zip
    if ($source.Name -notmatch $pattern) {
        throw "$($source.Name) is not a Synka-<version>-sidladdning.zip. Only the sideload zip goes in the shared folder."
    }
    $name = $source.Name
    $size = $source.Length
    $label = $name
}
else {
    if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
        throw "gh (GitHub CLI) is missing, and without -Zip the file comes from the newest release."
    }
    Push-Location $root
    try { $json = Invoke-Gh @("release", "view", "--json", "tagName,assets") }
    finally { Pop-Location }
    $release = ($json -join "`n") | ConvertFrom-Json
    $asset = $release.assets | Where-Object { $_.name -match $pattern } | Select-Object -First 1
    if (-not $asset) { throw "The release $($release.tagName) has no Synka-<version>-sidladdning.zip." }
    $name = $asset.name
    $size = [long]$asset.size
    $label = "$($release.tagName) ($name)"
}

if (-not $Force -and (Test-InPlace $name $size)) {
    Write-Host "$label is already in $TargetFolder, as $StableName too. Nothing to do." -ForegroundColor DarkGray
    Remove-OldVersions
    exit 0
}

New-Item -ItemType Directory -Force -Path $staging | Out-Null
try {
    if (-not $Zip) {
        Write-Host ("Downloading {0} from the release {1} ({2:N1} MB)" -f $name, $release.tagName, ($size / 1MB))
        Push-Location $root
        try { Invoke-Gh @("release", "download", $release.tagName, "--pattern", $name, "--dir", $staging, "--clobber") | Out-Null }
        finally { Pop-Location }
        $source = Get-Item -LiteralPath (Join-Path $staging $name)
        if ($source.Length -ne $size) {
            throw "The downloaded file is $($source.Length) bytes, the release says $size. Not the same file."
        }
    }

    Write-Host "Writing to $TargetFolder"
    foreach ($leaf in @($name, $StableName)) {
        Publish-File -Source $source.FullName -Destination (Join-Path $TargetFolder $leaf) -Expected $size
    }
    # After the write, never before: the new copy counts towards the tail.
    Remove-OldVersions
}
finally {
    Remove-Item -LiteralPath $staging -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ""
Write-Host "Shared. The folder now holds:" -ForegroundColor Green
Get-ChildItem -LiteralPath $TargetFolder -File |
    Where-Object { $_.Name -eq $StableName -or $_.Name -match $pattern } |
    Sort-Object Name |
    ForEach-Object { Write-Host ("   {0,-40} {1,6:N1} MB   {2:yyyy-MM-dd HH:mm}" -f $_.Name, ($_.Length / 1MB), $_.LastWriteTime) }
