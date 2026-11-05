#include "rwk_lineclaim.h"

#include <stdatomic.h>

static _Atomic int s_owner = RWK_LINE_FREE;

bool rwk_line_claim(rwk_line_owner_t who)
{
    int expected = RWK_LINE_FREE;
    if (atomic_compare_exchange_strong(&s_owner, &expected, (int)who)) {
        return true;
    }
    return expected == (int)who;
}

void rwk_line_release(rwk_line_owner_t who)
{
    int expected = (int)who;
    atomic_compare_exchange_strong(&s_owner, &expected, RWK_LINE_FREE);
}

rwk_line_owner_t rwk_line_owner(void)
{
    return (rwk_line_owner_t)atomic_load(&s_owner);
}
