// W1VE v2 trusted-LAN Player transport.
//
// The Keyer browses this Player's advertisement, sends a type 2 pair request with its nonce, and
// both ends derive a 32-byte session key from the two nonces. Everything after that is an
// authenticated envelope: magic "W1VE", version, type, player_id, session_id, sequence, length,
// payload and the first 16 bytes of HMAC-SHA-256 over all of it. Type 4 carries complete
// current-plus-history CW edge frames; the newest frame is also the KEY lease.
//
// The transport binds only to the joined station address, so the setup access point can never
// carry W1VE traffic.
#include "remote.h"

#include <stdio.h>
#include <string.h>

#include "esp_log.h"
#include "esp_mac.h"
#include "esp_netif.h"
#include "esp_random.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"
#include "keyer.h"
#include "lwip/sockets.h"
#include "mbedtls/md.h"
#include "mbedtls/sha256.h"
#include "net.h"
#include "nvs.h"
#include "nvs_flash.h"
#include "rwk/rwk_lineclaim.h"

static const char *TAG = "player";

#define MAGIC "W1VE"
#define PROTOCOL_VERSION 2

#define TYPE_ADVERTISE 1
#define TYPE_PAIR_REQUEST 2
#define TYPE_PAIR_ACCEPT 3
#define TYPE_EDGE 4
#define TYPE_PING 6
#define TYPE_PONG 7
#define TYPE_UNPAIR 8
#define TYPE_QUERY 10

#define HEADER_LEN 21
#define TAG_LEN 16
#define EDGE_ENTRY_LEN 12
#define EDGE_MIN_LEN 16
#define EDGE_MAX_LEN 52
#define PAIR_REQUEST_LEN 34
#define PAIR_ACCEPT_LEN 50
#define QUERY_LEN 12

#define ADVERTISE_MS 1500
#define PING_MS 1000
#define MISSED_PING_LIMIT 3
// No packet at all for this long and the Keyer is gone, not busy: the session is cleared and the
// box goes back to advertising. Same span as the missed-ping limit, so the state the panel shows
// and the moment the transport gives up agree with each other.
#define SESSION_SILENT_MS (MISSED_PING_LIMIT * PING_MS)
#define RX_WINDOW_BITS 64
#define SPEED_REMEASURE_MS 2000

#define DEFAULT_NAME "W1VE Keyer"

typedef struct {
    char name[REMOTE_NAME_MAX + 1];
    uint8_t player_id[6];

    bool paired;
    uint8_t session_key[32];
    uint32_t session_id;
    struct sockaddr_in peer;

    uint32_t next_tx_sequence;
    bool have_rx_sequence;
    uint32_t highest_rx_sequence;
    uint64_t received_bits;

    bool have_edge_epoch;
    uint16_t edge_epoch;
    bool have_edge_sequence;
    uint32_t last_edge_sequence;
    bool have_edge_timestamp;
    uint32_t last_edge_timestamp;
    int64_t last_edge_refresh_ms;

    bool key_down;
    bool have_down_ts;
    uint32_t down_ts;

    int64_t dit_ms;
    int64_t dit_at_ms;

    bool ping_outstanding;
    uint64_t pending_ping;
    uint8_t missed_pings;
    int rtt_ms;
    int64_t last_packet_ms;

    char reason[REMOTE_REASON_LEN];
    uint8_t last_keyer_id[6];
    uint32_t station_ip;
    uint32_t station_mask;
} player_state_t;

static player_state_t s_state;
static SemaphoreHandle_t s_lock;
static bool s_lan_enabled;
static esp_netif_t *s_sta_netif;
static int s_tx_fd = -1;
static int s_disc_fd = -1;
static int64_t s_next_advertise;
static int64_t s_next_ping;

// ------------------------------------------------------------------ helpers

static int64_t now_ms(void)
{
    return esp_timer_get_time() / 1000;
}

static uint32_t read_be32(const uint8_t *src)
{
    return ((uint32_t)src[0] << 24) | ((uint32_t)src[1] << 16) | ((uint32_t)src[2] << 8) | src[3];
}

static void write_be32(uint8_t *dst, uint32_t value)
{
    dst[0] = (uint8_t)(value >> 24);
    dst[1] = (uint8_t)(value >> 16);
    dst[2] = (uint8_t)(value >> 8);
    dst[3] = (uint8_t)value;
}

static uint64_t read_be64(const uint8_t *src)
{
    uint64_t value = 0;
    for (size_t i = 0; i < 8; i++) {
        value = (value << 8) | src[i];
    }
    return value;
}

static void write_be64(uint8_t *dst, uint64_t value)
{
    for (int i = 7; i >= 0; i--) {
        dst[i] = (uint8_t)value;
        value >>= 8;
    }
}

static uint16_t read_le16(const uint8_t *src)
{
    return (uint16_t)((uint16_t)src[0] | ((uint16_t)src[1] << 8));
}

static uint32_t read_le32(const uint8_t *src)
{
    return (uint32_t)src[0] | ((uint32_t)src[1] << 8) | ((uint32_t)src[2] << 16) |
           ((uint32_t)src[3] << 24);
}

static bool sha256(const uint8_t *message, size_t length, uint8_t out[32])
{
    mbedtls_sha256_context ctx;
    mbedtls_sha256_init(&ctx);
    const bool ok = mbedtls_sha256_starts(&ctx, 0) == 0 &&
                    mbedtls_sha256_update(&ctx, message, length) == 0 &&
                    mbedtls_sha256_finish(&ctx, out) == 0;
    mbedtls_sha256_free(&ctx);
    if (!ok) {
        ESP_LOGE(TAG, "SHA-256 failed");
        memset(out, 0, 32);
    }
    return ok;
}

static bool hmac_sha256(const uint8_t *key, size_t key_len, const uint8_t *message,
                        size_t message_len, uint8_t out[32])
{
    const mbedtls_md_info_t *info = mbedtls_md_info_from_type(MBEDTLS_MD_SHA256);
    if (info == NULL || mbedtls_md_hmac(info, key, key_len, message, message_len, out) != 0) {
        ESP_LOGE(TAG, "HMAC-SHA-256 failed");
        memset(out, 0, 32);
        return false;
    }
    return true;
}

static void random_nonce(uint8_t nonce[16])
{
    for (size_t i = 0; i < 16; i += 4) {
        write_be32(nonce + i, esp_random());
    }
}

static bool same_endpoint(const struct sockaddr_in *left, const struct sockaddr_in *right)
{
    return left->sin_addr.s_addr == right->sin_addr.s_addr && left->sin_port == right->sin_port;
}

static void keyer_id_text(char out[REMOTE_KEYER_LEN], const uint8_t id[6])
{
    snprintf(out, REMOTE_KEYER_LEN, "%02X:%02X:%02X:%02X:%02X:%02X", id[0], id[1], id[2], id[3],
             id[4], id[5]);
}

// ------------------------------------------------------------------ session state

static void clear_session_locked(void)
{
    memset(s_state.session_key, 0, sizeof(s_state.session_key));
    memset(&s_state.peer, 0, sizeof(s_state.peer));
    s_state.session_id = 0;
    s_state.next_tx_sequence = 0;
    s_state.have_rx_sequence = false;
    s_state.highest_rx_sequence = 0;
    s_state.received_bits = 0;
    s_state.have_edge_epoch = false;
    s_state.edge_epoch = 0;
    s_state.have_edge_sequence = false;
    s_state.last_edge_sequence = 0;
    s_state.have_edge_timestamp = false;
    s_state.last_edge_timestamp = 0;
    s_state.last_edge_refresh_ms = 0;
    s_state.have_down_ts = false;
    s_state.down_ts = 0;
    s_state.dit_ms = 0;
    s_state.dit_at_ms = 0;
    s_state.ping_outstanding = false;
    s_state.pending_ping = 0;
    s_state.missed_pings = 0;
    s_state.rtt_ms = 0;
    s_state.last_packet_ms = 0;
}

static void unpair_locked(const char *reason)
{
    const bool was_paired = s_state.paired;
    if (s_state.key_down) {
        keyer_remote_key(false);
        s_state.key_down = false;
    }
    s_state.paired = false;
    clear_session_locked();
    snprintf(s_state.reason, sizeof(s_state.reason), "%s", reason != NULL ? reason : "");
    if (was_paired) {
        rwk_line_release(RWK_LINE_LAN);
        keyer_set_remote_speed(0);  // no remote owns the line: macros use the station speed again
        ESP_LOGW(TAG, "unpaired: %s", s_state.reason);
    }
}

static bool derive_session_key(const uint8_t keyer_id[6], const uint8_t initiator[16],
                               const uint8_t responder[16], uint8_t out[32])
{
    static const char label[] = "W1VE-SESSION-v2";
    uint8_t material[(sizeof(label) - 1) + 6 + 6 + 16 + 16];
    size_t offset = 0;
    memcpy(material + offset, label, sizeof(label) - 1);
    offset += sizeof(label) - 1;
    memcpy(material + offset, s_state.player_id, 6);
    offset += 6;
    memcpy(material + offset, keyer_id, 6);
    offset += 6;
    memcpy(material + offset, initiator, 16);
    offset += 16;
    memcpy(material + offset, responder, 16);
    offset += 16;
    return sha256(material, offset, out);
}

static bool accept_rx_sequence_locked(uint32_t sequence)
{
    if (sequence == 0) {
        return false;
    }
    if (!s_state.have_rx_sequence) {
        s_state.have_rx_sequence = true;
        s_state.highest_rx_sequence = sequence;
        s_state.received_bits = 1;
        return true;
    }
    if (sequence > s_state.highest_rx_sequence) {
        const uint32_t shift = sequence - s_state.highest_rx_sequence;
        s_state.received_bits = shift >= RX_WINDOW_BITS ? 1 : (s_state.received_bits << shift) | 1;
        s_state.highest_rx_sequence = sequence;
        return true;
    }
    const uint32_t distance = s_state.highest_rx_sequence - sequence;
    if (distance >= RX_WINDOW_BITS) {
        return false;
    }
    const uint64_t bit = 1ULL << distance;
    if ((s_state.received_bits & bit) != 0) {
        return false;
    }
    s_state.received_bits |= bit;
    return true;
}

static bool authenticated_locked(const uint8_t *packet, size_t length,
                                 const struct sockaddr_in *source)
{
    if (!s_state.paired || length < HEADER_LEN + TAG_LEN || !same_endpoint(source, &s_state.peer) ||
        memcmp(packet + 6, s_state.player_id, 6) != 0 ||
        read_be32(packet + 12) != s_state.session_id) {
        return false;
    }
    const size_t unsigned_len = length - TAG_LEN;
    uint8_t expected[32];
    if (!hmac_sha256(s_state.session_key, sizeof(s_state.session_key), packet, unsigned_len,
                     expected)) {
        return false;
    }
    return memcmp(expected, packet + unsigned_len, TAG_LEN) == 0;
}

static bool send_authenticated_locked(uint8_t type, const uint8_t *payload, size_t payload_len)
{
    if (s_tx_fd < 0 || !s_state.paired || payload_len > 64) {
        return false;
    }
    uint8_t packet[HEADER_LEN + 64 + TAG_LEN];
    memcpy(packet, MAGIC, 4);
    packet[4] = PROTOCOL_VERSION;
    packet[5] = type;
    memcpy(packet + 6, s_state.player_id, 6);
    write_be32(packet + 12, s_state.session_id);
    // Reserve the last counter value rather than wrapping: a wrapped sequence would be graded
    // stale by the Keyer's replay window and the session would die silently.
    if (s_state.next_tx_sequence >= UINT32_MAX - 1U) {
        unpair_locked("transport sequence exhausted");
        return false;
    }
    write_be32(packet + 16, ++s_state.next_tx_sequence);
    packet[20] = (uint8_t)payload_len;
    if (payload_len > 0) {
        memcpy(packet + HEADER_LEN, payload, payload_len);
    }
    const size_t unsigned_len = HEADER_LEN + payload_len;
    uint8_t tag[32];
    if (!hmac_sha256(s_state.session_key, sizeof(s_state.session_key), packet, unsigned_len, tag)) {
        return false;
    }
    memcpy(packet + unsigned_len, tag, TAG_LEN);
    return sendto(s_tx_fd, packet, unsigned_len + TAG_LEN, 0,
                  (const struct sockaddr *)&s_state.peer, sizeof(s_state.peer)) >= 0;
}

// ------------------------------------------------------------------ keying

// Peer millisecond clock mapped onto ours: learned when a burst starts, then held, so a packet
// handled a little late still lands on the sidetone where it belongs.
static int32_t s_clock_off_ms;
static bool s_have_clock_off;
static uint32_t s_last_edge_rx_ms;

// A complete frame is the lease: repeating the newest frame repairs a lost key-up. Only the
// element boundaries inside it are timed, and they are timed on the sender's own clock.
static void apply_edge_locked(bool down, uint32_t peer_ms, uint32_t local_ms)
{
    if (down == s_state.key_down) {
        return;
    }
    if (!keyer_remote_edge(down, local_ms)) {
        unpair_locked("local paddle has the line");
        return;
    }
    s_state.key_down = down;
    if (down) {
        s_state.down_ts = peer_ms;
        s_state.have_down_ts = true;
        return;
    }
    if (s_state.have_down_ts && peer_ms > s_state.down_ts) {
        const int64_t held = (int64_t)(peer_ms - s_state.down_ts);
        const int64_t now = now_ms();
        if (held > 8 && held < 400) {
            // The shortest element in a run is the dit. A pause of more than two seconds starts a
            // fresh measurement, so an operator who slows down is followed instead of being held
            // at the old speed until the session ends.
            const bool fresh = now - s_state.dit_at_ms > SPEED_REMEASURE_MS;
            if (s_state.dit_ms == 0 || held < s_state.dit_ms ||
                (fresh && held > s_state.dit_ms * 3 / 2)) {
                s_state.dit_ms = held;
                // Keep the keyer's macro speed in step with the remote, so a macro fired during the
                // QSO matches it. Clamped to the keyer range.
                int wpm = (int)(1200 / s_state.dit_ms);
                if (wpm < KEYER_WPM_MIN) {
                    wpm = KEYER_WPM_MIN;
                }
                if (wpm > KEYER_WPM_MAX) {
                    wpm = KEYER_WPM_MAX;
                }
                keyer_set_remote_speed((uint8_t)wpm);
            }
            s_state.dit_at_ms = now;
        }
    }
    s_state.have_down_ts = false;
}

static void handle_edge_locked(const uint8_t *payload, size_t length)
{
    const uint16_t epoch = read_le16(payload);
    const uint16_t count = read_le16(payload + 2);
    if (count == 0 || count > 4 || length != 4 + (size_t)count * EDGE_ENTRY_LEN) {
        ESP_LOGW(TAG, "malformed edge frame (%u entries, %u bytes)", (unsigned)count,
                 (unsigned)length);
        return;
    }
    for (int i = 0; i < (int)count; i++) {
        const uint8_t *entry = payload + 4 + i * EDGE_ENTRY_LEN;
        if (entry[9] != 0 || read_le16(entry + 10) != 0) {
            ESP_LOGW(TAG, "edge frame with reserved bits set");
            return;
        }
    }

    if (!s_state.have_edge_epoch) {
        s_state.edge_epoch = epoch;
        s_state.have_edge_epoch = true;
    } else if (epoch != s_state.edge_epoch) {
        unpair_locked("edge epoch changed");
        return;
    }

    // A complete frame refreshes the lease even when every entry in it is already applied.
    s_state.last_edge_refresh_ms = now_ms();

    // Entries arrive newest first, so replay them oldest to newest.
    const uint32_t rx_ms = now_ms();
    const uint32_t newest_ts = read_le32(payload + 4 + 4);

    // Learn the sender's clock offset when a burst starts, then hold it: an edge keeps the time
    // the operator actually keyed it, not the time this packet happened to be handled. Stamping
    // arrivals meant a history frame collapsed its elements onto one instant, and a late tick
    // did the same to a run of separate frames.
    if (!s_have_clock_off || rx_ms - s_last_edge_rx_ms > 400) {
        s_clock_off_ms = (int32_t)(rx_ms - newest_ts);
        s_have_clock_off = true;
    }
    s_last_edge_rx_ms = rx_ms;
    {
        const int32_t drift = (int32_t)(rx_ms - (newest_ts + (uint32_t)s_clock_off_ms));
        if (drift > 60 || drift < -200) {
            s_clock_off_ms = (int32_t)(rx_ms - newest_ts);
        }
    }

    for (int i = (int)count - 1; i >= 0; i--) {
        if (!s_state.paired) {
            return;
        }
        const uint8_t *entry = payload + 4 + i * EDGE_ENTRY_LEN;
        const uint32_t sequence = read_le32(entry);
        const uint32_t timestamp = read_le32(entry + 4);
        const bool down = entry[8] != 0;

        if (s_state.have_edge_sequence && sequence <= s_state.last_edge_sequence) {
            continue;
        }
        if (s_state.have_edge_timestamp && timestamp < s_state.last_edge_timestamp) {
            unpair_locked("edge timestamp regressed");
            return;
        }
        if (s_state.have_edge_sequence && sequence > s_state.last_edge_sequence + 1 && down) {
            unpair_locked("unsafe edge sequence gap");
            return;
        }
        s_state.last_edge_sequence = sequence;
        s_state.last_edge_timestamp = timestamp;
        s_state.have_edge_sequence = true;
        s_state.have_edge_timestamp = true;
        const uint32_t local_ms = timestamp + (uint32_t)s_clock_off_ms;
        apply_edge_locked(down, timestamp, local_ms);
    }
}

// ------------------------------------------------------------------ packets

static bool station_unicast(const struct sockaddr_in *source)
{
    if (source == NULL || source->sin_family != AF_INET || source->sin_port == 0 ||
        s_state.station_ip == 0) {
        return false;
    }
    const uint32_t subnet = s_state.station_ip & s_state.station_mask;
    const uint32_t broadcast = subnet | ~s_state.station_mask;
    const uint32_t address = source->sin_addr.s_addr;
    return address != htonl(INADDR_ANY) && address != htonl(INADDR_BROADCAST) &&
           address != s_state.station_ip && (address & s_state.station_mask) == subnet &&
           address != subnet && address != broadcast;
}

static void send_advertisement_locked(const struct sockaddr_in *destination)
{
    if (s_tx_fd < 0) {
        return;
    }
    uint8_t packet[4 + 1 + 1 + 6 + 2 + 1 + REMOTE_NAME_MAX + 1];
    const size_t name_len = strnlen(s_state.name, REMOTE_NAME_MAX);
    size_t offset = 0;
    memcpy(packet + offset, MAGIC, 4);
    offset += 4;
    packet[offset++] = PROTOCOL_VERSION;
    packet[offset++] = TYPE_ADVERTISE;
    memcpy(packet + offset, s_state.player_id, 6);
    offset += 6;
    packet[offset++] = (uint8_t)(REMOTE_TRANSPORT_PORT >> 8);
    packet[offset++] = (uint8_t)(REMOTE_TRANSPORT_PORT & 0xFF);
    packet[offset++] = (uint8_t)name_len;
    memcpy(packet + offset, s_state.name, name_len);
    offset += name_len;
    packet[offset++] = s_state.paired ? 1 : 0;

    struct sockaddr_in target;
    if (destination != NULL) {
        target = *destination;
    } else {
        memset(&target, 0, sizeof(target));
        target.sin_family = AF_INET;
        target.sin_port = htons(REMOTE_DISCOVERY_PORT);
        target.sin_addr.s_addr = (s_state.station_ip & s_state.station_mask) | ~s_state.station_mask;
    }
    sendto(s_tx_fd, packet, offset, 0, (const struct sockaddr *)&target, sizeof(target));
}

static void handle_query(const uint8_t *packet, size_t length, const struct sockaddr_in *source)
{
    if (length != QUERY_LEN || packet[5] != TYPE_QUERY || !station_unicast(source)) {
        return;
    }
    uint8_t zero[6] = {0};
    if (memcmp(packet + 6, zero, 6) == 0) {
        return;
    }
    xSemaphoreTake(s_lock, portMAX_DELAY);
    send_advertisement_locked(source);
    xSemaphoreGive(s_lock);
}

static void handle_pair_request(const uint8_t *packet, size_t length,
                                const struct sockaddr_in *source)
{
    if (length != PAIR_REQUEST_LEN || packet[5] != TYPE_PAIR_REQUEST ||
        memcmp(packet + 6, s_state.player_id, 6) != 0 || !station_unicast(source)) {
        return;
    }
    const uint8_t *keyer_id = packet + 12;
    uint8_t zero[6] = {0};
    if (memcmp(keyer_id, zero, 6) == 0) {
        return;
    }
    const uint8_t *initiator = packet + 18;
    uint8_t responder[16];
    uint8_t key[32];
    uint8_t response[PAIR_ACCEPT_LEN];
    bool accepted = false;

    xSemaphoreTake(s_lock, portMAX_DELAY);
    // Claim the shared remote line. A WAN Keyer already keying this box refuses a LAN pairing, so
    // the two transports cannot both drive the line at once.
    if (!s_state.paired && !rwk_line_claim(RWK_LINE_LAN)) {
        xSemaphoreGive(s_lock);
        ESP_LOGW(TAG, "pair refused: a WAN Keyer holds the line");
        return;
    }
    if (!s_state.paired) {
        random_nonce(responder);
        if (derive_session_key(keyer_id, initiator, responder, key)) {
            clear_session_locked();
            memcpy(s_state.session_key, key, sizeof(s_state.session_key));
            s_state.session_id = read_be32(s_state.session_key);
            s_state.peer = *source;
            memcpy(s_state.last_keyer_id, keyer_id, 6);
            s_state.paired = true;
            s_state.last_packet_ms = now_ms();
            s_state.reason[0] = '\0';
            s_state.key_down = false;

            memcpy(response, MAGIC, 4);
            response[4] = PROTOCOL_VERSION;
            response[5] = TYPE_PAIR_ACCEPT;
            memcpy(response + 6, s_state.player_id, 6);
            memcpy(response + 12, keyer_id, 6);
            memcpy(response + 18, initiator, 16);
            memcpy(response + 34, responder, 16);
            accepted = true;
        } else {
            // Key derivation failed after we claimed the line; give it back.
            rwk_line_release(RWK_LINE_LAN);
        }
    }
    xSemaphoreGive(s_lock);

    if (accepted) {
        sendto(s_tx_fd, response, sizeof(response), 0, (const struct sockaddr *)source,
               sizeof(*source));
        char text[REMOTE_KEYER_LEN];
        keyer_id_text(text, keyer_id);
        ESP_LOGI(TAG, "paired with Keyer %s at %s:%u", text, inet_ntoa(source->sin_addr),
                 (unsigned)ntohs(source->sin_port));
    }
}

static void handle_authenticated(const uint8_t *packet, size_t length,
                                 const struct sockaddr_in *source)
{
    xSemaphoreTake(s_lock, portMAX_DELAY);
    if (!authenticated_locked(packet, length, source)) {
        xSemaphoreGive(s_lock);
        return;
    }
    const uint8_t type = packet[5];
    const uint8_t payload_len = packet[20];
    if (length != (size_t)(HEADER_LEN + payload_len + TAG_LEN)) {
        xSemaphoreGive(s_lock);
        return;
    }
    if (type != TYPE_EDGE && type != TYPE_PING && type != TYPE_PONG && type != TYPE_UNPAIR) {
        xSemaphoreGive(s_lock);
        return;
    }
    if (type == TYPE_UNPAIR && payload_len != 0) {
        xSemaphoreGive(s_lock);
        return;
    }
    // Even unpair has to clear the replay window first: a replayed unpair must not tear down a
    // newer stream.
    if (!accept_rx_sequence_locked(read_be32(packet + 16))) {
        xSemaphoreGive(s_lock);
        return;
    }
    s_state.last_packet_ms = now_ms();
    const uint8_t *payload = packet + HEADER_LEN;

    if (type == TYPE_UNPAIR) {
        unpair_locked("remote Keyer requested unpair");
    } else if (type == TYPE_EDGE) {
        if (payload_len >= EDGE_MIN_LEN && payload_len <= EDGE_MAX_LEN) {
            handle_edge_locked(payload, payload_len);
        }
    } else if (type == TYPE_PING) {
        if (payload_len == 8) {
            send_authenticated_locked(TYPE_PONG, payload, payload_len);
        }
    } else if (type == TYPE_PONG) {
        if (payload_len == 8) {
            const uint64_t stamp_us = read_be64(payload);
            if (s_state.ping_outstanding && stamp_us == s_state.pending_ping) {
                const int64_t elapsed_ms = now_ms() - (int64_t)(stamp_us / 1000);
                s_state.rtt_ms = elapsed_ms > 0 ? (int)elapsed_ms : 0;
                s_state.ping_outstanding = false;
                s_state.missed_pings = 0;
            }
        }
    }
    xSemaphoreGive(s_lock);
}

// ------------------------------------------------------------------ liveness

static void service_session(int64_t now)
{
    xSemaphoreTake(s_lock, portMAX_DELAY);
    if (!s_state.paired) {
        s_next_ping = 0;
        xSemaphoreGive(s_lock);
        return;
    }
    // A Keyer that stops answering and stops keying is gone. Clearing the session here is what
    // puts the box back to advertising; without it a dead peer left the panel claiming a session
    // it could no longer key.
    if (now - s_state.last_packet_ms >= SESSION_SILENT_MS) {
        unpair_locked("Keyer went silent");
        xSemaphoreGive(s_lock);
        return;
    }
    // Pings never extend the lease: only a complete edge frame can hold the line down.
    if (s_state.key_down && now - s_state.last_edge_refresh_ms >= REMOTE_LEASE_MS) {
        unpair_locked("key state refresh timed out");
        xSemaphoreGive(s_lock);
        return;
    }
    if (now >= s_next_ping) {
        if (s_state.ping_outstanding) {
            if (++s_state.missed_pings >= MISSED_PING_LIMIT) {
                unpair_locked("three Keyer ping replies missed");
                xSemaphoreGive(s_lock);
                return;
            }
        }
        uint8_t payload[8];
        const uint64_t stamp = (uint64_t)esp_timer_get_time();
        write_be64(payload, stamp);
        s_state.pending_ping = stamp;
        s_state.ping_outstanding = true;
        send_authenticated_locked(TYPE_PING, payload, sizeof(payload));
        s_next_ping = now + PING_MS;
    }
    xSemaphoreGive(s_lock);
}

// ------------------------------------------------------------------ sockets

static void close_socket(int *fd)
{
    if (*fd >= 0) {
        close(*fd);
        *fd = -1;
    }
}

static int bind_socket(uint16_t port, bool reuse, bool broadcast, uint32_t address)
{
    const int fd = socket(AF_INET, SOCK_DGRAM, IPPROTO_IP);
    if (fd < 0) {
        return -1;
    }
    int enabled = 1;
    if (broadcast) {
        setsockopt(fd, SOL_SOCKET, SO_BROADCAST, &enabled, sizeof(enabled));
    }
    if (reuse) {
        setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &enabled, sizeof(enabled));
    }
    struct sockaddr_in bind_addr;
    memset(&bind_addr, 0, sizeof(bind_addr));
    bind_addr.sin_family = AF_INET;
    bind_addr.sin_port = htons(port);
    bind_addr.sin_addr.s_addr = address;
    if (bind(fd, (const struct sockaddr *)&bind_addr, sizeof(bind_addr)) != 0) {
        close(fd);
        return -1;
    }
    return fd;
}

static void reconcile_station(void)
{
    uint32_t address = 0;
    uint32_t mask = 0;
    if (s_lan_enabled && s_sta_netif != NULL && net_get_status().connected) {
        esp_netif_ip_info_t info = {0};
        if (esp_netif_get_ip_info(s_sta_netif, &info) == ESP_OK && info.ip.addr != 0) {
            address = info.ip.addr;
            mask = info.netmask.addr;
        }
    }

    const bool bound = s_tx_fd >= 0 && s_disc_fd >= 0;
    if (address == s_state.station_ip && mask == s_state.station_mask && bound == (address != 0)) {
        return;
    }
    if (!bound && address == 0) {
        return;
    }

    xSemaphoreTake(s_lock, portMAX_DELAY);
    if (s_state.paired) {
        unpair_locked("station LAN changed");
    }
    s_state.station_ip = address;
    s_state.station_mask = mask;
    xSemaphoreGive(s_lock);

    close_socket(&s_tx_fd);
    close_socket(&s_disc_fd);
    if (address == 0) {
        ESP_LOGW(TAG, "station LAN gone, transport stopped");
        return;
    }

    s_tx_fd = bind_socket(REMOTE_TRANSPORT_PORT, false, true, address);
    s_disc_fd = bind_socket(REMOTE_DISCOVERY_PORT, true, false, address);
    if (s_tx_fd < 0 || s_disc_fd < 0) {
        ESP_LOGE(TAG, "cannot bind udp/%d on the station address", REMOTE_TRANSPORT_PORT);
        close_socket(&s_tx_fd);
        close_socket(&s_disc_fd);
        return;
    }
    s_next_advertise = 0;
    ESP_LOGI(TAG, "player advertising on udp/%d, transport udp/%d", REMOTE_DISCOVERY_PORT,
             REMOTE_TRANSPORT_PORT);
}

static void player_task(void *arg)
{
    (void)arg;
    uint8_t packet[128];

    for (;;) {
        reconcile_station();
        if (s_tx_fd < 0 || s_disc_fd < 0) {
            vTaskDelay(pdMS_TO_TICKS(100));
            continue;
        }

        const int64_t now = now_ms();
        if (now >= s_next_advertise) {
            xSemaphoreTake(s_lock, portMAX_DELAY);
            send_advertisement_locked(NULL);
            xSemaphoreGive(s_lock);
            s_next_advertise = now + ADVERTISE_MS;
        }
        service_session(now);

        fd_set read_fds;
        FD_ZERO(&read_fds);
        FD_SET(s_tx_fd, &read_fds);
        FD_SET(s_disc_fd, &read_fds);
        const int max_fd = s_tx_fd > s_disc_fd ? s_tx_fd : s_disc_fd;
        struct timeval timeout = {.tv_sec = 0, .tv_usec = 50000};
        if (select(max_fd + 1, &read_fds, NULL, NULL, &timeout) <= 0) {
            continue;
        }

        if (FD_ISSET(s_disc_fd, &read_fds)) {
            struct sockaddr_in source;
            socklen_t source_len = sizeof(source);
            const int length = recvfrom(s_disc_fd, packet, sizeof(packet), 0,
                                        (struct sockaddr *)&source, &source_len);
            if (length >= 6 && memcmp(packet, MAGIC, 4) == 0 &&
                packet[4] == PROTOCOL_VERSION && packet[5] == TYPE_QUERY) {
                handle_query(packet, (size_t)length, &source);
            }
        }
        if (FD_ISSET(s_tx_fd, &read_fds)) {
            struct sockaddr_in source;
            socklen_t source_len = sizeof(source);
            const int length = recvfrom(s_tx_fd, packet, sizeof(packet), 0,
                                        (struct sockaddr *)&source, &source_len);
            if (length < 6 || memcmp(packet, MAGIC, 4) != 0 || packet[4] != PROTOCOL_VERSION) {
                continue;
            }
            if (packet[5] == TYPE_PAIR_REQUEST) {
                handle_pair_request(packet, (size_t)length, &source);
            } else {
                handle_authenticated(packet, (size_t)length, &source);
            }
        }
    }
}

// ------------------------------------------------------------------ storage

static void store_settings(void)
{
    nvs_handle_t nvs;
    if (nvs_open("w1ve", NVS_READWRITE, &nvs) != ESP_OK) {
        return;
    }
    nvs_set_str(nvs, "pname", s_state.name);
    nvs_set_u8(nvs, "lan", s_lan_enabled ? 1 : 0);
    nvs_commit(nvs);
    nvs_close(nvs);
}

static void load_settings(void)
{
    snprintf(s_state.name, sizeof(s_state.name), "%s", DEFAULT_NAME);
    s_lan_enabled = true;

    nvs_handle_t nvs;
    if (nvs_open("w1ve", NVS_READONLY, &nvs) != ESP_OK) {
        return;
    }
    char name[REMOTE_NAME_MAX + 1] = {0};
    size_t len = sizeof(name);
    if (nvs_get_str(nvs, "pname", name, &len) == ESP_OK && name[0] != '\0') {
        snprintf(s_state.name, sizeof(s_state.name), "%s", name);
    }
    uint8_t lan = 1;
    if (nvs_get_u8(nvs, "lan", &lan) == ESP_OK) {
        s_lan_enabled = lan != 0;
    }
    nvs_close(nvs);
}

// ------------------------------------------------------------------ api

void remote_init(void)
{
    s_lock = xSemaphoreCreateMutex();
    if (s_lock == NULL) {
        ESP_LOGE(TAG, "no lock, LAN transport disabled");
        return;
    }
    memset(&s_state, 0, sizeof(s_state));
    load_settings();
    ESP_ERROR_CHECK(esp_read_mac(s_state.player_id, ESP_MAC_WIFI_STA));
    s_sta_netif = esp_netif_get_handle_from_ifkey("WIFI_STA_DEF");

    char text[REMOTE_KEYER_LEN];
    keyer_id_text(text, s_state.player_id);
    ESP_LOGI(TAG, "player %s as '%s', LAN %s", text, s_state.name, s_lan_enabled ? "on" : "off");

    xTaskCreate(player_task, "player", 6144, NULL, 5, NULL);
}

remote_identity_t remote_get_identity(void)
{
    remote_identity_t out = {0};
    if (s_lock == NULL) {
        return out;
    }
    xSemaphoreTake(s_lock, portMAX_DELAY);
    snprintf(out.name, sizeof(out.name), "%s", s_state.name);
    keyer_id_text(out.player_id, s_state.player_id);
    out.advertising = s_lan_enabled && s_tx_fd >= 0;
    xSemaphoreGive(s_lock);
    return out;
}

bool remote_set_name(const char *name)
{
    if (s_lock == NULL || name == NULL) {
        return false;
    }
    char trimmed[REMOTE_NAME_MAX + 1];
    snprintf(trimmed, sizeof(trimmed), "%s", name);
    if (trimmed[0] == '\0') {
        return false;
    }
    xSemaphoreTake(s_lock, portMAX_DELAY);
    snprintf(s_state.name, sizeof(s_state.name), "%s", trimmed);
    xSemaphoreGive(s_lock);
    store_settings();
    ESP_LOGI(TAG, "Player name set to '%s'", trimmed);
    return true;
}

void remote_set_advertising(bool on)
{
    if (s_lock == NULL) {
        return;
    }
    s_lan_enabled = on;
    if (!on) {
        // Drop the session now rather than at the next reconcile: the switch means stop, and the
        // panel must say Advertising the moment it is pressed.
        xSemaphoreTake(s_lock, portMAX_DELAY);
        if (s_state.paired) {
            unpair_locked("LAN turned off");
        }
        xSemaphoreGive(s_lock);
    }
    store_settings();
    ESP_LOGI(TAG, "LAN transport %s", on ? "on" : "off");
}

remote_status_t remote_get_status(void)
{
    remote_status_t out = {0};
    if (s_lock == NULL) {
        return out;
    }
    xSemaphoreTake(s_lock, portMAX_DELAY);
    out.paired = s_state.paired;
    out.alive = s_state.paired && (now_ms() - s_state.last_packet_ms < (int64_t)SESSION_SILENT_MS);
    out.available = s_tx_fd >= 0;
    out.lan_enabled = s_lan_enabled;
    out.active = s_lan_enabled && s_state.paired && out.alive;
    out.keying = s_state.key_down;
    out.session_id = s_state.session_id;
    out.rtt_ms = s_state.rtt_ms;
    // Once measured, the speed holds for the whole session: a Keyer resting between words is
    // still sending at that speed, and expiring the measurement made the panel and the page fall
    // back to the station speed, which read as the remote speed being wrong.
    if (s_state.dit_ms > 0) {
        int wpm = (int)(1200 / s_state.dit_ms);
        if (wpm < KEYER_WPM_MIN) {
            wpm = KEYER_WPM_MIN;
        }
        if (wpm > KEYER_WPM_MAX) {
            wpm = KEYER_WPM_MAX;
        }
        out.peer_wpm = (uint8_t)wpm;
    }
    snprintf(out.reason, sizeof(out.reason), "%s", s_state.reason);
    if (s_state.paired) {
        keyer_id_text(out.keyer, s_state.last_keyer_id);
        snprintf(out.peer, sizeof(out.peer), "%s:%u", inet_ntoa(s_state.peer.sin_addr),
                 (unsigned)ntohs(s_state.peer.sin_port));
    }
    xSemaphoreGive(s_lock);
    return out;
}

uint8_t remote_peer_wpm(void)
{
    return remote_get_status().peer_wpm;
}

void remote_unpair(void)
{
    if (s_lock == NULL) {
        return;
    }
    xSemaphoreTake(s_lock, portMAX_DELAY);
    unpair_locked("local disconnect");
    xSemaphoreGive(s_lock);
}
