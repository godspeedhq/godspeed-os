<#
    Deploy GodspeedOS onto a Raspberry Pi boot card - Pi 2 or Pi 4 - and DIAGNOSE one that will not
    boot.

    WHY THIS EXISTS, and why it takes a -Board. The Pi ports were the only shipping ports whose
    deploy was prose: copy two files, and RENAME one of them. The VisionFive has had a script for
    months. On 2026-09-25 a card came up on the RAINBOW SCREEN and the BUILD was suspected first.

    The build was fine. The card was a DUAL-BOOT card - it carries the Pi 2 firmware
    (bootcode.bin / start.elf / fixup.dat) AND the Pi 4 firmware (start4.elf / fixup4.dat), plus both
    kernels, and `config.txt` is the switch between them. It was set for the Pi 4 and was put in a
    Pi 2, so the firmware looked for `godspeed8.img`, found `kernel7.img`, and sat there.

    That is not a mistake anybody should have to remember not to make, which is the whole argument for
    this script: on a card that can boot either board, "which board is this card currently set for" is
    a permanent question and should be answerable in one command.

    ONE script rather than two near-identical ones, because two copies of a deploy procedure is two
    truths and the second one drifts (Commandment III).

    Everything is verified rather than assumed. The target is checked to be a boot partition for the
    REQUESTED board before anything is written; the kernel is compared by SHA256 after copying; and
    the config is read BACK OFF THE CARD and parsed, because "the write was issued" and "the card
    returns those bytes" are different claims.

    USAGE
        powershell -File scripts\deploy_pi.ps1 -Board pi2 -Drive E -Check
        powershell -File scripts\deploy_pi.ps1 -Board pi4 -Drive E
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][ValidateSet('pi2', 'pi4')][string] $Board,
    [Parameter(Mandatory = $true)][string] $Drive,
    [switch] $Check
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot

# The per-board facts, in ONE place. `KernelAs` differs from the source name on the Pi 4, and that
# rename is the single step this script exists to stop anybody performing by hand.
$boards = @{
    'pi2' = @{
        Name      = 'Raspberry Pi 2 (ARMv7, BCM2836)'
        KernelSrc = 'build\kernel7.img'
        KernelAs  = 'kernel7.img'
        ConfigSrc = 'build\config-pi2.txt'
        Firmware  = @('bootcode.bin', 'start.elf', 'fixup.dat')
        Expect    = 'kernel7.img'
    }
    'pi4' = @{
        Name      = 'Raspberry Pi 4 (AArch64, BCM2711)'
        KernelSrc = 'build\kernel8.img'
        KernelAs  = 'godspeed8.img'
        ConfigSrc = 'build\config-pi4.txt'
        Firmware  = @('start4.elf', 'fixup4.dat')
        Expect    = 'godspeed8.img'
    }
}
$b = $boards[$Board]

$root = $Drive.TrimEnd('\')
if ($root -match '^[A-Za-z]$') { $root = $root + ':' }
if (-not $root.EndsWith('\')) { $root = $root + '\' }

function Say([string] $m) { Write-Host $m }
function Fail([string] $m) { Write-Host "FAIL: $m" -ForegroundColor Red; exit 1 }
function FileSha([string] $p) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $fs = [System.IO.File]::OpenRead($p)
        try { ($sha.ComputeHash($fs) | ForEach-Object { $_.ToString('x2') }) -join '' }
        finally { $fs.Dispose() }
    } finally { $sha.Dispose() }
}

Say "board: $($b.Name)"
Say "drive: $root"

if (-not (Test-Path $root)) { Fail "no such drive. Is the card mounted, and is that the letter?" }

# ---- 1. Is this a boot partition for the REQUESTED board? --------------------------------------
$missing = @($b.Firmware | Where-Object { -not (Test-Path (Join-Path $root $_)) })
if ($missing.Count -eq $b.Firmware.Count) {
    Fail (("not a $Board boot partition - none of {0} is here. The boot partition is the small FAT " +
           "one, usually the FIRST. If this is a card for the other Pi, its firmware differs.") -f ($b.Firmware -join ', '))
}
if ($missing.Count -gt 0) {
    Fail (("$Board firmware incomplete - missing: {0}. Re-write the card with Raspberry Pi Imager, " +
           "then run this: the firmware comes from there, we never ship it.") -f ($missing -join ', '))
}
Say "  $Board firmware present: $($b.Firmware -join ', ')"

# A dual-boot card is normal here and worth SAYING, because it is what makes the config the switch.
$other = if ($Board -eq 'pi2') { $boards['pi4'] } else { $boards['pi2'] }
$otherPresent = @($other.Firmware | Where-Object { Test-Path (Join-Path $root $_) })
if ($otherPresent.Count -eq $other.Firmware.Count) {
    Say "  DUAL-BOOT CARD: it also carries $($other.Name) firmware, so config.txt is the switch"
}

# ---- 2. What is the card set for NOW? ----------------------------------------------------------
$cfgPath  = Join-Path $root 'config.txt'
$kernPath = Join-Path $root $b.KernelAs

$verdict = @()
if (Test-Path $cfgPath) {
    $cfgBytes = [System.IO.File]::ReadAllBytes($cfgPath)
    $cfgText = [System.Text.Encoding]::ASCII.GetString($cfgBytes)
    $kernelLine = ($cfgText -split "`n" | Where-Object { $_ -match '^\s*kernel\s*=' } | Select-Object -First 1)
    $crCount = ($cfgBytes | Where-Object { $_ -eq 13 }).Count

    if (-not $kernelLine) {
        $verdict += "config.txt has NO kernel= line - the rainbow screen, and it is the Imager's file"
    } elseif ($kernelLine -notmatch [regex]::Escape($b.Expect)) {
        $verdict += ("config.txt is set for a DIFFERENT board: '{0}' (this deploy wants {1})" -f $kernelLine.Trim(), $b.Expect)
    } else {
        Say "  config.txt already names: $($kernelLine.Trim())"
    }
    if ($crCount -gt 0) {
        $verdict += "config.txt contains $crCount carriage return(s) - a CR is read as part of the VALUE"
    }
} else {
    $verdict += 'no config.txt on the card at all'
}

if (Test-Path $kernPath) {
    $onCard = FileSha $kernPath
    $srcFull = Join-Path $repo $b.KernelSrc
    if (Test-Path $srcFull) {
        if ((FileSha $srcFull) -ne $onCard) {
            # REPORT THE HASH, because the hash is what was COMPARED. This used to print the two
            # FILE SIZES, which are routinely identical when only comments changed - so it announced
            # "STALE (3713984 bytes) - the build is 3713984", a verdict followed by two matching
            # numbers offered as its reason. The verdict was right and its evidence was unrelated,
            # which is the shape that teaches a reader to distrust a correct instrument.
            $verdict += ("$($b.KernelAs) on the card is STALE - card sha256 {0}..., build {1}... ({2} bytes vs {3})" -f `
                $onCard.Substring(0, 16), (FileSha $srcFull).Substring(0, 16), `
                (Get-Item $kernPath).Length, (Get-Item $srcFull).Length)
        } else {
            Say "  $($b.KernelAs) on the card is already current"
        }
    }
} else {
    $verdict += "no $($b.KernelAs) on the card"
}

if ($verdict.Count -gt 0) {
    Write-Host ''
    Write-Host 'DIAGNOSIS:' -ForegroundColor Yellow
    $verdict | ForEach-Object { Write-Host "  - $_" -ForegroundColor Yellow }
} elseif ($Check) {
    Write-Host ''
    Say 'The card is correctly set for this board. If it still will not boot, the fault is not the'
    Say 'config: try another card (a failing card reads fine in Windows), or check the PSU.'
}

if ($Check) { Write-Host ''; Say '-Check only: nothing was written.'; exit 0 }

# ---- 3. Write ----------------------------------------------------------------------------------
$kernelSrc = Join-Path $repo $b.KernelSrc
$configSrc = Join-Path $repo $b.ConfigSrc
foreach ($p in @($kernelSrc, $configSrc)) {
    if (-not (Test-Path $p)) { Fail "missing build artefact: $p. Run: py scripts\board.py $Board" }
}

Write-Host ''
Say 'writing:'
Copy-Item $kernelSrc $kernPath -Force
$renote = if ($b.KernelAs -ne (Split-Path $b.KernelSrc -Leaf)) { '   (RENAMED)' } else { '' }
Say "  $($b.KernelSrc)  -> $kernPath$renote"
Copy-Item $configSrc $cfgPath -Force
Say "  $($b.ConfigSrc)  -> $cfgPath   (RENAMED, the step that gets missed)"

# ---- 4. Verify by reading BACK off the card ----------------------------------------------------
if ((FileSha $kernelSrc) -ne (FileSha $kernPath)) { Fail "$($b.KernelAs) differs after copy - the card did not take it" }
Say "  kernel SHA256 matches after copy: $((FileSha $kernelSrc).Substring(0,16))..."

$backBytes  = [System.IO.File]::ReadAllBytes($cfgPath)
$backText   = [System.Text.Encoding]::ASCII.GetString($backBytes)
$backKernel = ($backText -split "`n" | Where-Object { $_ -match '^\s*kernel\s*=' } | Select-Object -First 1)
$backCr     = ($backBytes | Where-Object { $_ -eq 13 }).Count
if (-not $backKernel) { Fail 'config.txt on the card has no kernel= line after writing it' }
if ($backKernel -notmatch [regex]::Escape($b.Expect)) { Fail "config.txt on the card names: $backKernel" }
if ($backCr -gt 0) { Fail "config.txt on the card has $backCr carriage return(s) - it would not boot" }
Say "  config read back off the card: $($backKernel.Trim()), 0 carriage returns"

Write-Host ''
Say "Done. The card is now set for $($b.Name). Serial 115200 8N1."
if ($otherPresent.Count -eq $other.Firmware.Count) {
    Say "To switch it back: scripts\deploy_pi.ps1 -Board $(if ($Board -eq 'pi2') { 'pi4' } else { 'pi2' }) -Drive $Drive"
}
