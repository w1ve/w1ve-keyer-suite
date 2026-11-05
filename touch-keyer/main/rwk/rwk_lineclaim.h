// Cross-transport claim on the single remote session slot.
//
// The box can be keyed by a LAN Keyer (remote.c) or a WAN Keyer (wan.c), but only one at a time:
// they drive the same KEY line through the same keyer. This is the one bit of state they share so
// the second to arrive is refused while the first holds the line. A local paddle still overrides
// either, inside the keyer itself.
//
// It is a plain atomic test-and-set plus release, safe from any task. Owner ids are fixed so a
// release only clears the slot if the caller still holds it.
#pragma once

#include <stdbool.h>

typedef enum {
    RWK_LINE_FREE = 0,
    RWK_LINE_LAN = 1,
    RWK_LINE_WAN = 2,
} rwk_line_owner_t;

// Tries to claim the line for `who`. Returns true if it was free (or already held by `who`).
bool rwk_line_claim(rwk_line_owner_t who);

// Releases the line if `who` holds it; a no-op otherwise.
void rwk_line_release(rwk_line_owner_t who);

// Who holds the line right now.
rwk_line_owner_t rwk_line_owner(void);
