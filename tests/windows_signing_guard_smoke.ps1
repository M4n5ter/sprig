# Test original XML generation/guard behavior with installed MSBuild before the
# large EWDK download. No compilation, signing task or generated executable runs.
Set-StrictMode -Version Latest
$ErrorActionPreference='Stop'
$PSNativeCommandUseErrorActionPreference=$false  # expected veto exit is inspected below
$root=Join-Path $env:RUNNER_TEMP ('sprig-signing-guard-'+[guid]::NewGuid().ToString('N'))
$null=New-Item -ItemType Directory $root
$vswhere="${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$msbuild=@(& $vswhere -latest -products '*' -requires Microsoft.Component.MSBuild -find 'MSBuild\**\Bin\MSBuild.exe' | Where-Object {Test-Path $_ -PathType Leaf})[0]
if(-not $msbuild){throw 'Installed MSBuild unavailable for source-generator smoke.'}
foreach($namespace in @('', 'http://schemas.microsoft.com/developer/msbuild/2003')) {
    $name=if($namespace){'namespaced'}else{'plain'}
    $fixture=Join-Path $root ($name+'.proj')
    $guard=Join-Path $root ($name+'.targets')
    $report=Join-Path $root ($name+'.json')
    $xmlns=if($namespace){' xmlns="'+$namespace+'"'}else{''}
    ('<Project'+$xmlns+'><Target Name="TestSign"><SignTask /></Target><Target Name="Build"><Message Text="BENIGN_BUILD_ONLY" Importance="high" /></Target></Project>') | Set-Content $fixture -Encoding utf8
    & python "$PSScriptRoot/check_unsigned_build.py" $fixture $report $guard
    if($LASTEXITCODE -ne 0){throw 'Guard generation failed.'}
    $text=Get-Content $fixture -Raw
    $text.Replace('</Project>','<Import Project="'+$guard+'" /></Project>') | Set-Content $fixture -Encoding utf8
    $pp=Join-Path $root ($name+'.pp.xml')
    & $msbuild $fixture /nologo /noAutoResponse /nr:false /interactive:false "/pp:$pp"
    if($LASTEXITCODE -ne 0){throw 'Generated guard did not parse in installed MSBuild.'}
    & python "$PSScriptRoot/check_unsigned_build.py" $pp $report $guard --verify
    if($LASTEXITCODE -ne 0){throw 'Imported guard verification failed.'}
    & $msbuild $fixture /nologo /noAutoResponse /nr:false /interactive:false /t:Build
    if($LASTEXITCODE -ne 0){throw 'Guard blocked unrelated benign target.'}
    $output=(& $msbuild $fixture /nologo /noAutoResponse /nr:false /interactive:false /t:TestSign 2>&1 | Out-String)
    if($LASTEXITCODE -eq 0 -or $output -notmatch 'Build-only policy refuses signing/certificate target: TestSign' -or $output -match 'MSB4036') {throw 'Guard did not veto before the signing task.'}
    Write-Host "PASS $name XML parse, benign Build, signing target hard veto."
}
# Only our own source fixture, no toolkit or compiled output.
Remove-Item -LiteralPath $root -Recurse -Force

# The final native command intentionally failed at the veto; all assertions passed.
exit 0
