<#
.SYNOPSIS
  Build our libwebrtc for Windows (x64/arm64) and package it — the Windows
  counterpart of build.sh + package.sh.

  depot_tools on Windows is a native (cmd/PowerShell) toolchain: its CIPD
  bootstrap and vs_toolchain detection break under Git Bash, so this path is
  PowerShell, not bash. DEPOT_TOOLS_WIN_TOOLCHAIN=0 makes gn use the runner's
  installed Visual Studio instead of Google's internal toolchain package.

.PARAMETER Arch
  x64 | arm64
.PARAMETER Profile
  release | debug   (default release)
#>
[CmdletBinding()]
param(
  [ValidateSet('x64', 'arm64')] [string]$Arch = 'x64',
  [ValidateSet('release', 'debug')] [string]$Profile = 'release'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Run($file, [string[]]$cmdArgs) {
  # depot_tools ships .bat shims (gclient.bat, gn.bat, ninja.bat, fetch.bat);
  # invoke through cmd so PATHEXT resolves them, and surface non-zero exits.
  Write-Host "==> $file $($cmdArgs -join ' ')"
  & cmd /c "$file $($cmdArgs -join ' ')"
  if ($LASTEXITCODE -ne 0) { throw "$file failed ($LASTEXITCODE)" }
}

$HERE = Split-Path -Parent $MyInvocation.MyCommand.Path
$ROOT = Split-Path -Parent $HERE

# ── parse WEBRTC_VERSION (KEY=VALUE lines) ───────────────────────────────────
$ver = @{}
Get-Content (Join-Path $ROOT 'WEBRTC_VERSION') | ForEach-Object {
  if ($_ -match '^\s*([A-Z_]+)=(.*)$') { $ver[$Matches[1]] = $Matches[2].Trim() }
}
$BRANCH = $ver['WEBRTC_BRANCH']
$COMMIT = $ver['WEBRTC_COMMIT']
$MILESTONE = $ver['WEBRTC_MILESTONE']
$PATCHLVL = if ($ver.ContainsKey('REACTOR_PATCH_LEVEL')) { $ver['REACTOR_PATCH_LEVEL'] } else { '0' }

$DEPOT = Join-Path $HERE 'depot_tools'
$SRC = Join-Path $HERE 'src'            # gclient root (.gclient + src/)
$CHECKOUT = Join-Path $SRC 'src'
$CPU = if ($Arch -eq 'arm64') { 'arm64' } else { 'x64' }
$OUT = Join-Path $HERE "out\win-$Arch-$Profile"

Write-Host "==> reactor-webrtc build: os=win cpu=$CPU profile=$Profile"
Write-Host "    pinned: $BRANCH ($MILESTONE) commit='$COMMIT' patch=$PATCHLVL"

# ── 1. depot_tools ───────────────────────────────────────────────────────────
if (-not (Test-Path $DEPOT)) {
  Write-Host '==> cloning depot_tools'
  & git clone --depth 1 https://chromium.googlesource.com/chromium/tools/depot_tools.git $DEPOT
  if ($LASTEXITCODE -ne 0) { throw 'depot_tools clone failed' }
}
$env:PATH = "$DEPOT;$env:PATH"
$env:DEPOT_TOOLS_WIN_TOOLCHAIN = '0'   # use the runner's installed Visual Studio
$env:DEPOT_TOOLS_UPDATE = '1'
# NB: do NOT set vpython_BYPASS — depot_tools must use its *managed* Python,
# which vendors httplib2 etc.; bypassing it to system Python breaks gclient.

# Prime depot_tools: this first run bootstraps its bundled Python/cipd client,
# so the real fetch/sync don't race the bootstrap.
Run 'gclient.bat' @('--version')

# ── 2. fetch + sync at the pinned ref ────────────────────────────────────────
New-Item -ItemType Directory -Force -Path $SRC | Out-Null
Push-Location $SRC
try {
  if (-not (Test-Path $CHECKOUT)) {
    Write-Host '==> fetch webrtc (large; first run downloads tens of GB)'
    Run 'fetch.bat' @('--nohooks', 'webrtc')
  }
  $REF = if ($COMMIT) { $COMMIT } else { $BRANCH }
  # branch-heads/* aren't fetched by a default checkout — add the refspec.
  $fetchCfg = & git -C src config --get-all remote.origin.fetch
  if ($fetchCfg -notmatch 'branch-heads') {
    & git -C src config --add remote.origin.fetch '+refs/branch-heads/*:refs/remotes/branch-heads/*'
  }
  # gclient sync refuses a dirty tree (our patches from a prior build) → reset.
  if (Test-Path (Join-Path $CHECKOUT '.git')) { & git -C src reset --hard | Out-Null }
  Write-Host "==> gclient sync -> src@$REF (--with_branch_heads)"
  Run 'gclient.bat' @('sync', '--with_branch_heads', '--no-history', '--shallow', '-r', "src@$REF", '-D')
  $RESOLVED = (& git -C src rev-parse HEAD).Trim()
  Write-Host "==> resolved WebRTC commit: $RESOLVED"
}
finally { Pop-Location }

# ── 3. apply our patch series ────────────────────────────────────────────────
Push-Location $CHECKOUT
try {
  & git reset --hard $RESOLVED | Out-Null
  Get-ChildItem (Join-Path $HERE 'patches\*.patch') -ErrorAction SilentlyContinue | Sort-Object Name | ForEach-Object {
    Write-Host "==> applying patch $($_.Name)"
    # WebRTC's .gitattributes can force CRLF in the Windows working tree (which
    # overrides core.autocrlf), so our LF patch context won't match. Normalize
    # the files this patch touches to LF, then apply to the working tree.
    $patchFile = $_.FullName
    Select-String -Path $patchFile -Pattern '^\+\+\+ b/(.+)$' | ForEach-Object {
      $rel = $_.Matches[0].Groups[1].Value.Trim()
      $abs = Join-Path $CHECKOUT $rel
      if (Test-Path $abs) {
        [IO.File]::WriteAllText($abs, ([IO.File]::ReadAllText($abs) -replace "`r`n", "`n"))
      }
    }
    & git apply --ignore-whitespace --whitespace=nowarn $patchFile
    if ($LASTEXITCODE -ne 0) { throw "patch $($_.Name) failed" }
  }

  # ── 4. gn gen ──────────────────────────────────────────────────────────────
  $gnArgs = @(
    "is_debug=$([string]($Profile -eq 'debug').ToString().ToLower())"
    'is_component_build=false'
    'rtc_include_tests=false'
    'rtc_build_examples=false'
    'rtc_build_tools=false'
    'rtc_enable_protobuf=true'
    'treat_warnings_as_errors=false'
    'use_rtti=true'
    'rtc_libvpx_build_vp9=true'
    'target_os="win"'
    "target_cpu=`"$CPU`""
    'is_clang=true'
    # MSVC STL is the platform C++ lib on Windows; match it so the lib interops
    # with a consumer's cc-built glue (which uses MSVC).
    'use_custom_libcxx=false'
    'symbol_level=1'
  )
  $argStr = $gnArgs -join ' '
  Write-Host "==> gn gen $OUT"
  Write-Host "    args: $argStr"
  Run 'gn.bat' @('gen', "`"$OUT`"", "--args=`"$argStr`"")

  # ── 5. build the monolithic static lib ───────────────────────────────────────
  $ninjaTarget = if ($env:NINJA_TARGET) { $env:NINJA_TARGET } else { 'webrtc' }
  Write-Host "==> ninja -C $OUT $ninjaTarget"
  Run 'ninja.bat' @('-C', "`"$OUT`"", $ninjaTarget)
}
finally { Pop-Location }

# ── 6. assemble: copy the static lib next to the build dir ───────────────────
$libCandidates = @("$OUT\obj\libwebrtc.a", "$OUT\obj\webrtc.lib")
$lib = $libCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $lib) { throw "expected static lib not found (looked for: $($libCandidates -join ', '))" }
$libExt = [System.IO.Path]::GetExtension($lib)   # .a or .lib
New-Item -ItemType Directory -Force -Path "$OUT\dist\lib" | Out-Null
Copy-Item $lib "$OUT\dist\lib\libwebrtc$libExt" -Force
Write-Host "OK built $OUT\dist\lib\libwebrtc$libExt"

# ── 7. package: headers + archive + checksum + manifest ──────────────────────
$STAGE = Join-Path $OUT 'dist'
$DIST = Join-Path $HERE 'dist'
$NAME = "reactor-webrtc-win-$Arch-$Profile"
New-Item -ItemType Directory -Force -Path "$STAGE\include", $DIST | Out-Null

Write-Host "==> staging headers from $CHECKOUT"
# robocopy mirrors the .h/.inc tree; exit codes 0-7 are success (8+ = failure).
& robocopy $CHECKOUT "$STAGE\include" *.h *.inc /S /XD out .git test /NFL /NDL /NJH /NJS /NP | Out-Null
if ($LASTEXITCODE -ge 8) { throw "robocopy failed ($LASTEXITCODE)" }
$global:LASTEXITCODE = 0

$archive = Join-Path $DIST "$NAME.tar.zst"
Write-Host "==> archiving $archive"
# Windows 10+ ships bsdtar as `tar`; zstd.exe is installed by the CI step.
& tar --use-compress-program "zstd -19" -cf $archive -C $STAGE lib include
if ($LASTEXITCODE -ne 0) { throw "tar failed ($LASTEXITCODE)" }

$sha = (Get-FileHash $archive -Algorithm SHA256).Hash.ToLower()
"$sha  $NAME.tar.zst" | Out-File -Encoding ascii "$archive.sha256"
Write-Host "OK $archive"
Write-Host "   sha256: $sha"

@"
{
  "name": "$NAME",
  "os": "win",
  "arch": "$Arch",
  "profile": "$Profile",
  "archive": "$NAME.tar.zst",
  "sha256": "$sha",
  "webrtc_milestone": "$MILESTONE",
  "webrtc_commit": "$RESOLVED",
  "reactor_patch_level": "$PATCHLVL"
}
"@ | Out-File -Encoding ascii "$DIST\$NAME.manifest.json"
Write-Host "   manifest: $DIST\$NAME.manifest.json"

# NOTE: SBOM (sbom.sh) is a bash+python generator run on the POSIX targets;
# Windows packaging skips it for now (follow-up: port to PowerShell/python).
