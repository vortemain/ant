# Local Rust build benchmark (this repository, single machine). See
# docs/benchmarks/methodology.md and the report next to the results.
#
# Compares `ant build` with plain `cargo build --release --workspace` on the
# ant repository itself in the states a developer actually hits. ant build
# defaults to the release profile, so that is the plain-cargo baseline too.
#
#   cargo-nochange       warm target, nothing changed
#   ant-nochange         same, through ant (local result store hit, D-028)
#   ant-nochange-nometa  ant with the cargo-metadata cache cleared each rep (D-031)
#   cargo-onefile        one source file edited (unique comment per rep), rebuild
#   ant-onefile          same, through ant
#   cargo-switchback     edit -> untimed build -> restore -> timed build
#   ant-switchback       same; the timed build is the switch-back one (D-029)
#   cargo-cold           cargo clean + full rebuild, per rep
#   ant-cold             cargo clean + fresh ant caches + full rebuild, per rep
#
# Every observation is written to results.csv; nothing is discarded. All
# scenarios share one ANT_CACHE_HOME under the working directory, so the
# machine's real cache is never touched. The cold scenarios reset it (they run
# last in the default order for that reason).
#
# The timed command is the whole invocation, including process start. The
# switchback scenarios run an untimed build of the edited state first so the
# timed build is genuinely "return to already-built code"; that untimed build
# is setup, not a measurement. Edits are a one-line comment appended to
# -EditFile, unique per run, restored byte-for-byte at the end and verified;
# a previous crashed run is only rolled back when the file still carries a
# benchmark edit marker, so uncommitted work is never clobbered.
#
# Side effects on the repository: `cargo clean` (cold scenarios) and `ant
# build`'s artifact collection into dist\. ant is rebuilt from the current
# tree first, then copied outside target\ so cold reps cannot delete the
# binary under test.
#
# Long scenarios can be split across invocations with -Scenarios; rows append
# to the same results.csv. Each cold rep is a full clean rebuild (~3 min on
# the 2019 reference laptop), so run them separately with a small -ColdReps:
#   powershell -File benchmarks\local-rust\run.ps1 -Scenarios cargo-cold -ColdReps 1
param(
    [string]$Ant = (Join-Path $PSScriptRoot '..\..\target\release\ant.exe'),
    [string]$WorkDir = (Join-Path $env:TEMP 'ant-bench-local-rust'),
    [int]$Reps = 5,       # measured reps, nochange scenarios
    [int]$EditReps = 4,   # measured reps, one-file-edit scenarios
    [int]$SwitchReps = 3, # measured reps, switchback scenarios
    [int]$ColdReps = 2,   # measured reps, cold scenarios (no warmup rep)
    [string[]]$Scenarios = @('cargo-nochange', 'ant-nochange', 'ant-nochange-nometa',
        'cargo-onefile', 'ant-onefile', 'cargo-switchback', 'ant-switchback',
        'cargo-cold', 'ant-cold'),
    [string]$EditFile = 'crates\build-digest\src\lib.rs',
    [switch]$SkipAntBuild,  # reuse the ant.exe copy already in WorkDir
    [switch]$NoRepoCopy     # keep results in WorkDir only (split runs; copy on the last one)
)
$ErrorActionPreference = 'Stop'
# -Scenarios accepts either a comma-separated list or repeated arguments.
$Scenarios = @($Scenarios | ForEach-Object { $_ -split ',' } | ForEach-Object { $_.Trim() } | Where-Object { $_ })
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$editPath = Join-Path $repo $EditFile
New-Item -ItemType Directory $WorkDir -Force | Out-Null
$csv = Join-Path $WorkDir 'results.csv'   # outside the repo: appended rows must not change ant's inputs
$cacheHome = Join-Path $WorkDir 'cache-home'
$metadataCache = Join-Path $cacheHome 'cargo-metadata.json'
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$marker = 'ant local benchmark'

Push-Location $repo
try {
    # ---- ant under test: built from the current tree, copied out of target\ ----
    if (-not $SkipAntBuild) {
        cargo build --release -p ant-cli
        if ($LASTEXITCODE -ne 0) { throw 'cargo build -p ant-cli failed' }
    }
    $antCopy = Join-Path $WorkDir 'ant.exe'
    if (-not (Test-Path $antCopy)) { Copy-Item (Resolve-Path $Ant).Path $antCopy -Force }
    $Ant = $antCopy
    $env:ANT_CACHE_HOME = $cacheHome

    # ---- edit safety: restore only when the file still carries our marker ----
    $origPath = Join-Path $WorkDir 'edit-original.bin'
    if (([IO.File]::ReadAllText($editPath)) -like "*$marker*") {
        [IO.File]::WriteAllBytes($editPath, [IO.File]::ReadAllBytes($origPath))
        Write-Host "restored $EditFile from a crashed previous run"
    }
    [IO.File]::WriteAllBytes($origPath, [IO.File]::ReadAllBytes($editPath))

    function Restore-Original { [IO.File]::WriteAllBytes($editPath, [IO.File]::ReadAllBytes($origPath)) }
    function Append-Edit([string]$text) {
        $bytes = [IO.File]::ReadAllBytes($editPath)
        $add = [byte[]][Text.Encoding]::ASCII.GetBytes($text)
        if ($bytes.Length -gt 0 -and $bytes[$bytes.Length - 1] -ne 10) { $add = [byte[]]([byte[]]@(10) + $add) }
        [IO.File]::WriteAllBytes($editPath, [byte[]]($bytes + $add))
    }

    # ---- environment manifest ----
    $cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
    $os = Get-CimInstance Win32_OperatingSystem
    $disk = Get-PhysicalDisk | Select-Object -First 1
    $envJson = Join-Path $WorkDir 'environment.json'
    [ordered]@{
        date_utc = (Get-Date).ToUniversalTime().ToString('o')
        cpu = $cpu.Name.Trim(); cores = $cpu.NumberOfCores; logical_processors = $cpu.NumberOfLogicalProcessors
        ram_gib = [math]::Round($os.TotalVisibleMemorySize / 1MB, 1)
        os = "$($os.Caption) $($os.Version) build $($os.BuildNumber)"
        disk = "$($disk.FriendlyName) ($($disk.MediaType), $($disk.BusType))"
        power_plan = ((powercfg /getactivescheme) -replace '.*\((.*)\).*', '$1')
        ant_binary = $Ant
        ant_commit = (git -C $repo rev-parse --short HEAD)
        ant_dirty = [bool](git -C $repo status --porcelain)
        rustc = (rustc --version); cargo = (cargo --version)
        workload = "ant repository, cargo build --release --workspace; edit file $EditFile"
        params = [ordered]@{
            scenarios = ($Scenarios -join ',')
            reps_nochange = $Reps; reps_onefile = $EditReps
            reps_switchback = $SwitchReps; reps_cold = $ColdReps
            skip_ant_build = [bool]$SkipAntBuild; repo_copy = (-not $NoRepoCopy)
        }
    } | ConvertTo-Json -Depth 5 | Set-Content $envJson -Encoding ASCII

    # ---- rows ----
    $rows = New-Object System.Collections.Generic.List[object]
    if (Test-Path $csv) { Import-Csv $csv | ForEach-Object { $rows.Add($_) } }

    # Windows PowerShell 5.1 turns redirected native stderr into a terminating
    # error under $ErrorActionPreference 'Stop'; the builds under measurement
    # keep their stderr merged into the captured output instead.
    function Measure-Rep([string]$Name, [int]$Rep, [bool]$Warmup, [scriptblock]$Setup, [scriptblock]$Build, [string]$Note) {
        & $Setup
        $sw = [Diagnostics.Stopwatch]::StartNew()
        $prevEap = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try { $out = & $Build 2>&1 | ForEach-Object { "$_" } }
        finally { $ErrorActionPreference = $prevEap }
        $sw.Stop()
        $exit = $LASTEXITCODE
        $row = [pscustomobject]@{
            scenario = $Name; rep = $Rep; warmup = $Warmup; wall_ms = $sw.ElapsedMilliseconds; exit = $exit
            executed = @($out | Select-String 'status=succeeded').Count
            reused = @($out | Select-String 'status=reused').Count
            failed = @($out | Select-String 'status=failed').Count
            reported_s = [regex]::Match(($out -join "`n"), 'Built in ([\d.]+)s').Groups[1].Value
            note = $Note
        }
        $rows.Add($row)
        Write-Host ("{0,-20} rep {1} {2,7} ms exit={3} executed={4} reused={5} failed={6}" -f $Name, $Rep, $row.wall_ms, $exit, $row.executed, $row.reused, $row.failed)
        if ($exit -ne 0 -or $row.failed -gt 0) {
            $out | Select-Object -Last 12 | Write-Host
            throw "$Name rep $Rep failed (exit=$exit)"
        }
    }

    $cargoBuild = { cargo build --release --workspace }
    $antBuild = { & $Ant build --verbose --project $repo }
    function Invoke-Scenario([string]$Name, [int]$Measured, [bool]$WarmupRep, [scriptblock]$Setup, [scriptblock]$Build, [string]$Note) {
        $total = $Measured + [int][bool]$WarmupRep
        for ($rep = 0; $rep -lt $total; $rep++) {
            Measure-Rep $Name $rep ($WarmupRep -and $rep -eq 0) $Setup $Build $Note
        }
    }

    foreach ($name in $Scenarios) {
        switch ($name) {
            'cargo-nochange' { Invoke-Scenario $name $Reps $true {} $cargoBuild 'warm target' }
            'ant-nochange' { Invoke-Scenario $name $Reps $true {} $antBuild 'local result store hit' }
            'ant-nochange-nometa' {
                Invoke-Scenario $name $Reps $true { Remove-Item $metadataCache -Force -ErrorAction SilentlyContinue } $antBuild 'cargo metadata cache cleared'
            }
            'cargo-onefile' {
                Invoke-Scenario $name $EditReps $true { Restore-Original; Append-Edit "// $marker edit $stamp rep $Rep`n" } $cargoBuild "edit $EditFile"
                Restore-Original
            }
            'ant-onefile' {
                Invoke-Scenario $name $EditReps $true { Restore-Original; Append-Edit "// $marker edit $stamp rep $Rep`n" } $antBuild "edit $EditFile"
                Restore-Original
            }
            'cargo-switchback' {
                Invoke-Scenario $name $SwitchReps $true {
                    Append-Edit "// $marker switchback B $stamp`n"
                    cargo build --release --workspace | Write-Host
                    if ($LASTEXITCODE -ne 0) { throw 'switchback setup build failed' }
                    Restore-Original
                } $cargoBuild 'timed build returns to already-built code'
            }
            'ant-switchback' {
                Invoke-Scenario $name $SwitchReps $true {
                    Append-Edit "// $marker switchback B $stamp`n"
                    & $Ant build --verbose --project $repo | Write-Host
                    if ($LASTEXITCODE -ne 0) { throw 'switchback setup build failed' }
                    Restore-Original
                } $antBuild 'timed build returns to already-built code'
            }
            'cargo-cold' {
                Invoke-Scenario $name $ColdReps $false { cargo clean | Write-Host } $cargoBuild 'cargo clean, full rebuild'
            }
            'ant-cold' {
                Invoke-Scenario $name $ColdReps $false {
                    cargo clean | Write-Host
                    Remove-Item $cacheHome -Recurse -Force -ErrorAction SilentlyContinue
                } $antBuild 'cargo clean + fresh ant caches'
            }
            default { throw "unknown scenario $name" }
        }
    }

    # ---- results ----
    $rows | Export-Csv $csv -NoTypeInformation -Encoding ASCII
    if (-not $NoRepoCopy) {
        $date = (Get-Date).ToUniversalTime().ToString('yyyy-MM-dd')
        $repoResults = Join-Path $PSScriptRoot "results\$date"
        New-Item -ItemType Directory $repoResults -Force | Out-Null
        Copy-Item $csv $repoResults -Force
        Copy-Item $envJson $repoResults -Force
        Write-Host "results copied to $repoResults"
    }
    "`nmedian wall ms over measured reps (warmups excluded):"
    $rows | Where-Object { "$($_.warmup)" -ne 'True' } | Group-Object scenario | ForEach-Object {
        $v = @($_.Group | ForEach-Object { [long]$_.wall_ms } | Sort-Object)
        "{0,-20} median={1} min={2} max={3} n={4}" -f $_.Name, $v[[int][math]::Floor(($v.Count - 1) / 2)], $v[0], $v[-1], $v.Count
    }
} finally {
    $origPathEnd = Join-Path $WorkDir 'edit-original.bin'
    if ((Test-Path $editPath) -and (Test-Path $origPathEnd)) {
        $origHash = (Get-FileHash $origPathEnd).Hash
        [IO.File]::WriteAllBytes($editPath, [IO.File]::ReadAllBytes($origPathEnd))
        if ((Get-FileHash $editPath).Hash -eq $origHash) { Write-Host 'edit file restored byte-for-byte: PASS' }
        else { Write-Host 'edit file restore: FAIL' }
    }
    Pop-Location
}
