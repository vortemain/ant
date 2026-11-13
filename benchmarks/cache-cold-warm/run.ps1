# Cold/warm cache benchmark (synthetic, single machine). See
# docs/benchmarks/methodology.md and the report next to the results.
#
# Workload: N independent actions, each `tar -czf out-i.tgz -C src-i .` over a
# deterministic generated input tree (System32 tar.exe, so it runs as the
# network-isolation sandbox account). Scenarios, each with 1 warmup + -Reps
# measured runs at --jobs -Jobs:
#   direct-nocache     plain local executor, no cache
#   sandbox-nocache    --sandbox (restricted token), no cache
#   isolated-nocache   --network-isolated, no cache
#   isolated-cold      --network-isolated --cache-dir <fresh empty dir>
#   isolated-warm      --network-isolated --cache-dir <populated>, outputs deleted first
# Every observation is written to results.csv; nothing is discarded.
param(
    [string]$Ant = (Join-Path $PSScriptRoot '..\..\target\release\ant.exe'),
    [string]$WorkDir = (Join-Path $env:TEMP 'ant-bench-cache-cold-warm'),
    [int]$Actions = 8,
    [int]$FilesPerAction = 100,
    [int]$FileKiB = 100,
    [int]$Reps = 5,
    [int]$Jobs = 4
)
$ErrorActionPreference = 'Stop'
$Ant = (Resolve-Path $Ant).Path
$OutDir = $PSScriptRoot

# ---- environment manifest ----
$cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
$os = Get-CimInstance Win32_OperatingSystem
$disk = Get-PhysicalDisk | Select-Object -First 1
$repo = Join-Path $PSScriptRoot '..\..'
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
    rustc = (rustc --version)
    workload = "$Actions actions x $FilesPerAction files x $FileKiB KiB, tar -czf, jobs=$Jobs, reps=$Reps (+1 warmup)"
} | ConvertTo-Json | Set-Content (Join-Path $OutDir 'environment.json') -Encoding ASCII

# ---- workload (deterministic content) ----
Remove-Item $WorkDir -Recurse -Force -ErrorAction SilentlyContinue
$ws = Join-Path $WorkDir 'ws'
New-Item -ItemType Directory (Join-Path $ws '.ant\tmp') -Force | Out-Null
$rng = New-Object System.Random 42
$buffer = New-Object byte[] ($FileKiB * 768)
for ($a = 0; $a -lt $Actions; $a++) {
    $src = Join-Path $ws "src-$a"
    New-Item -ItemType Directory $src | Out-Null
    for ($f = 0; $f -lt $FilesPerAction; $f++) {
        $rng.NextBytes($buffer)
        [IO.File]::WriteAllText((Join-Path $src "f$f.txt"), [Convert]::ToBase64String($buffer).Substring(0, $FileKiB * 1024))
    }
}
$tar = 'C:\Windows\System32\tar.exe'
$tarDigest = 'sha256:' + (Get-FileHash $tar -Algorithm SHA256).Hash.ToLower()
$tarVersion = (& $tar --version | Select-Object -First 1)
$plan = @{ schema_version = 1; actions = @(for ($a = 0; $a -lt $Actions; $a++) {
    @{ action = @{
        schema_version = 1; id = "pack-$a"
        argv = @($tar, '-czf', "out-$a.tgz", '-C', "src-$a", '.')
        working_directory = '.'
        inputs = @(@{ path = "src-$a"; kind = 'tree' })
        outputs = @(@{ path = "out-$a.tgz"; kind = 'file' })
        environment = @(@('TEMP', '.ant\tmp'), @('TMP', '.ant\tmp'))
        cache_policy = 'eligible'
        toolchain = @{ name = 'bsdtar'; version = $tarVersion; executable_digest = $tarDigest }
        timeout_ms = 600000
    }; dependencies = @() }
}) }
$planPath = Join-Path $WorkDir 'plan.json'
$plan | ConvertTo-Json -Depth 8 | Set-Content $planPath -Encoding ASCII

function Remove-Outputs { Get-ChildItem $ws -Filter 'out-*.tgz' | Remove-Item -Force }

$rows = New-Object System.Collections.Generic.List[object]
function Invoke-Scenario([string]$Name, [string[]]$Flags, [scriptblock]$Before) {
    for ($rep = 0; $rep -le $Reps; $rep++) {
        & $Before
        $sw = [Diagnostics.Stopwatch]::StartNew()
        $out = & $Ant run --workspace $ws --plan $planPath --jobs $Jobs @Flags 2>&1 | ForEach-Object { "$_" }
        $sw.Stop()
        $exit = $LASTEXITCODE
        $row = [pscustomobject]@{
            scenario = $Name; rep = $rep; warmup = ($rep -eq 0); wall_ms = $sw.ElapsedMilliseconds; exit = $exit
            executed = @($out | Select-String 'status=succeeded').Count
            reused = @($out | Select-String 'status=reused').Count
            failed = @($out | Select-String 'status=failed').Count
        }
        $rows.Add($row)
        Write-Host ("{0,-17} rep {1} {2,7} ms exit={3} executed={4} reused={5}" -f $Name, $rep, $row.wall_ms, $exit, $row.executed, $row.reused)
    }
}

$cache = Join-Path $WorkDir 'cache'
Invoke-Scenario 'direct-nocache' @() { Remove-Outputs }
Invoke-Scenario 'sandbox-nocache' @('--sandbox') { Remove-Outputs }
Invoke-Scenario 'isolated-nocache' @('--network-isolated') { Remove-Outputs }
Invoke-Scenario 'isolated-cold' @('--network-isolated', '--cache-dir', $cache) { Remove-Outputs; Remove-Item $cache -Recurse -Force -ErrorAction SilentlyContinue }
Invoke-Scenario 'isolated-warm' @('--network-isolated', '--cache-dir', $cache) { Remove-Outputs }

$rows | Export-Csv (Join-Path $OutDir 'results.csv') -NoTypeInformation -Encoding ASCII
"`nmedian wall ms over measured reps (warmup excluded):"
$rows | Where-Object { -not $_.warmup } | Group-Object scenario | ForEach-Object {
    $v = @($_.Group.wall_ms | Sort-Object)
    "{0,-17} median={1} min={2} max={3} n={4}" -f $_.Name, $v[[int][math]::Floor(($v.Count - 1) / 2)], $v[0], $v[-1], $v.Count
}
