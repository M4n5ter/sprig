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
$maximumImageBytes = [uint64]1GB
$maximumAddedBytes = [uint64]2GB
$budgetReserveBytes = [uint64]64MB
$maximumProcessLogBytes = [int64]1MB
$maximumProcesses = 12
$maximumResultBytes = 8MB
$maximumVhdxMetadataBytes = [uint64]64MB
$maximumExternalBytes = [uint64]896MB
# The fixed-memory pumps keep draining after a log reaches its cap. Truncation
# is evidence of failure, never a reason to leave a child blocked on a full pipe.
Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Threading.Tasks;
public sealed class SprigVssLogResult {
    public long TotalBytes;
    public long WrittenBytes;
    public bool Overflow;
    public string WriteError;
}
public static class SprigVssBoundedLog {
    public static async Task<SprigVssLogResult> DrainAsync(Stream input, Stream output, long limit) {
        var result = new SprigVssLogResult();
        var buffer = new byte[8192];
        while (true) {
            int count = await input.ReadAsync(buffer, 0, buffer.Length).ConfigureAwait(false);
            if (count == 0) break;
            result.TotalBytes = result.TotalBytes > long.MaxValue - count ? long.MaxValue : result.TotalBytes + count;
            result.Overflow |= result.TotalBytes > limit;
            int keep = (int)Math.Min((long)count, Math.Max(0L, limit - result.WrittenBytes));
            if (keep != 0 && result.WriteError == null) {
                try {
                    await output.WriteAsync(buffer, 0, keep).ConfigureAwait(false);
                    result.WrittenBytes += keep;
                } catch (Exception error) {
                    result.WriteError = error.GetType().Name;
                }
            }
        }
        try { await output.FlushAsync().ConfigureAwait(false); }
        catch (Exception error) { result.WriteError = error.GetType().Name; }
        return result;
    }
}
'@
$processes = [Collections.Generic.List[object]]::new()
$result = [ordered]@{
    version = 1; ok = $false; run_id = $runId; image = $image; evidence_directory = $evidence
    maximum_image_bytes = $maximumImageBytes; maximum_added_bytes = $maximumAddedBytes
    authorized_image_maximum_bytes = [uint64]4GB
    maximum_diff_bytes = [uint64]512MB; payload_file_mib = 8; payload_files = 3
    min_diff_area_file_size_mib = $minimumMiB; image_os = $env:ImageOS
    operating_system = ($os | Select-Object Caption, Version, BuildNumber)
    created = $false; mounted = $false; formatted = $false; cleanup = 'not-mounted'
    captures = @(); storage_checkpoints = @(); storage_measurement = @(); errors = @()
    budget_plan = [ordered]@{
        capture_count = 4; fixture_file_bytes_ceiling = 25MB
        staging_bytes = 100MB; native_restore_bytes = 100MB; cas_payload_allowance_bytes = 100MB
        cache_bytes = 256MB; transient_materialization_bytes = 24MB; fixture_cli_log_bytes = 64MB
        cas_metadata_state_and_backup_allowance_bytes = 64MB; fixture_report_bytes = 4MB
        copied_capture_evidence_bytes = 68MB; orchestration_log_bytes = 24MB
        final_result_bytes = 8MB; control_file_allowance_bytes = 8MB
        host_allocation_rounding_and_extra_metadata_allowance_bytes = 76MB
        outside_volume_total_bytes = $maximumExternalBytes
        vhdx_virtual_capacity_bytes = $maximumImageBytes; vhdx_metadata_allowance_bytes = $maximumVhdxMetadataBytes
        total_normal_operation_allowance_bytes = ($maximumImageBytes + $maximumVhdxMetadataBytes + $maximumExternalBytes)
        limitation = 'Finite normal-operation bounds for four fixed six-entry fixtures: at most 6160 payload chunks at minimum 16 KiB, 53-byte packed index entries, and bounded manifests/state/backups. Not a filesystem quota or protection against arbitrary OS/software faults. Tool/build outputs are outside this synthetic experiment budget.'
    }
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
    if ($processes.Count -ge $maximumProcesses) { throw 'Fixed orchestration process/log count exhausted.' }
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
        OutTask = $null; ErrTask = $null; OutResult = $null; ErrResult = $null; ExitCode = $null
    }
    $processes.Add($record)
    $record.OutStream = [IO.FileStream]::new($stdout, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
    $record.ErrStream = [IO.FileStream]::new($stderr, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
    $record.Started = $process.Start()
    $record.StartUnknown = $false
    if (-not $record.Started) { throw "Process $Label did not start." }
    $process.StandardInput.Close()
    $record.OutTask = [SprigVssBoundedLog]::DrainAsync($process.StandardOutput.BaseStream, $record.OutStream, $maximumProcessLogBytes)
    $record.ErrTask = [SprigVssBoundedLog]::DrainAsync($process.StandardError.BaseStream, $record.ErrStream, $maximumProcessLogBytes)
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
    }
    $Record.OutResult = $Record.OutTask.GetAwaiter().GetResult()
    $Record.ErrResult = $Record.ErrTask.GetAwaiter().GetResult()
    $Record.OutStream.Dispose()
    $Record.ErrStream.Dispose()
    $Record.ExitCode = $Record.Process.ExitCode
    $Record.Drained = $true
    if ($Record.OutResult.Overflow -or $Record.ErrResult.Overflow -or $Record.OutResult.WriteError -or $Record.ErrResult.WriteError) {
        throw "Process $($Record.Label) log overflow/write failure after complete drain; no further experiment step is permitted."
    }
    return $Record.ExitCode
}
function Invoke-OwnedProcess([string] $File, [string[]] $Arguments, [string] $Label, [int] $Seconds = 300) {
    $record = Start-OwnedProcess $File $Arguments $Label
    $exitCode = Wait-OwnedProcess $record $Seconds
    if ($exitCode -ne 0) { throw "Process $Label exited $exitCode; see its retained logs." }
    return $record
}
function Record-StorageMeasurement([string] $Phase) {
    # Bounded point-in-time read only. Measure source separately, but sum host
    # consumption as VHDX + outside-volume work + evidence, each exactly once.
    # Do not add live source bytes or VSS diff bytes again: both are in the VHDX.
    $record = Invoke-OwnedProcess $Python @($Fixture, 'measure', $liveRoot, $image, $hostWork, $evidence) ('storage-' + $Phase) 120
    if ((Get-Item -LiteralPath $record.Stdout).Length -gt 1MB) { throw 'Storage measurement JSON exceeds its bounded report size.' }
    $measurement = Get-Content -LiteralPath $record.Stdout -Raw | ConvertFrom-Json -AsHashtable
    if ($measurement.schema_version -ne 1 -or $measurement.paths.Count -ne 4) { throw 'Unexpected storage measurement schema.' }
    foreach ($path in @($liveRoot, $image, $hostWork, $evidence)) {
        if (-not $measurement.paths.ContainsKey($path)) { throw 'Storage measurement omitted an expected exact path.' }
    }
    $hostLogical = [uint64]0
    $hostAllocated = [uint64]0
    $logicalComplete = $true
    $allocationComplete = $true
    foreach ($path in @($image, $hostWork, $evidence)) {
        $entry = $measurement.paths[$path]
        if ($entry.exists -ne $true -or $null -eq $entry.logical_bytes) { $logicalComplete = $false }
        else { $hostLogical += [uint64]$entry.logical_bytes }
        if ($entry.exists -ne $true -or $null -eq $entry.allocated_bytes) { $allocationComplete = $false }
        else { $hostAllocated += [uint64]$entry.allocated_bytes }
    }
    $result.storage_measurement += [ordered]@{
        phase = $Phase; observed_at_utc = [DateTime]::UtcNow.ToString('o')
        live_source = $measurement.paths[$liveRoot]; image = $measurement.paths[$image]
        host_work = $measurement.paths[$hostWork]; evidence = $measurement.paths[$evidence]
        host_total_logical_bytes = $(if ($logicalComplete) { $hostLogical } else { $null })
        host_total_allocated_bytes = $(if ($allocationComplete) { $hostAllocated } else { $null })
        allocation_complete = $allocationComplete
        note = 'Checkpoint sample, not a continuous or lifetime peak. Host total sums image, host work and evidence once; live/diff bytes are not added again. Subsequent report emission and host filesystem metadata are excluded.'
    }
    if ($allocationComplete -and $hostAllocated -gt ($maximumAddedBytes - $budgetReserveBytes)) {
        throw 'Measured host file allocation exhausted the conservative 2 GiB budget; no further capture is permitted.'
    }
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
    $allowance = [int64]17MB
    $files = @([pscustomobject]@{ Source = (Join-Path $CaptureWork 'capture.json'); Name = ('capture-' + $Label + '.json') })
    foreach ($number in 1..8) {
        foreach ($stream in @('stdout', 'stderr')) {
            $name = 'sprig-{0:D2}.{1}.log' -f $number, $stream
            $files += [pscustomobject]@{ Source = (Join-Path (Join-Path $CaptureWork 'logs') $name); Name = ('capture-' + $Label + '-' + $name) }
        }
    }
    foreach ($file in $files) {
        if (Test-Path -LiteralPath $file.Source -PathType Leaf) {
            Assert-PlainPath $file.Source
            $length = (Get-Item -LiteralPath $file.Source).Length
            if ($length -gt 1MB -or $length -gt $allowance) { throw 'Capture diagnostics exceed the evidence whitelist size limit.' }
            Copy-Item -LiteralPath $file.Source -Destination (Join-Path $evidence $file.Name)
            $allowance -= $length
        }
    }
}
function Invoke-Capture([string] $Label, [string] $PreviousReport = '') {
    if ($result.captures.Count -ge 4) { throw 'The fixed four-capture experiment budget is exhausted.' }
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
    $lines = @(Get-Content -LiteralPath $record.Stdout | Where-Object { $_.StartsWith('SPRIG_VSS_RESULT ') })
    if ($lines.Count -ne 1) { throw 'Missing or ambiguous requester final result; retain the image.' }
    $requestReport = $lines[0].Substring('SPRIG_VSS_RESULT '.Length) | ConvertFrom-Json
    # Preserve the requester's exact result before looking for an optional child
    # report: the requester can fail before it ever starts the collector.
    $captureEntry = [ordered]@{ label = $Label; requester_exit_code = $exitCode; requester = $requestReport; fixture = $null }
    $result.captures += $captureEntry
    Copy-CaptureEvidence $captureWork $Label
    $captureReportPath = Join-Path $captureWork 'capture.json'
    $captureReport = $null
    if (Test-Path -LiteralPath $captureReportPath -PathType Leaf) {
        $captureReport = Get-Content -LiteralPath $captureReportPath -Raw | ConvertFrom-Json
        $captureEntry.fixture = $captureReport
    }
    if ($exitCode -ne 0 -or $requestReport.version -ne 1 -or $requestReport.ok -ne $true -or
        $requestReport.cleanup_safe -ne $true -or $requestReport.volume -ne $volumeGuid -or
        $requestReport.provider_id -ne '{b5946137-7b9f-4925-af80-51abd60b20d5}' -or
        $requestReport.child_drained -ne $true -or $requestReport.child_exit_code -ne 0 -or
        $requestReport.snapshot_cleanup -ne 'confirmed' -or $requestReport.diff_area_cleanup -ne 'confirmed' -or $null -eq $captureReport -or
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
    $createScript = Join-Path $hostWork 'create-vhd.txt'
    "create vdisk file=`"$image`" maximum=1024 type=expandable`nexit" | Set-Content -LiteralPath $createScript -Encoding ascii
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
    $liveRoot = Join-Path $volumeRoot 'fixture'
    Record-StorageMeasurement 'post-format-before-fixture'
    $baseline = $result.storage_measurement[-1]
    if (-not $baseline.allocation_complete -or $null -eq $baseline.image.allocated_bytes) {
        throw 'Cannot verify post-format VHDX allocation and reserve the experiment budget; no fixture writes are allowed.'
    }
    if ($baseline.image.allocated_bytes -gt ($maximumImageBytes + $maximumVhdxMetadataBytes) -or
        ($maximumImageBytes + $maximumVhdxMetadataBytes + $maximumExternalBytes) -gt $maximumAddedBytes) {
        throw 'Formatted VHDX and conservative remaining allowances do not fit the 2 GiB budget.'
    }
    $baselineExternal = [uint64]$baseline.host_total_allocated_bytes - [uint64]$baseline.image.allocated_bytes
    if ($baselineExternal -gt $maximumExternalBytes) { throw 'Post-format external output already exceeds its reserved bound.' }
    $result.budget_plan.post_format_host_allocated_bytes = $baseline.host_total_allocated_bytes
    $result.budget_plan.remaining_vhdx_allocation_reserve_bytes = ($maximumImageBytes + $maximumVhdxMetadataBytes - [uint64]$baseline.image.allocated_bytes)
    $result.budget_plan.remaining_external_allocation_reserve_bytes = ($maximumExternalBytes - $baselineExternal)
    $marker = Join-Path $volumeRoot '.sprig-vss-synthetic-volume'
    if (Test-Path -LiteralPath $marker) { throw 'Fresh synthetic marker unexpectedly exists.' }
    [IO.File]::WriteAllText($marker, "sprig-vss-synthetic-only-v1`n$markerToken`n", [Text.ASCIIEncoding]::new())
    Assert-OwnVolume
    Assert-NoOwnVssState
    [void](Invoke-OwnedProcess $Python @($Fixture, 'prepare', '--live-root', $liveRoot, '--file-mib', '8') 'fixture-prepare' 120)
    [void](Assert-OutputBudget 'prepared')
    Record-StorageMeasurement 'prepared'
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
    Record-StorageMeasurement 'after-live-concurrent'
    [IO.File]::WriteAllText($writerStop, 'stop', [Text.ASCIIEncoding]::new())
    if ((Wait-OwnedProcess $writer 320) -ne 0) { throw 'Concurrent writer failed to stop cleanly.' }
    $settledReport = Invoke-Capture 'settled'
    [void](Invoke-Capture 'repeated' $settledReport)
    Record-StorageMeasurement 'completed-before-detach'
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
        [ordered]@{ label = $_.Label; started = $_.Started; start_unknown = $_.StartUnknown; drained = $_.Drained; exit_code = $_.ExitCode
            log_limit_bytes = $maximumProcessLogBytes; stdout = $_.OutResult; stderr = $_.ErrResult }
    })
    $json = $result | ConvertTo-Json -Depth 16
    if ([Text.Encoding]::UTF8.GetByteCount($json) -gt $maximumResultBytes) {
        # Detailed collector reports already have their exact whitelist files.
        # Keep requester/cleanup evidence and never write an oversized result.
        foreach ($capture in $result.captures) { $capture.fixture = $null }
        $result.ok = $false
        $result.errors += 'Result exceeded its 8 MiB bound; detailed collector reports remain in capture-*.json.'
        $json = $result | ConvertTo-Json -Depth 16
        if ([Text.Encoding]::UTF8.GetByteCount($json) -gt $maximumResultBytes) { throw 'Even compact result exceeded its fixed bound; no oversized output written.' }
        if ($null -eq $scriptFailure) { $scriptFailure = [InvalidOperationException]::new('Result evidence exceeded its bounded size.') }
    }
    [IO.File]::WriteAllText((Join-Path $evidence 'result.json'), $json, [Text.UTF8Encoding]::new($false))
    Write-Output $json
    # The evidence directory has only result.json, capture-*.json and *.log.
    # The workflow must upload this directory's explicit whitelist, NEVER work,
    # runner-temp globs, synthetic.vhdx, fixtures, CAS, staging or restore trees.
    # No files are deleted: preserve owned evidence even after successful detach.
}
if ($null -ne $scriptFailure) { throw $scriptFailure }
if ($null -ne $cleanupFailure) { throw $cleanupFailure }
