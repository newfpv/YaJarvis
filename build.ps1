$ErrorActionPreference = 'Stop'

function Invoke-BuildTool($File) {
    & $File @args
    if ($LASTEXITCODE -ne 0) { throw "$File exited with code $LASTEXITCODE" }
}

$savedEnvironment = @{}
function Set-BuildEnv($Name, $Value) {
    if (-not $savedEnvironment.ContainsKey($Name)) {
        $savedEnvironment[$Name] = [Environment]::GetEnvironmentVariable($Name)
    }
    [Environment]::SetEnvironmentVariable($Name, $Value)
}

$utf8 = New-Object System.Text.UTF8Encoding($false)
function Write-LfFile($Path, $Content) {
    [IO.File]::WriteAllText($Path, $Content.Replace("`r`n", "`n"), $utf8)
}

$targets = @{
    arm64 = @{ Rust = 'aarch64-unknown-linux-musl'; Zig = 'aarch64-linux-musl'; Cpu = ''; CMake = 'aarch64' }
    armv7 = @{ Rust = 'armv7-unknown-linux-musleabihf'; Zig = 'arm-linux-musleabihf'; Cpu = 'cortex_a7'; CMake = 'armv7' }
}
$arches = if ($env:STATION_ARCHES) { $env:STATION_ARCHES.Trim() -split '\s+' } else { 'arm64', 'armv7' }
foreach ($arch in $arches) {
    if ($arch -cnotin $targets.Keys) {
        throw "Unknown Station architecture: $arch"
    }
}
$version = if ($env:VERSION) { $env:VERSION -creplace '^v' } else { 'dev' }
$repo = if ($env:REPO) { $env:REPO } else { 'newfpv/YaJarvis' }
$zigName = if ($env:ZIG) { $env:ZIG } else { 'zig' }
$zig = (Get-Command $zigName -CommandType Application | Select-Object -First 1).Source
foreach ($name in 'cargo', 'cmake', 'ninja', 'python', 'tar.exe') {
    $null = Get-Command $name -CommandType Application
}

$workspace = Join-Path $PSScriptRoot 'jarvis'
$out = Join-Path $PSScriptRoot 'build/release'
$tools = Join-Path $PSScriptRoot 'build/station-build'
$targetDir = $env:CARGO_TARGET_DIR
if (-not $targetDir) { $targetDir = Join-Path $workspace 'target' }
if (-not [IO.Path]::IsPathRooted($targetDir)) { $targetDir = Join-Path $workspace $targetDir }
$coreOnly = $env:STATION_CORE_ONLY -eq '1'

Push-Location -LiteralPath $PSScriptRoot
try {
    $ref = $env:JARVIS_PLUGIN_REF
    if (-not $ref) { $ref = Invoke-BuildTool git rev-parse HEAD }
    Set-BuildEnv ZIG $zig
    Set-BuildEnv JARVIS_VERSION $version
    Set-BuildEnv JARVIS_REPO $repo
    Set-BuildEnv JARVIS_PLUGIN_REF $ref
    if (-not $env:JARVIS_UI) {
        Invoke-BuildTool npm.cmd run build:panel
    }
    if ($arches -contains 'armv7' -and -not $coreOnly -and -not $env:LIBCLANG_PATH) {
        $llvmBin = Join-Path $env:ProgramFiles 'LLVM/bin'
        if (Test-Path -LiteralPath (Join-Path $llvmBin 'libclang.dll') -PathType Leaf) {
            Set-BuildEnv LIBCLANG_PATH $llvmBin
        }
    }
    $null = New-Item -ItemType Directory -Force -Path $out, $tools

    # Preserve arguments such as -IC:/path, which PowerShell -File interprets as parameters.
    Write-LfFile (Join-Path $tools 'zig.py') @'
import os
import subprocess
import sys

tool, *args = sys.argv[1:]
command = [os.environ["ZIG"], tool]
if tool == "cc":
    command.extend(["-target", os.environ["JARVIS_ZIG_TARGET"]])
    cpu = os.environ.get("JARVIS_ZIG_CPU")
    if cpu:
        command.append(f"-mcpu={cpu}")
    # cc-rs passes a Clang target triple. Use the equivalent Zig target above.
    args = [arg for arg in args if not arg.startswith("--target=")]
sys.exit(subprocess.call(command + args))
'@
    $cc = Join-Path $tools 'cc.cmd'
    $ar = Join-Path $tools 'ar.cmd'
    [IO.File]::WriteAllLines($cc, @('@echo off', 'python "%~dp0zig.py" cc %*'), $utf8)
    [IO.File]::WriteAllLines($ar, @('@echo off', 'python "%~dp0zig.py" ar %*'), $utf8)

    $armhf = '-target', 'arm-linux-gnueabihf.2.30', '-mcpu=cortex_a7'
    Invoke-BuildTool $zig cc @armhf -shared -fPIC -O2 -s -o (Join-Path $tools 'vqe_tap-armhf.so') station/vqe_tap.c -ldl
    Invoke-BuildTool $zig cc -target aarch64-linux-gnu.2.30 -shared -fPIC -O2 -s -o (Join-Path $tools 'vqe_tap-arm64.so') station/vqe_tap.c -ldl
    Invoke-BuildTool $zig cc @armhf -O2 -s -o (Join-Path $tools 'spotcmd') station/spotcmd.c -ldl
    Invoke-BuildTool $zig cc -target arm-linux-gnueabihf.2.17 -Os -s -o (Join-Path $tools 'mini1-pcm') station/mini1-pcm.c -ldl

    $catalog = Get-Content -LiteralPath 'plugins/index.json' -Encoding UTF8 -Raw | ConvertFrom-Json
    $builds = @()
    $coreAssets = @()
    foreach ($arch in $arches) {
        $target = $targets[$arch]
        $key = $target.Rust.Replace('-', '_')
        Set-BuildEnv JARVIS_ZIG_TARGET $target.Zig
        Set-BuildEnv JARVIS_ZIG_CPU $target.Cpu
        Set-BuildEnv JARVIS_CMAKE_CPU $target.CMake
        Set-BuildEnv JARVIS_CMAKE_CC ($cc.Replace('\', '/'))
        Set-BuildEnv JARVIS_CMAKE_AR ($ar.Replace('\', '/'))
        Set-BuildEnv "CC_$key" $cc
        Set-BuildEnv "AR_$key" $ar
        Set-BuildEnv "CMAKE_TOOLCHAIN_FILE_$key" ((Join-Path $PSScriptRoot 'scripts/aarch64-musl.cmake').Replace('\', '/'))
        Set-BuildEnv "CMAKE_GENERATOR_$key" 'Ninja'
        if ($arch -eq 'armv7') {
            $probe = Join-Path $tools 'include-probe.c'
            Write-LfFile $probe ''
            $includeOutput = & {
                # Zig prints the include list to stderr even on success.
                $ErrorActionPreference = 'Continue'
                & $cc -E -v -x c $probe 2>&1
            }
            if ($LASTEXITCODE -ne 0) { throw ($includeOutput -join "`n") }
            $includes = [regex]::Match(($includeOutput -join "`n"), '(?s)#include <\.\.\.> search starts here:\s*(.*?)End of search list\.').Groups[1].Value.Trim()
            if (-not $includes) { throw 'Could not find Zig include directories for ARMv7 bindgen' }
            $flags = foreach ($path in ($includes -split '\r?\n')) {
                # Zig can print paths relative to the repository root. Cargo runs bindgen elsewhere.
                $includeDir = (Resolve-Path -LiteralPath $path.Trim()).Path
                '-isystem "' + $includeDir.Replace('\', '/') + '"'
            }
            Set-BuildEnv "BINDGEN_EXTRA_CLANG_ARGS_$key" ("-nostdinc " + ($flags -join ' '))
        }

        $cargoArgs = 'build', '--locked', '--release', '--target', $target.Rust
        Push-Location -LiteralPath $workspace
        try {
            Invoke-BuildTool cargo @cargoArgs -p jarvis
            Invoke-BuildTool cargo @cargoArgs -p jarvis-station
        } finally {
            Pop-Location
        }
        $release = Join-Path $targetDir "$($target.Rust)/release"
        $stage = Join-Path $tools $arch
        $null = New-Item -ItemType Directory -Force -Path $stage
        $core = Join-Path $release 'jarvis'
        Copy-Item -LiteralPath $core -Destination (Join-Path $stage 'jarvisd') -Force
        Copy-Item -LiteralPath (Join-Path $release 'jarvis-supervisor') -Destination $stage -Force
        foreach ($name in 'vqe_tap-armhf.so', 'vqe_tap-arm64.so', 'spotcmd', 'mini1-pcm') {
            Copy-Item -LiteralPath (Join-Path $tools $name) -Destination $stage -Force
        }
        foreach ($name in 'wake.bin', 'oww.bin', 'wake_oww.bin') {
            Copy-Item -LiteralPath (Join-Path $PSScriptRoot "models/$name") -Destination $stage -Force
        }
        foreach ($name in 'jarvisd.sh', 'S98jarvisd') {
            Write-LfFile (Join-Path $stage $name) ([IO.File]::ReadAllText((Join-Path $PSScriptRoot "station/$name")))
        }
        $packageFiles = foreach ($name in 'jarvisd', 'jarvis-supervisor', 'vqe_tap-armhf.so', 'vqe_tap-arm64.so',
            'spotcmd', 'mini1-pcm', 'wake.bin', 'oww.bin', 'wake_oww.bin', 'jarvisd.sh', 'S98jarvisd') {
            Join-Path $stage $name
        }
        Compress-Archive -LiteralPath $packageFiles -DestinationPath (Join-Path $out "station-$arch.zip") -Force
        Copy-Item -LiteralPath $core -Destination (Join-Path $out "jarvisd-$arch") -Force
        $coreAssets += "jarvisd-$arch"
        $builds += @{ arch = $arch; asset = "jarvisd-$arch"; modules = @(); bytes = (Get-Item -LiteralPath $core).Length }

        if (-not $coreOnly) {
            foreach ($plugin in ($catalog.plugins | Where-Object kind -eq 'rust')) {
                Push-Location -LiteralPath $workspace
                try {
                    Invoke-BuildTool cargo @cargoArgs -p jarvis-official-plugin --features $plugin.id
                } finally {
                    Pop-Location
                }
                Invoke-BuildTool python scripts/pack-official.py $out $arch $plugin.id (Join-Path $release 'jarvis-official-plugin')
            }
        }
    }

    $manifest = @{ version = $version; builds = $builds } | ConvertTo-Json -Depth 4 -Compress
    Write-LfFile (Join-Path $out 'rust-builds.json') "$manifest`n"
    foreach ($name in 'install.sh', 'install.ps1') {
        $content = [IO.File]::ReadAllText((Join-Path $PSScriptRoot "installer/$name"))
        Write-LfFile (Join-Path $out $name) ($content.Replace('__REPO__', $repo))
    }
    Invoke-BuildTool tar.exe -czf (Join-Path $out 'THIRD_PARTY_NOTICES.tar.gz') LICENSE LICENSES THIRD_PARTY.md CREDITS.md
    foreach ($id in 'weather', 'currency', 'dice') {
        $pluginDir = Join-Path $PSScriptRoot "plugins/$id"
        $files = (Join-Path $pluginDir 'plugin.json'), (Join-Path $pluginDir 'main.lua')
        Compress-Archive -LiteralPath $files -DestinationPath (Join-Path $out "plugin-$id.zip") -Force
    }
    $assets = @(
        Get-ChildItem -LiteralPath $out -Filter 'station-*.zip' -File | Sort-Object Name | Select-Object -ExpandProperty Name
        $coreAssets
        Get-ChildItem -LiteralPath $out -Filter 'install.*' -File | Sort-Object Name | Select-Object -ExpandProperty Name
        Get-ChildItem -LiteralPath $out -Filter 'plugin-*.zip' -File | Sort-Object Name | Select-Object -ExpandProperty Name
        'rust-builds.json', 'THIRD_PARTY_NOTICES.tar.gz'
    )
    $sums = foreach ($name in $assets) {
        $hash = Get-FileHash -LiteralPath (Join-Path $out $name) -Algorithm SHA256
        "$($hash.Hash.ToLowerInvariant())  $name"
    }
    Write-LfFile (Join-Path $out 'SHA256SUMS') (($sums -join "`n") + "`n")
} finally {
    foreach ($name in $savedEnvironment.Keys) {
        if ($null -eq $savedEnvironment[$name]) {
            Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue
        } else {
            [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name])
        }
    }
    Pop-Location
}
