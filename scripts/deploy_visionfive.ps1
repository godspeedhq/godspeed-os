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

    IT ALSO RE-READS THE KERNEL FROM THE CARD, because the verification above was NOT ENOUGH: it
    hashes the file immediately after copying it, and Windows serves that read from its own cache.
    It proves the right bytes were WRITTEN; it does not prove the card RETURNS them. Section 7 reads
    it back with FILE_FLAG_NO_BUFFERING so every read goes to the device.

    A LARGER DIAGNOSTIC SECTION LIVED HERE BRIEFLY AND IS GONE. It also re-read the stock Debian
    initrd, timed everything, reported disk health and printed a verdict about PSUs and reseating -
    apparatus built to test the theory that the card was not returning its data. On 2026-09-13 the
    board said:

        Retrieving file: /godspeed-riscv64-visionfive.img
        Failed to load '/godspeed-riscv64-visionfive.img'
        Retrieving file: /initrd.img-6.12.5-starfive
        Failed to load '/initrd.img-6.12.5-starfive'      <- DEBIAN'S OWN, never written by us

    while the small files on the same partition read fine. THE THEORY WAS WRONG. The cause was CRLF
    in `extlinux.conf` (see section 5): U-Boot read the trailing carriage return as part of every
    FILENAME, so nothing the config named could be opened, while the menu rendered perfectly because
    a stray CR in a display string only returns the cursor.

    So the card was never at fault, and the apparatus built to prove it was is deleted - a feature is
    pulled into existence by a real problem (CLAUDE.md 26.2), and that problem never existed. What
    survives is the one check that fixes a defect actually demonstrated: the cached-hash gap above.
    `backlog/26` has the full account, including the reasoning error that produced the wrong theory.

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

    # ---- 5. the config, WRITTEN AS LF and parsed back --------------------------------------
    #
    # NOT `Copy-Item`, and this is the whole of the 2026-09-13 failure. The repository stores this
    # file as LF, but `.gitattributes` marked it `*.conf text`, which means "normalize on commit,
    # convert to NATIVE on checkout" - and native on a Windows checkout is CRLF. A plain copy put
    # CRLF on the card. U-Boot's extlinux parser then read the trailing `\r` as part of each
    # FILENAME and every entry failed:
    #
    #     Retrieving file: /godspeed-riscv64-visionfive.img
    #     Failed to load '/godspeed-riscv64-visionfive.img'
    #
    # while the MENU rendered perfectly, because a trailing `\r` in a display string only returns
    # the cursor. So it looked like a load failure and not a config fault, and it cost two card
    # reflashes and two wrong theories (`backlog/26`).
    #
    # `.gitattributes` now pins `boot/** text eol=lf`, which fixes the checkout. This does NOT rely
    # on that: whether the board boots must not depend on a contributor's git settings, an editor
    # that helpfully "fixed" the file, or a copy through a tool that rewrites line endings. The
    # bytes are normalized HERE, and then checked on the card below.
    $confText = [System.IO.File]::ReadAllText($Conf) -replace "`r`n", "`n" -replace "`r", "`n"
    [System.IO.File]::WriteAllText($ConfPath, $confText, (New-Object System.Text.UTF8Encoding($false)))

    # VERIFIED ON THE CARD, not assumed from what we just wrote. A single CR in this file is a card
    # that shows a perfect menu and cannot boot anything on it.
    $onCard = [System.IO.File]::ReadAllBytes($ConfPath)
    $crs    = @($onCard | Where-Object { $_ -eq 13 }).Count
    if ($crs -gt 0) {
        throw "installed extlinux.conf contains $crs carriage return(s). U-Boot would read them as part of each filename and every entry would fail to load."
    }
    Ok ("extlinux.conf written LF-only, {0} bytes, no CR on the card" -f $onCard.Length)

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
    # ---- 7. the kernel, RE-READ FROM THE CARD ---------------------------------------------
    #
    # Section 4 hashes the file immediately after copying it, and Windows serves that read from its
    # own cache - so it verifies a write it never actually read back. That is a real gap whatever
    # else is going on, and this closes it: FILE_FLAG_NO_BUFFERING (0x20000000) sends every read to
    # the device.
    #
    # This is deliberately ONLY our kernel. An earlier version of this section also re-read the stock
    # Debian initrd, timed both, and printed a verdict about card health, PSUs and reseating - all of
    # it built to test the theory that the card was not returning its data. That theory was WRONG
    # (the fault was CRLF in the config, `backlog/26`), so the apparatus built for it is gone: a
    # feature is pulled into existence by a real problem, and this one never existed (26.2). What
    # remains is the check that fixes a defect we actually demonstrated.
    try {
        $len   = (Get-Item $dst).Length
        $chunk = 1MB
        $fs    = New-Object System.IO.FileStream(
                    $dst, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read,
                    [System.IO.FileShare]::ReadWrite, $chunk, [System.IO.FileOptions]0x20000000)
        $sha   = [System.Security.Cryptography.SHA256]::Create()
        $buf   = New-Object byte[] $chunk
        $done  = 0L
        try {
            while ($done -lt $len) {
                $n = $fs.Read($buf, 0, $chunk)
                if ($n -le 0) { break }
                $use = [Math]::Min([long]$n, $len - $done)
                [void]$sha.TransformBlock($buf, 0, $use, $null, 0)
                $done += $use
            }
            [void]$sha.TransformFinalBlock((New-Object byte[] 0), 0, 0)
        } finally { $fs.Dispose() }
        $cold = -join ($sha.Hash | ForEach-Object { $_.ToString('X2') })
        if ($done -ne $len)   { throw "short read: $done of $len bytes came back from the card" }
        if ($cold -ne $srcH)  { throw "cold read differs: card returned $($cold.Substring(0,16)), wrote $($srcH.Substring(0,16))" }
        Ok ("kernel re-read from the card (cache bypassed), {0} bytes, hash matches" -f $done)
    }
    catch {
        Write-Host ("FAIL  {0}" -f $_.Exception.Message) -ForegroundColor Red
        Write-Host '      The card did not return what was written to it. Replace it.'
        throw
    }

    Write-Host 'READY. Eject the card, put it in the board, power on.'
    Write-Host 'Expect:  riscv64: usable harts 1   then   smp: 1 core ready   (singular)'
    Write-Host 'If it fails, press any key during the ONE second countdown for the Debian menu (extlinux timeout is 10 DECISECONDS, not 10 seconds, so it goes past quickly).'
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
