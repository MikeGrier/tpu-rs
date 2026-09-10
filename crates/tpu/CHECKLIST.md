<!-- Copyright (c) 2026, Michael Grier -->
<!-- encoding-check: allow-mojibake (this file documents mojibake detection) -->

# Mojibake / encoding-corruption detection

Detect — and ultimately refuse to introduce — text-encoding corruption inside
the `tpu` library itself. Builds on the external `tools/check-encoding.ps1` /
CI guard by moving the checks one layer closer to the source of corruption.

The corruption pattern this targets: UTF-8 bytes that were decoded through a
single-byte code page (Windows-1252) and then re-saved as UTF-8, producing
multibyte sequences like `Ã©`, `â€"`, `â"€`, `Â<NBSP>`. PowerShell `Get-Content`
/ `Set-Content` are the historical culprits.

Most work has been moved to `COMPLETED-CHECKLIST.md` (Milestones 1–13,
all complete). This file will track new mojibake / encoding-hardening
milestones as they are planned; there is currently no active milestone.
