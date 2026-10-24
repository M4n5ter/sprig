# Executed only inside the new, explicitly approved, disposable no-NIC guest.
# Never invoke on the host. No raw process output, dumps, keys or credentials leave it.
[CmdletBinding()]
param([Parameter(Mandatory)][ValidateSet('Inspect','Prepare','Test','Verifier')][string]$Mode)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$probeStage = 'validate-payload'
$nativeStatus = $null
try {
$root = 'C:\SprigProbe'
$expectedSource = 'ddca7bd5481857a65ba552f643b8776fd070836f'
$names = @('winfsp-x64.sys','winfsp-x64.dll','memfs-x64.exe','freeze_probe.exe','signtool.exe')
if ($env:COMPUTERNAME -notmatch '^SPRIG-[A-F0-9]{8}$' -or @(Get-NetAdapter -ErrorAction Stop).Count -ne 0) { throw 'probe-guest-identity-invalid' }
if ((Get-CimInstance Win32_OperatingSystem).BuildNumber -ne '26100') { throw 'probe-guest-version-invalid' }
foreach ($path in @($root, "$root\payload-manifest.json", "$root\windows_guest_probe_payload.ps1")) {
    if (((Get-Item -LiteralPath $path -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'probe-reparse-refused' }
}
$manifest = Get-Content -LiteralPath "$root\payload-manifest.json" -Raw | ConvertFrom-Json
if ($manifest.sourceCommit -ne $expectedSource -or $manifest.experimentCommit -notmatch '^[0-9a-f]{40}$' -or @($manifest.files).Count -ne $names.Count) { throw 'probe-manifest-invalid' }
foreach ($name in $names) {
    $entries = @($manifest.files | Where-Object name -CEQ $name)
    if ($entries.Count -ne 1) { throw 'probe-manifest-entry-invalid' }
    $item = Get-Item -LiteralPath "$root\$name" -Force
    if ($item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'probe-payload-invalid' }
    # Signing changes only SYS; validate its original hash during Prepare, then
    # bind subsequent loads to the exact persisted signed hash below.
    if ($name -eq 'winfsp-x64.sys' -and $Mode -ne 'Prepare' -and (Test-Path "$root\signed-driver.json")) {
        $signed = Get-Content "$root\signed-driver.json" -Raw | ConvertFrom-Json
        if ($item.Length -ne $signed.bytes -or (Get-FileHash "$root\$name").Hash -ne $signed.sha256) { throw 'probe-signed-driver-changed' }
    } elseif ($item.Length -ne $entries[0].bytes -or (Get-FileHash "$root\$name").Hash -ne $entries[0].sha256) { throw 'probe-payload-hash-invalid' }
}
function Native([string]$Exe, [string[]]$Arguments, [int]$Seconds = 30) {
    $script:nativeStatus = [ordered]@{program=[IO.Path]::GetFileName($Exe);exitCode=$null;completed=$false}
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $Exe; $start.Arguments = ($Arguments -join ' ')
    $start.UseShellExecute = $false; $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true; $start.RedirectStandardError = $true
    $p = [Diagnostics.Process]::Start($start)
    $out = $p.StandardOutput.ReadToEndAsync(); $err = $p.StandardError.ReadToEndAsync()
    if (-not $p.WaitForExit($Seconds * 1000)) {
        # Do not return from this remote runspace and thereby terminate children
        # retaining mapped views / an in-flight kernel request. The outer owner
        # times out this job, retains it, and powers off only this disposable VM.
        while ($true) { Start-Sleep -Seconds 10 }
    }
    $text = $out.GetAwaiter().GetResult(); $null = $err.GetAwaiter().GetResult()
    $code = $p.ExitCode; $p.Dispose()
    $script:nativeStatus.exitCode=$code; $script:nativeStatus.completed=$true
    return [pscustomobject]@{code=$code;text=$text}
}
function Checked([string]$Exe, [string[]]$Arguments) {
    $r = Native $Exe $Arguments
    if ($r.code -ne 0) { throw 'probe-native-command-failed' }
    return $r.text
}
function VerifierSettings {
    $m = Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management'
    $flags = 0; $drivers = ''
    if ($null -ne $m.PSObject.Properties['VerifyDriverLevel']) { $flags = [uint32]$m.VerifyDriverLevel }
    if ($null -ne $m.PSObject.Properties['VerifyDrivers']) { $drivers = [string]$m.VerifyDrivers }
    [ordered]@{flags=$flags;drivers=$drivers}
}
function SafeTrial($t) {
    if ($t.reason -notmatch '^[a-z0-9_]{1,80}$') { throw 'probe-reason-invalid' }
    $v = [ordered]@{reason=[string]$t.reason; win32_error=[uint32]$t.win32_error; mode=[uint32]$t.mode; unfrozen_b_acks=[uint32]$t.unfrozen_b_acks}
    foreach ($k in @('ran','observation_completed','timing_valid','heartbeats_valid','a_overlap_reuses_pending_attempt','writers_recovered','negative_control_valid','no_in_hold_ack_observed','falsified_in_this_trial','attempt_windows_valid','continuous_store_samples_valid','continuous_stores_recovered','continuous_store_counterexample','a_pending_ack_at_or_after_end','b_pending_ack_at_or_after_end')) { $v[$k]=[bool]$t.$k }
    foreach ($k in @('a_first','a_overlap','b_overlap')) { $v[$k]=[ordered]@{attempt_started=[bool]$t.$k.attempt_started;acknowledged=[bool]$t.$k.acknowledged} }
    foreach ($k in @('a_ioctl','b_ioctl')) {
        $io=$t.$k
        $v[$k]=[ordered]@{win32_error=[uint32]$io.win32_error;returned_bytes=[uint32]$io.returned_bytes;phase=[uint32]$io.phase;result_status=[int]$io.result_status;flush_status=[int]$io.flush_status;wait_status=[int]$io.wait_status}
        foreach ($b in @('submitted','ready_seen','joined','transport_success','envelope_valid','completed_hold')) { $v[$k][$b]=[bool]$io.$b }
    }
    return $v
}
function Protection {
    $d = Get-CimInstance -Namespace root\Microsoft\Windows\DeviceGuard -ClassName Win32_DeviceGuard -ErrorAction Stop
    [ordered]@{ vbsStatus=[int]$d.VirtualizationBasedSecurityStatus; configured=@($d.SecurityServicesConfigured | Sort-Object); running=@($d.SecurityServicesRunning | Sort-Object) }
}
function CheckService([string]$Name, [string]$Path) {
    $s = @(Get-CimInstance Win32_SystemDriver -Filter "Name='$Name'")
    if ($s.Count -ne 1 -or $s[0].PathName -notin @($Path, ('\??\' + $Path))) { throw 'probe-driver-service-identity-invalid' }
    return $s[0]
}
function StopOwnServices {
    # Only after the harness has exited with all handles/threads recovered.
    $null = Checked "$env:SystemRoot\System32\sc.exe" @('stop','memfs')
    $end = [DateTime]::UtcNow.AddSeconds(30)
    while ((Get-Service memfs).Status -ne 'Stopped' -and [DateTime]::UtcNow -lt $end) { Start-Sleep -Milliseconds 200 }
    if ((Get-Service memfs).Status -ne 'Stopped') { throw 'probe-memfs-stop-unknown' }
    $null = CheckService 'WinFsp' "$root\winfsp-x64.sys"
    # Do not use SCM stop as WinFsp's privileged custom unload protocol.
    # The demand-start driver remains loaded until normal guest reboot/shutdown.
}
switch ($Mode) {
    'Inspect' {
        $probeStage='inspect-protection'
        [pscustomobject]@{protection=(Protection); driverServices=@(Get-CimInstance Win32_SystemDriver | Where-Object Name -Match '^WinFsp').Count; secureBoot=[bool](Confirm-SecureBootUEFI);verifier=(VerifierSettings)}
    }
    'Prepare' {
        $probeStage='prepare-signature'
        if (Confirm-SecureBootUEFI) { throw 'probe-guest-secureboot-still-enabled' }
        if (@(Get-CimInstance Win32_SystemDriver | Where-Object Name -Match '^WinFsp').Count -ne 0 -or (Get-Service memfs -ErrorAction SilentlyContinue)) { throw 'probe-existing-service-refused' }
        $existingVerifier=VerifierSettings
        if ($existingVerifier.flags -ne 0 -or $existingVerifier.drivers -ne '') { throw 'probe-preexisting-verifier-refused' }
        $toolSignature = Get-AuthenticodeSignature "$root\signtool.exe"
        if ($toolSignature.Status -ne 'Valid' -or $toolSignature.SignerCertificate.Subject -notmatch 'Microsoft') { throw 'probe-signing-tool-untrusted' }
        $probeStage='preflight-signing-tool'
        $null = Checked "$root\signtool.exe" @('/?')
        $probeStage='create-guest-certificate'
        $certificate = New-SelfSignedCertificate -Type CodeSigningCert -Subject 'CN=Sprig Disposable Kernel Probe' -CertStoreLocation Cert:\LocalMachine\My -KeyAlgorithm RSA -KeyLength 2048 -HashAlgorithm SHA256 -KeyExportPolicy NonExportable -NotBefore (Get-Date) -NotAfter (Get-Date).AddHours(72)
        foreach ($storeName in @('Root','TrustedPublisher')) {
            $store = [Security.Cryptography.X509Certificates.X509Store]::new($storeName, 'LocalMachine')
            try { $store.Open('ReadWrite'); $store.Add([Security.Cryptography.X509Certificates.X509Certificate2]::new($certificate.RawData)) } finally { $store.Close() }
        }
        $probeStage='sign-guest-driver'
        $null = Checked "$root\signtool.exe" @('sign','/sm','/s','My','/sha1',$certificate.Thumbprint,'/fd','SHA256',"$root\winfsp-x64.sys")
        $signature = Get-AuthenticodeSignature "$root\winfsp-x64.sys"
        if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Thumbprint -ne $certificate.Thumbprint) { throw 'probe-driver-signature-invalid' }
        $signed = [ordered]@{sha256=(Get-FileHash "$root\winfsp-x64.sys").Hash; certificateThumbprint=$certificate.Thumbprint; notAfterUtc=$certificate.NotAfter.ToUniversalTime().ToString('o'); bytes=(Get-Item "$root\winfsp-x64.sys").Length}
        $signed | ConvertTo-Json | Set-Content "$root\signed-driver.json" -Encoding UTF8
        $probeStage='enable-guest-testsigning'
        $null = Checked "$env:SystemRoot\System32\bcdedit.exe" @('/set','testsigning','on')
        [pscustomobject]@{signedDriver=$signed; testSigningRequested=$true; certificatePrivateKeyExported=$false}
    }
    'Test' {
        $probeStage='validate-test-boot'
        $bcd = Checked "$env:SystemRoot\System32\bcdedit.exe" @('/enum','{current}')
        if ($bcd -notmatch '(?im)^testsigning\s+Yes\s*$' -or (Confirm-SecureBootUEFI)) { throw 'probe-test-signing-not-active' }
        $signature = Get-AuthenticodeSignature "$root\winfsp-x64.sys"
        $signed = Get-Content "$root\signed-driver.json" -Raw | ConvertFrom-Json
        if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Thumbprint -ne $signed.certificateThumbprint) { throw 'probe-signed-driver-invalid' }
        if (@(Get-CimInstance Win32_SystemDriver -Filter "Name='WinFsp'").Count -eq 0) {
            if (@(Get-CimInstance Win32_SystemDriver | Where-Object Name -Match '^WinFsp').Count -ne 0 -or (Get-Service memfs -ErrorAction SilentlyContinue)) { throw 'probe-existing-service-refused' }
            $probeStage='create-guest-services'
            $null = Checked "$env:SystemRoot\System32\sc.exe" @('create','WinFsp','type=','filesys','start=','demand','binPath=',"$root\winfsp-x64.sys")
            $binary = '"' + "$root\memfs-x64.exe" + '"'
            New-Service -Name memfs -BinaryPathName $binary -StartupType Manual | Out-Null
        }
        $null = CheckService 'WinFsp' "$root\winfsp-x64.sys"
        $mem = Get-CimInstance Win32_Service -Filter "Name='memfs'"
        if ($mem.PathName -ne ('"' + "$root\memfs-x64.exe" + '"')) { throw 'probe-memfs-service-identity-invalid' }
        if (Test-Path 'R:\') { throw 'probe-existing-mount-refused' }
        $probeStage='start-experimental-driver'
        $null = Checked "$env:SystemRoot\System32\sc.exe" @('start','WinFsp')
        $verifierActive=$false; $verifierQuerySha256=$null
        if (Test-Path "$root\verifier-configured.json") {
            $q=Checked "$env:SystemRoot\System32\verifier.exe" @('/query')
            if ($q -notmatch '(?i)winfsp-x64\.sys' -or $q -notmatch '(?i)0x0*3b\b') { throw 'probe-verifier-not-observed' }
            $verifierActive=$true
            $verifierQuerySha256=([BitConverter]::ToString([Security.Cryptography.SHA256]::Create().ComputeHash([Text.Encoding]::UTF8.GetBytes($q))).Replace('-',''))
        } else {
            $v=VerifierSettings
            if ($v.flags -ne 0 -or $v.drivers -ne '') { throw 'probe-unexpected-verifier' }
        }
        $probeStage='start-ram-memfs'
        $null = Checked "$env:SystemRoot\System32\sc.exe" @('start','memfs','-F','-SPRIGPROBE','-m','R:','-n','4','-s','65536')
        $end = [DateTime]::UtcNow.AddSeconds(30)
        while (-not (Test-Path 'R:\') -and [DateTime]::UtcNow -lt $end) { Start-Sleep -Milliseconds 200 }
        if (-not (Test-Path 'R:\')) { throw 'probe-mount-not-ready' }
        $probeStage='run-mapping-harness'
        $h = Native "$root\freeze_probe.exe" @('--allow-experiment','R:\') 90
        if ($h.text.Length -gt 65536) { throw 'probe-output-oversize' }
        $j = $h.text | ConvertFrom-Json
        if ($j.outcome -notin @('no_in_hold_ack_observed','falsified','inconclusive') -or $j.retention_required -or -not $j.owned_fixture_cleanup_completed) { throw 'probe-uncertain-result-retain-guest' }
        # Reconstruct source-defined diagnostic fields only, never arbitrary stdout.
        if ($j.schema -ne 'sprig.mapping-freeze.v1' -or $j.architecture -ne 'x64' -or $j.reason -notmatch '^[a-z0-9_]{1,80}$' -or $j.root_validation_call -notin @('not_started','exact_drive_root','open_root_handle','root_handle_metadata','root_volume_information_by_handle','root_volume_binding','passed') -or
            ($j.outcome -eq 'no_in_hold_ack_observed' -and $h.code -ne 0) -or
            ($j.outcome -eq 'falsified' -and $h.code -ne 1) -or
            ($j.outcome -eq 'inconclusive' -and $h.code -ne 2)) { throw 'probe-result-envelope-invalid' }
        $safe = [ordered]@{ outcome=[string]$j.outcome;reason=[string]$j.reason;rootValidationCall=[string]$j.root_validation_call;win32Error=[uint32]$j.win32_error; exitCode=[int]$h.code; retentionRequired=[bool]$j.retention_required; fixtureCleanup=[bool]$j.owned_fixture_cleanup_completed; wholeTreeSnapshotVerified=$false; backingStoreSnapshotVerified=$false; negativeControlValid=[bool]$j.negative.negative_control_valid; negative=(SafeTrial $j.negative);coherency=(SafeTrial $j.coherency);verifierActive=$verifierActive;verifierQuerySha256=$verifierQuerySha256 }
        $probeStage='normal-stop-own-services'
        StopOwnServices
        $safe['normalMemfsStop']=$true
        $safe['driverRetainedUntilGuestPowerTransition']=$true
        [pscustomobject]$safe
    }
    'Verifier' {
        $probeStage='configure-oneboot-verifier'
        $null = CheckService 'WinFsp' "$root\winfsp-x64.sys"
        if ((Get-Service memfs).Status -ne 'Stopped') { throw 'probe-services-not-stopped' }
        $null = Checked "$env:SystemRoot\System32\verifier.exe" @('/flags','0x3B','/driver','winfsp-x64.sys')
        $null = Checked "$env:SystemRoot\System32\verifier.exe" @('/bootmode','oneboot')
        $mm=Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management'
        if ([string]$mm.VerifyDrivers -ne 'winfsp-x64.sys' -or [uint32]$mm.VerifyDriverLevel -ne 0x3B) { throw 'probe-verifier-settings-invalid' }
        [IO.File]::WriteAllText("$root\verifier-configured.json", '{"mask":59,"driver":"winfsp-x64.sys","bootMode":"oneboot"}')
        [pscustomobject]@{mask=59; driver='winfsp-x64.sys'; bootMode='oneboot'}
    }
}
} catch {
    $code='probe-operation-failed'
    if ($_.Exception.Message -match '^probe-[a-z-]{1,80}$') { $code=$_.Exception.Message }
    [pscustomobject]@{probeFailure=[ordered]@{stage=$probeStage;code=$code;hresult=$_.Exception.HResult;native=$nativeStatus}}
}
