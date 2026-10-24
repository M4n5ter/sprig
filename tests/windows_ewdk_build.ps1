# Explicit build-only tool inspection. Never installs or loads a driver.
[CmdletBinding()]
param([switch]$AllowBuildOnly)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
if (-not $AllowBuildOnly -or $env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:RUNNER_OS -ne 'Windows' -or $env:GITHUB_EVENT_NAME -ne 'workflow_dispatch') { throw 'Explicit disposable Windows CI build-only opt-in required.' }
$root = 'D:\sprig-ewdk-' + [Guid]::NewGuid().ToString('N')
$iso = Join-Path $root 'ewdk.iso'
$url = 'https://download.microsoft.com/download/be985897-caaa-4497-9ea4-17fa7065cd8a/EWDK_ge_release_svc_prod1_26100_250904-1728.iso'
$expectedBytes = [long]20002537472
$limit = [long](24GB)
$reportPath = Join-Path $PWD 'ewdk-result.json'
$result = [ordered]@{ outcome='failed'; phase='guard'; source=$url; expectedBytes=$expectedBytes; downloadLimit=$limit; outputLimit=[long](12GB); sha256=$null; mountedReadOnly=$false; scripts=@(); tools=@(); generatedProgramsExecuted=$false; driverLoaded=$false; cleanup='pending'; failure=$null }
$created=$false; $mountAttempted=$false
$privateScripts=@()
if (Test-Path -LiteralPath $reportPath) { throw 'Report already exists.' }
try {
    if ((Get-Volume -DriveLetter D).SizeRemaining -lt 40GB) { throw 'Insufficient build-only disk headroom.' }
    if ((Get-Item 'D:\').Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Reparse root refused.' }
    if (Test-Path -LiteralPath $root) { throw 'Fixture already exists.' }
    $null=New-Item -ItemType Directory -Path $root
    $created=$true
    $result.phase='download'
    & "$env:SystemRoot\System32\curl.exe" --fail --silent --show-error --location --max-redirs 2 --proto '=https' --proto-redir '=https' --max-filesize $limit --max-time 1800 --output $iso $url
    if ($LASTEXITCODE -ne 0) { throw "Official download failed with code $LASTEXITCODE." }
    $item=Get-Item -LiteralPath $iso
    if ($item.Length -ne $expectedBytes -or $item.Length -gt $limit -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Media size/type mismatch.' }
    $result.sha256=(Get-FileHash -LiteralPath $iso -Algorithm SHA256).Hash
    $result.phase='read-only-mount'
    $mountAttempted=$true
    $null=Mount-DiskImage -ImagePath $iso -StorageType ISO -Access ReadOnly -PassThru
    $volumes=@(Get-DiskImage -ImagePath $iso | Get-Volume)
    if ($volumes.Count -ne 1 -or -not $volumes[0].DriveLetter) { throw 'ISO mapping not unique.' }
    $drive="$($volumes[0].DriveLetter):\"
    $result.mountedReadOnly=$true
    $result.phase='inspect-official-environment'
    $result['rootFiles']=@(Get-ChildItem -LiteralPath $drive -File | Select-Object -ExpandProperty Name)
    $versionPath=Join-Path $drive 'Version.txt'
    if (Test-Path -LiteralPath $versionPath -PathType Leaf) {
        if ((Get-Item -LiteralPath $versionPath).Length -gt 16KB) { throw 'Unexpected version file size.' }
        $result['kitVersionText']=Get-Content -LiteralPath $versionPath -Raw
    }
    # Only these two environment entry scripts are inspected; no tool is executed.
    foreach ($relative in @('LaunchBuildEnv.cmd','BuildEnv\SetupBuildEnv.cmd')) {
        $path=Join-Path $drive $relative
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw 'Expected environment script absent.' }
        $file=Get-Item -LiteralPath $path
        if ($file.Length -gt 128KB) { throw 'Unexpected environment script size.' }
        $contents=Get-Content -LiteralPath $path -Raw
        $result.scripts+= [ordered]@{ path=$relative; sha256=(Get-FileHash -LiteralPath $path).Hash; bytes=$file.Length }
        $privateScripts+= [ordered]@{ path=$relative; contents=$contents }
    }
    $candidates=@(Get-ChildItem -LiteralPath $drive -Filter MSBuild.exe -Recurse -File) + @(Get-ChildItem -LiteralPath $drive -Filter cl.exe -Recurse -File) + @(Get-ChildItem -LiteralPath $drive -Filter signtool.exe -Recurse -File)
    foreach ($tool in $candidates) {
        $sig=Get-AuthenticodeSignature -LiteralPath $tool.FullName
        $result.tools+= [ordered]@{ path=$tool.FullName.Substring($drive.Length); version=$tool.VersionInfo.FileVersion; sha256=(Get-FileHash -LiteralPath $tool.FullName).Hash; signatureStatus=[string]$sig.Status; signer=$(if($sig.SignerCertificate){$sig.SignerCertificate.Subject}else{$null}) }
    }
    foreach ($name in @('MSBuild.exe','cl.exe','signtool.exe')) {
        $matches=@($result.tools | Where-Object { [IO.Path]::GetFileName($_.path) -eq $name })
        if (-not $matches.Count) { throw 'Expected tool class absent.' }
        if (@($matches | Where-Object { $_.signatureStatus -ne 'Valid' -or $_.signer -notmatch '(^|, )O=Microsoft Corporation(,|$)' }).Count) { throw 'Tool signature or Microsoft publisher verification failed.' }
    }
    # Do not redistribute Microsoft kit source through public CI artifacts.
    # Only a local, never-uploaded inspection private key can decrypt this envelope.
    $rsa=[Security.Cryptography.RSA]::Create()
    $rsa.ImportFromPem((Get-Content -LiteralPath (Join-Path $PSScriptRoot 'ewdk-inspection-public.pem') -Raw))
    $key=[Security.Cryptography.RandomNumberGenerator]::GetBytes(32)
    $nonce=[Security.Cryptography.RandomNumberGenerator]::GetBytes(12)
    $plaintext=[Text.Encoding]::UTF8.GetBytes(($privateScripts | ConvertTo-Json -Depth 5 -Compress))
    $ciphertext=[byte[]]::new($plaintext.Length)
    $tag=[byte[]]::new(16)
    $aes=[Security.Cryptography.AesGcm]::new($key,16)
    try {
        $aes.Encrypt($nonce,$plaintext,$ciphertext,$tag)
        $result['encryptedScripts']=[ordered]@{
            algorithm='RSA-OAEP-SHA256+A256GCM'
            wrappedKey=[Convert]::ToBase64String($rsa.Encrypt($key,[Security.Cryptography.RSAEncryptionPadding]::OaepSHA256))
            nonce=[Convert]::ToBase64String($nonce); tag=[Convert]::ToBase64String($tag)
            ciphertext=[Convert]::ToBase64String($ciphertext)
        }
    } finally { $aes.Dispose(); $rsa.Dispose(); [Array]::Clear($key); [Array]::Clear($plaintext) }
    $privateScripts=$null
    $result.outcome='inspection-passed-build-pending' 
} catch {
    $result.failure=[ordered]@{ type=$_.Exception.GetType().FullName; message=$_.Exception.Message; hresult=$_.Exception.HResult }
} finally {
    try {
        if ($mountAttempted -and (Get-DiskImage -ImagePath $iso).Attached) { Dismount-DiskImage -ImagePath $iso }
        if ($mountAttempted -and (Get-DiskImage -ImagePath $iso).Attached) { throw 'ISO still attached.' }
        if ($created) {
            if (-not $root.StartsWith('D:\sprig-ewdk-') -or (Get-Item -LiteralPath $root).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Cleanup identity check failed.' }
            # Only the newly generated, re-downloadable media fixture is removed.
            Remove-Item -LiteralPath $root -Recurse -Force
        }
        $result.cleanup='passed'
    } catch { $result.cleanup='failed'; $result.outcome='failed'; $result['cleanupFailure']=$_.Exception.Message }
    $result | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $reportPath -Encoding utf8
}
if ($result.outcome -ne 'inspection-passed-build-pending') { throw 'EWDK inspection failed; see ewdk-result.json.' }
Write-Host 'Official environment inspected, no generated program executed; compilation awaits exact environment review.'
