#include "player_json.h"

#include <limits.h>
#include <stdio.h>
#include <string.h>

static bool append_bytes(player_json_writer_t *writer, const char *bytes, size_t length)
{
    if (!writer || !writer->ok || !writer->output || writer->length >= writer->capacity ||
        length > writer->capacity - writer->length - 1) {
        if (writer) {
            writer->ok = false;
        }
        return false;
    }
    memcpy(writer->output + writer->length, bytes, length);
    writer->length += length;
    writer->output[writer->length] = '\0';
    return true;
}

static bool append_char(player_json_writer_t *writer, char value)
{
    return append_bytes(writer, &value, 1);
}

static size_t utf8_sequence_length(const uint8_t *value, size_t length, size_t offset)
{
    const uint8_t first = value[offset];
    if (first < 0x80) {
        return 1;
    }
    if (first >= 0xC2 && first <= 0xDF && offset + 1 < length &&
        value[offset + 1] >= 0x80 && value[offset + 1] <= 0xBF) {
        return 2;
    }
    if (first == 0xE0 && offset + 2 < length && value[offset + 1] >= 0xA0 &&
        value[offset + 1] <= 0xBF && value[offset + 2] >= 0x80 && value[offset + 2] <= 0xBF) {
        return 3;
    }
    if (((first >= 0xE1 && first <= 0xEC) || (first >= 0xEE && first <= 0xEF)) &&
        offset + 2 < length && value[offset + 1] >= 0x80 && value[offset + 1] <= 0xBF &&
        value[offset + 2] >= 0x80 && value[offset + 2] <= 0xBF) {
        return 3;
    }
    if (first == 0xED && offset + 2 < length && value[offset + 1] >= 0x80 &&
        value[offset + 1] <= 0x9F && value[offset + 2] >= 0x80 && value[offset + 2] <= 0xBF) {
        return 3;
    }
    if (first == 0xF0 && offset + 3 < length && value[offset + 1] >= 0x90 &&
        value[offset + 1] <= 0xBF && value[offset + 2] >= 0x80 && value[offset + 2] <= 0xBF &&
        value[offset + 3] >= 0x80 && value[offset + 3] <= 0xBF) {
        return 4;
    }
    if (first >= 0xF1 && first <= 0xF3 && offset + 3 < length &&
        value[offset + 1] >= 0x80 && value[offset + 1] <= 0xBF &&
        value[offset + 2] >= 0x80 && value[offset + 2] <= 0xBF &&
        value[offset + 3] >= 0x80 && value[offset + 3] <= 0xBF) {
        return 4;
    }
    if (first == 0xF4 && offset + 3 < length && value[offset + 1] >= 0x80 &&
        value[offset + 1] <= 0x8F && value[offset + 2] >= 0x80 && value[offset + 2] <= 0xBF &&
        value[offset + 3] >= 0x80 && value[offset + 3] <= 0xBF) {
        return 4;
    }
    return 0;
}

static size_t escaped_length(const uint8_t *value, size_t length)
{
    size_t encoded = 0;
    for (size_t offset = 0; offset < length;) {
        const uint8_t byte = value[offset];
        size_t add = 1;
        if (byte == '"' || byte == '\\' || byte == '\b' || byte == '\f' || byte == '\n' ||
            byte == '\r' || byte == '\t') {
            add = 2;
        } else if (byte < 0x20) {
            add = 6;
        } else if (byte >= 0x80) {
            const size_t sequence = utf8_sequence_length(value, length, offset);
            if (sequence) {
                add = sequence;
                offset += sequence;
                if (encoded > SIZE_MAX - add) {
                    return SIZE_MAX;
                }
                encoded += add;
                continue;
            }
            add = 6;
        }
        if (encoded > SIZE_MAX - add) {
            return SIZE_MAX;
        }
        encoded += add;
        ++offset;
    }
    return encoded;
}

void player_json_writer_init(player_json_writer_t *writer, char *output, size_t output_size)
{
    if (!writer) {
        return;
    }
    writer->output = output;
    writer->capacity = output_size;
    writer->length = 0;
    writer->ok = output && output_size > 0;
    if (writer->ok) {
        output[0] = '\0';
    }
}

bool player_json_append_literal(player_json_writer_t *writer, const char *literal)
{
    if (!literal) {
        if (writer) {
            writer->ok = false;
        }
        return false;
    }
    return append_bytes(writer, literal, strlen(literal));
}

bool player_json_append_bool(player_json_writer_t *writer, bool value)
{
    return player_json_append_literal(writer, value ? "true" : "false");
}

bool player_json_append_int(player_json_writer_t *writer, int value)
{
    char text[24];
    const int length = snprintf(text, sizeof(text), "%d", value);
    if (length < 0 || (size_t)length >= sizeof(text)) {
        if (writer) {
            writer->ok = false;
        }
        return false;
    }
    return append_bytes(writer, text, (size_t)length);
}

bool player_json_append_uint(player_json_writer_t *writer, unsigned value)
{
    char text[24];
    const int length = snprintf(text, sizeof(text), "%u", value);
    if (length < 0 || (size_t)length >= sizeof(text)) {
        if (writer) {
            writer->ok = false;
        }
        return false;
    }
    return append_bytes(writer, text, (size_t)length);
}

bool player_json_append_string_n(player_json_writer_t *writer, const uint8_t *value, size_t length)
{
    if (!writer || !value || !writer->ok) {
        if (writer) {
            writer->ok = false;
        }
        return false;
    }
    const size_t encoded = escaped_length(value, length);
    if (encoded == SIZE_MAX || writer->length >= writer->capacity || encoded > writer->capacity - writer->length - 3) {
        writer->ok = false;
        return false;
    }

    (void)append_char(writer, '"');
    static const char hex[] = "0123456789ABCDEF";
    for (size_t offset = 0; offset < length;) {
        const uint8_t byte = value[offset];
        if (byte == '"' || byte == '\\') {
            (void)append_char(writer, '\\');
            (void)append_char(writer, (char)byte);
            ++offset;
        } else if (byte == '\b') {
            (void)player_json_append_literal(writer, "\\b");
            ++offset;
        } else if (byte == '\f') {
            (void)player_json_append_literal(writer, "\\f");
            ++offset;
        } else if (byte == '\n') {
            (void)player_json_append_literal(writer, "\\n");
            ++offset;
        } else if (byte == '\r') {
            (void)player_json_append_literal(writer, "\\r");
            ++offset;
        } else if (byte == '\t') {
            (void)player_json_append_literal(writer, "\\t");
            ++offset;
        } else if (byte < 0x20) {
            const char escaped[] = { '\\', 'u', '0', '0', hex[byte >> 4], hex[byte & 0x0F] };
            (void)append_bytes(writer, escaped, sizeof(escaped));
            ++offset;
        } else if (byte >= 0x80) {
            const size_t sequence = utf8_sequence_length(value, length, offset);
            if (sequence) {
                (void)append_bytes(writer, (const char *)(value + offset), sequence);
                offset += sequence;
            } else {
                const char escaped[] = { '\\', 'u', '0', '0', hex[byte >> 4], hex[byte & 0x0F] };
                (void)append_bytes(writer, escaped, sizeof(escaped));
                ++offset;
            }
        } else {
            (void)append_char(writer, (char)byte);
            ++offset;
        }
    }
    (void)append_char(writer, '"');
    return writer->ok;
}

bool player_json_writer_ok(const player_json_writer_t *writer)
{
    return writer && writer->ok;
}
