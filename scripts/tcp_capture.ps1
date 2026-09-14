<#
.SYNOPSIS
    Capture TCP traffic on port 7777 with pktmon, to find out whether a board's SYN reaches this
    machine's NIC at all.

.DESCRIPTION
    The question this answers cannot be answered from a socket. A listening socket sees a connection
    only once Windows has accepted it, so "nothing arrived" and "something arrived and was discarded"
    look identical from user space - and they are completely different faults. pktmon sits at the
    NIC, below the firewall and below any filtering driver, so it sees the packet either way.

    Used while chasing a Raspberry Pi 2 whose TCP SYNs never reached this laptop while its ICMP did,
    with Malwarebytes stopped and an explicit firewall allow rule in place.

.PARAMETER start
    Reset filters, add a TCP/7777 filter, and begin capturing to build\pm.etl.

.PARAMETER stop
    Stop the capture, convert it to build\pm.txt, and report how many packets were seen.

.EXAMPLE
    .\scripts\tcp_capture.ps1 -start
    # ... run `tcp 192.168.4.40 7777 hello` on the board ...
    .\scripts\tcp_capture.ps1 -stop

.NOTES
    Must be run from an ELEVATED PowerShell - pktmon needs administrator. The script checks and says
    so rather than failing with a driver error that reads like a broken tool.
#>
[CmdletBinding()]
param(
    [switch]$start,
    [switch]$stop
)

$ErrorActionPreference = 'Stop'

# Anchor every path to the repository, not to wherever the shell happens to be. Running this from a
# different directory would otherwise write the capture somewhere nobody looks for it.
$repo  = Split-Path -Parent $PSScriptRoot
$build = Join-Path $repo 'build'
$etl   = Join-Path $build 'pm.etl'
$txt   = Join-Path $build 'pm.txt'

function Require-Admin {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    $pr = New-Object Security.Principal.WindowsPrincipal($id)
    if (-not $pr.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        Write-Host "tcp-capture: this needs an ADMINISTRATOR PowerShell." -ForegroundColor Red
        Write-Host "             pktmon talks to a kernel driver; without elevation it fails with"
        Write-Host "             'cannot find the file specified', which reads like a missing tool"
        Write-Host "             rather than a missing privilege."
        exit 1
    }
}

if (-not $start -and -not $stop) {
    Write-Host "usage: .\scripts\tcp_capture.ps1 -start   |   -stop"
    Write-Host "       -start  begin capturing TCP port 7777 to build\pm.etl"
    Write-Host "       -stop   stop, convert to build\pm.txt, and summarise"
    exit 0
}

Require-Admin
if (-not (Test-Path $build)) { New-Item -ItemType Directory -Path $build | Out-Null }

if ($start) {
    # A leftover session from an earlier run would keep capturing into the old file and make the next
    # result a mixture of two experiments. Stop it first, and do not treat "nothing was running" as
    # an error.
    try { pktmon stop 2>&1 | Out-Null } catch { }
    if (Test-Path $etl) { Remove-Item $etl -Force }
    if (Test-Path $txt) { Remove-Item $txt -Force }

    pktmon filter remove | Out-Null
    pktmon filter add TcpTest -t TCP -p 7777 | Out-Null

    Write-Host "tcp-capture: filters now in force:" -ForegroundColor Cyan
    pktmon filter list

    pktmon start --capture --pkt-size 128 -f $etl | Out-Null
    Write-Host ""
    Write-Host "tcp-capture: CAPTURING to $etl" -ForegroundColor Green
    Write-Host "             now run on the board:   tcp 192.168.4.40 7777 hello"
    Write-Host "             then:                   .\scripts\tcp_capture.ps1 -stop"
}

if ($stop) {
    pktmon stop | Out-Null
    if (-not (Test-Path $etl)) {
        Write-Host "tcp-capture: no capture file at $etl - was -start run?" -ForegroundColor Red
        exit 1
    }
    pktmon etl2txt $etl -o $txt | Out-Null

    if (-not (Test-Path $txt)) {
        Write-Host "tcp-capture: conversion produced no $txt" -ForegroundColor Red
        exit 1
    }

    # Count DATA lines, not file lines: etl2txt writes a header, and reporting its lines as packets
    # would turn an empty capture into a reassuring non-zero number. An instrument that reads a zero
    # it did not earn is the failure mode this whole exercise keeps running into.
    $lines = Get-Content $txt
    $pkts  = @($lines | Where-Object { $_ -match '\d+\.\d+\.\d+\.\d+' })

    Write-Host ""
    Write-Host "tcp-capture: $txt" -ForegroundColor Cyan
    Write-Host "             $($pkts.Count) packet line(s) mentioning an IPv4 address"
    Write-Host ""
    if ($pkts.Count -eq 0) {
        Write-Host "  EMPTY: no TCP/7777 packet reached this machine's NIC at all." -ForegroundColor Yellow
        Write-Host "  The board's SYN is not arriving - so the fault is upstream of this laptop"
        Write-Host "  (the board's own transmit path, or the router not forwarding)."
    } else {
        Write-Host "  Packets DID arrive. They reach the NIC and are being discarded above it," -ForegroundColor Yellow
        Write-Host "  which means the frame is malformed in a way QEMU's SLIRP tolerated."
        Write-Host ""
        Write-Host "  first few:"
        $pkts | Select-Object -First 12 | ForEach-Object { Write-Host "    $_" }
    }
    Write-Host ""
    Write-Host "tcp-capture: send build\pm.txt for the full decode."
}
