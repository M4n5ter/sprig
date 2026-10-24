# Explicitly opt-in synthetic ReFS validation on an ephemeral GitHub runner.
# Never select a physical disk by number, initialize an existing disk, or install a driver.
param([switch] $AllowSyntheticVolume)
$ErrorActionPreference = 'Stop'
if (-not $AllowSyntheticVolume -or $env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_OS -ne 'Windows' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted') {
    throw 'Requires explicit synthetic-volume opt-in on a Windows GitHub runner.'
}
$resolvedRoot = Resolve-Path -LiteralPath $env:RUNNER_TEMP
if ($resolvedRoot.Provider.Name -ne 'FileSystem') { throw 'Temporary root must be a filesystem path.' }
$root = $resolvedRoot.Path
if ($root -notmatch '^[A-Za-z]:\\') { throw 'Temporary root must be a local absolute drive path.' }
$ancestor = Get-Item -LiteralPath $root -Force
while ($null -ne $ancestor) {
    if (($ancestor.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'Temporary root ancestors must not be reparse points.' }
    $ancestor = $ancestor.Parent
}
if (@($root.ToCharArray() | Where-Object { [int]$_ -gt 127 }).Count -ne 0) { throw 'DiskPart fixture path must be ASCII.' }
if ($root.Contains('"') -or $root.Contains("`n") -or $root.Contains("`r")) { throw 'Unsafe temporary path.' }
$directory = Join-Path $root ('sprig-refs-' + [Guid]::NewGuid().ToString('N'))
if (Test-Path -LiteralPath $directory) { throw 'Fixture must not already exist.' }
[void][IO.Directory]::CreateDirectory($directory)
$image = Join-Path $directory 'synthetic.vhdx'
if (Test-Path -LiteralPath $image) { throw 'Image must not already exist.' }
$script = Join-Path $directory 'create.txt'
$diskpartLog = Join-Path $directory 'diskpart.log'
$result = [ordered]@{ image = $image; maximumBytes = 4294967296; created = $false; mounted = $false; formatted = $false; testExitCode = $null; cleanup = 'not-mounted' }
$startedAt = Get-Date
$before = @(Get-Disk | Select-Object -ExpandProperty Number)
$oldRoot = $env:SPRIG_REFS_TEST_ROOT
$mountAttempted = $false
$formatter = $null
$formatterStarted = $false
$formatterStartUnknown = $false
$prefix = [Text.StringBuilder]::new()
$stderr = $null
$remainingOutput = $null
$formatOutputCollected = $false
function Assert-FormatTarget {
    $mapped = @(Get-DiskImage -ImagePath $image -ErrorAction Stop | Get-Disk -ErrorAction Stop)
    if ($mapped.Count -ne 1) { throw 'Image mapping is not unique.' }
    $d = $mapped[0]
    if ($d.UniqueId -ne $initializedIdentity -or $d.Number -ne $partition.DiskNumber -or $d.Size -ne 4294967296 -or $d.PartitionStyle -ne 'GPT' -or $d.IsBoot -or $d.IsSystem) { throw 'Format image identity changed.' }
    $letters = @(Get-Partition -DriveLetter $partition.DriveLetter -ErrorAction Stop)
    if ($letters.Count -ne 1) { throw 'Format drive letter is not unique.' }
    $p = $letters[0]
    if ($p.DiskNumber -ne $partition.DiskNumber -or $p.PartitionNumber -ne $partition.PartitionNumber -or $p.Offset -ne $partition.Offset -or $p.Size -ne $partition.Size -or $p.GptType -ne $partition.GptType) { throw 'Format drive letter no longer identifies the new partition.' }
}
try {
    # DiskPart creates only this nonexistent file; attach and format are separate,
    # so every consequential step can bind to the exact returned image object.
    "create vdisk file=`"$image`" maximum=4096 type=expandable`nexit" | Set-Content -LiteralPath $script -Encoding ascii
    & diskpart.exe /s $script 2>&1 | Tee-Object -FilePath $diskpartLog
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $image -PathType Leaf)) { throw 'VHDX creation failed; no retry.' }
    if (((Get-Item -LiteralPath $image -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'Image unexpectedly became a reparse point.' }
    $result.created = $true
    $mountAttempted = $true
    Mount-DiskImage -ImagePath $image -NoDriveLetter -ErrorAction Stop | Out-Null
    $result.mounted = $true
    $disks = @(Get-DiskImage -ImagePath $image -ErrorAction Stop | Get-Disk -ErrorAction Stop)
    if ($disks.Count -ne 1) { throw 'Image must identify exactly one disk.' }
    $disk = $disks[0]
    if ($before -contains $disk.Number -or $disk.IsBoot -or $disk.IsSystem -or $disk.PartitionStyle -ne 'RAW' -or $disk.Size -ne 4294967296) {
        throw 'Refusing to initialize a preexisting, boot, system, nonblank or unexpected-size disk.'
    }
    $unique = $disk.UniqueId
    # Refresh the exact image mapping immediately before initializing it.
    $freshDisks = @(Get-DiskImage -ImagePath $image | Get-Disk)
    if ($freshDisks.Count -ne 1) { throw 'Image no longer maps to one disk.' }
    $fresh = $freshDisks[0]
    if ($fresh.UniqueId -ne $unique -or $fresh.Number -ne $disk.Number -or $fresh.Size -ne 4294967296 -or $fresh.PartitionStyle -ne 'RAW' -or $fresh.IsBoot -or $fresh.IsSystem) { throw 'Image mapping changed.' }
    $initializedDisks = @($fresh | Initialize-Disk -PartitionStyle GPT -PassThru -Confirm:$false)
    if ($initializedDisks.Count -ne 1) { throw 'Expected one initialized disk.' }
    $initialized = $initializedDisks[0]
    $initializedIdentity = $initialized.UniqueId
    $partitions = @($initialized | New-Partition -UseMaximumSize -AssignDriveLetter)
    if ($partitions.Count -ne 1) { throw 'Expected one new partition.' }
    $partition = $partitions[0]
    if ($partition.DiskNumber -ne $disk.Number) { throw 'Unexpected partition disk.' }
    $freshDisks = @(Get-DiskImage -ImagePath $image | Get-Disk)
    if ($freshDisks.Count -ne 1) { throw 'Image no longer maps to one disk.' }
    $fresh = $freshDisks[0]
    if ($fresh.UniqueId -ne $initializedIdentity -or $fresh.Number -ne $partition.DiskNumber -or $fresh.Size -ne 4294967296 -or $fresh.PartitionStyle -ne 'GPT' -or $fresh.IsBoot -or $fresh.IsSystem) { throw 'Image mapping changed before format.' }
    try {
        $method = (Get-CimClass -Namespace root/Microsoft/Windows/Storage -ClassName MSFT_Volume -Amended).CimClassMethods['Format']
        $result.formatReturnMap = @($method.Qualifiers | Where-Object Name -in 'ValueMap', 'Values' | Select-Object Name, Value)
    } catch {
        $result.formatMetadataError = $_.Exception.Message
    }
    $result.operatingSystem = Get-CimInstance Win32_OperatingSystem | Select-Object Caption, Version, BuildNumber, OSArchitecture
    $result.diskGeometry = $fresh | Select-Object Number, UniqueId, Size, LogicalSectorSize, PhysicalSectorSize, BusType, PartitionStyle, IsBoot, IsSystem
    $result.partitionGeometry = $partition | Select-Object DiskNumber, PartitionNumber, Offset, Size, GptType, DriveLetter
    $result.imageGeometry = Get-DiskImage -ImagePath $image | Select-Object Size, FileSize, BlockSize, LogicalSectorSize, Attached
    if (-not $partition.DriveLetter) { throw 'New partition has no drive letter.' }
    # Same ReFS quick-format, via the official formatter to preserve its precise
    # diagnostic. Do not add /X or /Y (force-dismount), cluster, integrity, Dev
    # Drive or registry overrides. Only the initial new-volume warning gets Y;
    # stdin then closes, so any further unexpected prompt fails rather than loops.
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = Join-Path $env:SystemRoot 'System32\format.com'
    $start.UseShellExecute = $false
    $start.RedirectStandardInput = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    foreach ($argument in @("$($partition.DriveLetter):", '/FS:ReFS', '/Q', '/V:SprigSynthetic')) {
        $start.ArgumentList.Add($argument)
    }
    $formatter = [Diagnostics.Process]::new()
    $formatter.StartInfo = $start
    Assert-FormatTarget
    $formatterStartUnknown = $true
    $formatterStarted = $formatter.Start()
    $formatterStartUnknown = $false
    if (-not $formatterStarted) { throw 'Native formatter did not start.' }
    $stderr = $formatter.StandardError.ReadToEndAsync()
    $prefix = [Text.StringBuilder]::new()
    $buffer = [char[]]::new(1)
    $pending = $null
    $confirmed = $false
    while ($prefix.Length -lt 65536) {
        $pending = $formatter.StandardOutput.ReadAsync($buffer, 0, 1)
        if (-not $pending.Wait(10000)) { break }
        if ($pending.GetAwaiter().GetResult() -eq 0) { $pending = $null; break }
        [void]$prefix.Append($buffer[0])
        $result.formatStdout = $prefix.ToString()
        $pending = $null
        if ($prefix.ToString() -match 'Proceed with Format \(Y/N\)\?') {
            Assert-FormatTarget
            $formatter.StandardInput.WriteLine('Y')
            $confirmed = $true
            $result.formatWarningConfirmed = $true
            break
        }
    }
    # Never answer an unrecognized prompt (especially a forced-dismount prompt).
    $formatter.StandardInput.Close()
    if ($null -ne $pending) {
        if (-not $pending.Wait(10000)) { throw 'Formatter output is still pending after stdin closed.' }
        if ($pending.GetAwaiter().GetResult() -gt 0) { [void]$prefix.Append($buffer[0]) }
    }
    $remainingOutput = $formatter.StandardOutput.ReadToEndAsync()
    if (-not $formatter.WaitForExit(30000)) { throw 'Formatter did not exit after stdin closed; preserve the attached image.' }
    $result.formatStdout = $prefix.ToString() + $remainingOutput.GetAwaiter().GetResult()
    $result.formatStderr = $stderr.GetAwaiter().GetResult()
    $result.formatWarningConfirmed = $confirmed
    $result.formatExitCode = $formatter.ExitCode
    $formatOutputCollected = $true
    $formatter.Dispose()
    $formatter = $null
    if ($result.formatExitCode -ne 0) { throw "Native ReFS formatter exited $($result.formatExitCode)." }
    $volumes = @($partition | Get-Volume)
    if ($volumes.Count -ne 1) { throw 'New partition must map to one volume.' }
    $volume = $volumes[0]
    if ($volume.FileSystem -ne 'ReFS' -or -not $volume.DriveLetter) { throw 'The new volume is not mounted ReFS.' }
    $result.formatted = $true
    $result.volume = $volume | Select-Object DriveLetter, FileSystem, AllocationUnitSize, Size, SizeRemaining
    $env:SPRIG_REFS_TEST_ROOT = "$($volume.DriveLetter):\"
    & cargo test --locked --test windows_native refs_block_clone_roundtrip_and_isolation -- --exact --nocapture
    $result.testExitCode = $LASTEXITCODE
    if ($LASTEXITCODE -ne 0) { throw 'ReFS runtime contract failed.' }
} catch {
    $originalError = $_
    $result.error = $originalError.Exception.Message
    try {
        if (-not $formatOutputCollected) { $result.formatStdout = $prefix.ToString() }
        if (-not $formatOutputCollected -and $null -ne $remainingOutput -and $remainingOutput.IsCompletedSuccessfully) { $result.formatStdout += $remainingOutput.GetAwaiter().GetResult() }
        if ($null -ne $stderr -and $stderr.IsCompletedSuccessfully) { $result.formatStderr = $stderr.GetAwaiter().GetResult() }
        if ($formatterStarted -and $null -ne $formatter -and $formatter.HasExited) { $result.formatExitCode = $formatter.ExitCode }
    } catch { $result.formatDiagnosticError = $_.Exception.Message }
    try {
        $result.errorRecord = $originalError | Format-List * -Force | Out-String
        $details = @()
        $exception = $originalError.Exception
        while ($null -ne $exception) {
            if ($exception -is [Microsoft.Management.Infrastructure.CimException] -and $null -ne $exception.ErrorData) {
                $details += @($exception.ErrorData.CimInstanceProperties | ForEach-Object {
                    [ordered]@{ Name = $_.Name; CimType = [string]$_.CimType; Value = $_.Value }
                })
            }
            $exception = $exception.InnerException
        }
        $result.extendedErrorData = $details
    } catch {
        $result.diagnosticError = $_.Exception.Message
    }
    try {
        $result.systemStorageEvents = @(Get-WinEvent -FilterHashtable @{ LogName = 'System'; StartTime = $startedAt } -MaxEvents 100 -ErrorAction Stop |
            Where-Object ProviderName -match '(?i)refs|disk|volmgr|partmgr|vhd' |
            Select-Object TimeCreated, Id, ProviderName, Message)
    } catch { $result.systemEventsError = $_.Exception.Message }
    $eventLogs = @()
    foreach ($name in @('Microsoft-Windows-StorageManagement/Operational', 'Microsoft-Windows-ReFS/Operational', 'Microsoft-Windows-VHDMP/Operational')) {
        try {
            $events = @(Get-WinEvent -FilterHashtable @{ LogName = $name; StartTime = $startedAt } -MaxEvents 20 -ErrorAction Stop |
                Select-Object TimeCreated, Id, ProviderName, Message)
            $eventLogs += [ordered]@{ log = $name; events = $events }
        } catch { $eventLogs += [ordered]@{ log = $name; readError = $_.Exception.Message } }
    }
    $result.storageEventLogs = $eventLogs
    throw $originalError
} finally {
    $env:SPRIG_REFS_TEST_ROOT = $oldRoot
    if ($mountAttempted) {
        try {
            # All test children have exited. No force/lazy option, disk deletion,
            # driver changes or detach of any unrelated image is performed.
            if ($formatterStartUnknown) { throw 'Formatter start outcome is unknown; preserve image for diagnostics.' }
            if ($formatterStarted -and $null -ne $formatter -and -not $formatter.HasExited) {
                throw 'Formatter is still active; no forced detach attempted.'
            }
            $currentImage = Get-DiskImage -ImagePath $image -ErrorAction Stop
            if ($currentImage.Attached) {
                Dismount-DiskImage -ImagePath $image -ErrorAction Stop
            }
            $remaining = Get-DiskImage -ImagePath $image -ErrorAction Stop
            if ($remaining.Attached) { throw 'Synthetic image remains attached.' }
            $result.cleanup = 'detached'
        } catch {
            $result.cleanup = 'failed: ' + $_.Exception.Message
            throw
        } finally {
            $result | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath (Join-Path $directory 'result.json')
            $result | ConvertTo-Json -Depth 12 | Write-Output
        }
    } else {
        $result | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath (Join-Path $directory 'result.json')
        $result | ConvertTo-Json -Depth 12 | Write-Output
    }
}
