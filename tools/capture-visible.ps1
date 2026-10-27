# Captures what the operator actually SEES on the desktop during a Station cold start.
# PrintWindow (used by the earlier timeline scripts) re-renders the webview's offscreen surface,
# so it can show a painted UI that the compositor has not presented to the screen yet. This
# script uses Graphics.CopyFromScreen against the live window rect instead: those are the real
# presented pixels, so the frames match the eye.
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Vis {
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L; public int T; public int R; public int B; }
}
"@

$out = 'E:\AI\w1ve-keyer-suite\shots-visible'
if (-not (Test-Path $out)) { New-Item -ItemType Directory -Path $out | Out-Null }
Get-ChildItem $out -Filter *.png -ErrorAction SilentlyContinue | Remove-Item -Force

Get-Process w1ve-rkr-station -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 800

$exe = 'E:\AI\w1ve-keyer-suite\target\release\w1ve-rkr-station.exe'
$t0 = Get-Date
$p = Start-Process -FilePath $exe -PassThru
for ($i = 0; $i -lt 60; $i++) { Start-Sleep -Milliseconds 50; $p.Refresh(); if ($p.MainWindowHandle -ne 0) { break } }
$hwnd = $p.MainWindowHandle
"window handle: $hwnd after $([int]((Get-Date) - $t0).TotalMilliseconds) ms"

for ($i = 0; $i -lt 40; $i++) {
  $ms = [int]((Get-Date) - $t0).TotalMilliseconds
  $p.Refresh()
  if ($p.MainWindowHandle -ne 0) { $hwnd = $p.MainWindowHandle }
  [void][Vis]::SetForegroundWindow($hwnd)
  $r = New-Object Vis+RECT
  [void][Vis]::GetWindowRect($hwnd, [ref]$r)
  $w = $r.R - $r.L; $h = $r.B - $r.T
  if ($w -gt 0 -and $h -gt 0) {
    $bmp = New-Object System.Drawing.Bitmap $w, $h
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    try {
      $g.CopyFromScreen($r.L, $r.T, 0, 0, (New-Object System.Drawing.Size $w, $h))
    } catch {
      # Screen locked / compositor unavailable: leave the frame blank rather than aborting.
    }
    $g.Dispose()
    $name = Join-Path $out ('v{0:d4}-{1}.png' -f $i, $ms)
    $bmp.Save($name, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
  }
  Start-Sleep -Milliseconds 350
}
"captures: $((Get-ChildItem $out -Filter *.png).Count)  pid=$($p.Id)  alive=$(-not $p.HasExited)"
