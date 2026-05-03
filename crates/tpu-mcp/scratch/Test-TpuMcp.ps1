# Copyright (c) 2026, Michael Grier
#
# Test-TpuMcp.ps1
#
# End-to-end smoke test: launches tpu-mcp.exe and speaks JSON-RPC 2.0
# over stdin/stdout to verify that the binary handles real requests.
# This exercises the full stack (binary -> tools.rs) without needing VS Code.
#
# Usage:
#   .\Test-TpuMcp.ps1                            # uses release build
#   .\Test-TpuMcp.ps1 -Binary path\to\tpu-mcp.exe
#   .\Test-TpuMcp.ps1 -VerifyDelayMs 0           # skip Defender delay
#
# Requirements: PowerShell 5.1+; run from repo root or pass -Binary explicitly.

param(
    [string]$Binary = ".\target\release\tpu-mcp.exe",
    [int]$VerifyDelayMs = 0
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

# --- helpers ------------------------------------------------------------------

$script:seq = 0
function New-Id { $script:seq++ ; $script:seq }

function Send-Request {
    param($Writer, [hashtable]$Msg)
    $json = $Msg | ConvertTo-Json -Depth 10 -Compress
    $Writer.WriteLine($json)
    $Writer.Flush()
}

function Read-Response {
    param($Reader, [int]$TimeoutMs = 5000)
    # ReadLineAsync + Task.Wait gives us a real timeout without blocking Peek()
    $task = $Reader.ReadLineAsync()
    if (-not $task.Wait($TimeoutMs)) {
        throw "tpu-mcp did not respond within ${TimeoutMs}ms"
    }
    $line = $task.Result
    if ($null -eq $line) { throw "tpu-mcp closed the connection unexpectedly" }
    return $line | ConvertFrom-Json
}

function Invoke-Rpc {
    param($Writer, $Reader, [string]$Method, [hashtable]$RpcParams = @{})
    $id = New-Id
    Send-Request $Writer @{ jsonrpc = "2.0"; id = $id; method = $Method; params = $RpcParams }
    $resp = Read-Response $Reader
    if ($resp.PSObject.Properties['error'] -and $resp.error) {
        throw "RPC error: $($resp.error.message)"
    }
    return $resp.result
}

function Assert-Has {
    param([string]$Label, [string]$Actual, [string]$Expected)
    if (-not $Actual.Contains($Expected)) {
        throw "FAIL [$Label]: expected '$Expected' in:`n$Actual"
    }
    Write-Host "  PASS  $Label"
}

function Assert-Lacks {
    param([string]$Label, [string]$Actual, [string]$Unexpected)
    if ($Actual.Contains($Unexpected)) {
        throw "FAIL [$Label]: did NOT expect '$Unexpected' in:`n$Actual"
    }
    Write-Host "  PASS  $Label"
}

# --- locate binary ------------------------------------------------------------

if (-not (Test-Path $Binary)) {
    Write-Error "Binary not found: $Binary -- run 'cargo build --release -p tpu-mcp' first."
    exit 1
}

$scratchDir = Join-Path $PSScriptRoot ""
if (-not (Test-Path $scratchDir)) {
    New-Item -ItemType Directory -Path $scratchDir | Out-Null
}

# --- start process ------------------------------------------------------------

Write-Host "Starting $Binary --verify-delay-ms=$VerifyDelayMs"

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = (Resolve-Path $Binary).Path
$psi.Arguments = "--verify-delay-ms=$VerifyDelayMs"
$psi.UseShellExecute = $false
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true

$proc = [System.Diagnostics.Process]::Start($psi)
$writer = $proc.StandardInput
$reader = $proc.StandardOutput

# --- initialize ---------------------------------------------------------------

Write-Host ""
Write-Host "[1] initialize"
$initResult = Invoke-Rpc $writer $reader "initialize" @{
    protocolVersion = "2024-11-05"
    capabilities    = @{}
    clientInfo      = @{ name = "Test-TpuMcp"; version = "1.0" }
}
Assert-Has "server name" ($initResult | ConvertTo-Json -Compress) "tpu-mcp"
# notifications don't get a response - send without waiting
Send-Request $writer @{ jsonrpc = "2.0"; method = "notifications/initialized"; params = @{} }

# --- tools/list ---------------------------------------------------------------

Write-Host ""
Write-Host "[2] tools/list"
$listResult = Invoke-Rpc $writer $reader "tools/list"
$toolNames = $listResult.tools | ForEach-Object { $_.name }
foreach ($t in @("tpu_read_file", "tpu_write_file", "tpu_replace_in_file",
        "tpu_edit_file", "tpu_append_file", "tpu_find",
        "tpu_count_file", "tpu_stat_file")) {
    if ($toolNames -notcontains $t) { throw "FAIL: tool '$t' missing from tools/list" }
    Write-Host "  PASS  found $t"
}

# --- helper: call a tool ------------------------------------------------------

function Invoke-Tool {
    param($Writer, $Reader, [string]$Tool, [hashtable]$ToolArgs)
    $result = Invoke-Rpc $Writer $Reader "tools/call" @{ name = $Tool; arguments = $ToolArgs }
    return ($result.content | ForEach-Object { $_.text }) -join ""
}

# --- write file ---------------------------------------------------------------

Write-Host ""
Write-Host "[3] tpu_write_file"
$testFile = Join-Path $scratchDir "mcp-test.txt"
$out = Invoke-Tool $writer $reader "tpu_write_file" @{
    file    = $testFile
    content = "hello world`nline two`nline three`n"
}
Assert-Has    "write success" $out "wrote"
Assert-Has    "write stamp"   $out "[mtime="
$ondisk = Get-Content $testFile -Raw
Assert-Has    "file on disk"  $ondisk "hello world"

# --- read file ----------------------------------------------------------------

Write-Host ""
Write-Host "[4] tpu_read_file"
$out = Invoke-Tool $writer $reader "tpu_read_file" @{ file = $testFile }
Assert-Has "read content" $out "hello world"
Assert-Has "read line 2"  $out "line two"

# --- replace in file - basic --------------------------------------------------

Write-Host ""
Write-Host "[5] tpu_replace_in_file (basic)"
$out = Invoke-Tool $writer $reader "tpu_replace_in_file" @{
    file        = $testFile
    pattern     = "world"
    replacement = "earth"
}
Assert-Has    "replace success" $out "replaced"
Assert-Has    "replace stamp"   $out "[mtime="
$ondisk = Get-Content $testFile -Raw
Assert-Has    "word replaced"   $ondisk "earth"
Assert-Lacks  "old word gone"   $ondisk "world"

# --- replace in file - \n expansion -------------------------------------------

Write-Host ""
Write-Host "[6] tpu_replace_in_file (\n expansion)"
$out = Invoke-Tool $writer $reader "tpu_replace_in_file" @{
    file        = $testFile
    pattern     = "line two"
    replacement = "second\nthird injected"
}
Assert-Has    "replace-n success"    $out "replaced"
$ondisk = Get-Content $testFile -Raw
Assert-Has    "second on own line"   $ondisk "second"
Assert-Has    "third on own line"    $ondisk "third injected"
Assert-Lacks  "no literal backslash" $ondisk "second\nthird"

# --- append file --------------------------------------------------------------

Write-Host ""
Write-Host "[7] tpu_append_file"
$out = Invoke-Tool $writer $reader "tpu_append_file" @{
    file    = $testFile
    content = "appended line`n"
}
Assert-Has "append success" $out "appended to"
Assert-Has "append stamp"   $out "[mtime="
$ondisk = Get-Content $testFile -Raw
Assert-Has "append on disk" $ondisk "appended line"

# --- count file ---------------------------------------------------------------

Write-Host ""
Write-Host "[8] tpu_count_file"
$out = Invoke-Tool $writer $reader "tpu_count_file" @{ file = $testFile }
Assert-Has "count result" $out "lines"

# --- stat file ----------------------------------------------------------------

Write-Host ""
Write-Host "[9] tpu_stat_file"
$out = Invoke-Tool $writer $reader "tpu_stat_file" @{ file = $testFile }
$stat = $out | ConvertFrom-Json
if ($null -eq $stat.mtime_epoch_ms) { throw "FAIL: stat missing mtime_epoch_ms" }
if ($null -eq $stat.size) { throw "FAIL: stat missing size" }
Write-Host "  PASS  stat: size=$($stat.size) mtime=$($stat.mtime_epoch_ms)"

# --- find ---------------------------------------------------------------------

Write-Host ""
Write-Host "[10] tpu_find"
$out = Invoke-Tool $writer $reader "tpu_find" @{
    pattern = "appended"
    path    = $testFile
}
Assert-Has "find hit" $out "appended"

# --- animals.txt smoke test ---------------------------------------------------

Write-Host ""
Write-Host "[11] scratch file smoke (animals.txt)"
$animalsFile = Join-Path $scratchDir "animals.txt"
if (Test-Path $animalsFile) {
    $out = Invoke-Tool $writer $reader "tpu_read_file" @{ file = $animalsFile }
    Assert-Has "animals read" $out "fox"
    $out = Invoke-Tool $writer $reader "tpu_replace_in_file" @{
        file        = $animalsFile
        pattern     = "lazy dog"
        replacement = "sleepy cat"
    }
    Assert-Has "animals replace" $out "replaced"
    $ondisk = Get-Content $animalsFile -Raw
    Assert-Has "animals updated" $ondisk "sleepy cat"
    # restore
    Invoke-Tool $writer $reader "tpu_replace_in_file" @{
        file        = $animalsFile
        pattern     = "sleepy cat"
        replacement = "lazy dog"
    } | Out-Null
    Write-Host "  INFO  animals.txt restored"
}
else {
    Write-Host "  SKIP  animals.txt not found"
}

# --- teardown -----------------------------------------------------------------

Write-Host ""
Write-Host "Stopping tpu-mcp..."
try { $writer.Close() } catch {}
$proc.WaitForExit(2000) | Out-Null
if (-not $proc.HasExited) { $proc.Kill() }

Remove-Item $testFile -ErrorAction SilentlyContinue

Write-Host ""
Write-Host "All tests passed."
