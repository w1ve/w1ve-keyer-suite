// Six macro keys: a short label for the panel, and the CW text they send.
//
// Slots live in NVS, so a canned exchange survives a power cycle. The panel shows the first six
// (two rows of three) and the web page edits both the label and the text.
#pragma once

#include <stdbool.h>
#include <stddef.h>

#define MACRO_SLOTS 6
#define MACRO_LABEL_MAX 10
#define MACRO_TEXT_MAX 48

// Loads the slots from NVS, falling back to the built-in defaults per slot.
void macro_init(void);

int macro_slots(void);

// Copies the slot's label and text out. Returns false for an out-of-range index.
bool macro_get(int index, char *label, size_t label_len, char *text, size_t text_len);

// Stores both fields and persists them. Fails only when NVS refuses the write.
bool macro_set(int index, const char *label, const char *text);

// Sends the slot as CW at the current speed. False when the line is busy, the slot is empty, or
// another macro is already playing.
bool macro_play(int index);
