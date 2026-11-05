#pragma once

// Builds the idle page.
void ui_init(void);

// Orientation self-test, gated by UI_ORIENT_SELFTEST in board.h. Cycles the four landscape
// MADCTL values, three seconds each, so the panel can be calibrated by eye. Blocks ~12 s.
void ui_orient_selftest(void);
