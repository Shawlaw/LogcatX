# Build assets/clipboard-helper.dex from the committed Java source.
#
# The dex is committed because the release/CI machines do not carry an Android
# SDK. Run this after editing assets/clipboard-helper/src/.../ClipboardBridge.java.
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$sdk = if ($env:ANDROID_HOME) { $env:ANDROID_HOME } elseif ($env:ANDROID_SDK_ROOT) { $env:ANDROID_SDK_ROOT } else { $null }
if (-not $sdk -or -not (Test-Path $sdk)) {
    throw 'Set ANDROID_HOME or ANDROID_SDK_ROOT to an Android SDK installation.'
}

$platformJar = Get-ChildItem -Path (Join-Path $sdk 'platforms') -Filter android.jar -Recurse -ErrorAction SilentlyContinue |
    Sort-Object FullName -Descending | Select-Object -First 1
if (-not $platformJar) {
    throw "No android.jar found under $sdk\platforms."
}
$d8 = Get-ChildItem -Path (Join-Path $sdk 'build-tools') -Filter d8.bat -Recurse -ErrorAction SilentlyContinue |
    Sort-Object FullName -Descending | Select-Object -First 1
if (-not $d8) {
    throw "No d8.bat found under $sdk\build-tools."
}

$source = Join-Path $root 'assets\clipboard-helper\src\com\logcatx\clipboard\ClipboardBridge.java'
$build = Join-Path $root 'target\clipboard-helper-build'
$classes = Join-Path $build 'classes'
$dexDir = Join-Path $build 'dex'
$output = Join-Path $root 'assets\clipboard-helper.dex'
Remove-Item -Recurse -Force $build -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path $classes, $dexDir -Force | Out-Null

Write-Host "javac: $($platformJar.FullName)"
& javac -source 8 -target 8 -cp $platformJar.FullName -d $classes $source
if ($LASTEXITCODE -ne 0) { throw 'javac failed.' }

Write-Host "d8: $($d8.FullName)"
& $d8.FullName --lib $platformJar.FullName --min-api 24 --output $dexDir (Get-ChildItem -Recurse $classes -Filter *.class | ForEach-Object FullName)
if ($LASTEXITCODE -ne 0) { throw 'd8 failed.' }

Copy-Item (Join-Path $dexDir 'classes.dex') $output -Force
$bytes = [System.IO.File]::ReadAllBytes($output)
$magic = [System.Text.Encoding]::ASCII.GetString($bytes, 0, 4)
if ($magic -ne "dex`n") { throw "Unexpected dex magic: $magic" }
Write-Host "Wrote $output ($($bytes.Length) bytes)."
