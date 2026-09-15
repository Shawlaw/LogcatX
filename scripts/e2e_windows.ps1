param(
    [string]$Exe = (Join-Path $PSScriptRoot '..\target\release\logcatx.exe'),
    [string]$Python = 'python',
    [string]$ArtifactRoot = (Join-Path $PSScriptRoot '..\target\e2e'),
    [switch]$KeepOpen,
    [switch]$RendererCapture,
    [switch]$SkipScreenshots,
    [switch]$SkipFullScan,
    [switch]$VisualOnly
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class E2EWindow {
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int w, int height, uint flags);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
}
'@

function Wait-For([scriptblock]$Check, [string]$Description, [int]$Seconds = 15) {
    $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
    do {
        $value = & $Check
        if ($value) { return $value }
        Start-Sleep -Milliseconds 150
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timeout: $Description"
}
function Get-Elements {
    $script:root.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
}
function Find-Element([string]$Name, [string]$Type = 'Button') {
    @(Get-Elements | Where-Object { $_.Current.Name -eq $Name -and $_.Current.ControlType.ProgrammaticName -eq "ControlType.$Type" }) | Select-Object -First 1
}
function Click-Element([string]$Name) {
    $element = Wait-For { Find-Element $Name } "button $Name"
    if (-not $element.Current.IsEnabled) { throw "Disabled button: $Name" }
    $pattern = $null
    if ($element.TryGetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern, [ref]$pattern)) { $pattern.Invoke() }
    elseif ($element.TryGetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern, [ref]$pattern)) { $pattern.Toggle() }
    elseif ($element.TryGetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern, [ref]$pattern)) { $pattern.Select() }
    else { throw "No click action for $Name" }
    Start-Sleep -Milliseconds 400
}
function Set-Edit([int]$Index, [string]$Text) {
    $edits = @(Get-Elements | Where-Object { $_.Current.ControlType -eq [System.Windows.Automation.ControlType]::Edit })
    if ($edits.Count -le $Index) { throw "Missing edit $Index" }
    $edits[$Index].SetFocus()
    Start-Sleep -Milliseconds 300
    $edits[$Index].GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern).SetValue($Text)
    Start-Sleep -Milliseconds 400
}
function Capture([string]$Name) {
    if ($SkipScreenshots) { return }
    if ($RendererCapture) {
        [IO.File]::WriteAllText((Join-Path $script:runRoot capture.request), $Name)
        Wait-For { Test-Path -LiteralPath (Join-Path $script:runRoot "$Name.png") } "renderer screenshot $Name" | Out-Null
        return
    }
    [void][E2EWindow]::SetForegroundWindow($script:handle)
    Start-Sleep -Milliseconds 250
    $rect = $script:root.Current.BoundingRectangle
    $bitmap = New-Object System.Drawing.Bitmap([int]$rect.Width, [int]$rect.Height)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    try {
        $graphics.CopyFromScreen([int]$rect.X, [int]$rect.Y, 0, 0, $bitmap.Size)
    } catch {
        # A remote/locked desktop may not expose a screen DC; ask DWM for this window.
        $dc = $graphics.GetHdc()
        try { if (-not [E2EWindow]::PrintWindow($script:handle, $dc, 2)) { throw 'Window capture unavailable' } }
        finally { $graphics.ReleaseHdc($dc) }
    }
    $bitmap.Save((Join-Path $script:runRoot "$Name.png"))
    $graphics.Dispose()
    $bitmap.Dispose()
}
function Check([bool]$Condition, [string]$Description) {
    if (-not $Condition) { throw "Assertion failed: $Description" }
    $script:results.Add($Description)
    Write-Output "PASS: $Description"
}
function Write-Fixture([string]$Text) {
    [IO.File]::WriteAllText((Join-Path $script:runRoot fixture.txt), $Text)
}
function Connected-To([string]$Target) {
    $path = Join-Path $script:runRoot connected
    (Test-Path -LiteralPath $path) -and ([IO.File]::ReadAllText($path) -eq $Target)
}

$sourceExe = (Resolve-Path -LiteralPath $Exe).Path
$script:runRoot = Join-Path ([IO.Path]::GetFullPath($ArtifactRoot)) ([DateTime]::Now.ToString('yyyyMMdd-HHmmss'))
New-Item -ItemType Directory -Force -Path $script:runRoot | Out-Null
$script:results = [Collections.Generic.List[string]]::new()
$app = $null
$tcp = $null
try {
    $fixtureExe = Join-Path $script:runRoot 'fake-adb.exe'
    & rustc --edition=2024 -O (Join-Path $PSScriptRoot '..\tests\fixtures\fake_adb.rs') -o $fixtureExe
    if ($LASTEXITCODE -ne 0) { throw 'Fixture build failed' }
    $appExe = Join-Path $script:runRoot 'logcatx-e2e.exe'
    Copy-Item -LiteralPath $sourceExe -Destination $appExe
    Write-Fixture "connect=127.0.0.1:39001`nbefore_pair=127.0.0.1:39000`ndisplay_devices=true"
    $config = @{ adb_path=$fixtureExe; log_dir=(Join-Path $script:runRoot logs); language='zh-CN'; auto_check_updates=$false }
    [IO.File]::WriteAllText((Join-Path $script:runRoot config.json), ($config | ConvertTo-Json))
    $app = Start-Process -FilePath $appExe -PassThru -WindowStyle Hidden -Environment @{ LOGCATX_E2E_CAPTURE_DIR=$script:runRoot }
    $pidCondition = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::ProcessIdProperty, $app.Id)
    $nameCondition = New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::NameProperty, 'LogcatX')
    $condition = New-Object System.Windows.Automation.AndCondition($pidCondition, $nameCondition)
    $script:root = Wait-For { [System.Windows.Automation.AutomationElement]::RootElement.FindFirst([System.Windows.Automation.TreeScope]::Children, $condition) } 'native app window'
    $script:handle = [IntPtr]$script:root.Current.NativeWindowHandle
    [void][E2EWindow]::SetWindowPos($script:handle, [IntPtr]::Zero, 20, 20, 2500, 1600, 0)
    [void][E2EWindow]::SetForegroundWindow($script:handle)
    Wait-For { Find-Element "Google Pixel Test`nFIXTURE_USB_A" } 'device rows' | Out-Null
    Capture '01-device-selection-before'
    Click-Element "Google Pixel Test`nFIXTURE_USB_A"
    Capture '02-device-selection-after'
    Check $true 'Native device selection action completed'

    Click-Element '⇄  连接设备'
    Wait-For { Find-Element '127.0.0.1:37001' 'Text' } 'mDNS pairing service' | Out-Null
    Capture '03-wireless-discovery'
    Click-Element '输入配对码'
    Set-Edit 0 '１２７。０。０。１：３７００１'
    Set-Edit 1 '０１２３４５'
    Capture '04-pairing-fullwidth'
    if ($VisualOnly) {
        Check (Find-Element '配对并连接').Current.IsEnabled 'Pairing form fits and has an enabled submit action'
        Write-Output "Visual artifacts: $script:runRoot"
        return
    }
    Click-Element '配对并连接'
    Wait-For { Connected-To '127.0.0.1:39001' } 'pair then connect current port' | Out-Null
    Wait-For { -not (Find-Element '关闭 / 取消任务') } 'connection dialog close' | Out-Null
    $saved = Get-Content -Raw -LiteralPath (Join-Path $script:runRoot config.json) | ConvertFrom-Json
    Check ($saved.recent_connections[0] -eq '127.0.0.1:39001') 'Paired connection saved its current port'
    Check ($saved.wireless_connections -contains '127.0.0.1:39001') 'Wireless history remembers connection type'
    $calls = [IO.File]::ReadAllText((Join-Path $script:runRoot calls.log))
    Check ($calls.Contains('pair stdin_valid=true args_has_code=false') -and -not $calls.Contains('012345')) 'Pairing code normalized, sent on stdin, absent from command arguments/log'
    Capture '05-paired-connected'

    Write-Fixture "connect=127.0.0.1:39002`ndisplay_devices=true"
    Click-Element '⇄  连接设备'
    Click-Element '127.0.0.1:39001 · 查找当前端口'
    Wait-For { Connected-To '127.0.0.1:39002' } 'dynamic-port history reconnect' | Out-Null
    Check $true 'History rediscovered and connected to changed port 39002'

    Click-Element '⇄  连接设备'
    Click-Element '手动连接'
    Set-Edit 0 '１２７。０。０。１：６５５３６'
    Check (-not (Find-Element '连接').Current.IsEnabled) 'Invalid full-width port cannot be submitted'
    Capture '06-invalid-port'
    Set-Edit 0 '１２７。０。０。１'
    Wait-For { Find-Element '实际使用：127.0.0.1:5555' 'Text' } 'normalized default-port preview' | Out-Null
    Capture '07-normalized-manual'
    Click-Element '连接'
    Wait-For { Connected-To '127.0.0.1:5555' } 'manual default port connection' | Out-Null
    Check $true 'Full-width IP connects with default port 5555'

    $portsPath = Join-Path $script:runRoot ports.json
    $tcpScript = (Resolve-Path (Join-Path $PSScriptRoot '..\tests\fixtures\adb_tcp_server.py')).Path
    $tcp = Start-Process -FilePath $Python -ArgumentList @(('"' + $tcpScript + '"'), ('"' + $portsPath + '"')) -PassThru -WindowStyle Hidden
    Wait-For { Test-Path -LiteralPath $portsPath } 'local TCP fixture startup' | Out-Null
    $ports = Get-Content -Raw -LiteralPath $portsPath | ConvertFrom-Json
    $scanTarget = "127.0.0.1:$($ports.wireless)"
    Write-Fixture "mode=no-mdns`nconnect=$scanTarget`ndisplay_devices=true"
    Click-Element '⇄  连接设备'
    Click-Element 'IP 查找'
    Set-Edit 0 '１２７。０。０。１'
    $scanStart = [DateTime]::UtcNow
    Click-Element '快速查找'
    Wait-For { Find-Element $scanTarget 'Text' } 'ADB handshake scan result' 60 | Out-Null
    $firstResultSeconds = ([DateTime]::UtcNow - $scanStart).TotalSeconds
    Capture '08-scan-result'
    Check (-not (Find-Element "127.0.0.1:$($ports.http)" 'Text')) 'Scanner did not identify HTTP as ADB'
    Click-Element '连接'
    Wait-For { Connected-To $scanTarget } 'connect scanned endpoint' | Out-Null
    Check $true "Scanned ADB service connected (first result $([Math]::Round($firstResultSeconds, 2)) s)"

    Click-Element '⇄  连接设备'
    Click-Element 'IP 查找'
    if (-not $SkipFullScan) {
        Click-Element '完整扫描'
        Wait-For { Find-Element '扫描完成。' 'Text' } 'full port scan completion' 200 | Out-Null
        Check (-not (Find-Element "127.0.0.1:$($ports.http)" 'Text')) 'Full 1–65535 scan excludes non-ADB service'
        Capture '09-full-scan'
    }
    Click-Element '完整扫描'
    Click-Element '停止扫描'
    Wait-For { Find-Element '扫描已停止，已发现的结果仍可使用。' 'Text' } 'scan cancellation' | Out-Null
    Capture '10-scan-cancelled'
    Check $true 'Native scan cancellation returns control'
    Click-Element '关闭 / 取消任务'
    Capture '11-final-devices'
    @{passed=$script:results; first_scan_result_seconds=$firstResultSeconds; source_sha256=(Get-FileHash -LiteralPath $sourceExe -Algorithm SHA256).Hash; finished_utc=[DateTime]::UtcNow.ToString('o')} | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $script:runRoot results.json) -Encoding utf8
    Write-Output "E2E artifacts: $script:runRoot"
} catch {
    $failure = $_
    if ($script:root) {
        try { Capture 'failure' } catch { Write-Warning $_ }
        Get-Elements | ForEach-Object { "$($_.Current.ControlType.ProgrammaticName): $($_.Current.Name)" } | Set-Content -LiteralPath (Join-Path $script:runRoot failure-ui.txt)
    }
    throw $failure
} finally {
    if ($app -and -not $KeepOpen) {
        if ($script:handle) { [void][E2EWindow]::PostMessage($script:handle, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) }
        if (-not $app.WaitForExit(5000)) { $app.Kill(); $app.WaitForExit() }
    }
    if ($tcp -and -not $tcp.HasExited) { $tcp.Kill(); $tcp.WaitForExit() }
}
