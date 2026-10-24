# Copyright (c) 2026 M4n5ter. SPDX-License-Identifier: MIT
# Explicit-dispatch synthetic NTFS/VSS experiment. Never run on a personal host.
# No driver/service-startup/registry/BCD/security changes, force detach, broad
# snapshot deletion, or existing-volume shadow-storage changes are permitted.
# The requester owns only its captured snapshot IDs and exact own-volume diff
# association. A missing/unsafe cleanup report preserves the image and evidence.
[CmdletBinding()]
param(
    [switch] $AllowSyntheticVolume,
    [Parameter(Mandatory)] [string] $Requester,
    [Parameter(Mandatory)] [string] $Sprig,
    [string] $Python = 'python',
    [string] $Fixture = (Join-Path $PSScriptRoot 'fixture.py')
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $AllowSyntheticVolume -or $PSVersionTable.PSVersion.Major -lt 7 -or
    $env:GITHUB_ACTIONS -ne 'true' -or $env:GITHUB_EVENT_NAME -ne 'workflow_dispatch' -or
    $env:RUNNER_OS -ne 'Windows' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or
    $env:ImageOS -notin @('win25', 'win25-vs2026')) {
    throw 'Requires explicit opt-in, PowerShell 7 and workflow_dispatch on a GitHub-hosted Windows 2025 image.'
}
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
try { $administrator = [Security.Principal.WindowsPrincipal]::new($identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator) }
finally { $identity.Dispose() }
if (-not $administrator) { throw 'The disposable runner must already be elevated; no privilege changes are attempted.' }
$os = Get-CimInstance -ClassName Win32_OperatingSystem
if ($os.Caption -notmatch 'Windows Server 2025' -or [int]$os.BuildNumber -ne 26100) {
    throw 'Unexpected operating system; this experiment is restricted to Windows Server 2025.'
}
function Assert-PlainPath([string] $Path) {
    if ($Path -notmatch '^[A-Za-z]:\\' -or $Path -match '[\r\n\x00"]') { throw "Unsafe local path: $Path" }
    $item = Get-Item -LiteralPath $Path -Force
    while ($null -ne $item) {
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'Reparse-point path is not allowed.' }
        $item = if ($item.PSIsContainer) { $item.Parent } else { $item.Directory }
    }
}
function Resolve-LocalFile([string] $Path) {
    $resolved = (Resolve-Path -LiteralPath $Path).Path
    Assert-PlainPath $resolved
    if (-not (Test-Path -LiteralPath $resolved -PathType Leaf)) { throw 'Expected an existing regular file.' }
    return $resolved
}
$Requester = Resolve-LocalFile $Requester
$Sprig = Resolve-LocalFile $Sprig
$Fixture = Resolve-LocalFile $Fixture
$pythonCommand = Get-Command $Python -CommandType Application -ErrorAction Stop
$Python = Resolve-LocalFile $pythonCommand.Source
$root = (Resolve-Path -LiteralPath $env:RUNNER_TEMP).Path
Assert-PlainPath $root
if (@($root.ToCharArray() | Where-Object { [int]$_ -gt 127 }).Count -ne 0) { throw 'DiskPart temporary path must be ASCII.' }
$hostLetter = [IO.Path]::GetPathRoot($root).Substring(0, 1)
$hostVolumes = @(Get-Volume -DriveLetter $hostLetter)
if ($hostVolumes.Count -ne 1 -or $hostVolumes[0].SizeRemaining -lt 8GB) { throw 'At least 8 GiB free space is required before this experiment.' }
# Read only. The system setting is not changed, even when it prevents the test.
$minimumMiB = $null
$registryPath = 'HKLM:\SYSTEM\CurrentControlSet\Services\VolSnap'
if (Test-Path -LiteralPath $registryPath) {
    $values = Get-ItemProperty -LiteralPath $registryPath
    if ($null -ne $values.PSObject.Properties['MinDiffAreaFileSize']) {
        $minimumMiB = [uint64]$values.MinDiffAreaFileSize
        if ([Math]::Ceiling($minimumMiB / 32.0) * 32 -gt 512) { throw 'Existing MinDiffAreaFileSize exceeds the authorized 512 MiB cap; no registry change is allowed.' }
    }
}
$runId = [Guid]::NewGuid().ToString('N')
$directory = Join-Path $root ('sprig-vss-work-' + $runId)
$evidence = Join-Path $root ('sprig-vss-evidence-' + $runId)
foreach ($path in @($directory, $evidence)) {
    if (Test-Path -LiteralPath $path) { throw 'A supposedly new run directory already exists.' }
    [void][IO.Directory]::CreateDirectory($path)
    Assert-PlainPath $path
}
$image = Join-Path $directory 'synthetic.vhdx'
$hostWork = Join-Path $directory 'host-work'
[void][IO.Directory]::CreateDirectory($hostWork)
$store = Join-Path $hostWork 'cas'
$writerReady = Join-Path $hostWork 'writer.ready'
$writerStop = Join-Path $hostWork 'writer.stop'
$markerToken = [Guid]::NewGuid().ToString('N')
$maximumImageBytes = [uint64]4GB
$maximumAddedBytes = [uint64]2GB
$budgetReserveBytes = [uint64]64MB
$processes = [Collections.Generic.List[object]]::new()
$result = [ordered]@{
    version = 1; ok = $false; run_id = $runId; image = $image; evidence_directory = $evidence
    maximum_image_bytes = $maximumImageBytes; maximum_added_bytes = $maximumAddedBytes
    maximum_diff_bytes = [uint64]512MB; payload_file_mib = 8; payload_files = 3
    min_diff_area_file_size_mib = $minimumMiB; image_os = $env:ImageOS
    operating_system = ($os | Select-Object Caption, Version, BuildNumber)
    created = $false; mounted = $false; formatted = $false; cleanup = 'not-mounted'
    captures = @(); storage_checkpoints = @(); errors = @()
}
$mountAttempted = $false
$runtimeUncertain = $false
$writer = $null
$partition = $null
$initializedIdentity = $null
$volumeGuid = $null
$serialHex = $null
$disk = $null
$scriptFailure = $null
$cleanupFailure = $null
$oldRuntimeOptIn = $env:SPRIG_VSS_ENABLE_RUNTIME
# Expose only the separate, whitelist-only evidence directory to the workflow.
if ($env:GITHUB_OUTPUT) { "evidence_directory=$evidence" | Add-Content -LiteralPath $env:GITHUB_OUTPUT -Encoding utf8 }

function Get-TreeBytes([string] $Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return [uint64]0 }
    $sum = [uint64]0
    $pending = [Collections.Generic.Stack[string]]::new()
    $pending.Push($Path)
    while ($pending.Count -gt 0) {
        try { $current = Get-Item -LiteralPath $pending.Pop() -Force }
        catch [System.Management.Automation.ItemNotFoundException] {
            # Sprig may atomically rename its own temporary metadata while this
            # read-only budget sample runs. Never suppress other read failures.
            continue
        }
        if (($current.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'Unexpected reparse point in owned output.' }
        if ($current.PSIsContainer) {
            foreach ($entry in @(Get-ChildItem -LiteralPath $current.FullName -Force)) { $pending.Push($entry.FullName) }
        } else { $sum += [uint64]$current.Length }
    }
    return $sum
}
function Assert-OutputBudget([string] $Stage) {
    # Counts VHDX backing-file bytes (including synthetic source/diff area), all
    # outside-volume outputs and all logs; does not double-count the VHD contents.
    $added = (Get-TreeBytes $directory) + (Get-TreeBytes $evidence)
    $result.storage_checkpoints += [ordered]@{ stage = $Stage; added_file_bytes = $added }
    if ($added -gt ($maximumAddedBytes - $budgetReserveBytes)) {
        throw 'Synthetic output exhausted the conservative 2 GiB budget; no further capture is permitted.'
    }
    return $added
}
function Start-OwnedProcess([string] $File, [string[]] $Arguments, [string] $Label) {
    if ($Label -notmatch '^[a-z0-9-]+$') { throw 'Invalid process evidence label.' }
    $stdout = Join-Path $evidence ($Label + '.stdout.log')
    $stderr = Join-Path $evidence ($Label + '.stderr.log')
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $File
    $start.UseShellExecute = $false
    $start.RedirectStandardInput = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $start.WorkingDirectory = $hostWork
    foreach ($argument in $Arguments) { $start.ArgumentList.Add($argument) }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $start
    $record = [pscustomobject]@{
        Label = $Label; Process = $process; StartUnknown = $true; Started = $false
        Drained = $false; Stdout = $stdout; Stderr = $stderr; OutStream = $null; ErrStream = $null
        OutTask = $null; ErrTask = $null; ExitCode = $null
    }
    $processes.Add($record)
    $record.OutStream = [IO.FileStream]::new($stdout, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
    $record.ErrStream = [IO.FileStream]::new($stderr, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
    $record.Started = $process.Start()
    $record.StartUnknown = $false
    if (-not $record.Started) { throw "Process $Label did not start." }
    $process.StandardInput.Close()
    $record.OutTask = $process.StandardOutput.BaseStream.CopyToAsync($record.OutStream)
    $record.ErrTask = $process.StandardError.BaseStream.CopyToAsync($record.ErrStream)
    return $record
}
function Wait-OwnedProcess($Record, [int] $Seconds) {
    $watch = [Diagnostics.Stopwatch]::StartNew()
    while (-not $Record.Process.WaitForExit(1000)) {
        if ($watch.Elapsed.TotalSeconds -ge $Seconds) { throw "Process $($Record.Label) is still active; no kill or forced cleanup will be attempted." }
        # The fixture inputs and capture count are fixed. This guard also catches
        # runaway logs or unexpectedly large storage output before further work.
        if ((Get-TreeBytes $directory) + (Get-TreeBytes $evidence) -gt ($maximumAddedBytes - $budgetReserveBytes)) {
            throw 'Output budget reached while a child is active; preserve evidence rather than killing it.'
        }
    }
    foreach ($task in @($Record.OutTask, $Record.ErrTask)) {
        if (-not $task.Wait(10000)) { throw "Process $($Record.Label) output is not drained; a descendant may still own a pipe." }
        $task.GetAwaiter().GetResult()
    }
    $Record.OutStream.Dispose()
    $Record.ErrStream.Dispose()
    $Record.ExitCode = $Record.Process.ExitCode
    $Record.Drained = $true
    return $Record.ExitCode
}
function Invoke-OwnedProcess([string] $File, [string[]] $Arguments, [string] $Label, [int] $Seconds = 300) {
    $record = Start-OwnedProcess $File $Arguments $Label
    $exitCode = Wait-OwnedProcess $record $Seconds
    if ($exitCode -ne 0) { throw "Process $Label exited $exitCode; see its retained logs." }
    return $record
}
function Get-MappedDisk {
    $mapped = @(Get-DiskImage -ImagePath $image | Get-Disk)
    if ($mapped.Count -ne 1) { throw 'Exact image must map to one disk.' }
    return $mapped[0]
}
function Assert-NewPartition {
    $fresh = Get-MappedDisk
    if ($fresh.UniqueId -ne $initializedIdentity -or $fresh.Number -ne $partition.DiskNumber -or
        $fresh.Size -ne $maximumImageBytes -or $fresh.PartitionStyle -ne 'GPT' -or $fresh.IsBoot -or $fresh.IsSystem) {
        throw 'Fresh image disk identity changed.'
    }
    $parts = @(Get-Partition -DriveLetter $partition.DriveLetter)
    if ($parts.Count -ne 1) { throw 'Fresh partition drive letter is not unique.' }
    $p = $parts[0]
    if ($p.DiskNumber -ne $partition.DiskNumber -or $p.PartitionNumber -ne $partition.PartitionNumber -or
        $p.Offset -ne $partition.Offset -or $p.Size -ne $partition.Size -or $p.GptType -ne $partition.GptType) {
        throw 'Drive letter no longer maps to the exact newly-created partition.'
    }
}
function Assert-OwnVolume {
    Assert-NewPartition
    $matches = @(Get-CimInstance -ClassName Win32_Volume | Where-Object DriveLetter -eq "$($partition.DriveLetter):")
    if ($matches.Count -ne 1 -or $matches[0].DeviceID -ne $volumeGuid -or
        ('{0:X8}' -f [uint32]$matches[0].SerialNumber) -ne $serialHex -or $matches[0].FileSystem -ne 'NTFS') {
        throw 'Synthetic volume GUID, serial or filesystem changed.'
    }
    $marker = Join-Path "$($partition.DriveLetter):\" '.sprig-vss-synthetic-volume'
    if ([IO.File]::ReadAllText($marker) -cne "sprig-vss-synthetic-only-v1`n$markerToken`n") { throw 'Synthetic marker changed.' }
}
function Assert-NoOwnVssState {
    # Independent read-only check. Never delete any snapshot or association here.
    $shadows = @(Get-CimInstance -ClassName Win32_ShadowCopy | Where-Object VolumeName -eq $volumeGuid)
    if ($shadows.Count -ne 0) { throw 'A shadow still references the synthetic volume; retain the image.' }
    foreach ($association in @(Get-CimInstance -ClassName Win32_ShadowStorage)) {
        foreach ($reference in @($association.Volume, $association.DiffVolume)) {
            if ($null -eq $reference -or $null -eq $reference.PSObject.Properties['DeviceID'] -or -not $reference.DeviceID) {
                throw 'Cannot verify shadow-storage reference identity; retain the image.'
            }
            if ($reference.DeviceID -eq $volumeGuid) { throw 'A diff-area association still references the synthetic volume; retain the image.' }
        }
    }
}
function Copy-CaptureEvidence([string] $CaptureWork, [string] $Label) {
    # Exact helper-created diagnostics only, with a small aggregate allowance.
    # This is deliberately not a recursive copy of the synthetic work directory.
    $allowance = [int64]16MB
    $files = @([pscustomobject]@{ Source = (Join-Path $CaptureWork 'capture.json'); Name = ('capture-' + $Label + '.json') })
    foreach ($number in 1..32) {
        foreach ($stream in @('stdout', 'stderr')) {
            $name = 'sprig-{0:D2}.{1}.log' -f $number, $stream
            $files += [pscustomobject]@{ Source = (Join-Path (Join-Path $CaptureWork 'logs') $name); Name = ('capture-' + $Label + '-' + $name) }
        }
    }
    foreach ($file in $files) {
        if (Test-Path -LiteralPath $file.Source -PathType Leaf) {
            Assert-PlainPath $file.Source
            $length = (Get-Item -LiteralPath $file.Source).Length
            if ($length -gt 8MB -or $length -gt $allowance) { throw 'Capture diagnostics exceed the evidence whitelist size limit.' }
            Copy-Item -LiteralPath $file.Source -Destination (Join-Path $evidence $file.Name)
            $allowance -= $length
        }
    }
}
function Invoke-Capture([string] $Label, [string] $PreviousReport = '') {
    Assert-OwnVolume
    [void](Assert-OutputBudget ('before-' + $Label))
    $captureWork = Join-Path $hostWork ('capture-' + $Label)
    if (Test-Path -LiteralPath $captureWork) { throw 'Capture destination already exists.' }
    $childArguments = @($Fixture, 'capture', '{snapshot-root}', '--live-root', $liveRoot,
        '--relative-path', 'fixture', '--work-root', $captureWork, '--store', $store,
        '--sprig', $Sprig, '--label', $Label)
    if ($PreviousReport) { $childArguments += @('--previous-report', $PreviousReport) }
    $arguments = @('--approved-synthetic-volume', $volumeGuid, '--expected-serial', $serialHex,
        '--marker-token', $markerToken, '--acknowledge-storage-snapshot-only', '--allow-own-volume-diff-512m',
        '--vss-timeout-seconds', '120', '--child-timeout-seconds', '300', '--child-exe', $Python, '--') + $childArguments
    # Nothing below can authorize detach until both independent reports prove
    # successful child drain and exact snapshot/association cleanup.
    $script:runtimeUncertain = $true
    $record = Start-OwnedProcess $Requester $arguments ('requester-' + $Label)
    $exitCode = Wait-OwnedProcess $record 600
    Copy-CaptureEvidence $captureWork $Label
    $lines = @(Get-Content -LiteralPath $record.Stdout | Where-Object { $_.StartsWith('SPRIG_VSS_RESULT ') })
    if ($lines.Count -ne 1) { throw 'Missing or ambiguous requester final result; retain the image.' }
    $requestReport = $lines[0].Substring('SPRIG_VSS_RESULT '.Length) | ConvertFrom-Json
    $captureReportPath = Join-Path $captureWork 'capture.json'
    $captureReport = Get-Content -LiteralPath $captureReportPath -Raw | ConvertFrom-Json
    $result.captures += [ordered]@{ label = $Label; requester = $requestReport; fixture = $captureReport }
    if ($exitCode -ne 0 -or $requestReport.version -ne 1 -or $requestReport.ok -ne $true -or
        $requestReport.cleanup_safe -ne $true -or $requestReport.volume -ne $volumeGuid -or
        $requestReport.provider_id -ne '{b5946137-7b9f-4925-af80-51abd60b20d5}' -or
        $requestReport.child_drained -ne $true -or $requestReport.child_exit_code -ne 0 -or
        $requestReport.snapshot_cleanup -ne 'confirmed' -or $requestReport.diff_area_cleanup -ne 'confirmed' -or
        $captureReport.cleanup_safe -ne $true) {
        throw "Capture $Label did not prove successful cleanup; retain the image."
    }
    Assert-NoOwnVssState
    $script:runtimeUncertain = $false
    [void](Assert-OutputBudget ('after-' + $Label))
    return $captureReportPath
}
try {
    $before = @(Get-Disk | Select-Object Number, UniqueId)
    if (Test-Path -LiteralPath $image) { throw 'Image must not exist before creation.' }
    $createScript = Join-Path $directory 'create-vhd.txt'
    "create vdisk file=`"$image`" maximum=4096 type=expandable`nexit" | Set-Content -LiteralPath $createScript -Encoding ascii
    [void](Invoke-OwnedProcess (Join-Path $env:SystemRoot 'System32\diskpart.exe') @('/s', $createScript) 'diskpart-create' 90)
    if (-not (Test-Path -LiteralPath $image -PathType Leaf)) { throw 'VHDX creation failed; no retry.' }
    Assert-PlainPath $image
    $result.created = $true
    $mountAttempted = $true
    # Bind to the actual Mount-DiskImage returned object, not a disk-number guess.
    $mounted = @(Mount-DiskImage -ImagePath $image -NoDriveLetter -PassThru)
    if ($mounted.Count -ne 1 -or $mounted[0].ImagePath -ne $image -or -not $mounted[0].Attached) { throw 'Unexpected mount result.' }
    $mapped = @($mounted[0] | Get-Disk)
    if ($mapped.Count -ne 1) { throw 'Mounted image must map to one disk.' }
    $disk = $mapped[0]
    if (@($before | Where-Object Number -eq $disk.Number).Count -ne 0 -or
        ($disk.UniqueId -and @($before | Where-Object UniqueId -eq $disk.UniqueId).Count -ne 0) -or
        $disk.IsBoot -or $disk.IsSystem -or $disk.PartitionStyle -ne 'RAW' -or $disk.Size -ne $maximumImageBytes) {
        throw 'Refusing a preexisting, boot, system, non-RAW or unexpected-size disk.'
    }
    $result.mounted = $true
    $fresh = Get-MappedDisk
    if ($fresh.Number -ne $disk.Number -or $fresh.UniqueId -ne $disk.UniqueId -or $fresh.Size -ne $maximumImageBytes -or
        $fresh.IsBoot -or $fresh.IsSystem -or $fresh.PartitionStyle -ne 'RAW') { throw 'Image mapping changed before initialization.' }
    $initialized = @($fresh | Initialize-Disk -PartitionStyle GPT -PassThru -Confirm:$false)
    if ($initialized.Count -ne 1 -or $initialized[0].Number -ne $disk.Number -or $initialized[0].IsBoot -or $initialized[0].IsSystem) { throw 'Unexpected initialized disk.' }
    $initializedIdentity = $initialized[0].UniqueId
    if (-not $initializedIdentity) { throw 'Initialized image has no stable disk identity.' }
    $fresh = Get-MappedDisk
    if ($fresh.Number -ne $disk.Number -or $fresh.UniqueId -ne $initializedIdentity -or
        $fresh.Size -ne $maximumImageBytes -or $fresh.PartitionStyle -ne 'GPT' -or $fresh.IsBoot -or $fresh.IsSystem) {
        throw 'Image mapping changed before partition creation.'
    }
    $parts = @($fresh | New-Partition -UseMaximumSize -AssignDriveLetter)
    if ($parts.Count -ne 1 -or $parts[0].DiskNumber -ne $disk.Number -or -not $parts[0].DriveLetter) { throw 'Unexpected fresh partition.' }
    $partition = $parts[0]
    Assert-NewPartition
    # Quick format only this just-created partition. Never -Force or a preexisting volume.
    $formatted = @($partition | Format-Volume -FileSystem NTFS -NewFileSystemLabel 'SprigVssSynthetic' -Confirm:$false)
    if ($formatted.Count -ne 1 -or $formatted[0].FileSystem -ne 'NTFS') { throw 'Fresh NTFS format did not succeed.' }
    Assert-NewPartition
    $volumes = @(Get-CimInstance -ClassName Win32_Volume | Where-Object DriveLetter -eq "$($partition.DriveLetter):")
    if ($volumes.Count -ne 1 -or $volumes[0].FileSystem -ne 'NTFS' -or $volumes[0].BootVolume -or $volumes[0].SystemVolume) { throw 'Unexpected formatted volume identity.' }
    $volumeGuid = [string]$volumes[0].DeviceID
    if ($volumeGuid -notmatch '^\\\\\?\\Volume\{[0-9A-Fa-f-]{36}\}\\$') { throw 'Expected a canonical volume GUID path.' }
    $serialHex = '{0:X8}' -f [uint32]$volumes[0].SerialNumber
    if ($partition.DriveLetter -eq $hostLetter -or $volumeGuid -eq $hostVolumes[0].UniqueId) { throw 'CAS/staging/restore must be on the separate host volume.' }
    $result.formatted = $true
    $result.disk = $fresh | Select-Object Number, UniqueId, Size, LogicalSectorSize, PhysicalSectorSize, BusType
    $result.volume = [ordered]@{ guid = $volumeGuid; serial = $serialHex; drive_letter = [string]$partition.DriveLetter; filesystem = 'NTFS' }
    $volumeRoot = "$($partition.DriveLetter):\"
    $marker = Join-Path $volumeRoot '.sprig-vss-synthetic-volume'
    if (Test-Path -LiteralPath $marker) { throw 'Fresh synthetic marker unexpectedly exists.' }
    [IO.File]::WriteAllText($marker, "sprig-vss-synthetic-only-v1`n$markerToken`n", [Text.ASCIIEncoding]::new())
    Assert-OwnVolume
    Assert-NoOwnVssState
    $liveRoot = Join-Path $volumeRoot 'fixture'
    [void](Invoke-OwnedProcess $Python @($Fixture, 'prepare', '--live-root', $liveRoot, '--file-mib', '8') 'fixture-prepare' 120)
    [void](Assert-OutputBudget 'prepared')
    $env:SPRIG_VSS_ENABLE_RUNTIME = '1'
    [void](Invoke-Capture 'initial')
    $writer = Start-OwnedProcess $Python @($Fixture, 'writer', '--live-root', $liveRoot, '--seconds', '300',
        '--generation', '1', '--ready-file', $writerReady, '--stop-file', $writerStop) 'fixture-writer'
    $readyDeadline = [Diagnostics.Stopwatch]::StartNew()
    while (-not (Test-Path -LiteralPath $writerReady -PathType Leaf)) {
        if ($writer.Process.HasExited -or $readyDeadline.Elapsed.TotalSeconds -gt 30) { throw 'Concurrent writer did not become ready.' }
        Start-Sleep -Milliseconds 100
    }
    [void](Invoke-Capture 'live-concurrent')
    [IO.File]::WriteAllText($writerStop, 'stop', [Text.ASCIIEncoding]::new())
    if ((Wait-OwnedProcess $writer 320) -ne 0) { throw 'Concurrent writer failed to stop cleanly.' }
    $settledReport = Invoke-Capture 'settled'
    [void](Invoke-Capture 'repeated' $settledReport)
    [void](Invoke-OwnedProcess $Python @($Fixture, 'measure', $liveRoot, $hostWork) 'fixture-measure' 120)
    [void](Assert-OutputBudget 'complete')
    $result.ok = $true
} catch {
    $scriptFailure = $_
    $result.errors += $_.Exception.Message
} finally {
    $env:SPRIG_VSS_ENABLE_RUNTIME = $oldRuntimeOptIn
    try {
        if ($null -ne $writer -and $writer.Started -and -not $writer.Drained) {
            # Cooperative stop request is confined to our new host directory.
            # Never kill a process, stop VSS services, or force an unmount.
            if (-not (Test-Path -LiteralPath $writerStop)) { [IO.File]::WriteAllText($writerStop, 'stop', [Text.ASCIIEncoding]::new()) }
            if ((Wait-OwnedProcess $writer 320) -ne 0) { throw 'Writer did not exit successfully.' }
        }
        foreach ($record in $processes) {
            if ($record.StartUnknown -or ($record.Started -and (-not $record.Process.HasExited -or -not $record.Drained))) {
                throw "Process $($record.Label) start/exit/drain is uncertain; image retained."
            }
        }
        if ($runtimeUncertain) { throw 'Requester/collector snapshot or child cleanup is unverified; image retained.' }
        if ($mountAttempted) {
            if ($volumeGuid) { Assert-OwnVolume; Assert-NoOwnVssState }
            $currentImage = Get-DiskImage -ImagePath $image
            if ($currentImage.Attached) { Dismount-DiskImage -ImagePath $image }
            if ((Get-DiskImage -ImagePath $image).Attached) { throw 'Image remains attached after normal detach.' }
            $result.cleanup = 'detached'
        }
    } catch {
        $cleanupFailure = $_
        $result.ok = $false
        $result.cleanup = 'retained: ' + $_.Exception.Message
        $result.errors += $_.Exception.Message
    }
    $result.processes = @($processes | ForEach-Object {
        [ordered]@{ label = $_.Label; started = $_.Started; start_unknown = $_.StartUnknown; drained = $_.Drained; exit_code = $_.ExitCode }
    })
    $result | ConvertTo-Json -Depth 16 | Set-Content -LiteralPath (Join-Path $evidence 'result.json') -Encoding utf8
    $result | ConvertTo-Json -Depth 16 | Write-Output
    # The evidence directory has only result.json, capture-*.json and *.log.
    # The workflow must upload this directory's explicit whitelist, NEVER work,
    # runner-temp globs, synthetic.vhdx, fixtures, CAS, staging or restore trees.
    # No files are deleted: preserve owned evidence even after successful detach.
}
if ($null -ne $scriptFailure) { throw $scriptFailure }
if ($null -ne $cleanupFailure) { throw $cleanupFailure }
