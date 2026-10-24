# Opt-in, disposable GitHub-hosted nested-VM boot smoke. The separately gated
# kernel-probe mode permits only the approved guest security/test operations.
# No host feature/security changes, networking, credential arguments, or
# preexisting disk/VM changes; boot-only mode still never loads a driver.
# BCDBoot /s confines its BCD/files to the explicit new ESP and does not create
# a firmware NVRAM entry. Never retry it without /s or fall back to host repair:
# https://learn.microsoft.com/windows-hardware/manufacture/desktop/bcdboot-command-line-options-techref-di
[CmdletBinding()]
param([switch] $AllowGuestBoot, [switch] $AllowKernelProbe, [string] $KernelPayloadDirectory = '')

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
if ((-not $AllowGuestBoot -and -not $AllowKernelProbe) -or $env:GITHUB_ACTIONS -ne 'true' -or
    $env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_OS -ne 'Windows') {
    throw 'Guest boot requires explicit opt-in on a disposable Windows GitHub-hosted runner.'
}
if ($PSVersionTable.PSVersion.Major -lt 7) { throw 'PowerShell 7 is required.' }
if ($AllowKernelProbe -and $env:GITHUB_EVENT_NAME -ne 'workflow_dispatch') { throw 'Kernel probe requires explicit workflow dispatch.' }

$isoUrl = 'https://software-static.download.prss.microsoft.com/dbazure/998969d5-f34g-4e03-ac9d-1f9786c66749/26100.32230.260111-0550.lt_release_svc_refresh_SERVER_EVAL_x64FRE_en-us.iso'
$isoBytes = [long]8152356864
$isoSha256 = '7b052573ba7894c9924e3e87ba732ccd354d18cb75a883efa9b900ea125bfd51'
$maximumDiskBytes = [long](40GB)
$memoryBytes = [long](6GB)
$runId = [Guid]::NewGuid().ToString('N')
$directory = 'D:\sprig-guest-' + $runId
$vmName = 'sprig-guest-' + $runId
$guestName = 'SPRIG-' + $runId.Substring(0, 8).ToUpperInvariant()
$isoPath = Join-Path $directory 'server2025-eval.iso'
$vhdPath = Join-Path $directory 'guest.vhdx'
$startedAt = [DateTime]::UtcNow
# Leave time for teardown before the outer 75-minute CI job timeout.
$workDeadline = $startedAt.AddMinutes(55)
$expectedSecureBoot = 'On'
$resultPath = Join-Path (Get-Location).Path 'guest-boot-result.json'
$eventsPath = Join-Path (Get-Location).Path 'guest-boot-events.json'
$phases = [Collections.Generic.List[object]]::new()
$result = [ordered]@{
    schemaVersion = 1; runId = $runId; startedAtUtc = $startedAt.ToString('o')
    outcome = 'failed'; failedPhase = $null; failure = $null; phases = $phases
    runner = [ordered]@{ image = "$env:ImageOS $env:ImageVersion"; environment = $env:RUNNER_ENVIRONMENT }
    scope = 'Server 2025 evaluation only; boot, PowerShell Direct, normal reboot and shutdown; no guest networking or drivers'
    limits = [ordered]@{ generation = 2; processors = 2; fixedMemoryBytes = $memoryBytes; maximumDynamicDiskBytes = $maximumDiskBytes; minimumWorkspaceFreeBytes = [long](60GB) }
    media = [ordered]@{ url = $isoUrl; expectedBytes = $isoBytes; expectedSha256 = $isoSha256; verified = $false; index = 1 }
    vm = [ordered]@{ name = $vmName; id = $null; networkAdapterCount = $null; secureBoot = $null }
    disk = $null; hostBoot = $null; firstBoot = $null; secondBoot = $null
    rebootObserved = $false; normalShutdownObserved = $false
    credentialHandling = [ordered]@{ separateHostAnswerFile = $false; guestAnswerFilesRemoved = $false; fixtureRemoved = $false }
    cleanup = [ordered]@{ status = 'not-started'; forcedOff = $false; vmRemoved = $false; vhdDetached = $false; isoDetached = $false; directoryRemoved = $false; errors = @() }
}
$phase = $null
$directoryCreated = $false
$reportReserved = $false
$vmCreationAttempted = $false
$vmId = $null
$priorVmIds = @()
$priorDiskNumbers = @()
$vhdIdentifier = $null
$diskNumber = $null
$diskUniqueId = $null
$diskDevicePath = $null
$efiPartition = $null
$osPartition = $null
$hostVhdMountAttempted = $false
$isoMountAttempted = $false
$nativeUnsettled = $false
$activeNative = $null
$guestCredential = $null
$securePassword = $null
$password = $null
$answerXml = $null
$smokePassed = $false
$guestJobs = [Collections.Generic.List[object]]::new()

# Only fixed source-defined codes can be copied from an exception message.
$safeFailureCodes = @(
    'probe-driver-service-identity-invalid', 'probe-driver-signature-invalid', 'probe-driver-stop-unknown', 'probe-existing-mount-refused', 'probe-existing-service-refused', 'probe-guest-identity-invalid', 'probe-guest-secureboot-still-enabled', 'probe-guest-version-invalid', 'probe-manifest-entry-invalid', 'probe-manifest-invalid', 'probe-memfs-service-identity-invalid', 'probe-memfs-stop-unknown', 'probe-mount-not-ready', 'probe-native-command-failed', 'probe-native-timeout-retain-guest', 'probe-output-oversize', 'probe-payload-hash-invalid', 'probe-payload-invalid', 'probe-preexisting-verifier-refused', 'probe-reason-invalid', 'probe-reparse-refused', 'probe-result-envelope-invalid', 'probe-services-not-stopped', 'probe-signed-driver-changed', 'probe-signed-driver-invalid', 'probe-signing-tool-untrusted', 'probe-test-signing-not-active', 'probe-uncertain-result-retain-guest', 'probe-unexpected-verifier', 'probe-verifier-not-observed', 'probe-verifier-settings-invalid',
    'probe-payload-invalid', 'probe-protection-changed', 'probe-no-valid-result', 'probe-test-not-safe', 'probe-shutdown-failed',
    'active-deadline-exceeded', 'administrator-required', 'answer-file-already-exists', 'applied-image-missing-kernel',
    'cleanup-directory-identity-changed', 'cleanup-directory-remains', 'cleanup-iso-still-attached', 'cleanup-reparse-point-refused',
    'cleanup-vhd-identity-changed', 'cleanup-vhd-still-attached', 'cleanup-vm-operation-pending', 'disk-identity-changed',
    'disk-may-still-be-in-use', 'drive-letter-target-changed', 'file-outside-owned-directory', 'fixture-or-report-already-exists',
    'fixture-still-in-use', 'guest-answer-file-remains', 'guest-answer-removal-unverified', 'guest-command-failed',
    'guest-command-not-completed', 'guest-normal-reboot-not-accepted', 'guest-normal-shutdown-not-accepted', 'guest-readiness-deadline-exceeded',
    'guest-reboot-request-unverified', 'guest-shutdown-request-unverified', 'host-boot-fingerprint-changed', 'host-drive-target-refused',
    'hyper-v-not-already-running', 'initialized-disk-mismatch', 'install-partition-types-invalid', 'install-volume-filesystems-invalid',
    'insufficient-d-drive-space-or-filesystem', 'insufficient-free-host-memory', 'iso-sha256-mismatch', 'iso-size-mismatch',
    'iso-volume-not-unique', 'msr-disk-mismatch', 'native-operation-may-use-iso', 'native-process-failed',
    'native-process-not-started', 'native-process-timeout', 'new-esp-boot-files-missing', 'new-vhd-disk-number-invalid',
    'new-vm-unexpected-state', 'nonblank-disk-refused', 'normal-guest-shutdown-deadline-exceeded', 'owned-vm-not-unique',
    'owned-vm-still-present', 'owned-vm-still-running', 'partition-drive-letter-changed', 'partition-identity-changed',
    'partition-mapping-not-unique', 'pinned-image-index-metadata-mismatch', 'reparse-point-refused', 'required-command-unavailable',
    'unexpected-event-provider', 'unexpected-partition-style', 'unexpected-vhd-attachment-during-cleanup', 'vhd-host-detach-not-confirmed',
    'vhd-identifier-missing', 'vhd-identity-changed', 'vhd-path-already-exists', 'vm-id-changed',
    'vm-identity-changed', 'vm-isolation-or-resource-limit-invalid', 'vm-name-already-exists', 'vm-not-created',
    'windows-2025-runner-required', 'windows-server-2025-host-required'
)

function Write-SafeResult {
    if (-not $reportReserved) { return }
    Assert-NoReparse $resultPath
    $result | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $resultPath -Encoding utf8
}
function Wait-CleanupJob($Job) {
    # Do not block teardown indefinitely, or kill a host service to unstick it.
    $null = Wait-Job -Job $Job -Timeout 90
    if ($Job.State -ne 'Completed') { throw 'cleanup-vm-operation-pending' }
    $null = Receive-Job -Job $Job -ErrorAction Stop
    Remove-Job -Job $Job -Force -ErrorAction SilentlyContinue
}

function Start-Phase([string] $Name) {
    Assert-TimeRemaining
    $script:phase = [ordered]@{ name = $Name; status = 'running'; startedAtUtc = [DateTime]::UtcNow.ToString('o'); completedAtUtc = $null }
    $phases.Add($script:phase)
    Write-Host "Guest smoke: $Name"
}
function Complete-Phase {
    $script:phase.status = 'passed'
    $script:phase.completedAtUtc = [DateTime]::UtcNow.ToString('o')
}
function Assert-TimeRemaining {
    if ([DateTime]::UtcNow -ge $workDeadline) { throw 'active-deadline-exceeded' }
}
function Get-RemainingSeconds([int] $Maximum) {
    Assert-TimeRemaining
    return [int][Math]::Max(1, [Math]::Min($Maximum, [Math]::Floor(($workDeadline - [DateTime]::UtcNow).TotalSeconds)))
}
function Assert-NoReparse([string] $Path) {
    $item = Get-Item -LiteralPath $Path -Force -ErrorAction Stop
    while ($null -ne $item) {
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'reparse-point-refused' }
        $item = $(if ($item -is [IO.FileInfo]) { $item.Directory } else { $item.Parent })
    }
}
function Assert-OwnedFile([string] $Path) {
    if (-not $directoryCreated -or -not $Path.StartsWith($directory + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'file-outside-owned-directory' }
    Assert-NoReparse $Path
}
function Invoke-Native([string] $Name, [string] $File, [string[]] $Arguments, [int] $TimeoutSeconds, [switch] $Capture) {
    $timeout = Get-RemainingSeconds $TimeoutSeconds
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $File
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    foreach ($argument in $Arguments) { $start.ArgumentList.Add($argument) }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $start
    $script:activeNative = $process
    try {
        # Unknown starts and timed-out process trees remain fail-closed: do not
        # detach or delete storage that a native child could still be using.
        $script:nativeUnsettled = $true
        if (-not $process.Start()) { $script:nativeUnsettled = $false; throw 'native-process-not-started' }
        $outTask = $process.StandardOutput.ReadToEndAsync()
        $errTask = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit($timeout * 1000)) {
            $script:nativeUnsettled = $true
            # Only this newly started process tree, never a service or other task.
            $process.Kill($true)
            $null = $process.WaitForExit(15000)
            # Parent exit does not prove every descendant has exited. Preserve
            # the fixture on any timeout; the ephemeral runner is the backstop.
            throw 'native-process-timeout'
        }
        $script:nativeUnsettled = $false
        $output = $outTask.GetAwaiter().GetResult()
        $null = $errTask.GetAwaiter().GetResult()
        $phase['native'] = [ordered]@{ name = $Name; exitCode = $process.ExitCode }
        if ($process.ExitCode -ne 0) { throw 'native-process-failed' }
        if ($Capture) { return $output }
    } finally {
        # Raw native output is deliberately never printed or included in artifacts.
        if (-not $script:nativeUnsettled) {
            $process.Dispose()
            $script:activeNative = $null
        }
    }
}
function Get-HostBootFingerprint {
    $bcdedit = Join-Path $env:SystemRoot 'System32\bcdedit.exe'
    $store = Invoke-Native 'bcdedit-read-system' $bcdedit @('/enum', 'all', '/v') 30 -Capture
    $firmware = Invoke-Native 'bcdedit-read-firmware' $bcdedit @('/enum', 'firmware', '/v') 30 -Capture
    $sha = [Security.Cryptography.SHA256]::Create()
    try {
        return [ordered]@{
            systemStoreSha256 = [Convert]::ToHexString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($store)))
            firmwareEntriesSha256 = [Convert]::ToHexString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($firmware)))
        }
    } finally { $sha.Dispose() }
}
function Get-OwnedDisk([switch] $Raw) {
    Assert-OwnedFile $vhdPath
    $vhd = Get-VHD -Path $vhdPath -ErrorAction Stop
    if ($vhd.DiskIdentifier -ne $vhdIdentifier -or $vhd.Size -ne $maximumDiskBytes -or
        $vhd.VhdType -ne 'Dynamic' -or -not $vhd.Attached -or $vhd.DiskNumber -ne $diskNumber) { throw 'vhd-identity-changed' }
    $disk = Get-Disk -Number $vhd.DiskNumber -ErrorAction Stop
    if ($priorDiskNumbers -contains $disk.Number -or $disk.Number -ne $diskNumber -or
        $disk.Size -ne $maximumDiskBytes -or $disk.IsBoot -or $disk.IsSystem -or
        $disk.Path -ne $diskDevicePath -or $disk.UniqueId -ne $diskUniqueId) { throw 'disk-identity-changed' }
    if ($Raw) {
        if ($disk.PartitionStyle -ne 'RAW' -or @(Get-Partition -DiskNumber $disk.Number -ErrorAction SilentlyContinue).Count -ne 0) { throw 'nonblank-disk-refused' }
    } elseif ($disk.PartitionStyle -ne 'GPT') { throw 'unexpected-partition-style' }
    return $disk
}
function Get-OwnedPartition($Expected) {
    $disk = Get-OwnedDisk
    $matches = @($disk | Get-Partition | Where-Object PartitionNumber -eq $Expected.PartitionNumber)
    if ($matches.Count -ne 1) { throw 'partition-mapping-not-unique' }
    $partition = $matches[0]
    if ($partition.DiskNumber -ne $diskNumber -or $partition.Guid -ne $Expected.Guid -or
        $partition.Offset -ne $Expected.Offset -or $partition.Size -ne $Expected.Size -or
        $partition.GptType -ne $Expected.GptType -or $partition.IsBoot -or $partition.IsSystem) { throw 'partition-identity-changed' }
    if (-not $partition.DriveLetter -or $partition.DriveLetter -ne $Expected.DriveLetter) { throw 'partition-drive-letter-changed' }
    $byLetter = @(Get-Partition -DriveLetter $partition.DriveLetter)
    if ($byLetter.Count -ne 1 -or $byLetter[0].DiskNumber -ne $diskNumber -or $byLetter[0].Guid -ne $Expected.Guid) { throw 'drive-letter-target-changed' }
    return $partition
}
function Assert-InstallTargets {
    $efi = Get-OwnedPartition $efiPartition
    $os = Get-OwnedPartition $osPartition
    if ($efi.GptType -ne '{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}' -or
        $os.GptType -ne '{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}' -or $efi.Guid -eq $os.Guid) { throw 'install-partition-types-invalid' }
    $efiVolume = @($efi | Get-Volume)
    $osVolume = @($os | Get-Volume)
    if ($efiVolume.Count -ne 1 -or $osVolume.Count -ne 1 -or
        $efiVolume[0].FileSystem -ne 'FAT32' -or $osVolume[0].FileSystem -ne 'NTFS') { throw 'install-volume-filesystems-invalid' }
    $source = "$($os.DriveLetter):\Windows"
    if ($source.Equals($env:SystemRoot, [StringComparison]::OrdinalIgnoreCase) -or
        $os.DriveLetter -eq $env:SystemDrive.TrimEnd(':') -or $efi.DriveLetter -eq $env:SystemDrive.TrimEnd(':') -or
        $efi.DriveLetter -eq 'D' -or $os.DriveLetter -eq 'D') { throw 'host-drive-target-refused' }
}
function Get-OwnedVM([switch] $AllowMissing) {
    if ($null -eq $vmId) {
        if (-not $vmCreationAttempted) { if ($AllowMissing) { return $null }; throw 'vm-not-created' }
        # Recover only a provably new VM if New-VM had an uncertain return.
        $candidates = @(Get-VM -Name $vmName -ErrorAction SilentlyContinue)
    } else { $candidates = @(Get-VM -Id $vmId -ErrorAction SilentlyContinue) }
    if ($candidates.Count -eq 0 -and $AllowMissing) { return $null }
    if ($candidates.Count -ne 1) { throw 'owned-vm-not-unique' }
    $vm = $candidates[0]
    if ($vm.Name -ne $vmName -or $priorVmIds -contains $vm.Id -or $vm.Generation -ne 2 -or
        -not $vm.ConfigurationLocation.StartsWith($directory + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'vm-identity-changed' }
    if ($null -ne $vmId -and $vm.Id -ne $vmId) { throw 'vm-id-changed' }
    if ($null -eq $vmId) { $script:vmId = $vm.Id; $result.vm.id = $vm.Id.ToString() }
    return $vm
}
function Assert-VMIsolation {
    $vm = Get-OwnedVM
    $adapters = @(Get-VMNetworkAdapter -VM $vm)
    $drives = @(Get-VMHardDiskDrive -VM $vm)
    $memory = Get-VMMemory -VM $vm
    $cpu = Get-VMProcessor -VM $vm
    $firmware = Get-VMFirmware -VM $vm
    $result.vm.networkAdapterCount = $adapters.Count
    $result.vm.secureBoot = [string]$firmware.SecureBoot
    if ($adapters.Count -ne 0 -or $drives.Count -ne 1 -or $drives[0].Path -ne $vhdPath -or
        $memory.DynamicMemoryEnabled -or $memory.Startup -ne $memoryBytes -or $cpu.Count -ne 2 -or
        [string]$firmware.SecureBoot -ne $expectedSecureBoot) { throw 'vm-isolation-or-resource-limit-invalid' }
}
function Invoke-Guest([scriptblock] $Script, [int] $TimeoutSeconds = 60, [object[]] $Arguments = @()) {
    Assert-VMIsolation
    $timeout = Get-RemainingSeconds $TimeoutSeconds
    $job = Invoke-Command -VMId $vmId -Credential $guestCredential -ScriptBlock $Script -ArgumentList $Arguments -AsJob -ErrorAction Stop
    $guestJobs.Add($job)
    try {
        $null = Wait-Job -Job $job -Timeout $timeout
        if ($job.State -ne 'Completed') { throw 'guest-command-not-completed' }
        $values = @(Receive-Job -Job $job -ErrorAction Stop)
        if ($job.ChildJobs.Count -ne 1 -or $job.ChildJobs[0].Error.Count -ne 0) { throw 'guest-command-failed' }
        return $values
    } finally {
        if ($job.State -notin @('Completed', 'Failed', 'Stopped') -and $AllowKernelProbe) {
            # Retain the remote session and its children until the owned VM is
            # off. Stopping this runspace could kill a live mapping harness.
        } else {
            if ($job.State -notin @('Completed', 'Failed', 'Stopped')) { Stop-Job -Job $job -ErrorAction SilentlyContinue }
            Remove-Job -Job $job -Force -ErrorAction SilentlyContinue
            $null = $guestJobs.Remove($job)
        }
    }
}
function Invoke-KernelProbe([string] $Mode, [int] $TimeoutSeconds = 90) {
    $values = @(Invoke-Guest { param($mode); & C:\SprigProbe\windows_guest_probe_payload.ps1 -Mode $mode } $TimeoutSeconds @($Mode))
    if ($values.Count -ne 1) { throw 'probe-no-valid-result' }
    if ($null -ne $values[0].PSObject.Properties['probeFailure']) {
        $failure = $values[0].probeFailure
        if ($failure.stage -notmatch '^[a-z-]{1,80}$' -or $failure.code -notmatch '^probe-[a-z-]{1,80}$') { throw 'probe-no-valid-result' }
        $safe = [ordered]@{stage=[string]$failure.stage;code=[string]$failure.code;hresult=[int]$failure.hresult;native=$null}
        if ($null -ne $failure.native) {
            if ($failure.native.program -notin @('signtool.exe','sc.exe','bcdedit.exe','verifier.exe','freeze_probe.exe')) { throw 'probe-no-valid-result' }
            $safe.native=[ordered]@{program=[string]$failure.native.program;completed=[bool]$failure.native.completed;exitCode=$failure.native.exitCode}
        }
        $result.kernelProbe['failure']=$safe
        Write-SafeResult
        throw 'probe-no-valid-result'
    }
    return $values[0]
}
function Wait-GuestReady([int] $TimeoutSeconds, [string] $PreviousBoot = '') {
    $until = [DateTime]::UtcNow.AddSeconds((Get-RemainingSeconds $TimeoutSeconds))
    while ([DateTime]::UtcNow -lt $until) {
        Assert-TimeRemaining
        Assert-VMIsolation
        try {
            $observations = @(Invoke-Guest {
                $ErrorActionPreference = 'Stop'
                $os = Get-CimInstance Win32_OperatingSystem
                $setup = Get-ItemProperty -LiteralPath 'HKLM:\SYSTEM\Setup'
                $setupActive = [int]$setup.SystemSetupInProgress
                $oobeActive = 0
                if ($null -ne $setup.PSObject.Properties['OOBEInProgress']) { $oobeActive = [int]$setup.OOBEInProgress }
                [pscustomobject]@{
                    computerName = $env:COMPUTERNAME; caption = $os.Caption; version = $os.Version
                    buildNumber = $os.BuildNumber; lastBootUpTimeUtc = $os.LastBootUpTime.ToUniversalTime().ToString('o')
                    setupInProgress = $setupActive; oobeInProgress = $oobeActive
                }
            } 45)
            if ($observations.Count -eq 1) {
                $observation = $observations[0]
                if ($observation.computerName -eq $guestName -and $observation.setupInProgress -eq 0 -and $observation.oobeInProgress -eq 0 -and
                    ($PreviousBoot -eq '' -or [DateTime]::Parse($observation.lastBootUpTimeUtc) -gt [DateTime]::Parse($PreviousBoot))) {
                    return [ordered]@{
                        computerName = [string]$observation.computerName; caption = [string]$observation.caption
                        version = [string]$observation.version; buildNumber = [string]$observation.buildNumber
                        lastBootUpTimeUtc = [string]$observation.lastBootUpTimeUtc
                    }
                }
            }
        } catch {
            # Setup/reboot can temporarily refuse PowerShell Direct. Do not print
            # authentication exceptions, credentials, remote streams or XML.
        }
        Start-Sleep -Seconds 10
    }
    throw 'guest-readiness-deadline-exceeded'
}
function Remove-GuestAnswerFiles {
    $removed = @(Invoke-Guest {
        $ErrorActionPreference = 'Stop'
        # Exact answer-file locations only. Never collect Panther/setup logs.
        $paths = @(
            'C:\unattend.xml', 'C:\Autounattend.xml',
            'C:\Windows\Panther\unattend.xml', 'C:\Windows\Panther\Unattend\unattend.xml',
            'C:\Windows\Panther\UnattendGC\unattend.xml', 'C:\Windows\System32\Sysprep\unattend.xml'
        )
        foreach ($path in $paths) {
            if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Force -ErrorAction Stop }
        }
        foreach ($path in $paths) { if (Test-Path -LiteralPath $path) { throw 'guest-answer-file-remains' } }
        return $true
    } 60)
    if ($removed.Count -ne 1 -or $removed[0] -ne $true) { throw 'guest-answer-removal-unverified' }
    $result.credentialHandling.guestAnswerFilesRemoved = $true
}

try {
    Start-Phase 'guard-host-and-new-fixture'
    foreach ($path in @($directory, $resultPath, $eventsPath)) {
        if (Test-Path -LiteralPath $path) { throw 'fixture-or-report-already-exists' }
    }
    Assert-NoReparse (Get-Location).Path
    # Reserve ownership atomically. A preexisting report is never overwritten,
    # including by the finally block after a failed no-clobber check.
    $reportHandle = [IO.File]::Open($resultPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    $reportHandle.Dispose()
    $reportReserved = $true
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'administrator-required' }
    if ($env:ImageOS -notin @('win25', 'win25-vs2026')) { throw 'windows-2025-runner-required' }
    $hostOS = Get-CimInstance Win32_OperatingSystem
    if (([Version]$hostOS.Version).Build -ne 26100 -or $hostOS.ProductType -ne 3) { throw 'windows-server-2025-host-required' }
    $result.runner['osVersion'] = [string]$hostOS.Version
    foreach ($command in @('Get-VM', 'New-VM', 'Get-VHD', 'New-VHD', 'Mount-VHD', 'Get-Disk', 'Get-WindowsImage')) {
        if (-not (Get-Command $command -ErrorAction SilentlyContinue)) { throw 'required-command-unavailable' }
    }
    if ((Get-Service -Name vmms).Status -ne 'Running') { throw 'hyper-v-not-already-running' }
    Assert-NoReparse 'D:\'
    $dVolume = Get-Volume -DriveLetter D
    # Reserve the full 40-GiB virtual capacity, 7.6-GiB ISO, up to 6-GiB
    # Hyper-V runtime state and 6.4-GiB scratch/headroom, despite dynamic allocation.
    if ($dVolume.FileSystem -ne 'NTFS' -or $dVolume.SizeRemaining -lt 60GB) { throw 'insufficient-d-drive-space-or-filesystem' }
    if ($hostOS.FreePhysicalMemory * 1KB -lt 8GB) { throw 'insufficient-free-host-memory' }
    $priorVmIds = @(Get-VM | ForEach-Object Id)
    $priorDiskNumbers = @(Get-Disk | ForEach-Object Number)
    if (@(Get-VM -Name $vmName -ErrorAction SilentlyContinue).Count -ne 0) { throw 'vm-name-already-exists' }
    $null = [IO.Directory]::CreateDirectory($directory)
    $directoryCreated = $true
    Assert-NoReparse $directory
    # Restrict only this brand-new private fixture directory, not host settings.
    $acl = [Security.AccessControl.DirectorySecurity]::new()
    $acl.SetAccessRuleProtection($true, $false)
    foreach ($sid in @($identity.User, [Security.Principal.SecurityIdentifier]::new('S-1-5-18'), [Security.Principal.SecurityIdentifier]::new('S-1-5-32-544'))) {
        $rule = [Security.AccessControl.FileSystemAccessRule]::new($sid, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow')
        $acl.AddAccessRule($rule)
    }
    Set-Acl -LiteralPath $directory -AclObject $acl
    Complete-Phase

    Start-Phase 'download-and-verify-pinned-media'
    Invoke-Native 'download-pinned-iso' (Join-Path $env:SystemRoot 'System32\curl.exe') @(
        '--fail', '--silent', '--show-error', '--location', '--max-redirs', '3',
        '--proto', '=https', '--proto-redir', '=https', '--tlsv1.2', '--connect-timeout', '30',
        '--max-time', '1200', '--output', $isoPath, $isoUrl
    ) 1230
    Assert-OwnedFile $isoPath
    if ((Get-Item -LiteralPath $isoPath).Length -ne $isoBytes) { throw 'iso-size-mismatch' }
    if ((Get-FileHash -LiteralPath $isoPath -Algorithm SHA256).Hash -ne $isoSha256) { throw 'iso-sha256-mismatch' }
    $result.media.verified = $true
    $isoMountAttempted = $true
    $null = Mount-DiskImage -ImagePath $isoPath -StorageType ISO -Access ReadOnly -PassThru
    $isoVolumes = @(Get-DiskImage -ImagePath $isoPath | Get-Volume)
    if ($isoVolumes.Count -ne 1 -or -not $isoVolumes[0].DriveLetter) { throw 'iso-volume-not-unique' }
    $wimPath = "$($isoVolumes[0].DriveLetter):\sources\install.wim"
    $imageInfo = Get-WindowsImage -ImagePath $wimPath -Index 1
    if ($imageInfo.EditionId -ne 'ServerStandardEval' -or $imageInfo.InstallationType -ne 'Server Core' -or
        $imageInfo.Architecture -ne 9 -or ([Version]$imageInfo.Version).Build -ne 26100) { throw 'pinned-image-index-metadata-mismatch' }
    $result.media['imageName'] = $imageInfo.ImageName
    $result.media['editionId'] = $imageInfo.EditionId
    Complete-Phase

    Start-Phase 'create-and-partition-new-vhdx'
    if (Test-Path -LiteralPath $vhdPath) { throw 'vhd-path-already-exists' }
    $newVhd = New-VHD -Path $vhdPath -SizeBytes $maximumDiskBytes -Dynamic
    $vhdIdentifier = $newVhd.DiskIdentifier
    if ($null -eq $vhdIdentifier -or $vhdIdentifier -eq [Guid]::Empty) { throw 'vhd-identifier-missing' }
    $hostVhdMountAttempted = $true
    $mounted = Mount-VHD -Path $vhdPath -NoDriveLetter -PassThru
    $diskNumber = $mounted.DiskNumber
    if ($null -eq $diskNumber -or $priorDiskNumbers -contains $diskNumber) { throw 'new-vhd-disk-number-invalid' }
    $initialDisk = Get-Disk -Number $diskNumber
    $diskUniqueId = $initialDisk.UniqueId
    $diskDevicePath = $initialDisk.Path
    $rawDisk = Get-OwnedDisk -Raw
    $initialized = @($rawDisk | Initialize-Disk -PartitionStyle GPT -PassThru -Confirm:$false)
    if ($initialized.Count -ne 1 -or $initialized[0].Number -ne $diskNumber) { throw 'initialized-disk-mismatch' }
    # GPT initialization assigns the new disk identity; retain it thereafter.
    $diskUniqueId = $initialized[0].UniqueId
    $diskDevicePath = $initialized[0].Path
    $disk = Get-OwnedDisk
    $efiPartition = $disk | New-Partition -Size 300MB -GptType '{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}' -AssignDriveLetter
    $disk = Get-OwnedDisk
    $msr = $disk | New-Partition -Size 16MB -GptType '{e3c9e316-0b5c-4db8-817d-f92df00215ae}'
    if ($msr.DiskNumber -ne $diskNumber) { throw 'msr-disk-mismatch' }
    $disk = Get-OwnedDisk
    $osPartition = $disk | New-Partition -UseMaximumSize -GptType '{ebd0a0a2-b9e5-4433-87c0-68b6b72699c7}' -AssignDriveLetter
    # Bind BOTH partitions before EACH format. No Clear-Disk or fixed disk number.
    $efi = Get-OwnedPartition $efiPartition
    $null = Get-OwnedPartition $osPartition
    $null = $efi | Format-Volume -FileSystem FAT32 -NewFileSystemLabel 'SPRIG_EFI' -Confirm:$false
    $null = Get-OwnedPartition $efiPartition
    $os = Get-OwnedPartition $osPartition
    $null = $os | Format-Volume -FileSystem NTFS -NewFileSystemLabel 'SPRIG_OS' -Confirm:$false
    Assert-InstallTargets
    $result.disk = [ordered]@{
        path = $vhdPath; vhdIdentifier = [string]$vhdIdentifier; uniqueId = [string]$diskUniqueId
        diskNumber = $diskNumber; maximumBytes = $maximumDiskBytes; partitionStyle = 'GPT'
        efiPartitionGuid = [string]$efiPartition.Guid; osPartitionGuid = [string]$osPartition.Guid
    }
    Complete-Phase

    Start-Phase 'apply-image-to-new-disk'
    $scratch = Join-Path $directory 'dism-scratch'
    $null = [IO.Directory]::CreateDirectory($scratch)
    $osRoot = "$($osPartition.DriveLetter):\"
    $offlineWindows = Join-Path $osRoot 'Windows'
    Assert-InstallTargets
    Invoke-Native 'dism-apply-image' (Join-Path $env:SystemRoot 'System32\dism.exe') @(
        '/Apply-Image', "/ImageFile:$wimPath", '/Index:1', "/ApplyDir:$osRoot", '/CheckIntegrity', '/Verify',
        "/ScratchDir:$scratch", "/LogPath:$(Join-Path $directory 'dism-private.log')", '/LogLevel:1'
    ) 1200
    Assert-InstallTargets
    Assert-NoReparse $offlineWindows
    if (-not (Test-Path -LiteralPath (Join-Path $offlineWindows 'System32\ntoskrnl.exe') -PathType Leaf)) { throw 'applied-image-missing-kernel' }
    Complete-Phase

    Start-Phase 'write-only-new-esp-boot-files'
    $beforeBoot = Get-HostBootFingerprint
    Assert-InstallTargets
    $efiRoot = "$($efiPartition.DriveLetter):"
    # Mandatory /s is never omitted. Source and destination were just re-bound to
    # the new disk. This does not modify the host system BCD or NVRAM boot order.
    Invoke-Native 'bcdboot-new-esp-only' (Join-Path $env:SystemRoot 'System32\bcdboot.exe') @($offlineWindows, '/s', $efiRoot, '/f', 'UEFI') 120
    $afterBoot = Get-HostBootFingerprint
    $result.hostBoot = [ordered]@{ before = $beforeBoot; after = $afterBoot; unchanged = $false }
    if ($beforeBoot.systemStoreSha256 -ne $afterBoot.systemStoreSha256 -or
        $beforeBoot.firmwareEntriesSha256 -ne $afterBoot.firmwareEntriesSha256) { throw 'host-boot-fingerprint-changed' }
    $result.hostBoot.unchanged = $true
    Assert-InstallTargets
    if (-not (Test-Path -LiteralPath ($efiRoot + '\EFI\Microsoft\Boot\BCD')) -or
        -not (Test-Path -LiteralPath ($efiRoot + '\EFI\Boot\bootx64.efi'))) { throw 'new-esp-boot-files-missing' }
    Complete-Phase

    Start-Phase 'prepare-ephemeral-guest-setup'
    $random = [byte[]]::new(48)
    [Security.Cryptography.RandomNumberGenerator]::Fill($random)
    $password = 'aA1!' + [Convert]::ToBase64String($random)
    [Array]::Clear($random, 0, $random.Length)
    $securePassword = ConvertTo-SecureString -String $password -AsPlainText -Force
    $guestCredential = [Management.Automation.PSCredential]::new('Administrator', $securePassword)
    # Only this answer file inside the fresh guest disk contains the password.
    # No AutoLogon, environment variable, command-line password or host-side copy.
    $answerXml = @"
<?xml version="1.0" encoding="utf-8"?>
<unattend xmlns="urn:schemas-microsoft-com:unattend">
  <settings pass="specialize">
    <component name="Microsoft-Windows-Shell-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <ComputerName>$guestName</ComputerName><TimeZone>UTC</TimeZone>
    </component>
  </settings>
  <settings pass="oobeSystem">
    <component name="Microsoft-Windows-International-Core" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <InputLocale>en-US</InputLocale><SystemLocale>en-US</SystemLocale><UILanguage>en-US</UILanguage><UserLocale>en-US</UserLocale>
    </component>
    <component name="Microsoft-Windows-Shell-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <UserAccounts><AdministratorPassword><Value>$password</Value><PlainText>true</PlainText></AdministratorPassword></UserAccounts>
      <OOBE><HideEULAPage>true</HideEULAPage><HideLocalAccountScreen>true</HideLocalAccountScreen><HideOnlineAccountScreens>true</HideOnlineAccountScreens><HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE></OOBE>
    </component>
  </settings>
</unattend>
"@
    if ($AllowKernelProbe) {
        # Copy only pinned build artifacts to this newly created offline guest.
        # No guest credential has been copied into this directory.
        if (-not $KernelPayloadDirectory -or $KernelPayloadDirectory -notmatch '^D:\\sprig-kernel-payload-[0-9a-f]{32}$') { throw 'probe-payload-invalid' }
        Assert-NoReparse $KernelPayloadDirectory
        $manifestPath = Join-Path $KernelPayloadDirectory 'payload-manifest.json'
        Assert-NoReparse $manifestPath
        $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
        $payloadNames = @('winfsp-x64.sys','winfsp-x64.dll','memfs-x64.exe','freeze_probe.exe','signtool.exe')
        if ($manifest.sourceCommit -ne 'ddca7bd5481857a65ba552f643b8776fd070836f' -or
            $manifest.experimentCommit -ne $env:GITHUB_SHA -or @($manifest.files).Count -ne $payloadNames.Count) { throw 'probe-payload-invalid' }
        $target = Join-Path $osRoot 'SprigProbe'
        if (Test-Path -LiteralPath $target) { throw 'probe-payload-invalid' }
        $null = [IO.Directory]::CreateDirectory($target)
        foreach ($name in $payloadNames) {
            $entry = @($manifest.files | Where-Object name -CEQ $name)
            $source = Join-Path $KernelPayloadDirectory $name
            Assert-NoReparse $source
            if ($entry.Count -ne 1 -or $entry[0].bytes -gt 64MB -or (Get-Item $source).Length -ne $entry[0].bytes -or
                (Get-FileHash $source).Hash -ne $entry[0].sha256) { throw 'probe-payload-invalid' }
            [IO.File]::Copy($source, (Join-Path $target $name), $false)
        }
        [IO.File]::Copy($manifestPath, (Join-Path $target 'payload-manifest.json'), $false)
        [IO.File]::Copy((Join-Path $PSScriptRoot 'windows_guest_probe_payload.ps1'), (Join-Path $target 'windows_guest_probe_payload.ps1'), $false)
        $result['kernelProbe'] = [ordered]@{sourceCommit=$manifest.sourceCommit; experimentCommit=$manifest.experimentCommit; protectionBefore=$null; protectionAfter=$null; signing=$null; ordinary=$null; verifier=$null; verified=$null; wholeTreeSnapshotVerified=$false}
        $result.scope = 'Explicit guest-only test signing and fixed experimental RAM mapping probe; no CAS, no guest NIC, no host security changes'
    } elseif ($KernelPayloadDirectory) { throw 'probe-payload-invalid' }

    Assert-InstallTargets
    $panther = Join-Path $offlineWindows 'Panther'
    $null = [IO.Directory]::CreateDirectory($panther)
    Assert-NoReparse $panther
    $answerPath = Join-Path $panther 'unattend.xml'
    if (Test-Path -LiteralPath $answerPath) { throw 'answer-file-already-exists' }
    [IO.File]::WriteAllText($answerPath, $answerXml, [Text.UTF8Encoding]::new($false))
    $answerXml = $null
    $password = $null
    $null = Get-OwnedDisk
    Dismount-VHD -Path $vhdPath
    if ((Get-VHD -Path $vhdPath).Attached) { throw 'vhd-host-detach-not-confirmed' }
    $hostVhdMountAttempted = $false
    Complete-Phase

    Start-Phase 'create-isolated-generation-2-vm'
    Assert-OwnedFile $vhdPath
    $vmCreationAttempted = $true
    $vm = New-VM -Name $vmName -Generation 2 -MemoryStartupBytes $memoryBytes -VHDPath $vhdPath -Path (Join-Path $directory 'vm')
    $vmId = $vm.Id
    $result.vm.id = $vmId.ToString()
    $vm = Get-OwnedVM
    # New-VM can create a disconnected NIC even without SwitchName. Remove it
    # while the new VM is OFF, before its first instruction can execute.
    if ($vm.State -ne 'Off') { throw 'new-vm-unexpected-state' }
    Get-VMNetworkAdapter -VM $vm | Remove-VMNetworkAdapter -Confirm:$false
    Set-VMProcessor -VM $vm -Count 2
    Set-VMMemory -VM $vm -DynamicMemoryEnabled $false -StartupBytes $memoryBytes
    Set-VM -VM $vm -AutomaticCheckpointsEnabled $false -CheckpointType Disabled -AutomaticStartAction Nothing -AutomaticStopAction ShutDown
    Assert-VMIsolation
    Start-VM -VM (Get-OwnedVM) | Out-Null
    Complete-Phase

    Start-Phase 'first-boot-powershell-direct'
    $result.firstBoot = Wait-GuestReady 900
    $lastBoot = $result.firstBoot
    Remove-GuestAnswerFiles
    Complete-Phase

    if ($AllowKernelProbe) {
        Start-Phase 'inspect-original-guest-protection'
        $before = @(Invoke-KernelProbe 'Inspect' 90)
        if ($before.Count -ne 1 -or $before[0].driverServices -ne 0 -or -not $before[0].secureBoot) { throw 'probe-no-valid-result' }
        $result.kernelProbe.protectionBefore = $before[0].protection
        Complete-Phase

        Start-Phase 'normal-shutdown-before-guest-only-secureboot-change'
        try { $null = Invoke-Guest { & "$env:SystemRoot\System32\shutdown.exe" /s /t 0; if ($LASTEXITCODE -ne 0) { throw 'probe-shutdown-failed' } } 60 } catch { }
        $until = [DateTime]::UtcNow.AddSeconds((Get-RemainingSeconds 180))
        while ((Get-OwnedVM).State -ne 'Off' -and [DateTime]::UtcNow -lt $until) { Start-Sleep -Seconds 2 }
        if ((Get-OwnedVM).State -ne 'Off') { throw 'probe-shutdown-failed' }
        Set-VMFirmware -VM (Get-OwnedVM) -EnableSecureBoot Off
        $expectedSecureBoot = 'Off'
        Assert-VMIsolation
        Start-VM -VM (Get-OwnedVM) | Out-Null
        $lastBoot = Wait-GuestReady 480 $result.firstBoot.lastBootUpTimeUtc
        $result.kernelProbe['secureBootRestart'] = $lastBoot
        Complete-Phase

        Start-Phase 'check-protection-and-prepare-guest-signature'
        $after = @(Invoke-KernelProbe 'Inspect' 90)
        if ($after.Count -ne 1 -or ($before[0].protection | ConvertTo-Json -Compress) -ne ($after[0].protection | ConvertTo-Json -Compress)) { throw 'probe-protection-changed' }
        $signed = @(Invoke-KernelProbe 'Prepare' 120)
        if ($signed.Count -ne 1 -or -not $signed[0].testSigningRequested) { throw 'probe-no-valid-result' }
        $result.kernelProbe.signing = $signed[0]
        Complete-Phase
    }

    Start-Phase 'normal-guest-reboot'
    Assert-VMIsolation
    # A successful normal power transition may disconnect PowerShell Direct
    # before it returns. This request is never treated as completion: only the
    # independent new boot time / VM Off observation below can prove success.
    try {
        $null = Invoke-Guest {
            & "$env:SystemRoot\System32\shutdown.exe" /r /t 0
            if ($LASTEXITCODE -ne 0) { throw 'guest-normal-reboot-not-accepted' }
        } 60
        $phase['requestReturned'] = $true
    } catch { $phase['requestReturned'] = $false }

    $result.secondBoot = Wait-GuestReady 480 $lastBoot.lastBootUpTimeUtc
    $result.rebootObserved = $true
    Remove-GuestAnswerFiles
    Complete-Phase

    if ($AllowKernelProbe) {
        Start-Phase 'verify-protection-after-testsigning'
        $after = @(Invoke-KernelProbe 'Inspect' 90)
        if ($after.Count -ne 1 -or ($before[0].protection | ConvertTo-Json -Compress) -ne ($after[0].protection | ConvertTo-Json -Compress)) { throw 'probe-protection-changed' }
        $result.kernelProbe.protectionAfter = $after[0].protection
        Complete-Phase

        Start-Phase 'ordinary-ram-mapping-falsification'
        $trial = @(Invoke-KernelProbe 'Test' 180)
        if ($trial.Count -ne 1) { throw 'probe-no-valid-result' }
        $result.kernelProbe.ordinary = $trial[0]
        Write-SafeResult
        if ($trial[0].outcome -ne 'no_in_hold_ack_observed' -or $trial[0].exitCode -ne 0 -or
            -not $trial[0].negativeControlValid -or -not $trial[0].normalMemfsStop -or
            -not $trial[0].coherency.a_ioctl.completed_hold -or -not $trial[0].coherency.b_ioctl.completed_hold -or
            -not $trial[0].coherency.writers_recovered -or -not $trial[0].coherency.continuous_stores_recovered -or
            -not $trial[0].coherency.observation_completed -or -not $trial[0].coherency.timing_valid -or -not $trial[0].coherency.heartbeats_valid) { throw 'probe-test-not-safe' }
        Complete-Phase

        Start-Phase 'configure-driver-only-oneboot-verifier'
        $v = @(Invoke-KernelProbe 'Verifier' 90)
        if ($v.Count -ne 1 -or $v[0].mask -ne 59 -or $v[0].driver -ne 'winfsp-x64.sys' -or $v[0].bootMode -ne 'oneboot') { throw 'probe-no-valid-result' }
        $result.kernelProbe.verifier = $v[0]
        try { $null = Invoke-Guest { & "$env:SystemRoot\System32\shutdown.exe" /r /t 0 } 60 } catch { }
        $verifierBoot = Wait-GuestReady 480 $result.secondBoot.lastBootUpTimeUtc
        $result.kernelProbe['verifierRestart'] = $verifierBoot
        $after = @(Invoke-KernelProbe 'Inspect' 90)
        if ($after.Count -ne 1 -or ($before[0].protection | ConvertTo-Json -Compress) -ne ($after[0].protection | ConvertTo-Json -Compress)) { throw 'probe-protection-changed' }
        Complete-Phase

        Start-Phase 'verifier-ram-mapping-falsification'
        $trial = @(Invoke-KernelProbe 'Test' 180)
        if ($trial.Count -ne 1) { throw 'probe-no-valid-result' }
        $result.kernelProbe.verified = $trial[0]
        if (-not $trial[0].verifierActive) { throw 'probe-no-valid-result' }
        Write-SafeResult
        if ($trial[0].outcome -ne 'no_in_hold_ack_observed' -or $trial[0].exitCode -ne 0 -or
            -not $trial[0].negativeControlValid -or -not $trial[0].normalMemfsStop -or
            -not $trial[0].coherency.a_ioctl.completed_hold -or -not $trial[0].coherency.b_ioctl.completed_hold -or
            -not $trial[0].coherency.writers_recovered -or -not $trial[0].coherency.continuous_stores_recovered -or
            -not $trial[0].coherency.observation_completed -or -not $trial[0].coherency.timing_valid -or -not $trial[0].coherency.heartbeats_valid) { throw 'probe-test-not-safe' }
        Complete-Phase
    }

    Start-Phase 'collect-whitelisted-event-metadata'
    $events = @(Invoke-Guest {
        $allowedProviders = @('Microsoft-Windows-Kernel-General', 'Microsoft-Windows-Kernel-Power', 'EventLog', 'User32')
        Get-WinEvent -FilterHashtable @{ LogName = 'System'; Id = @(12, 13, 41, 1074, 6005, 6006, 6008) } -MaxEvents 64 -ErrorAction SilentlyContinue |
            Where-Object ProviderName -in $allowedProviders | ForEach-Object {
                [pscustomobject]@{ timeCreatedUtc = $_.TimeCreated.ToUniversalTime().ToString('o'); id = [int]$_.Id; provider = $_.ProviderName; level = [int]$_.Level }
            }
    } 60)
    # Reconstruct the allowlist on the host. No message text, event payloads,
    # Setup/Panther logs, transcripts, screenshots, VHDs or answer files escape.
    $safeEvents = @($events | ForEach-Object {
        if ($_.provider -notin @('Microsoft-Windows-Kernel-General', 'Microsoft-Windows-Kernel-Power', 'EventLog', 'User32')) { throw 'unexpected-event-provider' }
        [ordered]@{ timeCreatedUtc = [string]$_.timeCreatedUtc; id = [int]$_.id; provider = [string]$_.provider; level = [int]$_.level }
    })
    $eventJson = [ordered]@{ runId = $runId; vmId = $vmId.ToString(); events = $safeEvents } | ConvertTo-Json -Depth 6
    $eventHandle = [IO.File]::Open($eventsPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    $eventWriter = [IO.StreamWriter]::new($eventHandle, [Text.UTF8Encoding]::new($false))
    try { $eventWriter.Write($eventJson) } finally { $eventWriter.Dispose() }
    Complete-Phase

    Start-Phase 'normal-guest-shutdown'
    Assert-VMIsolation
    # A successful normal power transition may disconnect PowerShell Direct
    # before it returns. This request is never treated as completion: only the
    # independent new boot time / VM Off observation below can prove success.
    try {
        $null = Invoke-Guest {
            & "$env:SystemRoot\System32\shutdown.exe" /s /t 0
            if ($LASTEXITCODE -ne 0) { throw 'guest-normal-shutdown-not-accepted' }
        } 60
        $phase['requestReturned'] = $true
    } catch { $phase['requestReturned'] = $false }

    $shutdownDeadline = [DateTime]::UtcNow.AddSeconds((Get-RemainingSeconds 180))
    while ([DateTime]::UtcNow -lt $shutdownDeadline -and (Get-OwnedVM).State -ne 'Off') { Start-Sleep -Seconds 3 }
    if ((Get-OwnedVM).State -ne 'Off') { throw 'normal-guest-shutdown-deadline-exceeded' }
    $result.normalShutdownObserved = $true
    if ($AllowKernelProbe) {
        $finalHostBoot = Get-HostBootFingerprint
        if ($beforeBoot.systemStoreSha256 -ne $finalHostBoot.systemStoreSha256 -or
            $beforeBoot.firmwareEntriesSha256 -ne $finalHostBoot.firmwareEntriesSha256) { throw 'host-boot-fingerprint-changed' }
        $result.hostBoot['afterGuestExperiment'] = $finalHostBoot
    }
    $smokePassed = $true
    Complete-Phase
} catch {
    $result.failedPhase = $(if ($null -eq $phase) { 'initialization' } else { $phase.name })
    # Error type and HRESULT only. Never serialize ErrorRecord, InvocationInfo,
    # ScriptStackTrace, raw Message or a command/script containing a credential.
    $failureCode = 'operation-failed'
    if ($safeFailureCodes -contains $_.Exception.Message) { $failureCode = $_.Exception.Message }
    $result.failure = [ordered]@{ code = $failureCode; type = $_.Exception.GetType().FullName; hresult = $_.Exception.HResult }
    if ($null -ne $phase -and $phase.status -eq 'running') {
        $phase.status = 'failed'; $phase.completedAtUtc = [DateTime]::UtcNow.ToString('o')
    }
} finally {
    $cleanupErrors = [Collections.Generic.List[string]]::new()
    $result.cleanup.status = 'pending'
    # Preserve safe diagnostics before any Hyper-V/storage teardown can stall.
    try { Write-SafeResult } catch { $cleanupErrors.Add('sanitized-report-write-failed') }
    if (-not $AllowKernelProbe) {
        foreach ($job in @($guestJobs.ToArray())) {
            try { Stop-Job -Job $job -ErrorAction SilentlyContinue; Remove-Job -Job $job -Force -ErrorAction Stop }
            catch { $cleanupErrors.Add('guest-job-cleanup-failed') }
        }
    }
    try {
        $ownedVM = Get-OwnedVM -AllowMissing
        if ($null -ne $ownedVM) {
            $safeTrialReturned = $false
            if ($AllowKernelProbe -and $result.Contains('kernelProbe')) {
                foreach ($trialResult in @($result.kernelProbe.ordinary, $result.kernelProbe.verified)) {
                    if ($null -ne $trialResult -and $trialResult.normalMemfsStop) { $safeTrialReturned = $true }
                }
            }
            if ($ownedVM.State -ne 'Off' -and $safeTrialReturned -and $guestJobs.Count -eq 0) {
                # A useful negative result is not a hang. Prefer normal power
                # down after all harness handles and memfs have closed.
                try {
                    $null = Invoke-Guest { & "$env:SystemRoot\System32\shutdown.exe" /s /t 0 } 30
                } catch { }
                $normalUntil = [DateTime]::UtcNow.AddSeconds(60)
                while ((Get-OwnedVM).State -ne 'Off' -and [DateTime]::UtcNow -lt $normalUntil) { Start-Sleep -Seconds 2 }
                if ((Get-OwnedVM).State -eq 'Off') { $result.normalShutdownObserved = $true }
            }
            if ((Get-OwnedVM).State -ne 'Off') {
                # Explicit authorization is limited to this exact newly created VM.
                $stopJob = Stop-VM -VM (Get-OwnedVM) -TurnOff -Force -Confirm:$false -AsJob
                Wait-CleanupJob $stopJob
                if ((Get-OwnedVM).State -ne 'Off') { throw 'owned-vm-still-running' }
                $result.cleanup.forcedOff = $true
            }
            if ((Get-OwnedVM).State -ne 'Off') { throw 'owned-vm-still-running' }
            $removeJob = Remove-VM -VM (Get-OwnedVM) -Force -Confirm:$false -AsJob
            Wait-CleanupJob $removeJob
            if ($null -ne (Get-OwnedVM -AllowMissing)) { throw 'owned-vm-still-present' }
        }
        $result.cleanup.vmRemoved = $true
    } catch { $cleanupErrors.Add('owned-vm-cleanup-failed') }
    if ($AllowKernelProbe -and $result.cleanup.vmRemoved) {
        foreach ($job in @($guestJobs.ToArray())) {
            try { Stop-Job -Job $job -ErrorAction SilentlyContinue; Remove-Job -Job $job -Force -ErrorAction Stop }
            catch { $cleanupErrors.Add('guest-job-cleanup-failed') }
        }
    }
    try {
        if ($nativeUnsettled -or -not $result.cleanup.vmRemoved) { throw 'disk-may-still-be-in-use' }
        if (Test-Path -LiteralPath $vhdPath) {
            Assert-OwnedFile $vhdPath
            $vhd = Get-VHD -Path $vhdPath
            if ($vhd.DiskIdentifier -ne $vhdIdentifier) { throw 'cleanup-vhd-identity-changed' }
            if ($vhd.Attached) {
                if (-not $hostVhdMountAttempted) { throw 'unexpected-vhd-attachment-during-cleanup' }
                Dismount-VHD -Path $vhdPath
            }
            if ((Get-VHD -Path $vhdPath).Attached) { throw 'cleanup-vhd-still-attached' }
        }
        $result.cleanup.vhdDetached = $true
    } catch { $cleanupErrors.Add('owned-vhd-detach-failed') }
    try {
        if ($nativeUnsettled) { throw 'native-operation-may-use-iso' }
        if ($isoMountAttempted) {
            Assert-OwnedFile $isoPath
            if ((Get-DiskImage -ImagePath $isoPath).Attached) { Dismount-DiskImage -ImagePath $isoPath }
            if ((Get-DiskImage -ImagePath $isoPath).Attached) { throw 'cleanup-iso-still-attached' }
        }
        $result.cleanup.isoDetached = $true
    } catch { $cleanupErrors.Add('owned-iso-detach-failed') }
    try {
        if ($nativeUnsettled -or -not $result.cleanup.vmRemoved -or -not $result.cleanup.vhdDetached -or -not $result.cleanup.isoDetached) { throw 'fixture-still-in-use' }
        if ($directoryCreated) {
            if ($directory -ne ('D:\sprig-guest-' + $runId)) { throw 'cleanup-directory-identity-changed' }
            Assert-NoReparse $directory
            if (@(Get-ChildItem -LiteralPath $directory -Recurse -Force | Where-Object { ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 }).Count -ne 0) { throw 'cleanup-reparse-point-refused' }
            # Remove only the newly owned directory, including secret-bearing VHD
            # and private setup/native logs, after all attachments are gone.
            Remove-Item -LiteralPath $directory -Recurse -Force
            if (Test-Path -LiteralPath $directory) { throw 'cleanup-directory-remains' }
        }
        $result.cleanup.directoryRemoved = $true
        $result.credentialHandling.fixtureRemoved = $true
    } catch { $cleanupErrors.Add('owned-fixture-removal-failed') }
    $answerXml = $null
    $password = $null
    $guestCredential = $null
    if ($null -ne $securePassword) { $securePassword.Dispose(); $securePassword = $null }
    $result.cleanup.errors = @($cleanupErrors.ToArray())
    $result.cleanup.status = $(if ($cleanupErrors.Count -eq 0) { 'passed' } else { 'failed' })
    $result['completedAtUtc'] = [DateTime]::UtcNow.ToString('o')
    if ($smokePassed -and $cleanupErrors.Count -eq 0) { $result.outcome = 'passed' }
    elseif ($cleanupErrors.Count -ne 0) { $result.outcome = 'cleanup-failed' }
    # Only these explicitly whitelisted JSON reports may be CI artifacts.
    Write-SafeResult
}
if ($result.outcome -ne 'passed') { throw 'Guest boot smoke failed; inspect the sanitized guest-boot-result.json phase and cleanup fields.' }
Write-Host 'Guest boot, PowerShell Direct, new-boot-time reboot, normal shutdown, and cleanup passed.'
