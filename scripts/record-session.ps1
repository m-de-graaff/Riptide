<#
.SYNOPSIS
Records memory, paging, disk and per-core CPU use while a program runs, to tell memory
overflow apart from CPU cost (open question Q11 in docs/design/2026-11-27-architecture.md).

.DESCRIPTION
Samples once per interval until the program exits, the duration runs out, or Ctrl+C.
Writes samples.csv and summary.md to local/sessions/<time>-<name>/. Nothing in local/ is
committed.

Uses the raw performance classes through CIM, so it works on any display language and on
Windows PowerShell 5.1 as well as PowerShell 7.

.EXAMPLE
scripts/record-session.ps1 -ProcessName VIctory
Start the runtime first, then run this, play the open-world section, and quit the runtime.

.EXAMPLE
scripts/record-session.ps1 -ProcessName VIctory -WaitForStart
Waits for the runtime to appear, so the recording covers its whole life.
#>
[CmdletBinding()]
param(
    # Process name without ".exe". Every process with this name is summed.
    [Parameter(Mandatory = $true)][string]$ProcessName,
    [ValidateRange(1, 60)][int]$IntervalSeconds = 1,
    # 0 means until the process exits or Ctrl+C.
    [int]$DurationSeconds = 0,
    [switch]$WaitForStart,
    # Default: local/sessions in this repository.
    [string]$OutDir = ''
)

Set-StrictMode -Version 2
$ErrorActionPreference = 'Stop'
if (-not $OutDir) { $OutDir = Join-Path (Split-Path $PSScriptRoot -Parent) 'local/sessions' }

# Pages read from disk per second above which a sample counts as "paging in".
# 1,000 pages is 4 MB/s.
$PagingPagesPerSecond = 1000
$GB = 1GB

function Get-TargetProcess {
    @(Get-Process -Name $ProcessName -ErrorAction SilentlyContinue)
}

function Get-Snapshot {
    $mem = Get-CimInstance Win32_PerfRawData_PerfOS_Memory
    $cpu = @(Get-CimInstance Win32_PerfRawData_PerfOS_Processor | Where-Object { $_.Name -ne '_Total' })
    $disk = Get-CimInstance Win32_PerfRawData_PerfDisk_PhysicalDisk | Where-Object { $_.Name -eq '_Total' }
    $page = Get-CimInstance Win32_PerfRawData_PerfOS_PagingFile | Where-Object { $_.Name -eq '_Total' }
    $procs = @(Get-TargetProcess)
    $private = 0L; $working = 0L
    foreach ($p in $procs) {
        try { $private += $p.PrivateMemorySize64; $working += $p.WorkingSet64 } catch { }
    }
    $pageFilePercent = 0.0
    if ($page -and $page.PercentUsage_Base -gt 0) {
        $pageFilePercent = 100.0 * $page.PercentUsage / $page.PercentUsage_Base
    }
    [pscustomobject]@{
        Time            = Get-Date
        ProcessCount    = $procs.Count
        PrivateBytes    = $private
        WorkingSetBytes = $working
        CommittedBytes  = [long]$mem.CommittedBytes
        CommitLimit     = [long]$mem.CommitLimit
        AvailableBytes  = [long]$mem.AvailableBytes
        PagesInput      = [long]$mem.PagesInputPersec     # cumulative in the raw class
        PagesOutput     = [long]$mem.PagesOutputPersec
        PageFilePercent = $pageFilePercent
        DiskReadBytes   = [long]$disk.DiskReadBytesPersec
        DiskWriteBytes  = [long]$disk.DiskWriteBytesPersec
        # Per logical core: cumulative idle time and its timestamp, both in 100 ns units.
        Cpu             = @($cpu | Sort-Object { [int]$_.Name } | ForEach-Object {
                [pscustomobject]@{ Name = $_.Name; Idle = [long]$_.PercentProcessorTime; Stamp = [long]$_.Timestamp_Sys100NS }
            })
    }
}

# Turns two snapshots into one row of rates.
function Get-Row($a, $b) {
    $seconds = ($b.Time - $a.Time).TotalSeconds
    if ($seconds -le 0) { return $null }
    $row = [ordered]@{
        time              = $b.Time.ToString('s')
        seconds           = [math]::Round($seconds, 2)
        processes         = $b.ProcessCount
        private_gb        = [math]::Round($b.PrivateBytes / $GB, 3)
        working_set_gb    = [math]::Round($b.WorkingSetBytes / $GB, 3)
        committed_gb      = [math]::Round($b.CommittedBytes / $GB, 3)
        commit_limit_gb   = [math]::Round($b.CommitLimit / $GB, 3)
        available_gb      = [math]::Round($b.AvailableBytes / $GB, 3)
        pages_in_per_s    = [math]::Round(($b.PagesInput - $a.PagesInput) / $seconds, 0)
        pages_out_per_s   = [math]::Round(($b.PagesOutput - $a.PagesOutput) / $seconds, 0)
        page_file_percent = [math]::Round($b.PageFilePercent, 1)
        disk_read_mb_s    = [math]::Round(($b.DiskReadBytes - $a.DiskReadBytes) / 1MB / $seconds, 1)
        disk_write_mb_s   = [math]::Round(($b.DiskWriteBytes - $a.DiskWriteBytes) / 1MB / $seconds, 1)
    }
    for ($i = 0; $i -lt $b.Cpu.Count; $i++) {
        $elapsed = $b.Cpu[$i].Stamp - $a.Cpu[$i].Stamp
        $busy = 0.0
        if ($elapsed -gt 0) {
            $busy = 100.0 * (1.0 - ($b.Cpu[$i].Idle - $a.Cpu[$i].Idle) / $elapsed)
        }
        $row["cpu$($b.Cpu[$i].Name)"] = [math]::Round([math]::Min(100.0, [math]::Max(0.0, $busy)), 1)
    }
    [pscustomobject]$row
}

function Get-Stat($rows, $name) {
    $rows | Measure-Object -Property $name -Maximum -Minimum -Average -Sum
}

function Write-Summary($rows, $dir) {
    $ramGb = [math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / $GB, 1)
    $private = Get-Stat $rows 'private_gb'
    $working = Get-Stat $rows 'working_set_gb'
    $committed = Get-Stat $rows 'committed_gb'
    $available = Get-Stat $rows 'available_gb'
    $pagesIn = Get-Stat $rows 'pages_in_per_s'
    $pagesOut = Get-Stat $rows 'pages_out_per_s'
    $pageFile = Get-Stat $rows 'page_file_percent'
    $read = Get-Stat $rows 'disk_read_mb_s'
    $limit = ($rows | Select-Object -Last 1).commit_limit_gb
    $pagingSamples = @($rows | Where-Object { $_.pages_in_per_s -ge $PagingPagesPerSecond }).Count
    $pagedInGb = [math]::Round(($rows | ForEach-Object { $_.pages_in_per_s * $_.seconds } | Measure-Object -Sum).Sum * 4096 / $GB, 2)
    $pagedOutGb = [math]::Round(($rows | ForEach-Object { $_.pages_out_per_s * $_.seconds } | Measure-Object -Sum).Sum * 4096 / $GB, 2)

    $lines = @(
        "# Session of $ProcessName, $((Get-Date).ToString('yyyy-MM-dd HH:mm'))"
        ''
        "$($rows.Count) samples over $([math]::Round(($rows | Measure-Object -Property seconds -Sum).Sum, 0)) s. Machine RAM: $ramGb GB."
        ''
        '| Measure | Value |'
        '| --- | --- |'
        "| Peak private memory | $($private.Maximum) GB |"
        "| Peak working set (in RAM) | $($working.Maximum) GB |"
        "| Peak system commit | $($committed.Maximum) GB of $limit GB limit |"
        "| Lowest available RAM | $($available.Minimum) GB |"
        "| Peak page file use | $($pageFile.Maximum) % |"
        "| Paged in from disk | $pagedInGb GB in total, peak $($pagesIn.Maximum) pages/s |"
        "| Paged out to disk | $pagedOutGb GB in total, peak $($pagesOut.Maximum) pages/s |"
        "| Samples paging in ($PagingPagesPerSecond pages/s or more) | $pagingSamples of $($rows.Count) |"
        "| Disk reads | mean $([math]::Round($read.Average, 1)) MB/s, peak $($read.Maximum) MB/s |"
        ''
        '| Logical core | Mean % | Peak % | Samples at 90 % or more |'
        '| --- | --- | --- | --- |'
    )
    $cores = @($rows[0].PSObject.Properties.Name | Where-Object { $_ -match '^cpu\d+$' })
    foreach ($core in $cores) {
        $s = Get-Stat $rows $core
        $busy = @($rows | Where-Object { $_.$core -ge 90 }).Count
        $lines += "| $($core.Substring(3)) | $([math]::Round($s.Average, 1)) | $($s.Maximum) | $busy of $($rows.Count) |"
    }
    $lines += @(
        ''
        'Reading it: private memory above the machine''s RAM, with page-outs and a busy page file, means'
        'memory overflow. "Paged in" also counts ordinary reads of memory-mapped files, so page-ins'
        'alone do not prove it. Cores pinned at 90 % or more with memory to spare means CPU cost.'
    )
    $path = Join-Path $dir 'summary.md'
    $lines | Set-Content -Path $path -Encoding UTF8
    $lines | ForEach-Object { Write-Host $_ }
    Write-Host ''
    Write-Host "Written: $path"
}

if ($WaitForStart) {
    Write-Host "Waiting for $ProcessName to start..."
    while (@(Get-TargetProcess).Count -eq 0) { Start-Sleep -Milliseconds 500 }
}
if (@(Get-TargetProcess).Count -eq 0) {
    throw "No process named '$ProcessName' is running. Start it first, or pass -WaitForStart."
}

$dir = Join-Path $OutDir ("{0}-{1}" -f (Get-Date).ToString('yyyy-MM-dd-HHmmss'), $ProcessName)
New-Item -ItemType Directory -Force $dir | Out-Null
$csv = Join-Path $dir 'samples.csv'
$rows = New-Object System.Collections.Generic.List[object]
$started = Get-Date
Write-Host "Recording $ProcessName to $dir. Ctrl+C or quitting the program stops it."

try {
    $previous = Get-Snapshot
    while ($true) {
        Start-Sleep -Seconds $IntervalSeconds
        $current = Get-Snapshot
        if ($current.ProcessCount -eq 0) { Write-Host "$ProcessName has exited."; break }
        $row = Get-Row $previous $current
        $previous = $current
        if ($null -eq $row) { continue }
        $rows.Add($row)
        $row | Export-Csv -Path $csv -Append -NoTypeInformation -Encoding UTF8
        if ($rows.Count % 10 -eq 0) {
            Write-Host ("{0,5} s  private {1,6} GB  available {2,5} GB  pages in {3,7}/s  out {4,7}/s" -f `
                    [int]((Get-Date) - $started).TotalSeconds, $row.private_gb, $row.available_gb, $row.pages_in_per_s, $row.pages_out_per_s)
        }
        if ($DurationSeconds -gt 0 -and ((Get-Date) - $started).TotalSeconds -ge $DurationSeconds) { break }
    }
}
finally {
    if ($rows.Count -gt 0) { Write-Summary $rows.ToArray() $dir } else { Write-Host 'No samples recorded.' }
}
