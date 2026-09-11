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

    The whole run is transcribed to build\deploy_visionfive.log so the result can be read from
    the file rather than copied out of a console.

    Usage:  pwsh -File scripts\deploy_visionfive.ps1 [-Esp P:] [-Repo <path>] [-Log <path>]
#>

param(
    [string]$Esp  = 'P:',
    [string]$Repo = 'C:\Downloads\Bankole\GodspeedOS\github\godspeed',
    [string]$Log
)

$ErrorActionPreference = 'Stop'

if (-not $Log) { $Log = Join-Path $Repo 'build\deploy_visionfive.log' }
$logDir = Split-Path $Log -Parent
if (-not (Test-Path $logDir)) { New-Item -ItemType Directory -Path $logDir -Force | Out-Null }

$Kernel     = Join-Path $Repo 'build\godspeed-riscv64-visionfive.img'
$Conf       = Join-Path $Repo 'boot\visionfive\extlinux-on-stock-esp.conf'
$KernelName = 'godspeed-riscv64-visionfive.img'
$ConfPath   = Join-Path $Esp 'extlinux\extlinux.conf'
$OrigPath   = Join-Path $Esp 'extlinux\extlinux.conf.orig'
$DtbRel     = 'dtbs\6.12.5-starfive\starfive\jh7110s-starfive-visionfive-2-lite.dtb'

function Ok([string]$m) { Write-Host "OK    $m" -ForegroundColor Green }

Start-Transcript -Path $Log -Force | Out-Null
$code = 0
try {
    Write-Host ("GodspeedOS -> VisionFive 2 Lite card     {0}" -f (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'))
    Write-Host ("repo {0}    esp {1}" -f $Repo, $Esp)
    Write-Host ''

    # ---- 1. is this really the card's ESP, and not some other drive --------------------
    if (-not (Test-Path (Join-Path $Esp '\'))) {
        throw "$Esp is not mounted. Assign it with: Add-PartitionAccessPath -DiskNumber 1 -PartitionNumber 3 -AccessPath '$Esp\'"
    }
    if (-not (Test-Path $ConfPath)) {
        throw "$Esp has no extlinux\extlinux.conf. That is the signature of the StarFive ESP, so this is probably the wrong partition. Nothing written."
    }
    if (-not (Test-Path (Join-Path $Esp $DtbRel))) {
        throw "$Esp has no $DtbRel. The config references that device tree, so deploying without it would produce a card that cannot boot. Nothing written."
    }
    Ok "$Esp is the VisionFive ESP (extlinux.conf and the board device tree are both present)"

    # ---- 2. do we have something to deploy ---------------------------------------------
    if (-not (Test-Path $Kernel)) {
        throw "kernel not built: $Kernel . Build it with: python scripts\riscv_build.py --release --visionfive [--features riscv-single-hart]"
    }
    if (-not (Test-Path $Conf)) { throw "config not found: $Conf" }
    Ok ("sources present, kernel is {0} bytes" -f (Get-Item $Kernel).Length)

    # ---- 3. keep a way back -------------------------------------------------------------
    if (-not (Test-Path $OrigPath)) {
        Copy-Item $ConfPath $OrigPath
        Ok "stock config backed up to extlinux.conf.orig"
    } else {
        Ok "extlinux.conf.orig already exists, left as it is (it is the STOCK file, do not overwrite it)"
    }

    # ---- 4. the kernel, verified by hash rather than by the copy returning quietly ------
    Copy-Item $Kernel (Join-Path $Esp '\') -Force
    $dst  = Join-Path $Esp $KernelName
    $srcH = (Get-FileHash $Kernel -Algorithm SHA256).Hash
    $dstH = (Get-FileHash $dst    -Algorithm SHA256).Hash
    if ($srcH -ne $dstH) { throw "kernel differs after copying. Card may be full or failing." }
    Ok ("kernel copied and verified by SHA256, {0} bytes, {1}" -f (Get-Item $dst).Length, $dstH.Substring(0,16))

    # ---- 5. the config, parsed back ------------------------------------------------------
    Copy-Item $Conf $ConfPath -Force
    $c = Get-Content $ConfPath -Raw
    if ($c -notmatch '(?m)^default\s+godspeed\s*$') { throw "installed config does not say 'default godspeed'" }
    if ($c -notmatch '(?m)^label\s+godspeed\s*$')   { throw "installed config has no 'label godspeed'" }
    if ($c -notmatch '(?m)^label\s+l0\s*$')         { throw "installed config lost Debian label l0 - the fallback would be gone" }
    if ($c -notmatch '(?m)^label\s+l0r\s*$')        { throw "installed config lost Debian label l0r" }
    Ok "extlinux.conf installed: default is godspeed, Debian l0 and l0r intact"

    # ---- 6. show what is actually on the card -------------------------------------------
    Write-Host ''
    Write-Host "--- $Esp\ ---"
    Get-ChildItem (Join-Path $Esp '\') -File |
        Select-Object Name, Length |
        Format-Table -AutoSize |
        Out-String -Width 200 |
        Write-Host
    Write-Host '--- extlinux.conf as installed ---'
    Get-Content $ConfPath | ForEach-Object { Write-Host $_ }
    Write-Host ''
    Write-Host 'READY. Eject the card, put it in the board, power on.'
    Write-Host 'Expect:  riscv64: usable harts 1   then   smp: 1 core ready   (singular)'
    Write-Host 'If it fails, press any key during the 5 second countdown for the Debian menu.'
    Write-Host ("To restore the stock card:  Copy-Item '{0}' '{1}' -Force" -f $OrigPath, $ConfPath)
}
catch {
    Write-Host ("FAIL  {0}" -f $_.Exception.Message) -ForegroundColor Red
    Write-Host 'Nothing further was written. The card is in the state the last OK line describes.'
    $code = 1
}
finally {
    Stop-Transcript | Out-Null
    Write-Host ''
    Write-Host ("log written to {0}" -f $Log)
}

exit $code
