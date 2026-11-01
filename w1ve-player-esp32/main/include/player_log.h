#pragma once

#include <stdbool.h>
#include <stddef.h>

#define PLAYER_LOG_MAX_ENTRIES 48
#define PLAYER_LOG_LEVEL_MAX 7
#define PLAYER_LOG_MESSAGE_MAX 119

typedef struct {
    char level[PLAYER_LOG_LEVEL_MAX + 1];
    char message[PLAYER_LOG_MESSAGE_MAX + 1];
} player_log_entry_t;

/** Initializes the in-memory action log used by the web UI. */
void player_log_init(void);
void player_log_set_streaming(bool enabled);
bool player_log_streaming_enabled(void);
void player_log_write(const char *level, const char *format, ...);
/** Copies a consistent, bounded log-ring snapshot for complete-record JSON serialization. */
size_t player_log_snapshot(player_log_entry_t *entries, size_t capacity);
