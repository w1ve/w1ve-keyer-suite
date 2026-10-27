# Captures the Station's cold start from a FIXED rect: the main window's bounds.
#
# tools/capture-visible.ps1 follows Process.MainWindowHandle, which hands back whichever top-level
# window that process most recently made visible. Once the splash popup exists, that can be the
# 300x168 splash itself, so its frames never show what is underneath. This script locates the main
# window by title once, keeps its rect for the whole run, and copies those screen pixels - so each
# frame is exactly what the operator sees in that area, splash included when it is up.
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public class Win {
  public delegate bool Proc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(Proc p, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumChildWindows(IntPtr h, Proc p, IntPtr l);
  [DllImport("user32.dll")] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L; public int T; public int R; public int B; }

  public static List<IntPtr> OfProcess(uint pid) {
    var found = new List<IntPtr>();
    EnumWindows((h, l) => { uint p; GetWindowThreadProcessId(h, out p); if (p == pid && IsWindowVisible(h)) found.Add(h); return true; }, IntPtr.Zero);
    return found;
  }
  public static string Title(IntPtr h) { var s = new StringBuilder(512); GetWindowTextW(h, s, 512); return s.ToString(); }
  public static string Size(IntPtr h) { RECT r; if (!GetWindowRect(h, out r)) return "?"; return string.Format("{0}x{1}", r.R - r.L, r.B - r.T); }
}
"@

$out = 'E:\AI\w1ve-keyer-suite\shots-visible-main'
if (-not (Test-Path $out)) { New-Item -ItemType Directory -Path $out | Out-Null }
Get-ChildItem $out -Filter *.png -ErrorAction SilentlyContinue | Remove-Item -Force

Get-Process w1ve-rkr-station -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 800

$exe = 'E:\AI\w1ve-keyer-suite\target\release\w1ve-rkr-station.exe'
$t0 = Get-Date
$p = Start-Process -FilePath $exe -PassThru

# Locate the main window by title; it is the only window wide enough to be the UI.
$main = [IntPtr]::Zero
for ($i = 0; $i -lt 200 -and $main -eq [IntPtr]::Zero; $i++) {
  Start-Sleep -Milliseconds 50
  foreach ($h in [Win]::OfProcess([uint32]$p.Id)) {
    $r = New-Object Win+RECT
    [void][Win]::GetWindowRect($h, [ref]$r)
    if (($r.R - $r.L) -gt 400 -and [Win]::Title($h) -ne '') { $main = $h; break }
  }
}
if ($main -eq [IntPtr]::Zero) { throw 'main window never appeared' }
"main window: $([Win]::Title($main)) $([Win]::Size($main)) after $([int]((Get-Date) - $t0).TotalMilliseconds) ms"

$fixed = New-Object Win+RECT
[void][Win]::GetWindowRect($main, [ref]$fixed)
$w = $fixed.R - $fixed.L; $h = $fixed.B - $fixed.T

for ($i = 0; $i -lt 40; $i++) {
  $ms = [int]((Get-Date) - $t0).TotalMilliseconds
  [void][Win]::SetForegroundWindow($main)

  # Record every visible window of the process, so the splash's own size and position are logged
  # alongside the pixels rather than inferred from them.
  $wins = foreach ($h2 in [Win]::OfProcess([uint32]$p.Id)) {
    $r2 = New-Object Win+RECT; [void][Win]::GetWindowRect($h2, [ref]$r2)
    '{0}x{1}@{2},{3}' -f ($r2.R - $r2.L), ($r2.B - $r2.T), $r2.L, $r2.T
  }
  $bmp = New-Object System.Drawing.Bitmap $w, $h
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  try { $g.CopyFromScreen($fixed.L, $fixed.T, 0, 0, (New-Object System.Drawing.Size $w, $h)) } catch { }
  $g.Dispose()
  $name = Join-Path $out ('m{0:d4}-{1}.png' -f $i, $ms)
  $bmp.Save($name, [System.Drawing.Imaging.ImageFormat]::Png)
  $bmp.Dispose()
  "frame {0} t={1}ms windows: {2}" -f $i, $ms, ($wins -join ' | ')
  Start-Sleep -Milliseconds 250
}
"captures: $((Get-ChildItem $out -Filter *.png).Count)  pid=$($p.Id)  alive=$(-not $p.HasExited)"
