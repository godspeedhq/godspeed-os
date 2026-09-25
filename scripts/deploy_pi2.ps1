<#
    Deploy GodspeedOS onto a Raspberry Pi 2 boot card - and DIAGNOSE one that does not boot.

    WHY THIS EXISTS. The Pi 2 was the only shipping port whose deploy was prose: "copy
    build/kernel7.img to the card, copy build/config-pi2.txt to the card AS config.txt". The
    VisionFive has had `deploy_visionfive.ps1` for months; the Pi 2 had a paragraph and a rename a
    human has to remember. On 2026-09-25 a card came up on the RAINBOW SCREEN, and the build was
    blamed first - correctly ruling out the one staging bug that had been fixed (`75cd691c`) before
    anybody looked at what was actually on the card.

    The rainbow screen means ONE thing: the firmware started and never loaded a kernel. On this
    board that is almost always the config, because the Raspberry Pi Imager writes its own
    `config.txt` and ours has to REPLACE it. A card holding our `kernel7.img` beside the Imager's
    `config.txt` has no `kernel=` line naming it, so the firmware sits there.

    Everything is verified rather than assumed. The target is checked to be a Pi boot partition
    before anything is written; the kernel is compared by SHA256 after copying; the installed
    config is read BACK OFF THE CARD and parsed, so "it was written" and "the card returns it" are
    two different claims and both are made. Any failure stops loudly.

    -Check alone diagnoses without writing anything, which is what you want when a card is already
    in a Pi that will not boot.

    USAGE
        powershell -ExecutionPolicy Bypass -File scripts\deploy_pi2.ps1 -Card D: -Check
        powershell -ExecutionPolicy Bypass -File scripts\deploy_pi2.ps1 -Card D:
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string] $Card,
    [switch] $Check
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$kernelSrc = Join-Path $repo 'build\kernel7.img'
$configSrc = Join-Path $repo 'build\config-pi2.txt'

# Accept `D`, `D:`, `D:\` or an ordinary directory path. The last form is what makes this
# script TESTABLE against a staged folder instead of only against real hardware - a guard that
# can only be exercised by plugging in a card is a guard nobody exercises, and this script's
# whole job is to be the thing that catches the mistake before the Pi does.
$root = $Card.TrimEnd('\')
if ($root -match '^[A-Za-z]$') { $root = $root + ':' }
if (-not $root.EndsWith('\')) { $root = $root + '\' }

function Say([string] $m) { Write-Host $m }
# SHA256 via .NET rather than Get-FileHash: that cmdlet is absent from some Windows PowerShell
# installs, and its absence surfaced only AFTER both files had been copied - a half-done deploy
# reporting a tooling error rather than a result.
function FileSha([string] $p) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $fs = [System.IO.File]::OpenRead($p)
        try { ($sha.ComputeHash($fs) | ForEach-Object { $_.ToString('x2') }) -join '' }
        finally { $fs.Dispose() }
    } finally { $sha.Dispose() }
}
function Fail([string] $m) { Write-Host "FAIL: $m" -ForegroundColor Red; exit 1 }

Say "card: $root"

if (-not (Test-Path $root)) { Fail "no such drive. Is the card mounted, and is that the letter?" }

# ---- 1. Is this actually a Pi BOOT partition? --------------------------------------------------
# The firmware files are what make it one. Writing our kernel onto a data partition, or onto the
# Linux rootfs, produces exactly the same rainbow screen and looks like our bug.
$firmware = @('bootcode.bin', 'start.elf', 'fixup.dat')
$missing = @($firmware | Where-Object { -not (Test-Path (Join-Path $root $_)) })
if ($missing.Count -eq $firmware.Count) {
    Fail (("this is not a Pi boot partition - none of {0} is here. The boot partition is the " +
           "small FAT one, usually the FIRST partition; Windows normally shows only that one.") -f ($firmware -join ', '))
}
if ($missing.Count -gt 0) {
    Fail (("Pi firmware incomplete - missing: {0}. Re-write the card with Raspberry Pi Imager " +
           "(any Pi OS), then run this script: the firmware comes from there, we never ship it.") -f ($missing -join ', '))
}
Say "  firmware present: $($firmware -join ', ')"

# ---- 2. Diagnose what is on the card NOW -------------------------------------------------------
$cfgPath = Join-Path $root 'config.txt'
$kernPath = Join-Path $root 'kernel7.img'

$verdict = @()
if (Test-Path $cfgPath) {
    $cfgBytes = [System.IO.File]::ReadAllBytes($cfgPath)
    $cfgText = [System.Text.Encoding]::ASCII.GetString($cfgBytes)
    $kernelLine = ($cfgText -split "`n" | Where-Object { $_ -match '^\s*kernel\s*=' } | Select-Object -First 1)
    $crCount = ($cfgBytes | Where-Object { $_ -eq 13 }).Count

    if (-not $kernelLine) {
        $verdict += "config.txt has NO kernel= line - this is the rainbow screen, and it is the Imager's file, not ours"
    } elseif ($kernelLine -notmatch 'kernel7\.img') {
        $verdict += "config.txt names the wrong kernel: '$($kernelLine.Trim())'"
    } else {
        Say "  config.txt names: $($kernelLine.Trim())"
    }
    if ($crCount -gt 0) {
        # The VisionFive lost two reflashes to exactly this in a U-Boot config: the loader reads the
        # trailing CR as part of the filename and looks for a file that does not exist.
        $verdict += "config.txt contains $crCount carriage return(s) - a CR is read as part of the VALUE"
    }
} else {
    $verdict += "no config.txt on the card at all"
}

if (Test-Path $kernPath) {
    Say "  kernel7.img present: $((Get-Item $kernPath).Length) bytes"
} else {
    $verdict += "no kernel7.img on the card"
}

if ($verdict.Count -gt 0) {
    Write-Host ''
    Write-Host 'DIAGNOSIS:' -ForegroundColor Yellow
    $verdict | ForEach-Object { Write-Host "  - $_" -ForegroundColor Yellow }
} elseif ($Check) {
    Write-Host ''
    Say 'The card looks correct. If it still shows the rainbow screen, the fault is not the config:'
    Say '  - try a different card (a failing card reads fine in Windows and not in the Pi)'
    Say '  - check the PSU; an undervolted Pi 2 can stall before the kernel runs'
}

if ($Check) { Write-Host ''; Say '-Check only: nothing was written.'; exit 0 }

# ---- 3. Write --------------------------------------------------------------------------------
foreach ($p in @($kernelSrc, $configSrc)) {
    if (-not (Test-Path $p)) { Fail "missing build artefact: $p. Run: py scripts\board.py pi2" }
}

Write-Host ''
Say 'writing:'
Copy-Item $kernelSrc $kernPath -Force
Say "  build\kernel7.img     -> $kernPath"
Copy-Item $configSrc $cfgPath -Force
Say "  build\config-pi2.txt  -> $cfgPath   (RENAMED, which is the step that gets missed)"

# ---- 4. Verify by READING BACK off the card ----------------------------------------------------
# Copy-Item reporting success proves a write was issued, not that the card returns those bytes.
$srcHash = FileSha $kernelSrc
$dstHash = FileSha $kernPath
if ($srcHash -ne $dstHash) { Fail "kernel7.img differs after copy - the card did not take it" }
Say "  kernel SHA256 matches after copy: $($srcHash.Substring(0,16))..."

$backBytes = [System.IO.File]::ReadAllBytes($cfgPath)
$backText = [System.Text.Encoding]::ASCII.GetString($backBytes)
$backKernel = ($backText -split "`n" | Where-Object { $_ -match '^\s*kernel\s*=' } | Select-Object -First 1)
$backCr = ($backBytes | Where-Object { $_ -eq 13 }).Count
if (-not $backKernel) { Fail "config.txt on the card has no kernel= line after writing it" }
if ($backKernel -notmatch 'kernel7\.img') { Fail "config.txt on the card names: $backKernel" }
if ($backCr -gt 0) { Fail "config.txt on the card has $backCr carriage return(s) - it would not boot" }
Say "  config read back off the card: $($backKernel.Trim()), 0 carriage returns"

Write-Host ''
Say 'Done. Eject the card, boot the Pi 2, and the prompt arrives on serial at 115200 8N1.'
