# tpu-mcp scratch / manual VS Code test area

These files are *throwaway test inputs* for verifying the tpu-mcp MCP integration inside VS Code.
They are intentionally unimportant.  Edit them freely; restore with git or the script below.

---

## Option A — Automated binary test (no VS Code needed)

Speaks raw JSON-RPC 2.0 to the tpu-mcp binary and checks responses.
Run from the repo root:

```powershell
.\src\tools\tpu-mcp\scratch\Test-TpuMcp.ps1
# or skip the 100ms Defender delay:
.\src\tools\tpu-mcp\scratch\Test-TpuMcp.ps1 -VerifyDelayMs 0
```

Covers: initialize, tools/list, write, read, replace (basic + \n expansion),
append, count, stat, find — plus a smoke test on animals.txt.

---

## Option B — Manual VS Code / Copilot test

1. Open **this workspace** (tpu-mcp must already be listed in `.vscode/mcp.json`).
2. Start a new Copilot Chat and paste each prompt below in turn.
3. Verify the result in the file using the VS Code editor or the next prompt.

### Prompt 1 — read
```
Read the file z:\s4\FunE-Tools\src\tools\tpu-mcp\scratch\animals.txt using tpu_read_file
and show me its contents.
```
Expected: you see all 5 lines about animals.  Response should NOT error.

### Prompt 2 — replace (tests \n expansion)
```
In z:\s4\FunE-Tools\src\tools\tpu-mcp\scratch\greek.txt, use tpu_replace_in_file to
replace the pattern "beta=2" with "beta=TWO\ngamma-extra=inserted".
Then read the file back and show me the result.
```
Expected: the word "beta=TWO" and "gamma-extra=inserted" appear on **separate lines**
(not with a literal backslash-n).  Response should include `[mtime=...`.

### Prompt 3 — write stamp
```
Use tpu_write_file to overwrite z:\s4\FunE-Tools\src\tools\tpu-mcp\scratch\lines.txt
with exactly this content (five lines):
alpha
beta
gamma
delta
epsilon
Show me the full response text including the stamp.
```
Expected: response contains `[mtime=` and `size=`.

### Prompt 4 — stat
```
Call tpu_stat_file on z:\s4\FunE-Tools\src\tools\tpu-mcp\scratch\lines.txt
and show me the JSON result.
```
Expected: JSON with `mtime_epoch_ms`, `size`, `readonly` fields.

### Prompt 5 — find
```
Use tpu_find to search for the pattern "penguin" in
z:\s4\FunE-Tools\src\tools\tpu-mcp\scratch\animals.txt
```
Expected: match on the "penguin wore a tuxedo" line.

### Prompt 6 — restore
```
Use tpu_write_file to restore z:\s4\FunE-Tools\src\tools\tpu-mcp\scratch\greek.txt
to exactly:
alpha=1
beta=2
gamma=3
delta=4
epsilon=5
```

---

## Scratch files

| File          | Purpose                                     |
|---------------|---------------------------------------------|
| animals.txt   | Plain text; used for read/replace tests     |
| greek.txt     | Key=value pairs; used for \n-expansion test |
| lines.txt     | Five lines; used for write/stat tests       |
| Test-TpuMcp.ps1 | Automated binary smoke test               |
