#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

typedef struct {
    char *output;
    size_t capacity;
    size_t length;
    bool ok;
} player_json_writer_t;

/** Initializes a bounded JSON writer. A failed append marks the writer invalid. */
void player_json_writer_init(player_json_writer_t *writer, char *output, size_t output_size);
bool player_json_append_literal(player_json_writer_t *writer, const char *literal);
bool player_json_append_bool(player_json_writer_t *writer, bool value);
bool player_json_append_int(player_json_writer_t *writer, int value);
bool player_json_append_uint(player_json_writer_t *writer, unsigned value);
/** Appends one complete JSON string, safely encoding arbitrary bytes and valid UTF-8. */
bool player_json_append_string_n(player_json_writer_t *writer, const uint8_t *value, size_t length);
bool player_json_writer_ok(const player_json_writer_t *writer);
