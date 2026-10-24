# Read-only capability discovery. No feature installation, VM creation, service
# changes, BCD writes, driver loading, network creation, or reboot.
$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted') {
    throw 'This diagnostic is limited to a disposable GitHub-hosted runner.'
}
$result = [ordered]@{
    image = "$env:ImageOS $env:ImageVersion"
    os = [Environment]::OSVersion.VersionString
    powershell = $PSVersionTable.PSVersion.ToString()
    observations = [ordered]@{}
    scope = 'read-only; no configuration, VM, driver, or service changes'
}
function Observe($Name, [scriptblock]$Read) {
    try { $result.observations[$Name] = @{ ok = $true; value = @(& $Read) } }
    catch { $result.observations[$Name] = @{ ok = $false; error = $_.Exception.Message } }
}
Observe 'computer' { Get-CimInstance Win32_ComputerSystem | Select-Object Manufacturer, Model, HypervisorPresent, TotalPhysicalMemory, NumberOfLogicalProcessors }
Observe 'cpu' { Get-CimInstance Win32_Processor | Select-Object Name, NumberOfCores, NumberOfLogicalProcessors, VirtualizationFirmwareEnabled, SecondLevelAddressTranslationExtensions, VMMonitorModeExtensions }
Observe 'hyperv_features' { Get-WindowsOptionalFeature -Online | Where-Object FeatureName -Match 'Hyper-V|HypervisorPlatform|VirtualMachinePlatform' | Select-Object FeatureName, State }
Observe 'services' { Get-Service -Name vmcompute,vmms -ErrorAction Stop | Select-Object Name, Status, StartType }
Observe 'cmdlets' { 'Get-VM','New-VM','Get-VMHost','Get-VMSwitch','Get-WindowsOptionalFeature','Get-WinEvent','Get-AuthenticodeSignature' | ForEach-Object { @{ name = $_; present = [bool](Get-Command $_ -ErrorAction SilentlyContinue) } } }
Observe 'vmhost' { if (Get-Command Get-VMHost -ErrorAction SilentlyContinue) { Get-VMHost | Select-Object LogicalProcessorCount, MemoryCapacity, VirtualMachineMigrationEnabled, NumaSpanningEnabled } else { 'Hyper-V management cmdlet unavailable' } }
Observe 'volumes' { Get-Volume | Select-Object DriveLetter, FileSystem, Size, SizeRemaining, HealthStatus }
Observe 'security_state' { Get-CimInstance -Namespace root/Microsoft/Windows/DeviceGuard -ClassName Win32_DeviceGuard | Select-Object VirtualizationBasedSecurityStatus, SecurityServicesConfigured, SecurityServicesRunning }
Observe 'secure_boot' { Confirm-SecureBootUEFI }
Observe 'crash_dump_configuration' { Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\CrashControl' | Select-Object CrashDumpEnabled, DumpFile, MinidumpDir, AlwaysKeepMemoryDump, LogEvent, AutoReboot }
Observe 'paging_configuration' { Get-CimInstance Win32_PageFileUsage | Select-Object Name, AllocatedBaseSize, CurrentUsage, PeakUsage }
Observe 'diagnostic_commands' { 'verifier.exe','dumpchk.exe','kd.exe','windbg.exe' | ForEach-Object { @{ name = $_; present = [bool](Get-Command $_ -ErrorAction SilentlyContinue) } } }
Observe 'event_logs' { Get-WinEvent -ListLog System | Select-Object LogName, IsEnabled, RecordCount, MaximumSizeInBytes }
$json = $result | ConvertTo-Json -Depth 8
$json | Set-Content -LiteralPath runner-capabilities.json -Encoding utf8
Write-Output $json
