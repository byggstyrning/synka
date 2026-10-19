#Requires -Version 7.0
<#
.SYNOPSIS
Runs Synka's isolated network, sync and IPC failure laboratory.
.EXAMPLE
pwsh -NoProfile -File tools/test-resilience.ps1 -Profile Stress -Seed 42 -Iterations 10
#>
[CmdletBinding()]
param(
    [ValidateSet('Quick', 'Stress', 'Soak')]
    [string]$Profile = 'Quick',
    [uint32]$Seed = 42,
    [ValidateRange(0, 10000)]
    [int]$Iterations = 0,
    [ValidateRange(1, 1440)]
    [int]$DurationMinutes = 30,
    [ValidateRange(10, 1800)]
    [int]$PassTimeoutSeconds = 180,
    [ValidateRange(0, 10000)]
    [int]$Operations = 0,
    [switch]$SkipBaseline,
    [switch]$SkipUiProtocol
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'Synka resilience tests require Windows. For Wine, see tools/test-under-wine.sh.' }

$repoRoot = Split-Path -Parent $PSScriptRoot
$runId = '{0}-{1}-{2}' -f [DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss'), $PID, [guid]::NewGuid().ToString('N').Substring(0, 8)
$runDirectory = Join-Path $repoRoot "target/resilience/$runId"
$scratchDirectory = $null
$scratchFileSystem = $null
New-Item -ItemType Directory -Path $runDirectory -Force | Out-Null
$summaryPath = Join-Path $runDirectory 'summary.json'
$junitPath = Join-Path $runDirectory 'junit.xml'
$stages = [System.Collections.Generic.List[object]]::new()
$started = [DateTime]::UtcNow
$effectiveOperations = if ($Operations -gt 0) { $Operations } elseif ($Profile -eq 'Quick') { 100 } else { 1000 }
$effectiveIterations = if ($Iterations -gt 0) { $Iterations } elseif ($Profile -eq 'Quick') { 1 } elseif ($Profile -eq 'Stress') { 10 } else { 10000 }
$script:finalStatus = 'running'

function New-TestScratchDirectory {
    # The checkout can live on a Dev Drive. Cloud Files and the USN regression
    # fixtures need NTFS. Relocation fixtures also need to be outside system folders
    # such as AppData and Windows Temp. Keep test TEMP independent of build output.
    $candidates = @(
        [Environment]::GetFolderPath([Environment+SpecialFolder]::UserProfile)
        $repoRoot
    ) | Select-Object -Unique
    $failures = [System.Collections.Generic.List[string]]::new()
    foreach ($candidate in $candidates) {
        try {
            if ([string]::IsNullOrWhiteSpace($candidate)) { continue }
            $candidate = [System.IO.Path]::GetFullPath($candidate)
            foreach ($variable in @('WINDIR', 'ProgramFiles', 'ProgramFiles(x86)', 'ProgramData', 'LOCALAPPDATA', 'APPDATA')) {
                $systemFolder = [Environment]::GetEnvironmentVariable($variable)
                if ([string]::IsNullOrWhiteSpace($systemFolder)) { continue }
                $prefix = [System.IO.Path]::GetFullPath($systemFolder).TrimEnd('\')
                if ($candidate.Equals($prefix, [StringComparison]::OrdinalIgnoreCase) -or $candidate.StartsWith("$prefix\", [StringComparison]::OrdinalIgnoreCase)) {
                    throw "$candidate is inside the system folder $systemFolder"
                }
            }
            $root = [System.IO.Path]::GetPathRoot([System.IO.Path]::GetFullPath($candidate))
            $drive = [System.IO.DriveInfo]::new($root)
            if ($drive.DriveFormat -ne 'NTFS') {
                $failures.Add("$candidate uses $($drive.DriveFormat), requires NTFS")
                continue
            }
            $directory = Join-Path $candidate "synka-resilience/$runId/work"
            New-Item -ItemType Directory -Path $directory -Force | Out-Null
            $probe = Join-Path $directory '.write-probe'
            [System.IO.File]::WriteAllText($probe, '')
            [System.IO.File]::Delete($probe)
            return [pscustomobject]@{ directory = $directory; fileSystem = $drive.DriveFormat }
        }
        catch { $failures.Add("${candidate}: $($_.Exception.Message)") }
    }
    throw "No writable NTFS scratch directory is available. $($failures -join '; ')"
}

function Save-Reports {
    $report = [ordered]@{
        schemaVersion = 1
        profile = $Profile
        seed = $Seed
        scratchDirectory = $scratchDirectory
        scratchFileSystem = $scratchFileSystem
        operations = $effectiveOperations
        requestedIterations = $effectiveIterations
        startedUtc = $started.ToString('o')
        updatedUtc = [DateTime]::UtcNow.ToString('o')
        status = $script:finalStatus
        stages = @($stages.ToArray())
    }
    $report | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $summaryPath -Encoding utf8
    $settings = [System.Xml.XmlWriterSettings]::new()
    $settings.Indent = $true
    $writer = [System.Xml.XmlWriter]::Create($junitPath, $settings)
    try {
        $writer.WriteStartDocument()
        $writer.WriteStartElement('testsuite')
        $writer.WriteAttributeString('name', 'Synka resilience stages')
        $writer.WriteAttributeString('tests', [string]$stages.Count)
        $failedStages = @($stages | Where-Object { $_.status -ne 'passed' })
        $writer.WriteAttributeString('failures', [string]$failedStages.Count)
        foreach ($stage in $stages) {
            $writer.WriteStartElement('testcase')
            $writer.WriteAttributeString('name', $stage.name)
            $writer.WriteAttributeString('classname', 'synka.resilience')
            $writer.WriteAttributeString('time', $stage.seconds.ToString('F3', [Globalization.CultureInfo]::InvariantCulture))
            if ($stage.status -ne 'passed') {
                $writer.WriteStartElement('failure')
                $writer.WriteAttributeString('message', "$($stage.status); seed=$($stage.seed); see $($stage.log)")
                $writer.WriteString($stage.command)
                $writer.WriteEndElement()
            }
            $writer.WriteElementString('system-out', "seed=$($stage.seed); operations=$effectiveOperations; log=$($stage.log)`n$($stage.command)")
            $writer.WriteEndElement()
        }
        $writer.WriteEndElement()
        $writer.WriteEndDocument()
    }
    finally { $writer.Dispose() }
}

function Invoke-Stage {
    param(
        [string]$Name,
        [string]$Executable,
        [string[]]$Arguments,
        [uint32]$StageSeed,
        [int]$TimeoutSeconds,
        [switch]$RequireTests,
        [string]$WorkingDirectory = $repoRoot,
        [string]$PassSummaryPattern = 'test result: (?:ok|FAILED)\.\s+(\d+) passed;'
    )
    $logName = "$Name.log"
    $logPath = Join-Path $runDirectory $logName
    $stdoutPath = Join-Path $runDirectory "$Name.stdout.log"
    $stderrPath = Join-Path $runDirectory "$Name.stderr.log"
    $command = '& "{0}" {1}' -f $Executable, (($Arguments | ForEach-Object { "'$($_.Replace("'", "''"))'" }) -join ' ')
    $info = [System.Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Executable
    $info.WorkingDirectory = $WorkingDirectory
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
    $info.Environment['SYNKA_TEST_SEED'] = [string]$StageSeed
    $info.Environment['SYNKA_STRESS_OPS'] = [string]$effectiveOperations
    $info.Environment['RUST_BACKTRACE'] = '1'
    $info.Environment['DOTNET_CLI_TELEMETRY_OPTOUT'] = '1'
    $info.Environment['DOTNET_SKIP_FIRST_TIME_EXPERIENCE'] = '1'
    $info.Environment['DOTNET_NOLOGO'] = '1'
    $info.Environment['DOTNET_CLI_WORKLOAD_UPDATE_NOTIFY_DISABLE'] = '1'
    $info.Environment['DOTNET_CLI_DO_NOT_USE_MSBUILD_SERVER'] = '1'
    $info.Environment['MSBUILDDISABLENODEREUSE'] = '1'
    $info.Environment['DOTNET_CLI_HOME'] = Join-Path $runDirectory 'dotnet-home'
    $info.Environment['DOTNET_ADD_GLOBAL_TOOLS_TO_PATH'] = 'false'
    # Only test processes use this private scratch directory. Cargo needs its usual TEMP.
    if ($RequireTests) {
        $info.Environment['TEMP'] = $scratchDirectory
        $info.Environment['TMP'] = $scratchDirectory
    }
    Write-Host "$Name (seed=$StageSeed, operations=$effectiveOperations)"
    $clock = [System.Diagnostics.Stopwatch]::StartNew()
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $info
    $timedOut = $false
    $stdoutFile = $null
    $stderrFile = $null
    try {
        $stdoutFile = [System.IO.FileStream]::new($stdoutPath, [System.IO.FileMode]::Create, [System.IO.FileAccess]::Write, [System.IO.FileShare]::Read, 1, $true)
        $stderrFile = [System.IO.FileStream]::new($stderrPath, [System.IO.FileMode]::Create, [System.IO.FileAccess]::Write, [System.IO.FileShare]::Read, 1, $true)
        if (-not $process.Start()) { throw "Cannot start $Executable" }
        # Stream logs to disk during execution so a hung test is identifiable immediately.
        $stdoutTask = $process.StandardOutput.BaseStream.CopyToAsync($stdoutFile)
        $stderrTask = $process.StandardError.BaseStream.CopyToAsync($stderrFile)
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            $timedOut = $true
            $process.Kill($true)
            $process.WaitForExit()
        }
        $drain = [System.Threading.Tasks.Task]::WhenAll([System.Threading.Tasks.Task[]]@($stdoutTask, $stderrTask))
        if (-not $drain.Wait(10000)) {
            throw "$Name left output streams open after process exit. See the streamed logs."
        }
        $exitCode = $process.ExitCode
    }
    finally {
        $clock.Stop()
        $process.Dispose()
        if ($stdoutFile) { $stdoutFile.Dispose() }
        if ($stderrFile) { $stderrFile.Dispose() }
    }
    $stdout = [string](Get-Content -LiteralPath $stdoutPath -Raw)
    $stderr = [string](Get-Content -LiteralPath $stderrPath -Raw)
    $testCount = 0
    if ($stdout -match $PassSummaryPattern) { $testCount = [int]$Matches[1] }
    $status = if ($timedOut) { 'timed_out' } elseif ($exitCode -ne 0) { 'failed' } elseif ($RequireTests -and $testCount -eq 0) { 'no_tests' } else { 'passed' }
    @(
        "Command: $command"
        "SYNKA_TEST_SEED=$StageSeed SYNKA_STRESS_OPS=$effectiveOperations"
        "Status: $status; exit=$exitCode; elapsed=$($clock.Elapsed.TotalSeconds)s"
        $stdout
        $stderr
    ) | Set-Content -LiteralPath $logPath -Encoding utf8
    $stages.Add([pscustomobject]@{
        name = $Name; seed = $StageSeed; status = $status; exitCode = $exitCode
        seconds = $clock.Elapsed.TotalSeconds; passedTests = $testCount; log = $logName; command = $command
    })
    Save-Reports
    if ($status -ne 'passed') {
        Write-Host $stdout
        Write-Host $stderr
        throw "$Name $status. Reproduce with seed=$StageSeed and operations=$effectiveOperations. Log: $logPath"
    }
    if ($RequireTests) { Write-Host "  $testCount tests passed in $($clock.Elapsed.TotalSeconds.ToString('F1'))s" }
    return $stdout
}

Write-Host "Reports: $runDirectory"
Save-Reports
try {
    $scratch = New-TestScratchDirectory
    $scratchDirectory = $scratch.directory
    $scratchFileSystem = $scratch.fileSystem
    Write-Host "Test scratch: $scratchDirectory ($scratchFileSystem)"
    Save-Reports
    $cargo = (Get-Command cargo -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
    $buildOutput = Invoke-Stage -Name 'build' -Executable $cargo -Arguments @('test', '--locked', '--no-run', '--message-format=json') -StageSeed $Seed -TimeoutSeconds 600
    $testBinary = $null
    foreach ($line in ($buildOutput -split '\r?\n')) {
        if (-not $line.StartsWith('{')) { continue }
        $entry = $line | ConvertFrom-Json
        if ($entry.reason -eq 'compiler-artifact' -and $entry.target.name -eq 'synka-engine' -and $entry.profile.test -and $entry.executable) {
            $testBinary = [string]$entry.executable
        }
    }
    if (-not $testBinary -or -not (Test-Path -LiteralPath $testBinary -PathType Leaf)) { throw 'Cargo did not report the Synka engine test binary.' }
    if (-not $SkipBaseline) {
        $null = Invoke-Stage -Name 'baseline' -Executable $testBinary -Arguments @('--skip', 'resilience_', '--test-threads=1') -StageSeed $Seed -TimeoutSeconds $PassTimeoutSeconds -RequireTests
    }
    if (-not $SkipUiProtocol) {
        $dotnet = (Get-Command dotnet -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
        $auditDirectory = Join-Path $repoRoot 'tools/ui-ipc-audit'
        $nugetConfig = Join-Path $auditDirectory 'NuGet.Config'
        $null = Invoke-Stage -Name 'ui-protocol-build' -Executable $dotnet -Arguments @('build', 'UiIpcAudit.csproj', '--configuration', 'Release', '--disable-build-servers', '-p:UseSharedCompilation=false', '-p:NuGetAudit=false', "-p:RestoreConfigFile=$nugetConfig", '--nologo') -WorkingDirectory $auditDirectory -StageSeed $Seed -TimeoutSeconds $PassTimeoutSeconds
        $auditBinary = Join-Path $auditDirectory 'bin/Release/net8.0/UiIpcAudit.exe'
        $null = Invoke-Stage -Name 'ui-protocol' -Executable $auditBinary -Arguments @() -WorkingDirectory $auditDirectory -StageSeed $Seed -TimeoutSeconds $PassTimeoutSeconds -RequireTests -PassSummaryPattern 'UI audit: (\d+) passed;'
    }
    $deadline = [DateTime]::UtcNow.AddMinutes($DurationMinutes)
    for ($iteration = 0; $iteration -lt $effectiveIterations; $iteration++) {
        if ($Profile -eq 'Soak' -and $iteration -gt 0 -and [DateTime]::UtcNow -ge $deadline) { break }
        $iterationSeed = [uint32](([uint64]$Seed + [uint64]$iteration) % 4294967296)
        $label = 'resilience-{0:D4}' -f ($iteration + 1)
        $null = Invoke-Stage -Name $label -Executable $testBinary -Arguments @('resilience_', '--test-threads=1', '--nocapture') -StageSeed $iterationSeed -TimeoutSeconds $PassTimeoutSeconds -RequireTests
        if ($Profile -ne 'Quick') {
            $null = Invoke-Stage -Name "$label-stress" -Executable $testBinary -Arguments @('resilience_', '--ignored', '--test-threads=1', '--nocapture') -StageSeed $iterationSeed -TimeoutSeconds $PassTimeoutSeconds -RequireTests
        }
    }
    $script:finalStatus = 'passed'
    Save-Reports
    Write-Host "Passed. Reports: $summaryPath"
}
catch {
    $script:finalStatus = 'failed'
    $_ | Out-String | Set-Content -LiteralPath (Join-Path $runDirectory 'runner-error.log') -Encoding utf8
    if ($stages.Count -eq 0 -or $stages[$stages.Count - 1].status -eq 'passed') {
        $stages.Add([pscustomobject]@{
            name = 'runner'; seed = $Seed; status = 'runner_error'; exitCode = -1
            seconds = ([DateTime]::UtcNow - $started).TotalSeconds; passedTests = 0
            log = 'runner-error.log'; command = "pwsh -NoProfile -File tools/test-resilience.ps1 -Profile $Profile -Seed $Seed -Operations $effectiveOperations"
        })
    }
    Save-Reports
    Write-Error $_ -ErrorAction Continue
    Write-Host "Failure reports: $runDirectory"
    exit 1
}
