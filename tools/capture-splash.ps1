# Full-screen frames during a Station cold start. Captures the real desktop (CopyFromScreen), so
# the splash popup is seen exactly where the operator sees it, relative to the main window.
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Screens { [DllImport("user32.dll")] public static extern int GetSystemMetrics(int i); }
"@

$out = 'E:\AI\w1ve-keyer-suite\shots-splash'
if (-not (Test-Path $out)) { New-Item -ItemType Directory -Path $out | Out-Null }
Get-ChildItem $out -Filter *.png -ErrorAction SilentlyContinue | Remove-Item -Force

Get-Process w1ve-rkr-station -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 800

$w = [Screens]::GetSystemMetrics(0)
$h = [Screens]::GetSystemMetrics(1)
$exe = 'E:\AI\w1ve-keyer-suite\target\release\w1ve-rkr-station.exe'
$t0 = Get-Date
$p = Start-Process -FilePath $exe -PassThru

for ($i = 0; $i -lt 30; $i++) {
  $ms = [int]((Get-Date) - $t0).TotalMilliseconds
  $bmp = New-Object System.Drawing.Bitmap $w, $h
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  try { $g.CopyFromScreen(0, 0, 0, 0, (New-Object System.Drawing.Size $w, $h)) } catch {}
  $g.Dispose()
  $name = Join-Path $out ('d{0:d4}-{1}.png' -f $i, $ms)
  $bmp.Save($name, [System.Drawing.Imaging.ImageFormat]::Png)
  $bmp.Dispose()
  Start-Sleep -Milliseconds 350
}
"captures: $((Get-ChildItem $out -Filter *.png).Count)  pid=$($p.Id)  alive=$(-not $p.HasExited)"
