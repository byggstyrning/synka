<#
.SYNOPSIS
    Installerar Synka på den här datorn. Kör Install.cmd i stället.

.DESCRIPTION
    Tre steg, i den här ordningen:

      1. Litar certifikatet som appen är signerad med. Det kräver administratör,
         en gång per dator, och det är det enda som görs med administratörsrätt.
      2. Installerar Microsoft.WindowsAppRuntime (ramverket som appen använder).
      3. Installerar Synka för den användare som kör skriptet, så appen hamnar
         i rätt användarprofil.

    Steg 1 körs i ett eget, upphöjt PowerShell-fönster. Att det lyckades kontrolleras
    genom att certifikatet faktiskt finns i butiken efteråt, inte på returkoden.

    Nyare paket installeras som uppdatering på plats. En redan installerad version
    med samma nummer lämnas orörd så att LocalState aldrig behöver kopieras medan
    synkmotorn kan skriva i sin SQLite-databas.

    Appen under en tidigare paketidentitet tas bort efter en fråga.
#>
[CmdletBinding()]
param(
    # Hoppar över "tryck Enter"-stoppen. Även SYNKA_NO_PAUSE=1 gör det.
    [switch]$NoPause
)

$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $PSCommandPath
$quiet = $NoPause -or [bool]$env:SYNKA_NO_PAUSE
$packageName = "Byggstyrning.Synka"

# Backstop. Väntade fel går via Stop-With; det som ändå slipper igenom landar här,
# så fönstret aldrig stängs tyst.
trap {
    Write-Host ""
    Write-Host "  OVÄNTAT FEL: $($_.Exception.Message)" -ForegroundColor Red
    Write-Host ""
    if (-not $quiet) { Read-Host "  Tryck Enter för att stänga" | Out-Null }
    exit 1
}

function Say([string]$text, [string]$color = "Gray") { Write-Host $text -ForegroundColor $color }

function Stop-With([string]$text) {
    Say ""
    Say "  FEL: $text" Red
    Say ""
    if (-not $quiet) { Read-Host "  Tryck Enter för att stänga" | Out-Null }
    exit 1
}

Say ""
Say "  Synka - installation" Cyan
Say "  ----------------------" Cyan
Say ""

# ------------------------------------------------------------- förutsättningar

# Paketet är byggt för x64. Kontrolleras före allt annat.
$arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
if ($arch -ne "AMD64") {
    Stop-With "Den här datorn är $arch. Synka är byggt för 64-bitars Windows (x64)."
}

# Windows 10 version 2004 (build 19041) är lägsta nivån för Windows App SDK 1.8.
$build = [int](Get-ItemProperty "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion").CurrentBuildNumber
if ($build -lt 19041) {
    Stop-With "Windows är för gammalt (build $build). Kräver minst build 19041, det vill säga Windows 10 version 2004."
}

# ------------------------------------------------------------- hitta delarna

# Innan något annat, och före upphöjningen. Den vanligaste missen är att köra
# inifrån zip-visaren; det ska märkas innan någon frågas om administratörsrätt.
$msix = Get-ChildItem $here -Filter "Synka_*_x64.msix" -File | Select-Object -First 1
if (-not $msix) { Stop-With "Ingen Synka_*.msix hittades bredvid skriptet. Packade du upp HELA zip-filen?" }

# Use the signed package's manifest, not its filename, to decide whether this is
# an upgrade. Removing the existing package would delete LocalState and copying
# a live SQLite database (including WAL) is not a safe replacement for an update.
Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive = [IO.Compression.ZipFile]::OpenRead($msix.FullName)
try {
    $manifestEntry = $archive.GetEntry("AppxManifest.xml")
    if (-not $manifestEntry) { Stop-With "Paketet saknar AppxManifest.xml." }
    $reader = New-Object IO.StreamReader($manifestEntry.Open())
    try { [xml]$manifest = $reader.ReadToEnd() }
    finally { $reader.Dispose() }
}
finally { $archive.Dispose() }
$identity = $manifest.SelectSingleNode("/*[local-name()='Package']/*[local-name()='Identity']")
if (-not $identity -or $identity.GetAttribute("Name") -ne $packageName -or
    $identity.GetAttribute("ProcessorArchitecture") -ne "x64") {
    Stop-With "Paketet har fel appidentitet eller arkitektur. Ladda ned Synka-paketet igen."
}
$targetVersion = [version]$identity.GetAttribute("Version")
$existing = Get-AppxPackage -Name $packageName -ErrorAction SilentlyContinue | Select-Object -First 1
if ($existing -and $existing.Publisher -ne $identity.GetAttribute("Publisher")) {
    Stop-With "Det installerade paketet har en annan utgivare. Ingenting ändrades."
}
if ($existing -and [version]$existing.Version -gt $targetVersion) {
    Stop-With "En nyare version ($($existing.Version)) finns redan. Ingenting ändrades."
}
$alreadyInstalled = $existing -and [version]$existing.Version -eq $targetVersion

$cer = Get-ChildItem $here -Filter "*.cer" -File | Select-Object -First 1
if (-not $cer) { Stop-With "Inget certifikat (*.cer) hittades bredvid skriptet. Packade du upp HELA zip-filen?" }

$runtime = Get-ChildItem (Join-Path $here "Dependencies") -Recurse -Filter "Microsoft.WindowsAppRuntime.*.msix" -File -ErrorAction SilentlyContinue |
    Select-Object -First 1
if (-not $runtime) { Stop-With "Ramverket hittades inte under Dependencies\. Packade du upp HELA zip-filen?" }

# The earlier package identities of this same app. Such a package cannot be upgraded in
# place; it must be removed first.
$legacyNames = @("Byggstyrning.BetterDesktopConnector", "Byggstyrning.FastADC")
$legacy = @($legacyNames | ForEach-Object { Get-AppxPackage -Name $_ -ErrorAction SilentlyContinue })
if ($legacy.Count -gt 0) {
    Say "  En tidigare version av appen är installerad, under en äldre paketidentitet." Yellow
    Say "  Synka ersätter den och måste ta bort den först. Kontrollera innan dess att den gamla" DarkGray
    Say "  appen visar 'Synkad' utan väntande ändringar: det som inte har synkats laddas inte upp" DarkGray
    Say "  av Synka. Gamla filer finns kvar i %USERPROFILE%\BDC; Synka använder %USERPROFILE%\Synka." DarkGray
    if ($quiet) { Stop-With "Avinstallera den äldre appen (Inställningar > Appar) och kör Install.cmd igen. Ingenting ändrades." }
    $answer = Read-Host "  Ta bort den äldre appen nu? (j/n)"
    if ($answer -notmatch '^(j|ja|y|yes)$') { Stop-With "Ingenting ändrades. Kör Install.cmd igen när du vill byta till Synka." }
    foreach ($old in $legacy) {
        try { Remove-AppxPackage -Package $old.PackageFullName -ErrorAction Stop; Say "        den tidigare versionen är borttagen" Green }
        catch { Stop-With "Den äldre appen kunde inte tas bort: $($_.Exception.Message)" }
    }
}

# Downloaded files carry the Mark of the Web. PowerShell refuses a marked .ps1, and
# Add-AppxPackage can object to a marked .msix. Removed before anything is run.
Get-ChildItem $here -Recurse -File | Unblock-File -ErrorAction SilentlyContinue

$releaseCertificate = New-Object Security.Cryptography.X509Certificates.X509Certificate2 $cer.FullName
$thumbprint = $releaseCertificate.Thumbprint
if ($identity.GetAttribute("Publisher") -ne $releaseCertificate.Subject) {
    Stop-With "Certifikatet och paketets utgivare matchar inte. Ingenting installerades."
}
$packageSignature = Get-AuthenticodeSignature -LiteralPath $msix.FullName
if (-not $packageSignature.SignerCertificate -or $packageSignature.SignerCertificate.Thumbprint -ne $thumbprint) {
    Stop-With "Paketet är inte signerat med det medföljande certifikatet. Ingenting installerades."
}

function Test-CertificateTrusted {
    return [bool](Get-ChildItem Cert:\LocalMachine\TrustedPeople -ErrorAction SilentlyContinue |
        Where-Object { $_.Thumbprint -eq $thumbprint })
}

# ------------------------------------------------ 1. trust the certificate (admin)

Say "  [1/3] Litar certifikatet" Cyan
if (Test-CertificateTrusted) {
    Say "        redan betrott" DarkGray
}
else {
    Say "        Windows frågar efter administratörsrätt. Svara Ja." Yellow
    Say "        Det behövs bara för att lita certifikatet, en gång per dator." DarkGray

    # Ett enda kommando, kodat, så att sökvägen inte behöver citattecken genom två
    # nivåer av PowerShell. En apostrof i sökvägen dubbleras.
    $escaped = $cer.FullName -replace "'", "''"
    $command = "Import-Certificate -FilePath '$escaped' -CertStoreLocation Cert:\LocalMachine\TrustedPeople | Out-Null"
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($command))

    try {
        Start-Process powershell.exe -ArgumentList @("-NoProfile", "-ExecutionPolicy", "Bypass", "-EncodedCommand", $encoded) `
            -Verb RunAs -Wait | Out-Null
    }
    catch {
        Stop-With "Administratörsrätten gavs inte, så certifikatet kunde inte litas. Ingenting installerades. Kör Install.cmd igen och svara Ja på frågan från Windows, eller högerklicka på Install.cmd och välj 'Kör som administratör'."
    }

    # Kontrollen är att certifikatet finns, inte att processen returnerade något.
    if (-not (Test-CertificateTrusted)) {
        Stop-With "Certifikatet är fortfarande inte betrott, så ingenting installerades. Kör Install.cmd igen och svara Ja på administratörsfrågan."
    }
    Say "        betrott" Green
}
if ((Get-AuthenticodeSignature -LiteralPath $msix.FullName).Status -ne "Valid") {
    Stop-With "Paketets signatur är inte giltig. Hämta installationsfilen igen."
}

# ------------------------------------------------------------- 2. ramverket

Say "  [2/3] Installerar ramverket (Windows App SDK)" Cyan
try {
    Add-AppxPackage -Path $runtime.FullName -ErrorAction Stop
    Say "        installerat" Green
}
catch {
    $message = "$($_.Exception.Message)"
    if ($message -match "0x80073D06") {
        # Datorn har redan en NYARE version. Paketet kräver 1.8 som lägsta, och en
        # högre version uppfyller det. Inget fel.
        Say "        en nyare version finns redan" DarkGray
    }
    elseif ($message -match "0x80073D02") {
        Stop-With "Ramverket används av ett program som kör just nu. Stäng öppna program och kör Install.cmd igen. Windows sa: $message"
    }
    else {
        Stop-With "Ramverket kunde inte installeras: $message"
    }
}

# ------------------------------------------------------------- 3. appen

Say "  [3/3] Installerar Synka" Cyan

if ($alreadyInstalled) {
    Say "        version $targetVersion finns redan - inga appdata ändrades" DarkGray
}
else {
    if ($existing) { Say "        uppdaterar version $($existing.Version) till $targetVersion på plats" DarkGray }
    try {
        Add-AppxPackage -Path $msix.FullName -ForceTargetApplicationShutdown -ErrorAction Stop
    }
    catch {
        Stop-With "Synka kunde inte installeras: $($_.Exception.Message)"
    }
}

$installed = Get-AppxPackage -Name $packageName -ErrorAction SilentlyContinue
if (-not $installed) { Stop-With "Installationen verkar lyckad, men Synka syns inte bland de installerade apparna." }

if ([version]$installed.Version -ne $targetVersion) { Stop-With "Appen har version $($installed.Version), inte den väntade $targetVersion." }
Say "        klart - version $($installed.Version)" Green

# ------------------------------------------------------------- starta

Say ""
Say "  Startar Synka." Cyan
Start-Process -FilePath "explorer.exe" -ArgumentList "shell:AppsFolder\$($installed.PackageFamilyName)!App"

Say ""
Say "  Installationen är klar. Synka startar vid inloggning och finns i Start-menyn." Green
Say ""
if (-not $quiet) { Read-Host "  Tryck Enter för att stänga" | Out-Null }
exit 0
