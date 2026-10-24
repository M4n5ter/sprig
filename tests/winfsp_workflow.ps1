# Opt-in validation only on a disposable GitHub-hosted Windows runner.
param([switch] $AllowRuntimeInstallation)
$ErrorActionPreference = 'Stop'
if (-not $AllowRuntimeInstallation -or $env:GITHUB_ACTIONS -ne 'true' -or $env:GITHUB_EVENT_NAME -ne 'workflow_dispatch' -or $env:RUNNER_OS -ne 'Windows' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted') {
    throw 'WinFsp validation requires explicit opt-in on a disposable Windows runner.'
}
$root = (Resolve-Path -LiteralPath $env:RUNNER_TEMP).Path
if ($root -notmatch '^[A-Za-z]:\\') { throw 'Temporary root must be a local absolute path.' }
$ancestor = Get-Item -LiteralPath $root -Force
while ($null -ne $ancestor) {
    if (($ancestor.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'Unsafe temporary root.' }
    $ancestor = $ancestor.Parent
}
$directory = Join-Path $root ('sprig-winfsp-' + [Guid]::NewGuid().ToString('N'))
if (Test-Path -LiteralPath $directory) { throw 'Validation directory already exists.' }
[void][IO.Directory]::CreateDirectory($directory)
$package = Join-Path $directory 'winfsp-2.1.25156.msi'
$log = Join-Path $directory 'install.log'
$result = [ordered]@{ version = '2.1.25156'; downloaded = $false; signature = $null; installStarted = $false; installExitCode = $null; testExitCode = $null }
try {
    if ((Test-Path 'HKLM:\SOFTWARE\WOW6432Node\WinFsp') -or (Test-Path 'HKLM:\SOFTWARE\WinFsp') -or (Get-Service -Name 'WinFsp.Launcher' -ErrorAction SilentlyContinue)) {
        throw 'An existing WinFsp installation is present; do not upgrade or replace it.'
    }
    Invoke-WebRequest -Uri 'https://github.com/winfsp/winfsp/releases/download/v2.1/winfsp-2.1.25156.msi' -OutFile $package
    $result.downloaded = $true
    $hash = (Get-FileHash -LiteralPath $package -Algorithm SHA256).Hash
    $result.sha256 = $hash
    if ($hash -ne '073A70E00F77423E34BED98B86E600DEF93393BA5822204FAC57A29324DB9F7A') { throw 'WinFsp package checksum mismatch.' }
    $signature = Get-AuthenticodeSignature -LiteralPath $package
    $result.signature = [ordered]@{ status = [string]$signature.Status; subject = $signature.SignerCertificate.Subject; thumbprint = $signature.SignerCertificate.Thumbprint }
    if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch '(^|,\s*)CN=Navimatics LLC(,|$)') {
        throw 'WinFsp package signature or publisher does not match the approved runtime.'
    }
    $result.installStarted = $true
    $result | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $directory 'result.json')
    $installer = Start-Process -FilePath "$env:SystemRoot\System32\msiexec.exe" -ArgumentList @('/i', "`"$package`"", '/qn', '/norestart', 'ADDLOCAL=F.Main,F.User,F.Developer', '/l*v', "`"$log`"") -Wait -PassThru
    $result.installExitCode = $installer.ExitCode
    if ($installer.ExitCode -ne 0) { throw "WinFsp install exited $($installer.ExitCode); no retry or reboot." }
    $env:SPRIG_TEST_WINDOWS_MOUNT = '1'
    $env:SPRIG_WINDOWS_MOUNT_RESULTS = $directory
    & cargo clippy --locked --all-targets --features windows-mount -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw 'Optional Windows adapter Clippy failed.' }
    & cargo test --locked --features windows-mount --lib writable_windows::tests -- --nocapture
    if ($LASTEXITCODE -ne 0) { throw 'Windows callback component contract failed.' }
    & cargo test --locked --features windows-mount --test windows_mount -- --nocapture
    $result.testExitCode = $LASTEXITCODE
    if ($LASTEXITCODE -ne 0) { throw 'Optional Windows mount runtime contract failed.' }
} catch {
    $result.error = $_.Exception.Message
    throw
} finally {
    # No uninstall or forced unmount: the runner itself is disposable. The test
    # reports normal mount cleanup, retaining failed generations for diagnosis.
    $result | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $directory 'result.json')
    $result | ConvertTo-Json -Depth 6 | Write-Output
}
