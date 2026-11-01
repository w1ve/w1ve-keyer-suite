#include "player_transport.h"

#include <stdio.h>
#include <string.h>

#include "esp_check.h"
#include "esp_log.h"
#include "esp_mac.h"
#include "esp_random.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/queue.h"
#include "freertos/semphr.h"
#include "freertos/task.h"
#include "lwip/inet.h"
#include "lwip/sockets.h"
#include "psa/crypto.h"
#include "player_gpio.h"
#include "player_log.h"
#include "player_web.h"
#include "player_wifi.h"

static const char *TAG = "w1ve_transport";

#define PROTOCOL_VERSION 2
#define MAGIC "W1VE"
#define PLAYER_ADVERTISE_TYPE 1
#define PAIR_REQUEST_TYPE 2
#define PAIR_ACCEPT_TYPE 3
#define EDGE_TYPE 4
#define PING_TYPE 6
#define PONG_TYPE 7
#define UNPAIR_TYPE 8
#define DISCOVERY_QUERY_TYPE 10
#define HEADER_LEN 21
#define AUTH_TAG_LEN 16
#define EDGE_ENTRY_LEN 12
#define EDGE_FRAME_MIN_LEN 16
#define EDGE_FRAME_MAX_LEN 52
#define PAIR_REQUEST_LEN (4 + 1 + 1 + 6 + 6 + 16)
#define PAIR_ACCEPT_LEN (4 + 1 + 1 + 6 + 6 + 16 + 16)
#define DISCOVERY_QUERY_LEN (4 + 1 + 1 + 6)
#define ADVERTISE_US 1500000
#define PING_US 1000000
#define KEY_LEASE_US ((int64_t)PLAYER_KEY_LEASE_MS * 1000)
#define RX_WINDOW_BITS 64
#define CONTROL_QUEUE_LENGTH 4

typedef enum {
    TRANSPORT_CONTROL_APPLY_CONFIG,
    TRANSPORT_CONTROL_UNPAIR,
    TRANSPORT_CONTROL_SYNC_NETWORK,
    TRANSPORT_CONTROL_STOP,
} transport_control_type_t;

typedef struct {
    transport_control_type_t type;
    player_config_t config;
    TaskHandle_t completion_task;
} transport_control_t;

typedef struct {
    player_config_t config;
    uint8_t player_id[6];
    uint8_t session_key[32];
    struct sockaddr_in peer;
    bool running;
    bool paired;
    bool station_ready;
    uint32_t station_generation;
    uint32_t station_address;
    uint32_t station_netmask;
    uint32_t session_generation;
    int64_t last_session_packet_us;
    int64_t last_edge_refresh_us;
    uint64_t pending_ping_stamp;
    bool ping_outstanding;
    uint8_t missed_pings;
    uint32_t session_id;
    uint32_t next_tx_sequence;
    bool have_rx_sequence;
    uint32_t highest_rx_sequence;
    uint64_t received_sequence_bits;
    uint32_t last_edge_sequence;
    uint32_t last_edge_timestamp;
    uint16_t edge_epoch;
    bool have_edge_sequence;
    bool have_edge_timestamp;
    bool have_edge_epoch;
    int rtt_ms;
    char disconnect_reason[96];
    SemaphoreHandle_t lock;
} transport_state_t;

static transport_state_t s_state;
static QueueHandle_t s_control_queue;
static TaskHandle_t s_transport_task;

static uint32_t read_be32(const uint8_t *src)
{
    return ((uint32_t)src[0] << 24) | ((uint32_t)src[1] << 16) |
           ((uint32_t)src[2] << 8) | src[3];
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
    for (size_t index = 0; index < 8; ++index) {
        value = (value << 8) | src[index];
    }
    return value;
}

static void write_be64(uint8_t *dst, uint64_t value)
{
    for (int index = 7; index >= 0; --index) {
        dst[index] = (uint8_t)value;
        value >>= 8;
    }
}

static uint32_t read_le32(const uint8_t *src)
{
    return (uint32_t)src[0] | ((uint32_t)src[1] << 8) |
           ((uint32_t)src[2] << 16) | ((uint32_t)src[3] << 24);
}

static uint16_t read_le16(const uint8_t *src)
{
    return (uint16_t)src[0] | ((uint16_t)src[1] << 8);
}

static bool sha256(const uint8_t *message, size_t message_len, uint8_t output[32])
{
    size_t output_len = 0;
    psa_status_t status = psa_hash_compute(
        PSA_ALG_SHA_256, message, message_len, output, 32, &output_len);
    if (status == PSA_SUCCESS && output_len == 32) {
        return true;
    }
    memset(output, 0, 32);
    ESP_LOGE(TAG, "SHA-256 computation failed: %d", (int)status);
    return false;
}

static bool hmac_sha256(const uint8_t *key, size_t key_len, const uint8_t *message,
                        size_t message_len, uint8_t output[32])
{
    psa_key_attributes_t attributes = PSA_KEY_ATTRIBUTES_INIT;
    psa_set_key_type(&attributes, PSA_KEY_TYPE_HMAC);
    psa_set_key_bits(&attributes, key_len * 8);
    psa_set_key_usage_flags(&attributes, PSA_KEY_USAGE_SIGN_MESSAGE);
    psa_set_key_algorithm(&attributes, PSA_ALG_HMAC(PSA_ALG_SHA_256));
    mbedtls_svc_key_id_t key_id = MBEDTLS_SVC_KEY_ID_INIT;
    size_t output_len = 0;
    psa_status_t status = psa_import_key(&attributes, key, key_len, &key_id);
    if (status == PSA_SUCCESS) {
        status = psa_mac_compute(key_id, PSA_ALG_HMAC(PSA_ALG_SHA_256),
                                 message, message_len, output, 32, &output_len);
        psa_destroy_key(key_id);
    }
    psa_reset_key_attributes(&attributes);
    if (status == PSA_SUCCESS && output_len == 32) {
        return true;
    }
    memset(output, 0, 32);
    ESP_LOGE(TAG, "HMAC computation failed: %d", (int)status);
    return false;
}

static bool matches_player_id(const uint8_t *packet)
{
    return memcmp(packet + 6, s_state.player_id, sizeof(s_state.player_id)) == 0;
}

static bool same_endpoint(const struct sockaddr_in *left, const struct sockaddr_in *right)
{
    return left->sin_addr.s_addr == right->sin_addr.s_addr && left->sin_port == right->sin_port;
}

static void random_nonce(uint8_t nonce[16])
{
    for (size_t index = 0; index < 16; index += 4) {
        write_be32(nonce + index, esp_random());
    }
}

static void advance_session_generation_locked(void)
{
    if (++s_state.session_generation == 0) {
        ++s_state.session_generation;
    }
}

static void clear_session_locked(void)
{
    memset(s_state.session_key, 0, sizeof(s_state.session_key));
    memset(&s_state.peer, 0, sizeof(s_state.peer));
    s_state.session_id = 0;
    s_state.next_tx_sequence = 0;
    s_state.have_rx_sequence = false;
    s_state.highest_rx_sequence = 0;
    s_state.received_sequence_bits = 0;
    s_state.last_edge_sequence = 0;
    s_state.last_edge_timestamp = 0;
    s_state.edge_epoch = 0;
    s_state.have_edge_sequence = false;
    s_state.have_edge_timestamp = false;
    s_state.have_edge_epoch = false;
    s_state.last_session_packet_us = 0;
    s_state.last_edge_refresh_us = 0;
    s_state.pending_ping_stamp = 0;
    s_state.ping_outstanding = false;
    s_state.missed_pings = 0;
    s_state.rtt_ms = 0;
}

static esp_err_t unpair_locked(const char *reason)
{
    const bool was_paired = s_state.paired;
    advance_session_generation_locked();
    s_state.paired = false;
    clear_session_locked();
    strlcpy(s_state.disconnect_reason, reason, sizeof(s_state.disconnect_reason));
    const esp_err_t release_err = player_gpio_deactivate_session();
    if (was_paired) {
        player_log_write("warn", "Unpaired: %s", reason);
    }
    return release_err;
}

static bool derive_session_key(const uint8_t keyer_id[6], const uint8_t initiator_nonce[16],
                               const uint8_t responder_nonce[16], uint8_t output[32])
{
    const char label[] = "W1VE-SESSION-v2";
    uint8_t material[(sizeof(label) - 1) + 6 + 6 + 16 + 16];
    size_t offset = 0;
    memcpy(material + offset, label, sizeof(label) - 1);
    offset += sizeof(label) - 1;
    memcpy(material + offset, s_state.player_id, 6);
    offset += 6;
    memcpy(material + offset, keyer_id, 6);
    offset += 6;
    memcpy(material + offset, initiator_nonce, 16);
    offset += 16;
    memcpy(material + offset, responder_nonce, 16);
    offset += 16;
    return sha256(material, offset, output);
}

static bool accept_rx_sequence_locked(uint32_t sequence)
{
    if (sequence == 0) {
        return false;
    }
    if (!s_state.have_rx_sequence) {
        s_state.have_rx_sequence = true;
        s_state.highest_rx_sequence = sequence;
        s_state.received_sequence_bits = 1;
        return true;
    }
    if (sequence > s_state.highest_rx_sequence) {
        const uint32_t shift = sequence - s_state.highest_rx_sequence;
        s_state.received_sequence_bits = shift >= RX_WINDOW_BITS
            ? 1
            : (s_state.received_sequence_bits << shift) | 1;
        s_state.highest_rx_sequence = sequence;
        return true;
    }
    const uint32_t distance = s_state.highest_rx_sequence - sequence;
    if (distance >= RX_WINDOW_BITS) {
        return false;
    }
    const uint64_t bit = 1ULL << distance;
    if (s_state.received_sequence_bits & bit) {
        return false;
    }
    s_state.received_sequence_bits |= bit;
    return true;
}

static bool authenticated_packet_locked(const uint8_t *packet, size_t length,
                                        const struct sockaddr_in *source)
{
    if (!s_state.paired || length < HEADER_LEN + AUTH_TAG_LEN ||
        !same_endpoint(source, &s_state.peer) || !matches_player_id(packet) ||
        read_be32(packet + 12) != s_state.session_id) {
        return false;
    }
    const size_t unsigned_length = length - AUTH_TAG_LEN;
    uint8_t expected[32];
    if (!hmac_sha256(s_state.session_key, sizeof(s_state.session_key), packet, unsigned_length, expected)) {
        return false;
    }
    return memcmp(expected, packet + unsigned_length, AUTH_TAG_LEN) == 0;
}

static bool send_authenticated_locked(int socket_fd, uint8_t type, const uint8_t *payload,
                                      size_t payload_len)
{
    if (socket_fd < 0 || !s_state.station_ready || !s_state.paired || payload_len > 64) {
        return false;
    }
    uint8_t packet[HEADER_LEN + 64 + AUTH_TAG_LEN];
    memcpy(packet, MAGIC, 4);
    packet[4] = PROTOCOL_VERSION;
    packet[5] = type;
    memcpy(packet + 6, s_state.player_id, 6);
    write_be32(packet + 12, s_state.session_id);
    // Reserve the final counter value instead of wrapping to 1. Both peers deliberately use
    // ordinary replay ordering, so wrapped envelopes would be classified as stale traffic.
    if (s_state.next_tx_sequence >= UINT32_MAX - 1U) {
        (void)unpair_locked("transport envelope sequence exhausted");
        return false;
    }
    ++s_state.next_tx_sequence;
    write_be32(packet + 16, s_state.next_tx_sequence);
    packet[20] = (uint8_t)payload_len;
    if (payload_len) {
        memcpy(packet + HEADER_LEN, payload, payload_len);
    }
    const size_t unsigned_length = HEADER_LEN + payload_len;
    uint8_t tag[32];
    if (!hmac_sha256(s_state.session_key, sizeof(s_state.session_key), packet, unsigned_length, tag)) {
        return false;
    }
    memcpy(packet + unsigned_length, tag, AUTH_TAG_LEN);
    return sendto(socket_fd, packet, unsigned_length + AUTH_TAG_LEN, 0,
                  (const struct sockaddr *)&s_state.peer, sizeof(s_state.peer)) >= 0;
}

static bool station_matches_transport(void)
{
    player_wifi_status_t station;
    player_wifi_get_status(&station);
    return s_state.station_ready && station.transport_available &&
           station.generation == s_state.station_generation &&
           station.address == s_state.station_address && station.netmask == s_state.station_netmask;
}

static bool source_is_station_unicast(const struct sockaddr_in *source)
{
    if (!source || source->sin_family != AF_INET || source->sin_port == 0 || !station_matches_transport()) {
        return false;
    }
    const uint32_t source_address = source->sin_addr.s_addr;
    const uint32_t subnet = s_state.station_address & s_state.station_netmask;
    const uint32_t broadcast = subnet | ~s_state.station_netmask;
    return source_address != htonl(INADDR_ANY) && source_address != htonl(INADDR_BROADCAST) &&
           source_address != s_state.station_address && (source_address & s_state.station_netmask) == subnet &&
           source_address != subnet && source_address != broadcast;
}

static void send_player_advertisement(int transport_fd, const struct sockaddr_in *destination)
{
    uint8_t packet[4 + 1 + 1 + 6 + 2 + 1 + PLAYER_NAME_MAX + 1];
    struct sockaddr_in target;
    char name[PLAYER_NAME_MAX + 1];
    uint8_t player_id[6];
    uint8_t flags;
    xSemaphoreTake(s_state.lock, portMAX_DELAY);
    strlcpy(name, s_state.config.device_name, sizeof(name));
    memcpy(player_id, s_state.player_id, sizeof(player_id));
    flags = s_state.paired ? 1 : 0;
    const uint32_t station_address = s_state.station_address;
    const uint32_t station_netmask = s_state.station_netmask;
    const bool station_ready = s_state.station_ready;
    xSemaphoreGive(s_state.lock);
    if (!station_ready || transport_fd < 0) {
        return;
    }

    const size_t name_len = strnlen(name, PLAYER_NAME_MAX);
    size_t offset = 0;
    memcpy(packet + offset, MAGIC, 4); offset += 4;
    packet[offset++] = PROTOCOL_VERSION;
    packet[offset++] = PLAYER_ADVERTISE_TYPE;
    memcpy(packet + offset, player_id, 6); offset += 6;
    packet[offset++] = (uint8_t)(PLAYER_TRANSPORT_PORT >> 8);
    packet[offset++] = (uint8_t)(PLAYER_TRANSPORT_PORT & 0xffu);
    packet[offset++] = (uint8_t)name_len;
    memcpy(packet + offset, name, name_len); offset += name_len;
    packet[offset++] = flags;

    if (destination) {
        target = *destination;
    } else {
        target = (struct sockaddr_in) {
            .sin_family = AF_INET,
            .sin_port = htons(PLAYER_DISCOVERY_PORT),
            .sin_addr.s_addr = (station_address & station_netmask) | ~station_netmask,
        };
    }
    (void)sendto(transport_fd, packet, offset, 0, (const struct sockaddr *)&target, sizeof(target));
}

static void handle_discovery_query(int transport_fd, const uint8_t *packet, size_t length,
                                   const struct sockaddr_in *source)
{
    if (!source_is_station_unicast(source) || length != DISCOVERY_QUERY_LEN ||
        packet[5] != DISCOVERY_QUERY_TYPE || memcmp(packet + 6, "\0\0\0\0\0\0", 6) == 0) {
        return;
    }
    // The Player remains the advertiser: this only makes an advertisement reach a Keyer when the
    // station LAN drops broadcast packets between radio bands.
    send_player_advertisement(transport_fd, source);
}

static void handle_pair_request(int transport_fd, const uint8_t *packet, size_t length,
                                const struct sockaddr_in *source)
{
    if (!source_is_station_unicast(source) || length != PAIR_REQUEST_LEN ||
        packet[5] != PAIR_REQUEST_TYPE || !matches_player_id(packet) ||
        memcmp(packet + 12, "\0\0\0\0\0\0", 6) == 0) {
        return;
    }
    const uint8_t *keyer_id = packet + 12;
    const uint8_t *initiator_nonce = packet + 18;
    uint8_t responder_nonce[16];
    uint8_t response[PAIR_ACCEPT_LEN];
    bool paired = false;

    xSemaphoreTake(s_state.lock, portMAX_DELAY);
    if (!s_state.paired && station_matches_transport()) {
        random_nonce(responder_nonce);
        uint8_t session_key[32];
        if (derive_session_key(keyer_id, initiator_nonce, responder_nonce, session_key)) {
            clear_session_locked();
            advance_session_generation_locked();
            memcpy(s_state.session_key, session_key, sizeof(s_state.session_key));
            s_state.session_id = read_be32(s_state.session_key);
            s_state.peer = *source;
            s_state.paired = true;
            s_state.last_session_packet_us = esp_timer_get_time();
            s_state.disconnect_reason[0] = '\0';
            if (player_gpio_activate_session(s_state.session_generation) == ESP_OK) {
                memcpy(response, MAGIC, 4);
                response[4] = PROTOCOL_VERSION;
                response[5] = PAIR_ACCEPT_TYPE;
                memcpy(response + 6, s_state.player_id, 6);
                memcpy(response + 12, keyer_id, 6);
                memcpy(response + 18, initiator_nonce, 16);
                memcpy(response + 34, responder_nonce, 16);
                paired = true;
            } else {
                s_state.paired = false;
                clear_session_locked();
                strlcpy(s_state.disconnect_reason, "station LAN unavailable", sizeof(s_state.disconnect_reason));
            }
        }
    }
    xSemaphoreGive(s_state.lock);

    if (paired) {
        (void)sendto(transport_fd, response, sizeof(response), 0,
                     (const struct sockaddr *)source, sizeof(*source));
        player_log_write("info", "W1VE ESP32 Player paired with %s:%u",
                         inet_ntoa(source->sin_addr), ntohs(source->sin_port));
        player_web_publish_status();
    }
}

static void handle_authenticated(int transport_fd, const uint8_t *packet, size_t length,
                                 const struct sockaddr_in *source)
{
    if (!source_is_station_unicast(source)) {
        return;
    }
    bool publish_status = false;
    xSemaphoreTake(s_state.lock, portMAX_DELAY);
    if (!authenticated_packet_locked(packet, length, source)) {
        xSemaphoreGive(s_state.lock);
        return;
    }
    const uint8_t type = packet[5];
    const uint32_t sequence = read_be32(packet + 16);
    const uint8_t payload_len = packet[20];
    if (length != (size_t)(HEADER_LEN + payload_len + AUTH_TAG_LEN)) {
        xSemaphoreGive(s_state.lock);
        return;
    }
    const uint8_t *payload = packet + HEADER_LEN;

    if (type != EDGE_TYPE && type != PING_TYPE && type != PONG_TYPE && type != UNPAIR_TYPE) {
        xSemaphoreGive(s_state.lock);
        return;
    }
    if (type == UNPAIR_TYPE && payload_len != 0) {
        xSemaphoreGive(s_state.lock);
        return;
    }
    // Every authenticated envelope, including unpair, passes the bounded replay window before it
    // can affect session state. A replayed unpair must not tear down a newer packet stream.
    if (!accept_rx_sequence_locked(sequence)) {
        xSemaphoreGive(s_state.lock);
        return;
    }
    s_state.last_session_packet_us = esp_timer_get_time();

    if (type == UNPAIR_TYPE) {
        (void)unpair_locked("remote Keyer requested unpair");
        xSemaphoreGive(s_state.lock);
        player_web_publish_status();
        return;
    }

    if (type == EDGE_TYPE && payload_len >= EDGE_FRAME_MIN_LEN && payload_len <= EDGE_FRAME_MAX_LEN) {
        const uint16_t epoch = read_le16(payload);
        const uint16_t count = read_le16(payload + 2);
        if (count == 0 || count > 4 || payload_len != 4 + count * EDGE_ENTRY_LEN) {
            player_log_write("warn", "Rejected malformed RWK-PADDLE frame");
        } else {
            bool malformed = false;
            for (int index = (int)count - 1; index >= 0; --index) {
                const uint8_t *entry = payload + 4 + index * EDGE_ENTRY_LEN;
                if (entry[9] != 0 || read_le16(entry + 10) != 0) {
                    malformed = true;
                    break;
                }
            }
            if (malformed) {
                player_log_write("warn", "Rejected RWK-PADDLE frame with reserved v2 bits");
            } else if (!s_state.have_edge_epoch) {
                s_state.edge_epoch = epoch;
                s_state.have_edge_epoch = true;
            } else if (epoch != s_state.edge_epoch) {
                (void)unpair_locked("edge epoch changed");
                publish_status = true;
            }

            if (s_state.paired && !malformed) {
                // A complete current-plus-history frame is the key-state lease. Repeating the
                // same latest frame is safe and repairs a lost terminal key-up without a separate
                // message directly changing GPIO.
                s_state.last_edge_refresh_us = esp_timer_get_time();
                for (int index = (int)count - 1; index >= 0; --index) {
                    const uint8_t *entry = payload + 4 + index * EDGE_ENTRY_LEN;
                    const uint32_t edge_sequence = read_le32(entry);
                    const uint32_t timestamp = read_le32(entry + 4);
                    const bool key_down = entry[8] != 0;
                    if (s_state.have_edge_sequence && edge_sequence <= s_state.last_edge_sequence) {
                        continue;
                    }
                    if (s_state.have_edge_timestamp && timestamp < s_state.last_edge_timestamp) {
                        (void)unpair_locked("edge timestamp regressed");
                        publish_status = true;
                        break;
                    }
                    if (s_state.have_edge_sequence && edge_sequence > s_state.last_edge_sequence + 1 && key_down) {
                        (void)unpair_locked("unsafe edge sequence gap");
                        publish_status = true;
                        break;
                    }
                    s_state.last_edge_sequence = edge_sequence;
                    s_state.last_edge_timestamp = timestamp;
                    s_state.have_edge_sequence = true;
                    s_state.have_edge_timestamp = true;
                    if (player_gpio_apply_key(s_state.session_generation, key_down) != ESP_OK) {
                        (void)unpair_locked("GPIO output application failed");
                        publish_status = true;
                        break;
                    }
                    publish_status = true;
                }
            }
        }
    } else if (type == PING_TYPE && payload_len == 8) {
        if (!send_authenticated_locked(transport_fd, PONG_TYPE, payload, payload_len) && !s_state.paired) {
            publish_status = true;
        }
    } else if (type == PONG_TYPE && payload_len == 8) {
        const uint64_t stamp = read_be64(payload);
        if (s_state.ping_outstanding && stamp == s_state.pending_ping_stamp) {
            const int64_t elapsed_us = esp_timer_get_time() - (int64_t)stamp;
            s_state.rtt_ms = elapsed_us > 0 ? (int)(elapsed_us / 1000) : 0;
            s_state.ping_outstanding = false;
            s_state.missed_pings = 0;
            publish_status = true;
        }
    }
    xSemaphoreGive(s_state.lock);
    if (publish_status) {
        player_web_publish_status();
    }
}

static void service_session(int transport_fd, int64_t now, int64_t *next_ping)
{
    bool publish_status = false;
    xSemaphoreTake(s_state.lock, portMAX_DELAY);
    if (!s_state.paired) {
        *next_ping = 0;
        xSemaphoreGive(s_state.lock);
        return;
    }

    // Never let PING/PONG keep an asserted key alive. Only a valid type-4 frame refreshes this
    // lease, and it is repeated by the Keyer's edge refresh sender while key state is active.
    if (player_gpio_key_down() && now - s_state.last_edge_refresh_us >= KEY_LEASE_US) {
        (void)unpair_locked("key state refresh timed out");
        publish_status = true;
    }

    if (s_state.paired && now >= *next_ping) {
        if (s_state.ping_outstanding) {
            ++s_state.missed_pings;
            if (s_state.missed_pings >= PLAYER_MISSED_PING_LIMIT) {
                (void)unpair_locked("three Keyer ping replies missed");
                publish_status = true;
            }
        }
        if (s_state.paired) {
            uint8_t payload[8];
            const uint64_t stamp = (uint64_t)now;
            write_be64(payload, stamp);
            s_state.pending_ping_stamp = stamp;
            s_state.ping_outstanding = true;
            if (!send_authenticated_locked(transport_fd, PING_TYPE, payload, sizeof(payload)) && !s_state.paired) {
                publish_status = true;
            }
            *next_ping = now + PING_US;
        }
    }
    xSemaphoreGive(s_state.lock);
    if (publish_status) {
        player_web_publish_status();
    }
}

static int make_bound_udp_socket(uint16_t port, bool reuse_address, bool broadcast,
                                 uint32_t station_address)
{
    const int socket_fd = socket(AF_INET, SOCK_DGRAM, IPPROTO_IP);
    if (socket_fd < 0) {
        return -1;
    }
    int enabled = 1;
    if (broadcast) {
        (void)setsockopt(socket_fd, SOL_SOCKET, SO_BROADCAST, &enabled, sizeof(enabled));
    }
    if (reuse_address) {
        (void)setsockopt(socket_fd, SOL_SOCKET, SO_REUSEADDR, &enabled, sizeof(enabled));
    }
    const struct sockaddr_in bind_addr = {
        .sin_family = AF_INET,
        .sin_port = htons(port),
        .sin_addr.s_addr = station_address,
    };
    if (bind(socket_fd, (const struct sockaddr *)&bind_addr, sizeof(bind_addr)) != 0) {
        close(socket_fd);
        return -1;
    }
    return socket_fd;
}

static void close_socket(int *socket_fd)
{
    if (*socket_fd >= 0) {
        close(*socket_fd);
        *socket_fd = -1;
    }
}

static void acknowledge_command(const transport_control_t *command, esp_err_t result)
{
    if (command->completion_task) {
        xTaskNotify(command->completion_task, (uint32_t)result, eSetValueWithOverwrite);
    }
}

static bool process_control_commands(void)
{
    transport_control_t command;
    while (xQueueReceive(s_control_queue, &command, 0) == pdPASS) {
        bool publish_status = false;
        esp_err_t result = ESP_OK;
        xSemaphoreTake(s_state.lock, portMAX_DELAY);
        switch (command.type) {
        case TRANSPORT_CONTROL_APPLY_CONFIG:
            memcpy(&s_state.config, &command.config, sizeof(s_state.config));
            result = unpair_locked("local configuration change");
            publish_status = true;
            break;
        case TRANSPORT_CONTROL_UNPAIR:
            result = unpair_locked("local disconnect");
            publish_status = true;
            break;
        case TRANSPORT_CONTROL_SYNC_NETWORK:
            s_state.station_ready = false;
            result = unpair_locked("station LAN unavailable");
            publish_status = true;
            break;
        case TRANSPORT_CONTROL_STOP:
            result = unpair_locked("Player transport stopped");
            s_state.running = false;
            publish_status = true;
            break;
        }
        xSemaphoreGive(s_state.lock);
        if (publish_status) {
            player_web_publish_status();
        }
        acknowledge_command(&command, result);
    }
    return s_state.running;
}

static void reconcile_station(int *transport_fd, int *discovery_fd)
{
    player_wifi_status_t station;
    player_wifi_get_status(&station);
    const bool desired = station.transport_available;
    const bool changed = s_state.station_ready != desired ||
                         s_state.station_generation != station.generation ||
                         s_state.station_address != station.address ||
                         s_state.station_netmask != station.netmask;
    if (!changed) {
        return;
    }

    const char *reason = station.connected ? "station subnet unavailable" : "station LAN unavailable";
    bool publish_status = false;
    xSemaphoreTake(s_state.lock, portMAX_DELAY);
    s_state.station_ready = false;
    s_state.station_generation = station.generation;
    s_state.station_address = station.address;
    s_state.station_netmask = station.netmask;
    if (s_state.paired) {
        (void)unpair_locked(reason);
        publish_status = true;
    } else if (!desired) {
        strlcpy(s_state.disconnect_reason, reason, sizeof(s_state.disconnect_reason));
        publish_status = true;
    }
    xSemaphoreGive(s_state.lock);

    close_socket(discovery_fd);
    close_socket(transport_fd);
    if (!desired) {
        if (publish_status) {
            player_web_publish_status();
        }
        return;
    }

    *transport_fd = make_bound_udp_socket(PLAYER_TRANSPORT_PORT, false, true, station.address);
    if (*transport_fd < 0) {
        ESP_LOGE(TAG, "Cannot bind station Player transport port %d", PLAYER_TRANSPORT_PORT);
    } else {
        *discovery_fd = make_bound_udp_socket(PLAYER_DISCOVERY_PORT, true, false, station.address);
        if (*discovery_fd < 0) {
            ESP_LOGE(TAG, "Cannot bind station discovery port %d", PLAYER_DISCOVERY_PORT);
            close_socket(transport_fd);
        }
    }

    if (*transport_fd >= 0 && *discovery_fd >= 0) {
        player_wifi_status_t current;
        player_wifi_get_status(&current);
        if (current.transport_available && current.generation == station.generation &&
            current.address == station.address && current.netmask == station.netmask) {
            xSemaphoreTake(s_state.lock, portMAX_DELAY);
            s_state.station_ready = true;
            if (!s_state.paired) {
                s_state.disconnect_reason[0] = '\0';
            }
            xSemaphoreGive(s_state.lock);
            player_log_write("info", "W1VE ESP32 Player advertising on station UDP %d", PLAYER_DISCOVERY_PORT);
            publish_status = true;
        } else {
            close_socket(discovery_fd);
            close_socket(transport_fd);
        }
    }
    if (publish_status) {
        player_web_publish_status();
    }
}

static void transport_task(void *arg)
{
    (void)arg;
    int transport_fd = -1;
    int discovery_fd = -1;
    int64_t next_advertise = 0;
    int64_t next_ping = 0;
    uint8_t packet[128];

    while (process_control_commands()) {
        reconcile_station(&transport_fd, &discovery_fd);
        if (transport_fd < 0 || discovery_fd < 0) {
            (void)ulTaskNotifyTake(pdTRUE, pdMS_TO_TICKS(50));
            continue;
        }

        const int64_t now = esp_timer_get_time();
        if (now >= next_advertise) {
            send_player_advertisement(transport_fd, NULL);
            next_advertise = now + ADVERTISE_US;
        }
        service_session(transport_fd, now, &next_ping);

        fd_set read_fds;
        FD_ZERO(&read_fds);
        FD_SET(transport_fd, &read_fds);
        FD_SET(discovery_fd, &read_fds);
        const int max_fd = transport_fd > discovery_fd ? transport_fd : discovery_fd;
        struct timeval timeout = { .tv_sec = 0, .tv_usec = 50000 };
        if (select(max_fd + 1, &read_fds, NULL, NULL, &timeout) <= 0) {
            continue;
        }

        if (FD_ISSET(discovery_fd, &read_fds)) {
            struct sockaddr_in source;
            socklen_t source_len = sizeof(source);
            const int length = recvfrom(discovery_fd, packet, sizeof(packet), 0,
                                        (struct sockaddr *)&source, &source_len);
            if (length >= 6 && memcmp(packet, MAGIC, 4) == 0 &&
                packet[4] == PROTOCOL_VERSION && packet[5] == DISCOVERY_QUERY_TYPE) {
                handle_discovery_query(transport_fd, packet, (size_t)length, &source);
            }
        }
        if (FD_ISSET(transport_fd, &read_fds)) {
            struct sockaddr_in source;
            socklen_t source_len = sizeof(source);
            const int length = recvfrom(transport_fd, packet, sizeof(packet), 0,
                                        (struct sockaddr *)&source, &source_len);
            if (length < 6 || memcmp(packet, MAGIC, 4) != 0 || packet[4] != PROTOCOL_VERSION) {
                continue;
            }
            if (packet[5] == PAIR_REQUEST_TYPE) {
                handle_pair_request(transport_fd, packet, (size_t)length, &source);
            } else {
                handle_authenticated(transport_fd, packet, (size_t)length, &source);
            }
        }
    }
    close_socket(&discovery_fd);
    close_socket(&transport_fd);
    s_transport_task = NULL;
    vTaskDelete(NULL);
}

static esp_err_t send_control_command(transport_control_type_t type, const player_config_t *config)
{
    if (!s_control_queue || !s_transport_task) {
        return ESP_ERR_INVALID_STATE;
    }
    transport_control_t command = {
        .type = type,
        .completion_task = xTaskGetCurrentTaskHandle(),
    };
    if (config) {
        memcpy(&command.config, config, sizeof(command.config));
    }

    uint32_t ignored;
    while (xTaskNotifyWait(0, UINT32_MAX, &ignored, 0) == pdTRUE) {
    }
    if (xQueueSend(s_control_queue, &command, pdMS_TO_TICKS(250)) != pdPASS) {
        return ESP_ERR_TIMEOUT;
    }
    xTaskNotifyGive(s_transport_task);
    uint32_t result = (uint32_t)ESP_FAIL;
    (void)xTaskNotifyWait(0, UINT32_MAX, &result, portMAX_DELAY);
    return (esp_err_t)result;
}

esp_err_t player_transport_start(const player_config_t *config)
{
    if (!config) {
        return ESP_ERR_INVALID_ARG;
    }
    memset(&s_state, 0, sizeof(s_state));
    memcpy(&s_state.config, config, sizeof(s_state.config));
    ESP_RETURN_ON_ERROR(esp_read_mac(s_state.player_id, ESP_MAC_WIFI_STA), TAG, "read MAC");
    s_state.lock = xSemaphoreCreateMutex();
    if (!s_state.lock) {
        return ESP_ERR_NO_MEM;
    }
    s_control_queue = xQueueCreate(CONTROL_QUEUE_LENGTH, sizeof(transport_control_t));
    if (!s_control_queue) {
        vSemaphoreDelete(s_state.lock);
        s_state.lock = NULL;
        return ESP_ERR_NO_MEM;
    }
    s_state.running = true;
    if (xTaskCreate(transport_task, "w1ve_transport", 6144, NULL, 5, &s_transport_task) != pdPASS) {
        s_state.running = false;
        vQueueDelete(s_control_queue);
        s_control_queue = NULL;
        vSemaphoreDelete(s_state.lock);
        s_state.lock = NULL;
        return ESP_ERR_NO_MEM;
    }
    return ESP_OK;
}

esp_err_t player_transport_apply_config(const player_config_t *config)
{
    if (!config) {
        return ESP_ERR_INVALID_ARG;
    }
    return send_control_command(TRANSPORT_CONTROL_APPLY_CONFIG, config);
}

void player_transport_stop(void)
{
    if (send_control_command(TRANSPORT_CONTROL_STOP, NULL) != ESP_OK) {
        (void)player_gpio_deactivate_session();
    }
}

esp_err_t player_transport_unpair(void)
{
    return send_control_command(TRANSPORT_CONTROL_UNPAIR, NULL);
}

void player_transport_network_changed(void)
{
    if (s_transport_task) {
        xTaskNotifyGive(s_transport_task);
    }
}

esp_err_t player_transport_sync_network(void)
{
    return send_control_command(TRANSPORT_CONTROL_SYNC_NETWORK, NULL);
}

esp_err_t player_transport_get_status(player_transport_status_t *status)
{
    if (!status) {
        return ESP_ERR_INVALID_ARG;
    }
    memset(status, 0, sizeof(*status));
    if (!s_state.lock) {
        return ESP_ERR_INVALID_STATE;
    }
    xSemaphoreTake(s_state.lock, portMAX_DELAY);
    status->paired = s_state.paired;
    status->peer_alive = s_state.paired &&
        (esp_timer_get_time() - s_state.last_session_packet_us < (int64_t)PLAYER_MISSED_PING_LIMIT * PING_US);
    status->transport_available = s_state.station_ready;
    status->session_id = s_state.session_id;
    status->last_transport_sequence = s_state.highest_rx_sequence;
    status->last_edge_sequence = s_state.last_edge_sequence;
    status->rtt_ms = s_state.rtt_ms;
    status->missed_pings = s_state.missed_pings;
    strlcpy(status->disconnect_reason, s_state.disconnect_reason, sizeof(status->disconnect_reason));
    if (s_state.paired) {
        snprintf(status->peer_address, sizeof(status->peer_address), "%s:%u",
                 inet_ntoa(s_state.peer.sin_addr), ntohs(s_state.peer.sin_port));
    }
    xSemaphoreGive(s_state.lock);
    return ESP_OK;
}
