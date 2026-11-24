<#
.SYNOPSIS
Checks that this machine has what VIctory needs to build and run, and prints
the versions it finds, so one setup can be compared with another.

.DESCRIPTION
Read only: it queries the system and runs version commands; it installs and
changes nothing. Each line starts with "ok", "MISS" (required and missing)
or "warn" (optional, or below a heuristic threshold). Exit code 0 when every
required item is present, 1 otherwise. How to install what is missing:
docs/windows-setup.md.

Windows PowerShell 5.1 or later.

.EXAMPLE
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\check-env.ps1
#>
[CmdletBinding()]
param()

$repo = Split-Path -Parent $PSScriptRoot
$script:missing = New-Object System.Collections.Generic.List[string]

function Show-Check([string]$name, [string]$value, [bool]$ok, [bool]$required = $true) {
    if ($ok) { $mark = 'ok  ' } elseif ($required) { $mark = 'MISS' } else { $mark = 'warn' }
    Write-Output ('{0} {1,-24} {2}' -f $mark, $name, $value)
    if (-not $ok -and $required) { $script:missing.Add($name) }
}

function Show-Section([string]$title) {
    Write-Output ''
    Write-Output "== $title"
}

# Runs a native command and returns its first line of standard output, or $null.
function Get-FirstLine([string]$exe, [string[]]$arguments) {
    $cmd = Get-Command $exe -ErrorAction SilentlyContinue
    if (-not $cmd) { return $null }
    $out = & $cmd.Source @arguments 2>$null | Select-Object -First 1
    if ($out) { return "$out".Trim() }
    return $null
}

Add-Type -Namespace VictoryEnv -Name Native -MemberDefinition @'
[DllImport("kernel32.dll")]
public static extern bool IsProcessorFeaturePresent(uint feature);
'@

# --- Windows, CPU, memory -----------------------------------------------------
Show-Section 'System'
$os = Get-CimInstance Win32_OperatingSystem
$is64 = [Environment]::Is64BitOperatingSystem
Show-Check 'Windows' ("{0} build {1}" -f $os.Caption, $os.BuildNumber) ($is64 -and [int]$os.BuildNumber -ge 19041)

$cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
# PF_AVX2_INSTRUCTIONS_AVAILABLE (winnt.h).
$avx2 = [VictoryEnv.Native]::IsProcessorFeaturePresent(40)
Show-Check 'CPU' ("{0} ({1}C/{2}T)" -f $cpu.Name.Trim(), $cpu.NumberOfCores, $cpu.NumberOfLogicalProcessors) $true
Show-Check 'CPU AVX2' "$avx2" $avx2
if ($cpu.Manufacturer -eq 'AuthenticAMD') {
    Show-Check 'Console-only instructions' 'AMD CPU: clzero, rdpru, mwaitx, SSE4a run natively from Zen 2 on' $true
} else {
    Show-Check 'Console-only instructions' 'non-AMD CPU: emulated by exception (slower)' $false $false
}

$ramGB = [math]::Round($os.TotalVisibleMemorySize / 1MB, 1)
$commitGB = [math]::Round($os.TotalVirtualMemorySize / 1MB, 1)
$freeCommitGB = [math]::Round($os.FreeVirtualMemory / 1MB, 1)
Show-Check 'RAM' "$ramGB GB" ($ramGB -ge 16) $false
# Heuristic: the title commits several GiB at start and uses ~24 GB in the world.
Show-Check 'Commit limit / free' "$commitGB GB / $freeCommitGB GB" ($freeCommitGB -ge 16) $false

# --- GPU ------------------------------------------------------------------------
Show-Section 'GPU'
$displayClass = 'HKLM:\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}'
foreach ($gpu in Get-CimInstance Win32_VideoController) {
    $vram = ''
    $key = Get-ChildItem $displayClass -ErrorAction SilentlyContinue | Where-Object {
        (Get-ItemProperty $_.PSPath -Name DriverDesc -ErrorAction SilentlyContinue).DriverDesc -eq $gpu.Name
    } | Select-Object -First 1
    if ($key) {
        $bytes = (Get-ItemProperty $key.PSPath -Name 'HardwareInformation.qwMemorySize' -ErrorAction SilentlyContinue).'HardwareInformation.qwMemorySize'
        if ($bytes) { $vram = ", {0} GB" -f [math]::Round([double]$bytes / 1GB) }
    }
    Show-Check 'Adapter' ("{0}, driver {1}{2}" -f $gpu.Name, $gpu.DriverVersion, $vram) $true
}
# Shader model 6.6, mesh shaders and 64-lane waves are checked by the GPU tests
# (`cargo test -p victory-shader`), which create a D3D12 device.

$sys32 = Join-Path $env:WINDIR 'System32'
foreach ($dll in 'd3d12.dll', 'D3DCompiler_47.dll', 'xinput1_4.dll', 'xaudio2_9.dll') {
    Show-Check $dll (Join-Path $sys32 $dll) (Test-Path (Join-Path $sys32 $dll))
}
# The D3D12 debug layer (VICTORY_D3D12_DEBUG, VICTORY_D3D12_GBV): Windows'
# optional feature "Graphics Tools".
$layers = Join-Path $sys32 'd3d12SDKLayers.dll'
Show-Check 'Graphics Tools' $layers (Test-Path $layers) $false

# --- Build toolchain ------------------------------------------------------------
Show-Section 'Build toolchain'
Show-Check 'git' "$(Get-FirstLine 'git' @('--version'))" ([bool](Get-Command git -ErrorAction SilentlyContinue))

$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
if ((Test-Path $cargoBin) -and ($env:Path -notlike "*$cargoBin*")) { $env:Path = "$cargoBin;$env:Path" }
$rustup = Get-FirstLine 'rustup' @('--version')
Show-Check 'rustup' "$rustup" ([bool]$rustup)

$pinned = $null
$toolchainFile = Join-Path $repo 'rust-toolchain.toml'
if (Test-Path $toolchainFile) {
    $m = Select-String -Path $toolchainFile -Pattern 'channel\s*=\s*"([^"]+)"' | Select-Object -First 1
    if ($m) { $pinned = $m.Matches[0].Groups[1].Value }
}
Push-Location $repo
try {
    # In the repository, rustup selects (and installs if needed) the pinned toolchain.
    $rustc = Get-FirstLine 'rustc' @('--version')
    $cargo = Get-FirstLine 'cargo' @('--version')
    $clippy = Get-FirstLine 'cargo' @('clippy', '--version')
    $fmt = Get-FirstLine 'rustfmt' @('--version')
} finally {
    Pop-Location
}
$rustcOk = [bool]$rustc -and ((-not $pinned) -or ($rustc -like "rustc $pinned *"))
Show-Check 'rustc' "$rustc (pinned: $pinned)" $rustcOk
Show-Check 'cargo' "$cargo" ([bool]$cargo)
Show-Check 'clippy' "$clippy" ([bool]$clippy)
Show-Check 'rustfmt' "$fmt" ([bool]$fmt)

$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$vsPath = $null
if (Test-Path $vswhere) {
    $vsPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null | Select-Object -First 1
}
if ($vsPath) {
    $vsName = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property displayName 2>$null | Select-Object -First 1
    $vsVer = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property catalog_productDisplayVersion 2>$null | Select-Object -First 1
    $vcVerFile = Join-Path $vsPath 'VC\Auxiliary\Build\Microsoft.VCToolsVersion.default.txt'
    $vcVer = if (Test-Path $vcVerFile) { (Get-Content $vcVerFile -TotalCount 1).Trim() } else { $null }
    $link = if ($vcVer) { Join-Path $vsPath "VC\Tools\MSVC\$vcVer\bin\Hostx64\x64\link.exe" } else { $null }
    Show-Check 'Visual Studio' "$vsName $vsVer" $true
    Show-Check 'MSVC x64 linker' "$vcVer" ([bool]$link -and (Test-Path $link))
} else {
    Show-Check 'MSVC x64 linker' 'no Visual Studio / Build Tools with the C++ x64 tools' $false
}

$kits = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows Kits\Installed Roots' -Name KitsRoot10 -ErrorAction SilentlyContinue).KitsRoot10
$sdk = $null
if ($kits -and (Test-Path (Join-Path $kits 'Lib'))) {
    $sdk = Get-ChildItem (Join-Path $kits 'Lib') -Directory | Where-Object {
        $_.Name -like '10.*' -and (Test-Path (Join-Path $_.FullName 'um\x64\kernel32.lib'))
    } | Sort-Object { [version]$_.Name } | Select-Object -Last 1
}
if ($sdk) {
    Show-Check 'Windows SDK' "$($sdk.Name) ($kits)" $true
    # The DXIL validator, used by tests and `victory translate --validate`.
    $dxil = Join-Path $kits "bin\$($sdk.Name)\x64\dxil.dll"
    $dxc = Join-Path $kits "bin\$($sdk.Name)\x64\dxc.exe"
    Show-Check 'dxil.dll' $dxil (Test-Path $dxil) $false
    Show-Check 'dxc.exe' $dxc (Test-Path $dxc) $false
} else {
    Show-Check 'Windows SDK' 'no Windows 10/11 SDK with x64 libraries' $false
}

# --- This checkout and its configuration ---------------------------------------
Show-Section 'Repository and configuration'
$hooks = Get-FirstLine 'git' @('-C', $repo, 'config', 'core.hooksPath')
Show-Check 'Asset-policy hook' "core.hooksPath = $hooks (cargo xtask install-hooks)" ($hooks -eq '.githooks') $false

$configFile = if ($env:VICTORY_CONFIG) { $env:VICTORY_CONFIG } else { Join-Path $repo 'victory.toml' }
$gameDir = $env:VICTORY_GAME_DIR
$cacheDir = $env:VICTORY_CACHE_DIR
if (Test-Path $configFile) {
    Show-Check 'Config file' $configFile $true
    $text = Get-Content $configFile -Raw
    if (-not $gameDir -and $text -match "(?m)^\s*game_dir\s*=\s*['""]([^'""]+)['""]") { $gameDir = $Matches[1] }
    if (-not $cacheDir -and $text -match "(?m)^\s*cache_dir\s*=\s*['""]([^'""]+)['""]") { $cacheDir = $Matches[1] }
} else {
    Show-Check 'Config file' "$configFile not found (copy victory.example.toml)" $false
}
if (-not $cacheDir) { $cacheDir = Join-Path $env:LOCALAPPDATA 'victory\cache' }

if ($gameDir -and (Test-Path $gameDir)) {
    $manifest = Get-ChildItem $gameDir -Filter 'MicrosoftGame.config' -File -ErrorAction SilentlyContinue | Select-Object -First 1
    $exeName = $null
    if ($manifest) {
        try { $exeName = ([xml](Get-Content $manifest.FullName -Raw)).Game.ExecutableList.Executable | Select-Object -First 1 -ExpandProperty Name } catch { $exeName = $null }
    }
    $exeOk = [bool]$exeName -and (Test-Path (Join-Path $gameDir $exeName))
    Show-Check 'Game directory' $gameDir $true
    Show-Check 'Title executable' "$exeName" $exeOk
} else {
    Show-Check 'Game directory' "'$gameDir' not found (game_dir or VICTORY_GAME_DIR)" $false
}

$root = [System.IO.Path]::GetPathRoot([System.IO.Path]::GetFullPath($cacheDir))
$drive = Get-CimInstance Win32_LogicalDisk -Filter ("DeviceID='{0}'" -f $root.TrimEnd('\'))
if ($drive) {
    $freeGB = [math]::Round($drive.FreeSpace / 1GB)
    # Heuristic: target\ (~10 GB) plus shader and pipeline caches.
    Show-Check 'Cache drive free space' "$cacheDir ($freeGB GB free)" ($freeGB -ge 30) $false
}

Write-Output ''
if ($script:missing.Count -gt 0) {
    Write-Output ("Missing: {0}. See docs/windows-setup.md." -f ($script:missing -join ', '))
    exit 1
}
Write-Output 'Everything required is present.'
exit 0
