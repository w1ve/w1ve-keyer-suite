# Measures how long the splash popup is actually on screen, to the nearest poll interval.
#
# The frame captures in capture-visible-main.ps1 sample every 250 ms, which is too coarse to tell a
# 4.8 s splash from a 5.0 s one. This polls the process's visible top-level windows every 20 ms and
# records the first and last sighting of the 300x168 splash, so the duration is bounded to ~20 ms.
$ErrorActionPreference = 'Stop'
Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public class Poll {
  public delegate bool Proc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(Proc p, IntPtr l);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L; public int T; public int R; public int B; }

  // True when the process owns a visible 300x168 top-level window: the splash popup.
  public static bool HasSplash(uint pid) {
    bool found = false;
    EnumWindows((h, l) => {
      uint p; GetWindowThreadProcessId(h, out p);
      if (p != pid || !IsWindowVisible(h)) return true;
      RECT r;
      if (GetWindowRect(h, out r) && (r.R - r.L) == 300 && (r.B - r.T) == 168) { found = true; return false; }
      return true;
    }, IntPtr.Zero);
    return found;
  }
}
"@

Get-Process w1ve-rkr-station -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 800

$exe = 'E:\AI\w1ve-keyer-suite\target\release\w1ve-rkr-station.exe'
$t0 = Get-Date
$p = Start-Process -FilePath $exe -PassThru
$pid32 = [uint32]$p.Id

$first = $null; $last = $null
$deadline = (Get-Date).AddSeconds(20)
while ((Get-Date) -lt $deadline) {
  $ms = ((Get-Date) - $t0).TotalMilliseconds
  if ([Poll]::HasSplash($pid32)) {
    if ($null -eq $first) { $first = $ms }
    $last = $ms
  } elseif ($null -ne $first) {
    break   # it was up and is now gone: $last is the final sighting
  }
  Start-Sleep -Milliseconds 20
}

if ($null -eq $first) { throw 'splash never appeared' }
'splash first seen: {0} ms' -f [int]$first
'splash last seen:  {0} ms' -f [int]$last
'visible duration:  {0} ms (last-first, ±20 ms poll; true value lies in [{0}, {1}])' -f ([int]($last - $first)), ([int]($last - $first + 40))
'pid={0} alive={1}' -f $p.Id, (-not $p.HasExited)
