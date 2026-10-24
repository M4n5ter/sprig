# Only invoked after the parent has verified and read-only mounted the fixed kit.
[CmdletBinding()]
param([Parameter(Mandatory)][string]$KitRoot,[Parameter(Mandatory)][string]$WorkRoot)
Set-StrictMode -Version Latest
$ErrorActionPreference='Stop'
$commit='ddca7bd5481857a65ba552f643b8776fd070836f'
$source=Join-Path $WorkRoot ('winfsp-'+$commit)
$archive=Join-Path $WorkRoot 'winfsp-source.tar.gz'
$package=Join-Path $PSScriptRoot '..\experiments\windows-freeze-probe'
$logs=Join-Path $PWD 'ewdk-logs'
if (Test-Path $logs) { throw 'Build log destination already exists.' }
$null=New-Item -ItemType Directory $logs
$result=[ordered]@{ sourceCommit=$commit; sourceSha256='399554574e1741bf3852254b98b763b107a81ea7a646cdf0cec7f98cac044173'; steps=@(); outputs=@(); compiled=$false; executed=$false; peakOutputBytes=0 }
$msbuild=Join-Path $KitRoot 'Program Files\Microsoft Visual Studio\2022\BuildTools\MSBuild\Current\Bin\amd64\MSBuild.exe'
$setup=Join-Path $KitRoot 'BuildEnv\SetupBuildEnv.cmd'
$compiler=Join-Path $KitRoot 'Program Files\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC\14.44.35207\bin\Hostx64\x64\cl.exe'
$solution=$source+'\build\VStudio\'
$empty=Join-Path $WorkRoot 'empty-user-props'
$null=New-Item -ItemType Directory $empty
function Output-Size {
    $bytes=[long]0
    foreach($f in Get-ChildItem -LiteralPath $WorkRoot -File -Recurse) { if($f.FullName -ne (Join-Path $WorkRoot 'ewdk.iso')) { $bytes+=$f.Length } }
    foreach($f in Get-ChildItem -LiteralPath $logs -File) { $bytes+=$f.Length }
    if($bytes -gt $result.peakOutputBytes){$result.peakOutputBytes=$bytes}
    return $bytes
}
function Run-BuildCommand([string]$Name,[string]$Command,[string]$ClExtra='') {
    Write-Host "Build-only phase: $Name"
    $batch=Join-Path $WorkRoot ($Name+'.cmd')
    $stdout=Join-Path $logs ($Name+'.stdout.txt');$stderr=Join-Path $logs ($Name+'.stderr.txt')
    $lines=@('@echo off','setlocal','set WindowsSystemKit=','set CL=','set _CL_=','set LINK=','set _LINK_=',('call "'+$setup+'" amd64'),'if errorlevel 1 exit /b 81',('set CL='+$ClExtra),('cd /d "'+$source+'"'),$Command,'exit /b %errorlevel%')
    $lines | Set-Content -LiteralPath $batch -Encoding ascii
    $process=Start-Process -FilePath "$env:SystemRoot\System32\cmd.exe" -ArgumentList @('/d','/s','/c',('""'+$batch+'""')) -PassThru -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    $watch=[Diagnostics.Stopwatch]::StartNew()
    while(-not $process.WaitForExit(1000)) {
        if((Output-Size) -gt 12GB -or $watch.Elapsed.TotalMinutes -gt 12) {
            # Only this invocation's compiler process tree; never a driver/service.
            & "$env:SystemRoot\System32\taskkill.exe" /PID $process.Id /T /F | Out-Null
            throw 'Build output or bounded compilation time limit exceeded.'
        }
    }
    $process.Refresh()
    $result.steps += [ordered]@{ name=$Name; command=$Command; compilerExtra=$ClExtra; exitCode=$process.ExitCode; seconds=$watch.Elapsed.TotalSeconds }
    if((Output-Size) -gt 12GB){throw 'Build output limit exceeded.'}
    if($process.ExitCode -ne 0){ throw "Build-only phase failed: $Name ($($process.ExitCode))." }
}
function Msbuild-Command([string]$Project,[string]$Target='Build') {
    return '"'+$msbuild+'" "'+(Join-Path $solution $Project)+'" /nologo /noAutoResponse /interactive:false /m:2 /nr:false /t:'+$Target+' /p:Configuration=Release /p:Platform=x64 /p:SignMode=Off /p:BuildProjectReferences=false /p:MyTargetPlatformVersion=10.0.26100.0 /p:MyNtddiVersion=0x0A000006 /p:MyWin32Version=0x0A00 /p:MyGitHead='+$commit+' /p:MyGitRevision=ddca7bd /p:ImportDirectoryBuildProps=false /p:ImportDirectoryBuildTargets=false /p:UserRootDir='+$empty+' /p:SolutionDir='+$solution+' /verbosity:minimal'
}
try {
    Write-Host 'Downloading pinned official WinFsp source (no executable is run).'
    & "$env:SystemRoot\System32\curl.exe" --fail --silent --show-error --location --max-redirs 2 --proto '=https' --proto-redir '=https' --max-filesize 67108864 --max-time 300 --output $archive ('https://codeload.github.com/winfsp/winfsp/tar.gz/'+$commit)
    if($LASTEXITCODE -ne 0 -or (Get-FileHash $archive).Hash.ToLowerInvariant() -ne $result.sourceSha256){throw 'Pinned source download/hash failed.'}
    & "$env:SystemRoot\System32\tar.exe" -xzf $archive -C $WorkRoot
    if($LASTEXITCODE -ne 0 -or -not(Test-Path $source -PathType Container)){throw 'Source extraction failed.'}
    # Evaluation only: /pp and /getProperty never execute a target. Inspect the
    # actual imported signing tasks before any build, and fail closed on unknowns.
    foreach($project in @('winfsp_sys.vcxproj','winfsp_dll.vcxproj','testing\memfs.vcxproj','testing\winfsp-tests.vcxproj')) {
        $name=[IO.Path]::GetFileNameWithoutExtension($project)
        $base=(Msbuild-Command $project).Replace(' /t:Build','')
        $pp=Join-Path $WorkRoot ($name+'.pp.xml')
        Run-BuildCommand ($name+'-preprocess') ($base+' /pp:"'+$pp+'"')
        Run-BuildCommand ($name+'-properties') ($base+' /getProperty:SignMode,EnterpriseWDK,DisableRegistryUse,WindowsTargetPlatformVersion,VCTargetsPath')
        $props=Get-Content (Join-Path $logs ($name+'-properties.stdout.txt')) -Raw
        if($props -notmatch '"SignMode"\s*:\s*"Off"' -or $props -notmatch '"EnterpriseWDK"\s*:\s*"(?i:true)"' -or $props -notmatch '"DisableRegistryUse"\s*:\s*"(?i:true)"'){throw 'Evaluated isolated kit/signing policy mismatch.'}
        & python (Join-Path $PSScriptRoot 'check_unsigned_build.py') $pp (Join-Path $logs ($name+'-signing-guard.txt'))
        if($LASTEXITCODE -ne 0){throw 'Signing preflight refused; no Build executed.'}
    }
    Run-BuildCommand 'baseline-sys' (Msbuild-Command 'winfsp_sys.vcxproj')
    $sys=Join-Path $solution 'build\Release\winfsp-x64.sys'
    if(-not(Test-Path $sys)){throw 'Baseline SYS missing.'}
    $result.outputs += [ordered]@{name='baseline-sys';bytes=(Get-Item $sys).Length;sha256=(Get-FileHash $sys).Hash}
    foreach($project in @('winfsp_dll.vcxproj','testing\memfs.vcxproj','testing\winfsp-tests.vcxproj')) {
        Run-BuildCommand ([IO.Path]::GetFileNameWithoutExtension($project)) (Msbuild-Command $project)
    }
    & python (Join-Path $package 'apply_probe_patch.py') --source $source
    if($LASTEXITCODE -ne 0){throw 'Exact experimental source preparation failed.'}
    Run-BuildCommand 'experimental-sys' (Msbuild-Command 'winfsp_sys.vcxproj' 'Rebuild') '/DFSP_TEST_FREEZE_PROBE=1'
    $harness=Join-Path $WorkRoot 'harness'
    $null=New-Item -ItemType Directory $harness
    Copy-Item (Join-Path $package 'freeze_probe.c'),(Join-Path $package 'probe_protocol.h') $harness
    Run-BuildCommand 'freeze-probe' ('"'+$compiler+'" /nologo /W4 /std:c11 /O2 /DUNICODE /D_UNICODE "'+$harness+'\freeze_probe.c" /Fo"'+$harness+'\freeze_probe.obj" /Fe"'+$harness+'\freeze_probe.exe" /link advapi32.lib')
    foreach($file in Get-ChildItem $WorkRoot -Recurse -File | Where-Object { $_.Extension -in '.sys','.dll','.exe' -and ($_.FullName.StartsWith($solution+'build\') -or $_.FullName.StartsWith($harness+'\')) }) {
        $result.outputs += [ordered]@{name=$file.FullName.Substring($WorkRoot.Length+1);bytes=$file.Length;sha256=(Get-FileHash $file.FullName).Hash}
    }
    $result.compiled=$true
} finally {
    $result | ConvertTo-Json -Depth 8 | Set-Content (Join-Path $PWD 'ewdk-compile-result.json') -Encoding utf8
    # Upload only bounded textual build logs, never binaries, source or toolkit.
    foreach($file in Get-ChildItem $logs -File) { if($file.Length -gt 8MB){Remove-Item $file.FullName; "Omitted oversized build log; inspect bounded result." | Set-Content $file.FullName} }
}
