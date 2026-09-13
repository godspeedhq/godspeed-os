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

    IT ALSO DIAGNOSES THE CARD, and that is not decoration - it is here because the verification
    above was NOT ENOUGH ONCE. On 2026-09-13 the deploy reported "kernel copied and verified by
    SHA256" and the board then said:

        Retrieving file: /godspeed-riscv64-visionfive.img
        Failed to load '/godspeed-riscv64-visionfive.img'
        Retrieving file: /initrd.img-6.12.5-starfive
        Failed to load '/initrd.img-6.12.5-starfive'      <- DEBIAN'S OWN, never written by us

    while the two SMALL files on the same partition read fine (uEnv.txt 419 bytes, extlinux.conf
    1605 bytes, both at ~400 KiB/s). Every multi-megabyte read failed; every small one worked. A
    reflash of the whole card changed nothing.

    The hash check above cannot see that, for a reason worth stating: it hashes the file IMMEDIATELY
    after copying it, and Windows serves that read from its own cache. So it proves the right bytes
    were WRITTEN. It does not prove the card RETURNS them.

    So the diagnostics below re-read the card with the Windows cache BYPASSED (FILE_FLAG_NO_BUFFERING
    - every read goes to the device), hash what comes back, and do the same to the stock Debian
    initrd, which is the control: ~13 MB this script never touched, that U-Boot also fails on. The
    verdict that falls out is the one that matters:

      - cold reads correct  -> the card returns large reads to a PC, so U-Boot failing to load them
                               is on the BOARD side (slot contact, power sag under sustained read).
      - cold reads wrong    -> the card does not return its own data, and no reflash will fix that.

    Diagnostics NEVER fail the deploy. A card that was written correctly has been written correctly
    whether or not this section can read it back.

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
    # ---- 7. COLD-READ DIAGNOSTICS --------------------------------------------------------
    #
    # Everything here REPORTS. Nothing here throws: a diagnostic that cannot run must not turn a
    # good deploy into a failure, so the whole section is its own try/catch.
    Write-Host ''
    Write-Host '--- card diagnostics (cache bypassed, so these are reads of the DEVICE) ---'
    try {
        # Push anything Windows is still holding out to the card before reading it back. Without
        # this a "cold" read can still be served correct data that has not reached the medium.
        try {
            Write-VolumeCache -DriveLetter $Esp.TrimEnd(':') -ErrorAction Stop
            Ok 'volume cache flushed to the device'
        } catch {
            Write-Host ("WARN  could not flush the volume cache: {0}" -f $_.Exception.Message)
        }

        # Read a file with FILE_FLAG_NO_BUFFERING (0x20000000) and return its SHA256 plus how long
        # it took. No-buffering requires sector-aligned requests, hence the 1 MiB aligned chunks;
        # the final chunk simply returns fewer bytes and only the valid ones are hashed.
        function Get-ColdHash([string]$Path) {
            $len   = (Get-Item $Path).Length
            $chunk = 1MB
            $opts  = [System.IO.FileOptions]0x20000000
            $fs    = New-Object System.IO.FileStream(
                        $Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read,
                        [System.IO.FileShare]::ReadWrite, $chunk, $opts)
            $sha   = [System.Security.Cryptography.SHA256]::Create()
            $buf   = New-Object byte[] $chunk
            $done  = 0L
            $sw    = [System.Diagnostics.Stopwatch]::StartNew()
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
            $sw.Stop()
            $hex = -join ($sha.Hash | ForEach-Object { $_.ToString('X2') })
            [pscustomobject]@{
                Hash = $hex; Bytes = $done; Expected = $len
                Ms = [int]$sw.Elapsed.TotalMilliseconds
                KiBps = if ($sw.Elapsed.TotalSeconds -gt 0) { [int](($done / 1KB) / $sw.Elapsed.TotalSeconds) } else { 0 }
            }
        }

        $bad = @()

        # (a) OUR kernel, cold. This is the file the board reported "Failed to load".
        $cold = Get-ColdHash $dst
        if ($cold.Bytes -ne $cold.Expected) {
            $bad += ("kernel: short read, {0} of {1} bytes" -f $cold.Bytes, $cold.Expected)
        } elseif ($cold.Hash -ne $srcH) {
            $bad += ("kernel: cold hash {0} != written {1}" -f $cold.Hash.Substring(0,16), $srcH.Substring(0,16))
        }
        Write-Host ("      kernel      {0,10:N0} bytes  {1,6} ms  {2,7} KiB/s  {3}" -f `
                    $cold.Bytes, $cold.Ms, $cold.KiBps, $cold.Hash.Substring(0,16))

        # (b) THE CONTROL: a large stock file this script has never written, and the second thing
        #     U-Boot failed on. If OUR file reads and THIS one does not, the fault is ours; if
        #     both read, nothing on the card is wrong at all.
        $initrd = Join-Path $Esp 'initrd.img-6.12.5-starfive'
        if (Test-Path $initrd) {
            $ci = Get-ColdHash $initrd
            if ($ci.Bytes -ne $ci.Expected) {
                $bad += ("stock initrd: short read, {0} of {1} bytes" -f $ci.Bytes, $ci.Expected)
            }
            Write-Host ("      initrd      {0,10:N0} bytes  {1,6} ms  {2,7} KiB/s  (stock, never written by this script)" -f `
                        $ci.Bytes, $ci.Ms, $ci.KiBps)
        } else {
            Write-Host '      initrd      absent - no stock control file to compare against'
        }

        # (c) A SMALL file, for contrast. U-Boot reads these fine and fails on the two above, so if
        #     that same shape appears here it is the card; if it does not, it is the board.
        $small = Join-Path $Esp 'extlinux\extlinux.conf'
        $cs = Get-ColdHash $small
        Write-Host ("      extlinux    {0,10:N0} bytes  {1,6} ms  {2,7} KiB/s" -f $cs.Bytes, $cs.Ms, $cs.KiBps)

        # (d) WHICH physical card this is, so a failing one can be told apart from its neighbours.
        try {
            $part = Get-Partition | Where-Object { $_.AccessPaths -contains ($Esp + '\') } | Select-Object -First 1
            if ($part) {
                $pd = Get-PhysicalDisk -DeviceNumber $part.DiskNumber -ErrorAction SilentlyContinue
                $dk = Get-Disk -Number $part.DiskNumber -ErrorAction SilentlyContinue
                Write-Host ("      disk {0} partition {1}   {2}   {3}   health {4} / {5}" -f `
                            $part.DiskNumber, $part.PartitionNumber,
                            $(if ($dk) { $dk.FriendlyName } else { '?' }),
                            $(if ($dk) { "{0:N1} GB" -f ($dk.Size / 1GB) } else { '?' }),
                            $(if ($pd) { $pd.HealthStatus } else { '?' }),
                            $(if ($pd) { $pd.OperationalStatus } else { '?' }))
            }
        } catch { }

        # ---- the verdict, stated rather than left to the reader --------------------------------
        Write-Host ''
        if ($bad.Count -eq 0) {
            Ok 'cold reads all correct - this card returns its large files to a PC'
            Write-Host '      So if the board still says "Failed to load", the card is NOT the problem and'
            Write-Host '      reflashing it again will not help. That failure is on the BOARD side of the'
            Write-Host '      read: slot contact, or the supply sagging during a sustained multi-MB read.'
            Write-Host '      Next: reseat the card, try a different PSU, then try a different card.'
        } else {
            Write-Host 'FAIL  the card did not return what was written to it:' -ForegroundColor Red
            $bad | ForEach-Object { Write-Host ("        {0}" -f $_) -ForegroundColor Red }
            Write-Host '      This is the card itself, and a reflash will not fix it. Replace it.'
        }
    }
    catch {
        Write-Host ("WARN  diagnostics could not run: {0}" -f $_.Exception.Message)
        Write-Host '      The deploy above still stands - this section only reads.'
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
