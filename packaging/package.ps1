<#
.SYNOPSIS
  Builds a self-contained RawGraph folder + zip for another Windows PC.
.PARAMETER OutDir
  Where RawGraph-<version>-windows-x64\ and the .zip are created.
.PARAMETER SkipBuild
  Do not run cargo build; use the exe already in the dist target dir.
.PARAMETER RetryMinutes
  If the build fails, retry every 60 s for up to this many minutes (default 0 = no retry).
#>
param(
    [string]$OutDir = (Join-Path $env:LOCALAPPDATA 'rawgraph\dist'),
    [switch]$SkipBuild,
    [int]$RetryMinutes = 0
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2

$PkgDir   = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = Split-Path -Parent $PkgDir
$Cargo    = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
$TargetDir = Join-Path $env:LOCALAPPDATA 'rawgraph\target-dist'
$BuiltExe = Join-Path $TargetDir 'release\rawgraph-app.exe'

function Fail([string]$msg) { Write-Host "ERROR: $msg" -ForegroundColor Red; throw $msg }

if (-not (Test-Path $Cargo)) { Fail "cargo not found at $Cargo" }

# ---- version -------------------------------------------------------------
$rootToml = Get-Content (Join-Path $RepoRoot 'Cargo.toml') -Raw
$m = [regex]::Match($rootToml, '(?ms)\[workspace\.package\].*?^\s*version\s*=\s*"([^"]+)"')
if (-not $m.Success) { $m = [regex]::Match($rootToml, '(?m)^\s*version\s*=\s*"([^"]+)"') }
if (-not $m.Success) { Fail 'cannot read version from Cargo.toml' }
$Version = $m.Groups[1].Value
$PkgName = "RawGraph-$Version-windows-x64"
Write-Host "Version $Version -> $PkgName"

# ---- OutDir safety -------------------------------------------------------
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = (Resolve-Path $OutDir).Path
$Stage = Join-Path $OutDir $PkgName
$Zip   = Join-Path $OutDir "$PkgName.zip"
if ((Split-Path -Leaf $Stage) -notlike 'RawGraph-*') { Fail 'internal: stage name check failed' }
if ((Split-Path -Parent $Stage) -ne $OutDir) { Fail 'internal: stage parent check failed' }
if (Test-Path $Stage) { Remove-Item -Recurse -Force -LiteralPath $Stage }
New-Item -ItemType Directory -Force -Path $Stage | Out-Null
$LicDir = Join-Path $Stage 'LICENSES'
New-Item -ItemType Directory -Force -Path $LicDir | Out-Null

# ---- (d) licences from cargo metadata ------------------------------------
Write-Host 'Collecting licences (cargo metadata) ...'
Push-Location $RepoRoot
try {
    $metaFile = Join-Path $env:TEMP "rawgraph-meta-$PID.json"
    & $Cargo metadata --format-version 1 --filter-platform x86_64-pc-windows-msvc 1> $metaFile
    if ($LASTEXITCODE -ne 0) { Fail 'cargo metadata failed' }
} finally { Pop-Location }
Add-Type -AssemblyName System.Web.Extensions
$ser = New-Object System.Web.Script.Serialization.JavaScriptSerializer
$ser.MaxJsonLength = [int]::MaxValue
$meta = $ser.DeserializeObject([IO.File]::ReadAllText($metaFile))
Remove-Item $metaFile -Force

$pkgById = @{}
foreach ($p in $meta['packages']) { $pkgById[$p['id']] = $p }
$nodeById = @{}
foreach ($n in $meta['resolve']['nodes']) { $nodeById[$n['id']] = $n }
$root = $null
foreach ($p in $meta['packages']) { if ($p['name'] -eq 'rawgraph-app') { $root = $p['id'] } }
if (-not $root) { Fail 'rawgraph-app not found in metadata' }

# closure over normal + build edges (dev edges skipped)
$seen = @{}
$queue = New-Object System.Collections.Queue
$queue.Enqueue($root); $seen[$root] = $true
while ($queue.Count -gt 0) {
    $id = $queue.Dequeue()
    foreach ($d in $nodeById[$id]['deps']) {
        $use = $false
        foreach ($k in $d['dep_kinds']) { if ($k['kind'] -ne 'dev') { $use = $true } }
        if ($use -and -not $seen.ContainsKey($d['pkg'])) { $seen[$d['pkg']] = $true; $queue.Enqueue($d['pkg']) }
    }
}
$third = @()
foreach ($id in $seen.Keys) {
    $p = $pkgById[$id]
    if ($p['source']) { $third += $p }   # workspace/path crates have no source
}
$third = @($third | Sort-Object { $_['name'] }, { $_['version'] })
Write-Host ("Third-party packages in closure: {0} (workspace crates: {1})" -f $third.Count, ($seen.Count - $third.Count))

$licPattern = '^(LICEN[CS]E|COPYING|NOTICE|UNLICENSE)'
$entries = @()
foreach ($p in $third) {
    $dir = Split-Path -Parent $p['manifest_path']
    $files = @(Get-ChildItem -LiteralPath $dir -File -ErrorAction SilentlyContinue | Where-Object { $_.Name -match $licPattern -and $_.Length -gt 0 })
    if ($files.Count -eq 0) {
        # some crates (e.g. epaint_default_fonts) keep their licence texts in a fonts\ folder
        $fd = Join-Path $dir 'fonts'
        if (Test-Path -LiteralPath $fd) {
            $files = @(Get-ChildItem -LiteralPath $fd -File -Filter '*.txt' -ErrorAction SilentlyContinue | Where-Object { $_.Length -gt 0 })
        }
    }
    $expr = $p['license']
    if (-not $expr) { $expr = '' }
    $entries += [pscustomobject]@{
        Name = $p['name']; Version = $p['version']; License = $expr
        Repo = [string]$p['repository']; Dir = $dir; Files = $files
        LicenseFileField = [string]$p['license_file']
    }
    if ($files.Count -gt 0) {
        $dest = Join-Path $LicDir ("{0}-{1}" -f $p['name'], $p['version'])
        New-Item -ItemType Directory -Force -Path $dest | Out-Null
        foreach ($f in $files) { Copy-Item -LiteralPath $f.FullName -Destination $dest }
    }
}
$noLic = @($entries | Where-Object { $_.Files.Count -eq 0 })

# standard texts, only copied from other packages on disk
$std = Join-Path $LicDir '_standard'
$stdNotes = @()
if ($noLic.Count -gt 0) {
    $tokens = @{}
    foreach ($e in $noLic) {
        foreach ($t in ($e.License -split '[\s()/]+|\bOR\b|\bAND\b|\bWITH\b')) {
            if ($t -and $t -notin @('OR','AND','WITH')) { $tokens[$t] = $true }
        }
    }
    foreach ($t in ($tokens.Keys | Sort-Object)) {
        # donor: a package whose expression is exactly this token (or contains it) and which has a licence file
        $donor = $null; $file = $null
        $cands = @($entries | Where-Object { $_.Files.Count -gt 0 -and $_.License -eq $t })
        if ($cands.Count -eq 0) { $cands = @($entries | Where-Object { $_.Files.Count -gt 0 -and ($_.License -split '[\s()/]+') -contains $t }) }
        foreach ($c in $cands) {
            $tl = $t.ToLower()
            $f = $c.Files | Where-Object { $_.Name.ToLower() -like "*$tl*" -and $_.Name -notmatch 'NOTICE' } | Select-Object -First 1
            if (-not $f -and $c.License -eq $t) { $f = $c.Files | Where-Object { $_.Name -match '^(LICEN[CS]E|COPYING)' } | Select-Object -First 1 }
            if ($f) { $donor = $c; $file = $f; break }
        }
        if ($donor) {
            New-Item -ItemType Directory -Force -Path $std | Out-Null
            Copy-Item -LiteralPath $file.FullName -Destination (Join-Path $std ("{0}.txt" -f $t))
            $stdNotes += ("{0}.txt  <- copied unchanged from {1} {2} ({3}); copyright line is that package's, not the packages listed as having no licence file." -f $t, $donor.Name, $donor.Version, $file.Name)
        } else {
            $stdNotes += ("{0}: no text available on this machine; not provided." -f $t)
        }
    }
    if (Test-Path $std) { Set-Content -Path (Join-Path $std 'SOURCES.txt') -Value $stdNotes -Encoding UTF8 }
}

# THIRD-PARTY-NOTICES.txt
$nl = [Environment]::NewLine
$sb = New-Object System.Text.StringBuilder
[void]$sb.AppendLine("RawGraph $Version - third-party components")
[void]$sb.AppendLine('Generated from cargo metadata (normal and build dependencies of rawgraph-app, target x86_64-pc-windows-msvc).')
[void]$sb.AppendLine('Licence texts are in the LICENSES folder, one sub-folder per package.')
[void]$sb.AppendLine('')
[void]$sb.AppendLine(("{0} packages:" -f $entries.Count))
[void]$sb.AppendLine('')
foreach ($e in $entries) {
    $lic = $e.License; if (-not $lic) { $lic = '(none declared)'; if ($e.LicenseFileField) { $lic = "(see file $($e.LicenseFileField))" } }
    $repo = $e.Repo; if (-not $repo) { $repo = '-' }
    [void]$sb.AppendLine(("{0} {1} | {2} | {3}" -f $e.Name, $e.Version, $lic, $repo))
}
[void]$sb.AppendLine('')
[void]$sb.AppendLine('No licence file found in the package:')
if ($noLic.Count -eq 0) { [void]$sb.AppendLine('(none)') }
foreach ($e in $noLic) {
    $lic = $e.License; if (-not $lic) { $lic = '(none declared)' }
    [void]$sb.AppendLine(("{0} {1} | {2}" -f $e.Name, $e.Version, $lic))
}
if ($stdNotes.Count -gt 0) {
    [void]$sb.AppendLine('')
    [void]$sb.AppendLine('Standard licence texts for the packages above (LICENSES\_standard):')
    foreach ($s in $stdNotes) { [void]$sb.AppendLine($s) }
}
[IO.File]::WriteAllText((Join-Path $Stage 'THIRD-PARTY-NOTICES.txt'), $sb.ToString(), (New-Object Text.UTF8Encoding($true)))

# README
$readme = Join-Path $PkgDir 'README.txt'
if (-not (Test-Path $readme)) { Fail "template missing: $readme" }
Copy-Item -LiteralPath $readme -Destination (Join-Path $Stage 'README.txt')

# ---- (a) build -----------------------------------------------------------
if (-not $SkipBuild) {
    Write-Host 'Building release with static CRT ...'
    $oldFlags = $env:RUSTFLAGS; $oldTarget = $env:CARGO_TARGET_DIR
    $env:RUSTFLAGS = '-C target-feature=+crt-static'
    $env:CARGO_TARGET_DIR = $TargetDir
    $deadline = (Get-Date).AddMinutes($RetryMinutes)
    try {
        Push-Location $RepoRoot
        while ($true) {
            & $Cargo build --release -p rawgraph-app -j 8
            if ($LASTEXITCODE -eq 0) { break }
            if ((Get-Date) -ge $deadline) { Fail "cargo build failed (exit $LASTEXITCODE)" }
            Write-Host 'Build failed, retrying in 60 s ...' -ForegroundColor Yellow
            Start-Sleep -Seconds 60
        }
    } finally {
        Pop-Location
        $env:RUSTFLAGS = $oldFlags; $env:CARGO_TARGET_DIR = $oldTarget
        if ($null -eq $oldFlags) { Remove-Item Env:\RUSTFLAGS -ErrorAction SilentlyContinue }
        if ($null -eq $oldTarget) { Remove-Item Env:\CARGO_TARGET_DIR -ErrorAction SilentlyContinue }
    }
}
if (-not (Test-Path $BuiltExe)) { Fail "no exe at $BuiltExe (run without -SkipBuild first). Licence files were collected in $Stage." }
$Exe = Join-Path $Stage 'RawGraph.exe'
Copy-Item -LiteralPath $BuiltExe -Destination $Exe

# ---- (c) dumpbin check ---------------------------------------------------
$dumpbin = $null
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
if (Test-Path $vswhere) {
    $found = & $vswhere -latest -products * -find 'VC\Tools\MSVC\**\bin\Hostx64\x64\dumpbin.exe' 2>$null
    if ($found) { $dumpbin = @($found)[0] }
}
if (-not $dumpbin) {
    foreach ($pf in @($env:ProgramFiles, ${env:ProgramFiles(x86)})) {
        $hit = Get-ChildItem -Path (Join-Path $pf 'Microsoft Visual Studio\*\*\VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe') -ErrorAction SilentlyContinue | Sort-Object FullName | Select-Object -Last 1
        if ($hit) { $dumpbin = $hit.FullName; break }
    }
}
$deps = @()
if ($dumpbin) {
    $out = & $dumpbin /dependents $Exe
    $deps = @($out | ForEach-Object { $_.Trim() } | Where-Object { $_ -match '\.dll$' })
    Write-Host 'Imported DLLs:'; $deps | ForEach-Object { Write-Host "  $_" }
    $bad = @($deps | Where-Object { $_ -match '^(VCRUNTIME|MSVCP|api-ms-win-crt-|ucrtbase\.dll|VCOMP|CONCRT)' })
    if ($bad.Count -gt 0) { Fail ("RawGraph.exe imports the C/C++ runtime ({0}); the static CRT link did not take effect. Check RUSTFLAGS / .cargo\config.toml." -f ($bad -join ', ')) }
    Write-Host 'dumpbin: no VC runtime / UCRT imports.' -ForegroundColor Green
} else {
    Write-Host 'WARNING: dumpbin.exe not found; runtime-import check skipped.' -ForegroundColor Yellow
}

# ---- (e) summary ---------------------------------------------------------
$exeSize = (Get-Item $Exe).Length
Write-Host ''
Write-Host '=== Summary ==='
Write-Host ("Exe size: {0:N1} MiB" -f ($exeSize / 1MB))
Write-Host ("Third-party packages: {0}" -f $entries.Count)
Write-Host 'Packages per licence expression:'
$entries | Group-Object { if ($_.License) { $_.License } else { '(none)' } } | Sort-Object Count -Descending | ForEach-Object { Write-Host ("  {0,4}  {1}" -f $_.Count, $_.Name) }
Write-Host ("No licence file found in package: {0}" -f $noLic.Count)
$review = @($entries | Where-Object { (-not $_.License) -or ($_.License -replace 'LGPL','') -match 'GPL|SSPL' })
if ($review.Count -gt 0) {
    foreach ($e in $review) { $l = $e.License; if (-not $l) { $l = '(missing)' }; Write-Host ("NEEDS REVIEW: {0} {1} | {2}" -f $e.Name, $e.Version, $l) -ForegroundColor Yellow }
} else { Write-Host 'NEEDS REVIEW: none' }

# ---- (f) zip -------------------------------------------------------------
# ZIP cannot store timestamps before 1980 (some crates ship such files); fix the staged copies only.
$zipMin = New-Object DateTime 1980, 1, 1, 0, 0, 0
$zipNow = Get-Date
$fixed = 0
foreach ($f in (Get-ChildItem -LiteralPath $Stage -Recurse -File -Force)) {
    if ($f.LastWriteTime -lt $zipMin -or $f.LastWriteTime -gt $zipNow) { $f.LastWriteTime = $zipNow; $fixed++ }
}
Write-Host ("Timestamps reset to now (outside ZIP range): {0} file(s)" -f $fixed)
if (Test-Path -LiteralPath $Zip) { Remove-Item -LiteralPath $Zip -Force }
Compress-Archive -Path $Stage -DestinationPath $Zip -CompressionLevel Optimal
Write-Host ("Zip: {0} ({1:N1} MiB)" -f $Zip, ((Get-Item $Zip).Length / 1MB))
Write-Host "Folder: $Stage"
