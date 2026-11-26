<#
.SYNOPSIS
    Pre-warm: translates the title's shaders and makes its pipelines into the
    cache a run of this tree will read (`victory prebuild`), until it ends.

.DESCRIPTION
    The steps of a pre-warm, for this tree:

    1. The cache folder is the run's: `victory config` resolves it as a run
       does (VICTORY_CACHE_DIR, else `cache_dir` of victory.toml in -Repo,
       else %LOCALAPPDATA%\victory\cache). Run this from the same place, with
       the same environment, as the game.
    2. The switches that pick what a run translates or makes are taken from
       this shell's environment and printed; they must be the run's
       (see $KeySwitches below). A run with other values finds nothing it
       can use and translates again.
    3. `victory prebuild` translates every shader of the title's shader pack
       and makes every pipeline into <cache>\shaders and <cache>\pipelines.
       This tree has no `shaders` or `warm --used` command: the pre-build
       does both, and every run already writes the pipelines it used to
       <cache>\pipelines\early-pipelines.txt (what other trees ask for with
       VICTORY_PIPELINE_BOOK=1), which the next run starts at once.
    4. A pre-build that the driver's compiler kills is run again: it finds
       the culprit in its journal, lists it in
       <cache>\pipelines\host-compiler-crashes.txt and goes on without it.
    5. When it ends, a stamp (<cache>\prewarm-stamp.json) records what it was
       made with. The next call with the same sources, driver, executable,
       folders and switches says "up to date" and skips the ~4 minutes a
       pre-build takes even with nothing to make. -Force runs it anyway.

    It never deletes anything from the cache (old builds' entries stay).

.PARAMETER Build
    The build whose executable runs the pre-build (target\<Build>\victory.exe).
    Translations and pipelines do not depend on it: release and diag share them.

.PARAMETER Check
    Only say whether a pre-warm is needed (exit code 0: up to date, 1: needed).

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\prewarm.ps1
.EXAMPLE
    $env:VICTORY_RAYS = '1'; powershell -ExecutionPolicy Bypass -File scripts\prewarm.ps1 -Force
#>
param(
    [ValidateSet('release', 'diag')] [string]$Build = 'release',
    [string]$Repo = (Split-Path -Parent $PSScriptRoot),
    [int]$Jobs = 0,
    [int]$MaxAttempts = 8,
    [switch]$Force,
    [switch]$Check
)

$ErrorActionPreference = 'Stop'

# Switches that change what is translated or made, or where it is kept
# (victory-shader emit, victory-gpu shader_cache and pipeline, victory-core config).
$KeySwitches = @(
    'VICTORY_CONFIG',            # which victory.toml
    'VICTORY_CACHE_DIR',         # where the cache is
    'VICTORY_GAME_DIR',          # where the shader pack is
    'VICTORY_ADAPTER',           # which GPU: wave sizes and the pipeline library are its own
    'VICTORY_WARP',
    'VICTORY_RAYS',              # part of the shader cache's key
    'VICTORY_SHADER_LOOP_LIMIT', # part of the shader cache's key (diagnostics builds)
    'VICTORY_SHADER_PATCH',      # entries of their own (diagnostics builds)
    'VICTORY_SHADER_BUILD_OF',   # another build's entries (diagnostics builds)
    'VICTORY_SHADER_SWAP',       # replaced shaders (diagnostics builds)
    'VICTORY_SKIP_PIPELINES',    # pipelines left out (diagnostics builds)
    'VICTORY_HAIR',              # hair passes made or not
    'VICTORY_ACCC'               # compute pipelines without DXIL made or not
)

Set-Location $Repo
$exe = Join-Path $Repo "target\$Build\victory.exe"
if (-not (Test-Path $exe)) { throw "no $($exe): build it first (cargo build --profile $Build -p victory)" }

$running = Get-Process victory -ErrorAction SilentlyContinue
if ($running) { throw "victory.exe is running (pid $($running.Id -join ', ')): the pre-build and a run write the same pipeline library" }

# 1. The run's folders, resolved by the executable itself.
$config = @{}
foreach ($line in (& $exe config 2>$null)) {
    if ($line -match '^\s*([a-z_ ]+?)\s*:\s*(.+)$') { $config[$Matches[1].Trim()] = $Matches[2].Trim() }
}
$cache = $config['cache_dir']
if (-not $cache) { throw 'victory config did not name a cache folder' }
"cache folder : $cache"
"game folder  : $($config['game_dir'])"
"config file  : $($config['config file'])"

# 2. The switches.
$switches = [ordered]@{}
foreach ($k in $KeySwitches) {
    $v = [Environment]::GetEnvironmentVariable($k, 'Process')
    if ($null -ne $v -and $v -ne '') { $switches[$k] = $v }
}
if ($switches.Count -eq 0) { 'switches     : none set (a run must not set any of them either)' }
else { foreach ($k in $switches.Keys) { "switch       : $k=$($switches[$k])" } }

# What the cache depends on, for the stamp.
function Get-SourceState {
    $head = (& git -C $Repo rev-parse 'HEAD:crates' 2>$null)
    $dirty = (& git -C $Repo status --porcelain -- crates apps 2>$null) -join "`n"
    if ($dirty) { $head = "$head+uncommitted:" + [BitConverter]::ToString(
        [Security.Cryptography.SHA256]::Create().ComputeHash([Text.Encoding]::UTF8.GetBytes($dirty))).Replace('-', '').Substring(0, 16) }
    $head
}
$gpu = Get-CimInstance Win32_VideoController | Where-Object { $_.AdapterCompatibility -notmatch 'Microsoft' } |
    ForEach-Object { "$($_.Name) $($_.DriverVersion)" }
$exeItem = Get-Item $exe
$state = [ordered]@{
    sources  = Get-SourceState
    gpu      = ($gpu -join '; ')
    exe      = "$($exeItem.Length) $($exeItem.LastWriteTimeUtc.ToString('o'))"
    cache    = $cache
    game     = $config['game_dir']
    switches = ($switches.Keys | ForEach-Object { "$_=$($switches[$_])" }) -join ' '
}
$stampFile = Join-Path $cache 'prewarm-stamp.json'
$stamp = $null
if (Test-Path $stampFile) { $stamp = Get-Content $stampFile -Raw | ConvertFrom-Json }
$same = $stamp -and $stamp.finished -and (@($state.Keys | Where-Object { [string]$stamp.$_ -ne [string]$state[$_] }).Count -eq 0)
if ($same -and -not $Force) {
    "up to date   : pre-warmed $($stamp.finished) with these sources, driver, executable, folders and switches"
    exit 0
}
if ($stamp) {
    foreach ($k in $state.Keys) { if ([string]$stamp.$k -ne [string]$state[$k]) { "changed      : $k ($($stamp.$k) -> $($state[$k]))" } }
} else { 'changed      : no pre-warm recorded for this cache folder' }
if ($Check) { exit 1 }

# 3-4. The pre-build, again after a crash of the driver's compiler.
$logDir = Join-Path $cache 'prewarm-logs'
New-Item -ItemType Directory -Force $logDir | Out-Null
$pbArgs = @('prebuild')
if ($Jobs -gt 0) { $pbArgs += @('--jobs', "$Jobs") }
$ended = $false
for ($attempt = 1; $attempt -le $MaxAttempts; $attempt++) {
    $tag = (Get-Date).ToString('yyyyMMdd-HHmmss')
    $out = Join-Path $logDir "$tag-out.txt"
    $err = Join-Path $logDir "$tag-err.txt"
    "pre-build    : attempt $attempt of $MaxAttempts (logs $logDir\$tag-*.txt)"
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $p = Start-Process -FilePath $exe -ArgumentList $pbArgs -WorkingDirectory $Repo -NoNewWindow -PassThru -Wait `
        -RedirectStandardOutput $out -RedirectStandardError $err
    $code = $p.ExitCode
    "               exit $code after $([int]$sw.Elapsed.TotalSeconds) s"
    Get-Content $out, $err -ErrorAction SilentlyContinue |
        Where-Object { $_ -match 'pipelines\s+\d+ in the pack|skipped|crashing the host|shaders? .*(cache|translate)' } |
        ForEach-Object { "               $_" }
    if ($code -eq 0) { $ended = $true; break }
}
if (-not $ended) { throw "the pre-build did not end in $MaxAttempts attempts: see $logDir" }

# 5. The stamp.
$state['finished'] = (Get-Date).ToString('o')
$state | ConvertTo-Json | Set-Content -Path $stampFile -Encoding utf8
"done         : stamp $stampFile"
'check a run  : its log should say "shaders from the cache and translated so far ... misses=0"'
'               and "host pipelines all made made=<small>"'
