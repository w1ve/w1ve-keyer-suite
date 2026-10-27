# Captures the splash popup's last second, to see the fade-out frame by frame.
#
# capture-visible-main.ps1 samples every 250 ms: enough to prove the popup is up for five seconds,
# far too coarse to tell a 200 ms fade from an instant disappearance. This polls for the splash,
# then samples in a tight loop and records each frame's mean luminance over the popup's rect, so
# the fade shows up both as pixels (the PNGs) and as numbers (the log lines).
#
# Two modes:
#   (default)  frames every ~40 ms, of the WHOLE main window so the frames show what the fade
#              reveals, not just the artwork dissolving against an unknown backdrop.
#   -Timing    mean only, popup rect only, every ~17 ms: saving a PNG and copying the whole window
#              cost too much to state the fade's duration to better than a frame or two.
#
# A param block has to precede every other statement, so it sits above $ErrorActionPreference.
param([switch]$Timing)

$ErrorActionPreference = 'Stop'

$periodMs = if ($Timing) { 4 } else { 40 }
$saveFrames = -not $Timing
$out = 'E:\AI\w1ve-keyer-suite\shots-splash-fade'
if (-not (Test-Path $out)) { New-Item -ItemType Directory -Path $out | Out-Null }
if ($saveFrames) { Get-ChildItem $out -Filter *.png -ErrorAction SilentlyContinue | Remove-Item -Force }

Add-Type -AssemblyName System.Drawing
Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @"
using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;
using System.Text;
public class Fade {
  public delegate bool Proc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(Proc p, IntPtr l);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L; public int T; public int R; public int B; }

  // The process's visible 300x168 top-level window (the splash) and its rect, or IntPtr.Zero.
  public static IntPtr Splash(uint pid, out RECT rect) {
    IntPtr splash = IntPtr.Zero;
    RECT found = new RECT();
    EnumWindows((h, l) => {
      uint p; GetWindowThreadProcessId(h, out p);
      if (p != pid || !IsWindowVisible(h)) return true;
      RECT r;
      if (GetWindowRect(h, out r) && (r.R - r.L) == 300 && (r.B - r.T) == 168) { splash = h; found = r; return false; }
      return true;
    }, IntPtr.Zero);
    rect = found;
    return splash;
  }

  // Brings the app's main window (the only one wider than 400 px) to the front and returns its
  // rect. The popup is always-on-top, so it paints regardless — but whatever window happens to sit
  // in front of the Station is what the fade appears to reveal, which reads as the fade exposing
  // some other application.
  public static bool FocusMain(uint pid, out RECT rect) {
    IntPtr main = IntPtr.Zero;
    RECT found = new RECT();
    EnumWindows((h, l) => {
      uint p; GetWindowThreadProcessId(h, out p);
      if (p != pid || !IsWindowVisible(h)) return true;
      RECT r;
      if (GetWindowRect(h, out r) && (r.R - r.L) > 400) { main = h; found = r; return false; }
      return true;
    }, IntPtr.Zero);
    rect = found;
    return main != IntPtr.Zero && SetForegroundWindow(main);
  }

  // Mean 0-255 luminance of a region of a frame. Fading the window to transparent must move this
  // towards the luminance of whatever is underneath, one step per sampled frame.
  public static double Mean(Bitmap bmp, Rectangle r) {
    long sum = 0; long n = 0;
    BitmapData d = bmp.LockBits(r, ImageLockMode.ReadOnly, PixelFormat.Format24bppRgb);
    try {
      int stride = d.Stride;
      byte[] buf = new byte[stride * r.Height];
      Marshal.Copy(d.Scan0, buf, 0, buf.Length);
      for (int y = 0; y < r.Height; y++) {
        int row = y * stride;
        for (int x = 0; x < r.Width; x++) {
          int i = row + x * 3;
          sum += (buf[i] * 29 + buf[i + 1] * 150 + buf[i + 2] * 77) >> 8;
          n++;
        }
      }
    } finally { bmp.UnlockBits(d); }
    return (double)sum / n;
  }

  // Samples `cap` every `periodMs` for `totalMs`, saving each frame when `save` is set and
  // returning "t=<ms> mean=<lum>" lines. `popup` is measured wherever it falls inside `cap`.
  //
  // The bitmap is allocated once and the lines are returned rather than printed as they are
  // produced: a per-sample WriteLine through the PowerShell host costs ~20 ms, which would be the
  // dominant term and blur exactly the timing this is meant to measure.
  public static string Record(RECT cap, RECT popup, uint pid, int periodMs, int totalMs, string dir, bool save) {
    int w = cap.R - cap.L, h = cap.B - cap.T;
    StringBuilder log = new StringBuilder();
    Stopwatch sw = Stopwatch.StartNew();
    int i = 0;
    using (Bitmap bmp = new Bitmap(w, h, PixelFormat.Format24bppRgb))
    using (Graphics g = Graphics.FromImage(bmp)) {
      while (sw.ElapsedMilliseconds <= totalMs) {
        int t = (int)sw.ElapsedMilliseconds;
        if (save) { RECT ignored; FocusMain(pid, out ignored); }
        try { g.CopyFromScreen(cap.L, cap.T, 0, 0, new Size(w, h)); } catch { }
        // Measure only the popup's own rectangle, wherever it lands inside the capture.
        Rectangle p = Rectangle.Intersect(
          new Rectangle(popup.L - cap.L, popup.T - cap.T, popup.R - popup.L, popup.B - popup.T),
          new Rectangle(0, 0, w, h));
        double mean = p.Width > 0 && p.Height > 0 ? Mean(bmp, p) : 0.0;
        if (save) bmp.Save(System.IO.Path.Combine(dir, string.Format("f{0:d4}-{1}.png", i, t)), ImageFormat.Png);
        log.AppendFormat("t={0} mean={1:F1}\n", t, mean);
        i++;
        int next = i * periodMs;
        int wait = next - (int)sw.ElapsedMilliseconds;
        if (wait > 0) System.Threading.Thread.Sleep(wait);
      }
    }
    return log.ToString();
  }
}
"@

Get-Process w1ve-rkr-station -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 800

$exe = 'E:\AI\w1ve-keyer-suite\target\release\w1ve-rkr-station.exe'
$t0 = Get-Date
$p = Start-Process -FilePath $exe -PassThru
$pid32 = [uint32]$p.Id

# Wait for the popup, then note how long it took to appear.
$rect = New-Object Fade+RECT
$splash = [IntPtr]::Zero
$deadline = (Get-Date).AddSeconds(15)
while ((Get-Date) -lt $deadline -and $splash -eq [IntPtr]::Zero) {
  Start-Sleep -Milliseconds 20
  $splash = [Fade]::Splash($pid32, [ref]$rect)
}
if ($splash -eq [IntPtr]::Zero) { throw 'splash never appeared' }
'splash up after {0} ms at {1},{2} {3}x{4}' -f [int]((Get-Date) - $t0).TotalMilliseconds, $rect.L, $rect.T, ($rect.R - $rect.L), ($rect.B - $rect.T)

$mainRect = New-Object Fade+RECT
$raised = [Fade]::FocusMain($pid32, [ref]$mainRect)
'main window at {0},{1} {2}x{3}, raised: {4}' -f $mainRect.L, $mainRect.T, ($mainRect.R - $mainRect.L), ($mainRect.B - $mainRect.T), $raised

# Frames show the whole window; the timing pass only needs the popup's own pixels.
$cap = if ($saveFrames) { $mainRect } else { $rect }

# Five seconds of splash, then the fade: record the last ~700 ms of it plus the frame after.
Start-Sleep -Milliseconds 4600
'phase 2: recording (t is ms from the start of this recording; the fade begins near t=400)'
[Fade]::Record($cap, $rect, $pid32, $periodMs, 700, $out, $saveFrames) | Write-Host -NoNewline

Start-Sleep -Milliseconds 600
[void][Fade]::FocusMain($pid32, [ref]$mainRect)
$after = Join-Path $out 'after-close.png'
$w = $cap.R - $cap.L; $h = $cap.B - $cap.T
$bmp = New-Object System.Drawing.Bitmap $w, $h
$g = [System.Drawing.Graphics]::FromImage($bmp)
try { $g.CopyFromScreen($cap.L, $cap.T, 0, 0, (New-Object System.Drawing.Size $w, $h)) } catch { }
$g.Dispose()
$bmp.Save($after, [System.Drawing.Imaging.ImageFormat]::Png)
$popupInCap = New-Object System.Drawing.Rectangle ($rect.L - $cap.L), ($rect.T - $cap.T), ($rect.R - $rect.L), ($rect.B - $rect.T)
$afterMean = [Fade]::Mean($bmp, $popupInCap)
$bmp.Dispose()
'after close: popup-area mean={0:F1} saved {1}' -f $afterMean, $after
'captures: {0}  pid={1}  alive={2}' -f (Get-ChildItem $out -Filter 'f*.png').Count, $p.Id, (-not $p.HasExited)
