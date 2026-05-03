# Copyright (c) 2026, Michael Grier
#
# tools/check-encoding.ps1 -- fail if any tracked text file is not valid
# UTF-8 or contains characteristic mojibake digraphs.
#
# encoding-check: allow-mojibake  (this file contains literal examples
# of mojibake patterns in regexes and comments)
#
# Usage:
#   pwsh tools/check-encoding.ps1                 # check every tracked file
#   pwsh tools/check-encoding.ps1 -Path src/foo   # check one file or dir
#
# Exits 0 on success, 1 on any failure.  Designed for both local use after
# a fallback edit and for CI invocation on every pull request.
#
# This script is the cheap binary gate ("are there any bad bytes?").  For
# richer per-file diagnostics (encoding detection, mojibake fingerprints,
# JSON output) or in-place repair of single-layer mojibake, use
# `tpu doctor <path>` (optionally with `--fix=peel`).

[CmdletBinding()]
param(
    # Optional path to restrict the check to.  Default: all files tracked
    # by git.
    [string]$Path
)

$ErrorActionPreference = 'Stop'

# Common UTF-8-misread-as-Windows-1252 digraphs / trigraphs.  These are
# not exhaustive but catch the vast majority of real-world corruption:
#   Ã  prefix     — most Latin-1 letters misread (Ã©, Ãª, Ã¨, Ã , Ã¯, …)
#   â€  prefix    — typographic punctuation (—, –, ', ', ", ", …, •)
#   â"  prefix    — box drawing (─, │, ┌, ┐, └, ┘, ├, ┤, ┬, ┴, ┼)
#   Â<NBSP>       — stray NBSP in front of an ASCII char
$Patterns = @(
    [pscustomobject]@{ Name = 'Latin-1 mojibake (Ã…)';     Regex = '[\u00C3][\u0080-\u00BF]' }
    [pscustomobject]@{ Name = 'Punctuation mojibake (â€…)'; Regex = '\u00E2\u20AC[\u0080-\u20FF]' }
    [pscustomobject]@{ Name = 'Box-draw mojibake (â"…)';   Regex = '\u00E2\u201D[\u0080-\u20FF]' }
    [pscustomobject]@{ Name = 'NBSP mojibake (Â<sp>)';     Regex = '\u00C2\u00A0' }
)

# File extensions we consider "text" and therefore subject to the check.
# Binary files (images, archives, .b64 fixtures, etc.) are skipped.
$TextExtensions = @(
    '.rs', '.toml', '.md', '.txt', '.json', '.yaml', '.yml',
    '.ps1', '.psm1', '.psd1', '.sh', '.cfg', '.ini',
    '.gitignore', '.gitattributes'
)

function Test-IsTextFile([string]$file) {
    $ext = [System.IO.Path]::GetExtension($file).ToLowerInvariant()
    if ($ext -and $TextExtensions -contains $ext) { return $true }
    # Files with no extension that look like text (LICENSE, README, etc.).
    $name = [System.IO.Path]::GetFileName($file)
    if (-not $ext -and $name -match '^(LICENSE|README|CHANGELOG|AUTHORS|NOTICE|MAINTAINERS|CODEOWNERS)') {
        return $true
    }
    return $false
}

function Get-TargetFiles {
    if ($Path) {
        if (Test-Path -LiteralPath $Path -PathType Leaf) {
            return @((Resolve-Path -LiteralPath $Path).Path)
        }
        if (Test-Path -LiteralPath $Path -PathType Container) {
            return Get-ChildItem -LiteralPath $Path -Recurse -File |
                Where-Object { Test-IsTextFile $_.FullName } |
                ForEach-Object { $_.FullName }
        }
        Write-Error "Path not found: $Path"
        exit 2
    }
    # Default: all files tracked by git.
    $repoRoot = (& git rev-parse --show-toplevel 2>$null)
    if (-not $repoRoot) {
        Write-Error 'Not inside a git repository; pass -Path explicitly.'
        exit 2
    }
    Push-Location $repoRoot
    try {
        $tracked = & git ls-files
        return $tracked |
            Where-Object { Test-IsTextFile $_ } |
            ForEach-Object { Join-Path $repoRoot $_ }
    } finally {
        Pop-Location
    }
}

$files = @(Get-TargetFiles)
$failures = @()
$strictUtf8 = [System.Text.UTF8Encoding]::new($false, $true)

# Files that legitimately contain mojibake digraphs as documentation /
# regex content (this script and the agent instructions) opt out of the
# pattern check by including this marker on a line of their own.  They
# are still validated as UTF-8.
$AllowMojibakeMarker = 'encoding-check: allow-mojibake'

foreach ($file in $files) {
    if (-not (Test-Path -LiteralPath $file)) { continue }
    $bytes = [System.IO.File]::ReadAllBytes($file)
    if ($bytes.Length -eq 0) { continue }

    # 1. Must be valid UTF-8.
    try {
        $text = $strictUtf8.GetString($bytes)
    } catch {
        $failures += "INVALID UTF-8: $file ($($_.Exception.Message))"
        continue
    }

    # 2. Must not contain characteristic mojibake patterns -- unless the
    #    file explicitly opts out.
    if ($text.Contains($AllowMojibakeMarker)) { continue }
    foreach ($p in $Patterns) {
        if ([regex]::IsMatch($text, $p.Regex)) {
            $failures += "MOJIBAKE: $file [$($p.Name)]"
            break
        }
    }
}

if ($failures.Count -gt 0) {
    foreach ($f in $failures) { Write-Host $f -ForegroundColor Red }
    Write-Host ''
    Write-Host "Encoding check failed: $($failures.Count) file(s) flagged out of $($files.Count) checked." -ForegroundColor Red
    exit 1
}

Write-Host "Encoding check passed: $($files.Count) file(s) clean." -ForegroundColor Green
exit 0
