#Requires -RunAsAdministrator
<#
    Deploy GodspeedOS onto the StarFive VisionFive 2 Lite boot card.

    The card carries an official StarFive image, whose EFI System Partition is PARTITION 3.
    That is the only partition this board's U-Boot reads: its environment is the compiled-in
    default ("bad CRC, using default environment") and loads from `mmc 0:3`, full stop. The
    bootloader itself is in the board's 16 MB SPI flash and is not touched by any of this.

    Windows hides an EFI System Partition and ACLs it to administrators, which is why this
    script requires elevation and why the partition needs a drive letter first:

        Add-PartitionAccessPath -DiskNumber <N> -PartitionNumber 3 -AccessPath 'P:\'

    Everything here is verified rather than assumed: the target is checked to be the right
    partition before anything is written, the kernel is compared by SHA256 after copying, and
    the installed config is parsed back to confirm both that GodspeedOS is the default and
    that the Debian fallback labels survived. Any failure stops the script loudly.

    Usage:  pwsh -File scripts\deploy_visionfive.ps1 [-Esp P:] [-Repo <path>]
#>

param(
    [string]$Esp  = 'P:',
    [string]$Repo = 'C:\Downloads\Bankole\GodspeedOS\github\godspeed'
)

$ErrorActionPreference = 'Stop'

$Kernel     = Join-Path $Repo 'build\godspeed-riscv64-visionfive.img'
$Conf       = Join-Path $Repo 'boot\visionfive\extlinux-on-stock-esp.conf'
$KernelName = 'godspeed-riscv64-visionfive.img'
$ConfPath   = Join-Path $Esp 'extlinux\extlinux.conf'
$OrigPath   = Join-Path $Esp 'extlinux\extlinux.conf.orig'

function Fail([string]$m) { Write-Host "FAIL  $m" -ForegroundColor Red; exit 1 }
function Ok  ([string]$m) { Write-Host "OK    $m" -ForegroundColor Green }

# ---- 1. is this really the card's ESP, and not some other drive ------------------------
if (-not (Test-Path (Join-Path $Esp '\'))) {
    Fail "$Esp is not mounted. Assign it with: Add-PartitionAccessPath -DiskNumber 1 -PartitionNumber 3 -AccessPath '$Esp\'"
}
if (-not (Test-Path $ConfPath)) {
    Fail "$Esp has no extlinux\extlinux.conf. That is the signature of the StarFive ESP, so this is probably the wrong partition. Nothing written."
}
if (-not (Test-Path (Join-Path $Esp 'dtbs\6.12.5-starfive\starfive\jh7110s-starfive-visionfive-2-lite.dtb'))) {
    Fail "$Esp has no dtbs\6.12.5-starfive\starfive\jh7110s-starfive-visionfive-2-lite.dtb. The config references that device tree, so deploying without it would produce a card that cannot boot. Nothing written."
}
Ok "$Esp is the VisionFive ESP (extlinux.conf and the board device tree are both present)"

# ---- 2. do we have something to deploy -------------------------------------------------
if (-not (Test-Path $Kernel)) {
    Fail "kernel not built: $Kernel`n      Build it with: python scripts\riscv_build.py --release --visionfive [--features riscv-single-hart]"
}
if (-not (Test-Path $Conf)) { Fail "config not found: $Conf" }

# ---- 3. keep a way back -----------------------------------------------------------------
if (-not (Test-Path $OrigPath)) {
    Copy-Item $ConfPath $OrigPath
    Ok "stock config backed up to extlinux.conf.orig"
} else {
    Ok "extlinux.conf.orig already exists, left as it is (it is the STOCK file, do not overwrite it)"
}

# ---- 4. the kernel, verified by hash rather than by the copy returning quietly ----------
Copy-Item $Kernel (Join-Path $Esp '\') -Force
$dst = Join-Path $Esp $KernelName
$srcH = (Get-FileHash $Kernel -Algorithm SHA256).Hash
$dstH = (Get-FileHash $dst    -Algorithm SHA256).Hash
if ($srcH -ne $dstH) { Fail "kernel differs after copying. Card may be full or failing." }
Ok ("kernel copied and verified by SHA256, {0} bytes" -f (Get-Item $dst).Length)

# ---- 5. the config, parsed back ---------------------------------------------------------
Copy-Item $Conf $ConfPath -Force
$c = Get-Content $ConfPath -Raw
if ($c -notmatch '(?m)^default\s+godspeed\s*$') { Fail "installed config does not say 'default godspeed'" }
if ($c -notmatch '(?m)^label\s+godspeed\s*$')   { Fail "installed config has no 'label godspeed'" }
if ($c -notmatch '(?m)^label\s+l0\s*$')         { Fail "installed config lost Debian label l0 - the fallback would be gone" }
if ($c -notmatch '(?m)^label\s+l0r\s*$')        { Fail "installed config lost Debian label l0r" }
Ok "extlinux.conf installed: default is godspeed, Debian l0 and l0r intact"

# ---- 6. show what is actually on the card ----------------------------------------------
Write-Host ''
Write-Host "--- $Esp\ ---"
Get-ChildItem (Join-Path $Esp '\') -File | Select-Object Name, Length | Format-Table -AutoSize
Write-Host '--- extlinux.conf as installed ---'
Get-Content $ConfPath
Write-Host ''
Write-Host 'Card is ready. Eject it, put it in the board, power on.' -ForegroundColor Cyan
Write-Host 'Expect:  riscv64: usable harts 1   then   smp: 1 core ready   (singular)'
Write-Host 'If it fails, press any key during the 5 second countdown for the Debian menu.'
Write-Host ("To restore the stock card:  Copy-Item '{0}' '{1}' -Force" -f $OrigPath, $ConfPath)
