# Explicit build-only validation. Never installs, loads, or runs its outputs.
[CmdletBinding()]
param([switch]$AllowBuildOnly, [string]$GuestPayloadDirectory)
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
$result = [ordered]@{ scope='host-build-only'; outcome='failed'; phase='guard'; source=$url; expectedBytes=$expectedBytes; downloadLimit=$limit; outputLimit=[long](12GB); sha256=$null; mountedReadOnly=$false; scripts=@(); tools=@(); generatedProgramsExecuted=$false; driverLoaded=$false; cleanup='pending'; failure=$null }
$created=$false; $mountAttempted=$false

if (Test-Path -LiteralPath $reportPath) { throw 'Report already exists.' }
try {
    if ((Get-Volume -DriveLetter D).SizeRemaining -lt 40GB) { throw 'Insufficient build-only disk headroom.' }
    if ((Get-Item 'D:\').Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Reparse root refused.' }
    if (Test-Path -LiteralPath $root) { throw 'Fixture already exists.' }
    $null=New-Item -ItemType Directory -Path $root
    $created=$true
    $result.phase='download'
    Write-Host 'Downloading fixed official EWDK ISO (20,002,537,472 bytes; maximum 30 minutes).'
    & "$env:SystemRoot\System32\curl.exe" --fail --silent --show-error --location --max-redirs 2 --proto '=https' --proto-redir '=https' --max-filesize $limit --max-time 1800 --output $iso $url
    if ($LASTEXITCODE -ne 0) { throw "Official download failed with code $LASTEXITCODE." }
    $item=Get-Item -LiteralPath $iso
    if ($item.Length -ne $expectedBytes -or $item.Length -gt $limit -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Media size/type mismatch.' }
    $result.sha256=(Get-FileHash -LiteralPath $iso -Algorithm SHA256).Hash
    if ($result.sha256 -ne '9F48251DD24AD31AAC206D8256E95BDA5F90A9783982C45A8AAFEB9054562379') { throw 'Previously inspected official media hash changed.' }
    Write-Host 'ISO download and hash complete; mounting read-only.'
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
        $result.scripts+= [ordered]@{ path=$relative; sha256=(Get-FileHash -LiteralPath $path).Hash; bytes=$file.Length }
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
    $expectedScripts=@{
        'LaunchBuildEnv.cmd'='DA9E9F26DF10D22E98D2D4B1F5E98241CB526A95537DF55BF3A177F91DA1AF09'
        'BuildEnv\SetupBuildEnv.cmd'='6AD415932B630D12DFEC283EC472C8D1ED0B9C0AF462CD4EC52C076F517D2387'
    }
    foreach($entry in $result.scripts) { if($entry.sha256 -ne $expectedScripts[$entry.path]) { throw 'Reviewed environment entry script changed.' } }
    Write-Host 'Reviewed initialization and official tool signatures verified; beginning build-only validation.'
    $result.phase='compile'
    & (Join-Path $PSScriptRoot 'windows_ewdk_compile.ps1') -KitRoot $drive -WorkRoot $root -InspectGuestTools:([bool]$GuestPayloadDirectory)
    if ($GuestPayloadDirectory) {
        # Export only this successful build for the separately opted-in guest.
        # Never publish this directory as an artifact or execute its programs here.
        if ($GuestPayloadDirectory -notmatch '^D:\\sprig-kernel-payload-[0-9a-f]{32}$' -or (Test-Path -LiteralPath $GuestPayloadDirectory)) { throw 'Guest payload destination must be a new bounded fixture.' }
        $null=New-Item -ItemType Directory -Path $GuestPayloadDirectory
        $release=Join-Path $root 'winfsp-ddca7bd5481857a65ba552f643b8776fd070836f\build\VStudio\build\Release'
        $files=@{}
        foreach($name in @('winfsp-x64.sys','winfsp-x64.dll','memfs-x64.exe')) { $files[$name]=Join-Path $release $name }
        $files['freeze_probe.exe']=Join-Path $root 'harness\freeze_probe.exe'
        $files['signtool.exe']=Join-Path $drive 'Program Files\Windows Kits\10\bin\10.0.26100.0\x64\signtool.exe'
        $buildResult=Get-Content -LiteralPath (Join-Path $PWD 'ewdk-compile-result.json') -Raw | ConvertFrom-Json
        if(-not $buildResult.compiled -or $buildResult.executed) { throw 'Guest export requires a completed build-only result.' }
        $manifest=[ordered]@{sourceCommit='ddca7bd5481857a65ba552f643b8776fd070836f';experimentCommit=$env:GITHUB_SHA;files=@()}
        foreach($name in ($files.Keys | Sort-Object)) {
            $source=$files[$name]
            $item=Get-Item -LiteralPath $source
            if(($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -or $item.Length -gt 16MB) { throw 'Guest payload source type or bound invalid.' }
            if($name -eq 'signtool.exe') {
                $sig=Get-AuthenticodeSignature -LiteralPath $source
                if($sig.Status -ne 'Valid' -or $sig.SignerCertificate.Subject -notmatch '(^|, )O=Microsoft Corporation(,|$)') { throw 'Guest signing tool must retain valid Microsoft signature.' }
            }
            Copy-Item -LiteralPath $source -Destination (Join-Path $GuestPayloadDirectory $name)
            $hash=(Get-FileHash -LiteralPath $source).Hash
            if($name -ne 'signtool.exe') {
                $relative=$source.Substring($root.Length+1)
                $built=@($buildResult.outputs | Where-Object name -CEQ $relative)
                if($built.Count -ne 1 -or $built[0].sha256 -ne $hash -or $built[0].bytes -ne $item.Length) { throw 'Guest payload does not match completed build evidence.' }
            }
            if((Get-FileHash -LiteralPath (Join-Path $GuestPayloadDirectory $name)).Hash -ne $hash) { throw 'Guest payload copy hash mismatch.' }
            $manifest.files += [ordered]@{name=$name;bytes=$item.Length;sha256=$hash}
        }
        $manifest | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $GuestPayloadDirectory 'payload-manifest.json') -Encoding utf8
        $result['guestPayload']=$manifest
    }
    $result.outcome='build-only-passed'

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
if ($result.outcome -ne 'build-only-passed') { throw 'EWDK build-only validation failed; see result and bounded build logs.' }
Write-Host 'Baseline and experimental code compiled. No generated program executed or driver loaded.'
