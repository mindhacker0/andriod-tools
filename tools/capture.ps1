param(
  [string]$Out = "G:\andriodTools\ui.png",
  [int]$ClickX = -1,
  [int]$ClickY = -1,
  [int]$DragFromX = -1,
  [int]$DragFromY = -1,
  [int]$DragToX = -1,
  [int]$DragToY = -1,
  [int]$WheelClicks = 0,
  [int]$WheelX = -1,
  [int]$WheelY = -1,
  [switch]$Close
)

# Keep this script ASCII-only: Windows PowerShell 5.1 reads .ps1 as ANSI/GBK,
# so non-ASCII literals break the parser.

# 让 PowerShell 进程按物理像素取坐标，避免 DPI 虚拟化导致截图区域错位
Add-Type -Namespace Dpi -Name Helper -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(System.IntPtr value);
[DllImport("user32.dll")] public static extern bool GetWindowRect(System.IntPtr hWnd, out RECT lpRect);
[DllImport("user32.dll")] public static extern bool SetForegroundWindow(System.IntPtr hWnd);
[DllImport("user32.dll")] public static extern bool ShowWindow(System.IntPtr hWnd, int nCmdShow);
[DllImport("user32.dll")] public static extern bool SetCursorPos(int X, int Y);
[DllImport("user32.dll")] public static extern void mouse_event(uint dwFlags, uint dx, uint dy, int dwData, System.IntPtr dwExtraInfo);
[DllImport("user32.dll")] public static extern bool PostMessage(System.IntPtr hWnd, uint msg, System.IntPtr wParam, System.IntPtr lParam);
[StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
'@ | Out-Null

try { [Dpi.Helper]::SetProcessDpiAwarenessContext([IntPtr](-4)) | Out-Null } catch { }

Add-Type -AssemblyName System.Drawing

$proc = Get-Process phone-tool -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
if (-not $proc) { throw "phone-tool window not found; start the app first" }

$h = $proc.MainWindowHandle

# -Close: politely ask the window to close (WM_CLOSE) so the app can run its
# shutdown path. Never force-kill it: an un-closed MTP session wedges the phone.
if ($Close) {
  [Dpi.Helper]::PostMessage($h, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null
  Start-Sleep -Seconds 3
  "close requested"
  return
}
# 直接 SetForegroundWindow 常被 Windows 的前台锁定规则拒绝，
# 先最小化再还原一次通常能拿到前台（否则截图/点击会落到别的窗口上）。
[Dpi.Helper]::ShowWindow($h, 6) | Out-Null          # SW_MINIMIZE
Start-Sleep -Milliseconds 250
[Dpi.Helper]::ShowWindow($h, 9) | Out-Null          # SW_RESTORE
[Dpi.Helper]::SetForegroundWindow($h) | Out-Null
Start-Sleep -Milliseconds 700

$r = New-Object Dpi.Helper+RECT
[Dpi.Helper]::GetWindowRect($h, [ref]$r) | Out-Null
$w = $r.Right - $r.Left
$hh = $r.Bottom - $r.Top
if ($w -le 0 -or $hh -le 0) { throw "bad window rect: $w x $hh" }

# Optional: click once inside the window (window-relative coords)
if ($ClickX -ge 0 -and $ClickY -ge 0) {
  [Dpi.Helper]::SetCursorPos($r.Left + $ClickX, $r.Top + $ClickY) | Out-Null
  Start-Sleep -Milliseconds 250
  [Dpi.Helper]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)   # LEFTDOWN
  Start-Sleep -Milliseconds 60
  [Dpi.Helper]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)   # LEFTUP
  Start-Sleep -Milliseconds 3000
}

# Optional: drag inside the window, e.g. to test panel resizing
if ($DragFromX -ge 0 -and $DragToX -ge 0) {
  [Dpi.Helper]::SetCursorPos($r.Left + $DragFromX, $r.Top + $DragFromY) | Out-Null
  Start-Sleep -Milliseconds 300
  [Dpi.Helper]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)   # LEFTDOWN
  for ($i = 1; $i -le 12; $i++) {
    $x = $r.Left + [int]($DragFromX + ($DragToX - $DragFromX) * $i / 12)
    $y = $r.Top  + [int]($DragFromY + ($DragToY - $DragFromY) * $i / 12)
    [Dpi.Helper]::SetCursorPos($x, $y) | Out-Null
    Start-Sleep -Milliseconds 70
  }
  [Dpi.Helper]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)   # LEFTUP
  Start-Sleep -Milliseconds 1500
}

# Optional: mouse wheel (positive = scroll down / towards the end)
if ($WheelClicks -ne 0) {
  $wx = if ($WheelX -ge 0) { $WheelX } else { [int]($w / 2) }
  $wy = if ($WheelY -ge 0) { $WheelY } else { [int]($hh / 2) }
  [Dpi.Helper]::SetCursorPos($r.Left + $wx, $r.Top + $wy) | Out-Null
  Start-Sleep -Milliseconds 250
  $delta = if ($WheelClicks -gt 0) { 120 } else { -120 }
  for ($i = 0; $i -lt [Math]::Abs($WheelClicks); $i++) {
    [Dpi.Helper]::mouse_event(0x0800, 0, 0, $delta, [IntPtr]::Zero)   # MOUSEEVENTF_WHEEL
    Start-Sleep -Milliseconds 25
  }
  Start-Sleep -Milliseconds 1500
}
Start-Sleep -Milliseconds 400

$bmp = New-Object System.Drawing.Bitmap($w, $hh)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($r.Left, $r.Top, 0, 0, (New-Object System.Drawing.Size($w, $hh)))
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose()

"saved $Out (window $w x $hh at $($r.Left),$($r.Top))"
