#pragma once

// Spawns a one-shot, lowest-priority task that checks the WAN crypto against known answers and
// logs what the operations cost on this chip.
void rwk_selftest_start(void);
