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
$result = [ordered]@{ image = $image; maximumBytes = 1073741824; created = $false; mounted = $false; formatted = $false; testExitCode = $null; cleanup = 'not-mounted' }
$before = @(Get-Disk | Select-Object -ExpandProperty Number)
$oldRoot = $env:SPRIG_REFS_TEST_ROOT
$mountAttempted = $false
try {
    # DiskPart creates only this nonexistent file; attach and format are separate,
    # so every consequential step can bind to the exact returned image object.
    "create vdisk file=`"$image`" maximum=1024 type=expandable`nexit" | Set-Content -LiteralPath $script -Encoding ascii
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
    if ($before -contains $disk.Number -or $disk.IsBoot -or $disk.IsSystem -or $disk.PartitionStyle -ne 'RAW' -or $disk.Size -ne 1073741824) {
        throw 'Refusing to initialize a preexisting, boot, system, nonblank or unexpected-size disk.'
    }
    $unique = $disk.UniqueId
    # Refresh the exact image mapping immediately before initializing it.
    $freshDisks = @(Get-DiskImage -ImagePath $image | Get-Disk)
    if ($freshDisks.Count -ne 1) { throw 'Image no longer maps to one disk.' }
    $fresh = $freshDisks[0]
    if ($fresh.UniqueId -ne $unique -or $fresh.Number -ne $disk.Number -or $fresh.Size -ne 1073741824 -or $fresh.PartitionStyle -ne 'RAW' -or $fresh.IsBoot -or $fresh.IsSystem) { throw 'Image mapping changed.' }
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
    if ($fresh.UniqueId -ne $initializedIdentity -or $fresh.Number -ne $partition.DiskNumber -or $fresh.Size -ne 1073741824 -or $fresh.PartitionStyle -ne 'GPT' -or $fresh.IsBoot -or $fresh.IsSystem) { throw 'Image mapping changed before format.' }
    $volume = $partition | Format-Volume -FileSystem ReFS -NewFileSystemLabel SprigSynthetic -Confirm:$false
    if ($volume.FileSystem -ne 'ReFS' -or -not $volume.DriveLetter) { throw 'The new volume is not mounted ReFS.' }
    $result.formatted = $true
    $result.volume = $volume | Select-Object DriveLetter, FileSystem, AllocationUnitSize, Size, SizeRemaining
    $env:SPRIG_REFS_TEST_ROOT = "$($volume.DriveLetter):\"
    & cargo test --locked --test windows_native refs_block_clone_roundtrip_and_isolation -- --exact --nocapture
    $result.testExitCode = $LASTEXITCODE
    if ($LASTEXITCODE -ne 0) { throw 'ReFS runtime contract failed.' }
} catch {
    $result.error = $_.Exception.Message
    throw
} finally {
    $env:SPRIG_REFS_TEST_ROOT = $oldRoot
    if ($mountAttempted) {
        try {
            # All test children have exited. No force/lazy option, disk deletion,
            # driver changes or detach of any unrelated image is performed.
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
            $result | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $directory 'result.json')
            $result | ConvertTo-Json -Depth 5 | Write-Output
        }
    } else {
        $result | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $directory 'result.json')
        $result | ConvertTo-Json -Depth 5 | Write-Output
    }
}
