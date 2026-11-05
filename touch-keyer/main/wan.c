// WAN transport: a remote W1VE Keyer over the Internet, via the RWK relay and WireGuard.
//
// This is the Station/Player end of the desktop RWK WAN flow (see WAN.md):
//   1. bind a UDP socket, learn the NAT-mapped port with STUN;
//   2. TCP to the relay, send a Pair frame (our key + mapped port + the shared code);
//   3. on Paired, try a direct WireGuard hole-punch to the peer's address;
//   4. if that fails, Register on the relay and run WireGuard over Send/Recv frames;
//   5. carry the keying edge UDP flow (10.6.0.1:5555 <-> 10.6.0.2:5555) inside the tunnel and feed
//      it to the shared edge applier, exactly as the LAN path feeds it.
//
// One task owns every socket and all WireGuard state. It never blocks: sockets are non-blocking and
// it polls on a short select. The keyer runs on its own higher-priority task and is untouched.
#include "wan.h"

#include <errno.h>
#include <stdio.h>
#include <string.h>

#include "esp_log.h"
#include "esp_mac.h"
#include "esp_netif.h"
#include "esp_random.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/queue.h"
#include "freertos/task.h"
#include "keyer.h"
#include "lwip/netdb.h"
#include "lwip/sockets.h"
#include "net.h"
#include "nvs.h"
#include "rwk/rwk_crypto.h"
#include "rwk/rwk_edge.h"
#include "rwk/rwk_frame.h"
#include "rwk/rwk_inner.h"
#include "rwk/rwk_jitter.h"
#include "rwk/rwk_lineclaim.h"
#include "rwk/rwk_stun.h"
#include "rwk/rwk_wg.h"

static const char *TAG = "wan";

// From the W1VE Keyer Suite (pairing.rs DEFAULT_WAN_RELAY). Editable on the web page.
#define WAN_DEFAULT_RELAY "107.172.138.103:9000"
#define WAN_DEFAULT_STUN "stun.l.google.com:19302"
// The pairing code defaults to a random 9-digit number generated once per device, so two boxes out
// of the box do not collide on a shared relay. The operator can change it to anything (e.g. a
// callsign) on the web page. Clearing the field generates a fresh random code.
#define WAN_CODE_DIGITS 9

// WireGuard datagram tracing for diagnosing pairing. Off by default; build with -DWAN_WG_TRACE=1
// to log each received datagram's type, length, index, counter and decode result.
#ifndef WAN_WG_TRACE
#define WAN_WG_TRACE 0
#endif

#define STUN_TIMEOUT_MS 1500
#define RENDEZVOUS_TIMEOUT_MS 90000
#define PUNCH_TIMEOUT_MS 6000
#define RELAY_HANDSHAKE_TIMEOUT_MS 20000
#define REINITIATE_MS 250
#define HEARTBEAT_MS 250
#define PEER_SILENCE_MS 1500
#define RECONNECT_WINDOW_MS 15000
#define LINK_STABLE_MS 5000
#define BACKOFF_MIN_MS 1000
#define BACKOFF_MAX_MS 30000
#define TUNNEL_TICK_MS 100

// CW playout buffer added at the start of each keying burst, so bunched frames are reordered into
// clean elements. Direct paths are near-LAN; the relay adds a round trip and batches, so it gets a
// larger cushion. These are one-time latency on the remote op's keying, chosen as small as will
// still smooth a DERP path's delivery.
#define WAN_PLAYOUT_DIRECT_MS 60
#define WAN_PLAYOUT_RELAY_MS 120

// ------------------------------------------------------------------ state

typedef struct {
    // Persisted settings.
    bool enabled;
    char relay[WAN_RELAY_MAX + 1];
    char code[WAN_CODE_MAX + 1];
    char stun[WAN_STUN_MAX + 1];

    // Identity.
    uint8_t sk[32];
    uint8_t pk[32];
    char pubkey_fp[WAN_FP_LEN + 8];

    // Live.
    wan_state_t state;
    char reason[WAN_REASON_LEN];
    bool direct;
    int rtt_ms;
    uint32_t jitter_ms;

    // Peer learned from rendezvous.
    uint8_t peer_pk[32];
    uint32_t peer_ip_be;
    uint16_t peer_port;
    bool have_peer_addr;
} wan_shared_t;

static wan_shared_t s;
static SemaphoreHandle_t s_lock;
static esp_netif_t *s_sta_netif;

// Owned by the task only (no lock needed).
static int s_udp = -1;         // WireGuard UDP socket (direct path and STUN)
static int s_tcp = -1;         // relay TCP connection (rendezvous, then relay data)
static uint16_t s_mapped_port; // our NAT-mapped UDP port from STUN, else the local port
static rwk_wg_t s_wg;
static bool s_wg_live;
static rwk_edge_t s_edge;
static rwk_jitter_t s_jitter;
static struct sockaddr_in s_direct_peer;
static int64_t s_last_heard_ms;
static int64_t s_connected_at_ms;
static int64_t s_backoff_ms;
static int64_t s_budget_start_ms;
static bool s_budget_open;
static uint64_t s_pending_ping;
static bool s_ping_outstanding;
static uint8_t s_relay_buf[2048];
static rwk_parser_t s_relay_parser;

// Playout queue: edges are accepted by rwk_edge with an absolute local play time and fired by a
// dedicated high-priority task when that instant arrives, so the KEY line keeps the sender's exact
// element spacing even when relay/TCP batching delivers several frames at once. This is why WAN CW
// is clean: the network task never touches the key, it only schedules.
typedef struct {
    bool down;
    int64_t play_ms;
    uint32_t sidetone_ms;
} wan_edge_item_t;

static QueueHandle_t s_playout_q;
static TaskHandle_t s_playout_task;
static volatile bool s_playout_paddle_lost;  // the playout task saw a local paddle take the line

static int64_t now_ms(void)
{
    return esp_timer_get_time() / 1000;
}

// ------------------------------------------------------------------ edge applier hooks

// Called by rwk_edge for each accepted edge: enqueue it for the playout task to fire at play_ms.
// Returns false only if a local paddle has already taken the line, which ends the session.
static bool edge_key(bool down, uint32_t sidetone_ms, int64_t play_ms, void *ctx)
{
    (void)ctx;
    if (s_playout_paddle_lost) {
        return false;
    }
    if (s_playout_q == NULL) {
        return keyer_remote_edge(down, sidetone_ms);  // no scheduler (should not happen): apply now
    }
    wan_edge_item_t item = {.down = down, .play_ms = play_ms, .sidetone_ms = sidetone_ms};
    // The queue is sized for the worst burst; a full queue means the far end is flooding, so drop
    // rather than block the network task (keying safety: never stall).
    (void)xQueueSend(s_playout_q, &item, 0);
    return true;
}

// Fires scheduled edges at their play time. Highest WAN-side priority so playout timing is precise,
// but still below the keyer's own esp_timer task. Sleeps exactly until the next edge is due.
static void playout_task(void *arg)
{
    (void)arg;
    wan_edge_item_t item;
    for (;;) {
        if (xQueueReceive(s_playout_q, &item, portMAX_DELAY) != pdTRUE) {
            continue;
        }
        // Wait until this edge's instant. Relative delay keeps us accurate against the monotonic
        // clock; a late edge (delay <= 0) fires immediately.
        const int64_t wait = item.play_ms - now_ms();
        if (wait > 0) {
            vTaskDelay(pdMS_TO_TICKS(wait));
        }
#if WAN_WG_TRACE
        static int64_t s_prev_fire;
        const int64_t fire = now_ms();
        ESP_LOGI(TAG, "play %s dt=%lld late=%lld", item.down ? "DOWN" : "up ",
                 (long long)(s_prev_fire ? fire - s_prev_fire : 0), (long long)(fire - item.play_ms));
        s_prev_fire = fire;
#endif
        if (!keyer_remote_edge(item.down, item.sidetone_ms)) {
            // A local paddle owns the line: stop driving it and signal the session to end.
            s_playout_paddle_lost = true;
        }
    }
}

static void playout_flush(void)
{
    if (s_playout_q != NULL) {
        xQueueReset(s_playout_q);
    }
    s_playout_paddle_lost = false;
}

static int64_t edge_now(void *ctx)
{
    (void)ctx;
    return now_ms();
}

// ------------------------------------------------------------------ identity and settings

static void fingerprint(char *out, const uint8_t pk[32])
{
    // A short, human-comparable tag so an operator can confirm two ends mean the same key without
    // exposing anything secret (it is derived from the public key).
    snprintf(out, WAN_FP_LEN + 8, "%02x%02x %02x%02x %02x%02x", pk[0], pk[1], pk[2], pk[3], pk[4],
             pk[5]);
}

static void load_identity(void)
{
    nvs_handle_t nvs;
    bool have = false;
    if (nvs_open("w1ve", NVS_READWRITE, &nvs) == ESP_OK) {
        size_t len = 32;
        if (nvs_get_blob(nvs, "wan_sk", s.sk, &len) == ESP_OK && len == 32) {
            have = true;
        }
        if (!have) {
            // First use: generate and persist. The private key never leaves NVS or appears in any
            // API or log line.
            rwk_rng(s.sk, 32);
            nvs_set_blob(nvs, "wan_sk", s.sk, 32);
            nvs_commit(nvs);
            ESP_LOGI(TAG, "generated a new WAN identity");
        }
        nvs_close(nvs);
    } else {
        rwk_rng(s.sk, 32);
    }
    if (!rwk_x25519_pub(s.pk, s.sk)) {
        ESP_LOGE(TAG, "identity public key derivation failed");
    }
    fingerprint(s.pubkey_fp, s.pk);
}

static void store_settings(void);

// Writes a random WAN_CODE_DIGITS-digit decimal pairing code into `out` (size >= WAN_CODE_DIGITS+1).
static void random_code(char *out)
{
    for (int i = 0; i < WAN_CODE_DIGITS; i++) {
        out[i] = (char)('0' + (esp_random() % 10u));
    }
    out[WAN_CODE_DIGITS] = '\0';
}

static void load_settings(void)
{
    snprintf(s.relay, sizeof(s.relay), "%s", WAN_DEFAULT_RELAY);
    snprintf(s.stun, sizeof(s.stun), "%s", WAN_DEFAULT_STUN);
    s.code[0] = '\0';
    s.enabled = false;

    bool have_code = false;
    nvs_handle_t nvs;
    if (nvs_open("w1ve", NVS_READONLY, &nvs) == ESP_OK) {
        char buf[WAN_RELAY_MAX + 1];
        size_t len = sizeof(buf);
        if (nvs_get_str(nvs, "wan_relay", buf, &len) == ESP_OK && buf[0] != '\0') {
            snprintf(s.relay, sizeof(s.relay), "%s", buf);
        }
        char cbuf[WAN_CODE_MAX + 1];
        len = sizeof(cbuf);
        if (nvs_get_str(nvs, "wan_code", cbuf, &len) == ESP_OK && cbuf[0] != '\0') {
            snprintf(s.code, sizeof(s.code), "%s", cbuf);
            have_code = true;
        }
        len = sizeof(buf);
        if (nvs_get_str(nvs, "wan_stun", buf, &len) == ESP_OK && buf[0] != '\0') {
            snprintf(s.stun, sizeof(s.stun), "%s", buf);
        }
        uint8_t en = 0;
        if (nvs_get_u8(nvs, "wan_en", &en) == ESP_OK) {
            s.enabled = en != 0;
        }
        nvs_close(nvs);
    }

    // First boot (or a cleared code): pick a random per-device code and persist it, so two boxes
    // never share the default and the same code is shown every boot.
    if (!have_code) {
        random_code(s.code);
        store_settings();
        ESP_LOGI(TAG, "generated a random pairing code");
    }
}

static void store_settings(void)
{
    nvs_handle_t nvs;
    if (nvs_open("w1ve", NVS_READWRITE, &nvs) != ESP_OK) {
        return;
    }
    nvs_set_str(nvs, "wan_relay", s.relay);
    nvs_set_str(nvs, "wan_code", s.code);
    nvs_set_str(nvs, "wan_stun", s.stun);
    nvs_set_u8(nvs, "wan_en", s.enabled ? 1 : 0);
    nvs_commit(nvs);
    nvs_close(nvs);
}

// ------------------------------------------------------------------ sockets

static void close_fd(int *fd)
{
    if (*fd >= 0) {
        close(*fd);
        *fd = -1;
    }
}

static bool set_nonblock(int fd)
{
    const int flags = fcntl(fd, F_GETFL, 0);
    return flags >= 0 && fcntl(fd, F_SETFL, flags | O_NONBLOCK) == 0;
}

// Splits "host:port" into its parts. Port defaults to `def_port` if absent.
static bool split_host_port(const char *in, char *host, size_t host_cap, uint16_t *port,
                            uint16_t def_port)
{
    const char *colon = strrchr(in, ':');
    size_t host_len = colon != NULL ? (size_t)(colon - in) : strlen(in);
    if (host_len == 0 || host_len >= host_cap) {
        return false;
    }
    memcpy(host, in, host_len);
    host[host_len] = '\0';
    *port = colon != NULL ? (uint16_t)atoi(colon + 1) : def_port;
    return *port != 0;
}

static bool resolve(const char *host, uint16_t port, struct sockaddr_in *out)
{
    memset(out, 0, sizeof(*out));
    out->sin_family = AF_INET;
    out->sin_port = htons(port);
    // Numeric IP first, so a relay given as an address needs no DNS.
    if (inet_aton(host, &out->sin_addr) == 1) {
        return true;
    }
    struct addrinfo hints = {.ai_family = AF_INET, .ai_socktype = SOCK_DGRAM};
    struct addrinfo *res = NULL;
    if (getaddrinfo(host, NULL, &hints, &res) != 0 || res == NULL) {
        return false;
    }
    out->sin_addr = ((struct sockaddr_in *)res->ai_addr)->sin_addr;
    freeaddrinfo(res);
    return true;
}

static uint32_t station_ip(void)
{
    if (s_sta_netif == NULL || !net_get_status().connected) {
        return 0;
    }
    esp_netif_ip_info_t info = {0};
    if (esp_netif_get_ip_info(s_sta_netif, &info) != ESP_OK) {
        return 0;
    }
    return info.ip.addr;
}

// Binds the WireGuard UDP socket on an ephemeral port on the station address.
static bool open_udp(void)
{
    close_fd(&s_udp);
    const uint32_t ip = station_ip();
    if (ip == 0) {
        return false;
    }
    s_udp = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
    if (s_udp < 0) {
        return false;
    }
    struct sockaddr_in bind_addr = {.sin_family = AF_INET, .sin_port = 0, .sin_addr.s_addr = ip};
    if (bind(s_udp, (struct sockaddr *)&bind_addr, sizeof(bind_addr)) != 0 || !set_nonblock(s_udp)) {
        close_fd(&s_udp);
        return false;
    }
    socklen_t len = sizeof(bind_addr);
    getsockname(s_udp, (struct sockaddr *)&bind_addr, &len);
    s_mapped_port = ntohs(bind_addr.sin_port);
    return true;
}

// ------------------------------------------------------------------ STUN

// Asks a STUN server for our NAT-mapped port. Best effort: on any failure we fall back to the
// local port, which still works behind a port-preserving NAT and always works via the relay.
static void run_stun(void)
{
    char host[WAN_STUN_MAX + 1];
    uint16_t port;
    if (s.stun[0] == '\0' || !split_host_port(s.stun, host, sizeof(host), &port, 3478)) {
        return;
    }
    struct sockaddr_in dst;
    if (!resolve(host, port, &dst)) {
        return;
    }
    uint8_t txid[12];
    rwk_rng(txid, 12);
    uint8_t req[RWK_STUN_REQUEST_LEN];
    rwk_stun_build_request(req, txid);
    sendto(s_udp, req, sizeof(req), 0, (struct sockaddr *)&dst, sizeof(dst));

    const int64_t deadline = now_ms() + STUN_TIMEOUT_MS;
    while (now_ms() < deadline) {
        fd_set r;
        FD_ZERO(&r);
        FD_SET(s_udp, &r);
        struct timeval tv = {.tv_sec = 0, .tv_usec = 100000};
        if (select(s_udp + 1, &r, NULL, NULL, &tv) <= 0) {
            continue;
        }
        uint8_t buf[256];
        struct sockaddr_in src;
        socklen_t sl = sizeof(src);
        const int n = recvfrom(s_udp, buf, sizeof(buf), 0, (struct sockaddr *)&src, &sl);
        if (n <= 0 || src.sin_addr.s_addr != dst.sin_addr.s_addr) {
            continue;
        }
        uint32_t ip_be;
        uint16_t mapped;
        if (rwk_stun_parse_response(buf, (size_t)n, txid, &ip_be, &mapped)) {
            s_mapped_port = mapped;
            ESP_LOGI(TAG, "STUN mapped port %u", (unsigned)mapped);
            return;
        }
    }
    ESP_LOGW(TAG, "STUN did not answer; reporting the local port");
}

// ------------------------------------------------------------------ relay TCP

// Connects to the relay with a bounded blocking connect (own task, so a few seconds is fine).
static bool relay_connect(const struct sockaddr_in *addr)
{
    close_fd(&s_tcp);
    s_tcp = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    if (s_tcp < 0) {
        return false;
    }
    struct timeval tv = {.tv_sec = 8, .tv_usec = 0};
    setsockopt(s_tcp, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));
    setsockopt(s_tcp, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof(tv));
    const int one = 1;
    setsockopt(s_tcp, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));
    if (connect(s_tcp, (const struct sockaddr *)addr, sizeof(*addr)) != 0) {
        close_fd(&s_tcp);
        return false;
    }
    return true;
}

static bool tcp_write_all(const uint8_t *buf, size_t len)
{
    size_t sent = 0;
    while (sent < len) {
        const int n = send(s_tcp, buf + sent, len - sent, 0);
        if (n > 0) {
            sent += (size_t)n;
        } else if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR)) {
            vTaskDelay(pdMS_TO_TICKS(2));
        } else {
            return false;
        }
    }
    return true;
}

// ------------------------------------------------------------------ WireGuard send paths

static void send_wg_datagram(const uint8_t *dg, size_t n)
{
    if (s.direct) {
        sendto(s_udp, dg, n, 0, (struct sockaddr *)&s_direct_peer, sizeof(s_direct_peer));
    } else if (s_tcp >= 0) {
        uint8_t frame[RWK_FRAME_HEADER + 32 + RWK_WG_MAX_DATAGRAM];
        const size_t fn = rwk_frame_enc_send(frame, sizeof(frame), s.peer_pk, dg, n);
        if (fn > 0) {
            tcp_write_all(frame, fn);
        }
    }
}

// Wraps an edge payload in an inner UDP packet (Station -> Client) and encrypts it.
static void send_edge_payload(const uint8_t *payload, size_t len)
{
    if (!s_wg_live || !rwk_wg_can_send(&s_wg, now_ms())) {
        return;
    }
    uint8_t pkt[128];
    const size_t ip_len = rwk_inner_build_udp(pkt, sizeof(pkt), RWK_INNER_STATION_IP, RWK_EDGE_PORT,
                                              RWK_INNER_CLIENT_IP, RWK_EDGE_PORT, payload, len);
    if (ip_len == 0) {
        return;
    }
    uint8_t dg[256];
    const size_t n = rwk_wg_encrypt(&s_wg, now_ms(), pkt, ip_len, dg, sizeof(dg));
    if (n > 0) {
        send_wg_datagram(dg, n);
    }
}

// ------------------------------------------------------------------ session lifecycle

static void set_state_locked(wan_state_t st, const char *reason)
{
    s.state = st;
    if (reason != NULL) {
        snprintf(s.reason, sizeof(s.reason), "%s", reason);
    }
}

static void teardown_session(const char *reason)
{
    // Drop any scheduled-but-unfired edges first, so a stale queued key-down cannot fire after the
    // session has gone and leave the rig keyed.
    playout_flush();
    keyer_set_remote_speed(0);  // no remote owns the line: macros use the station speed again
    xSemaphoreTake(s_lock, portMAX_DELAY);
    if (rwk_edge_keying(&s_edge)) {
        keyer_remote_key(false);
    }
    if (s_wg_live) {
        rwk_wg_free(&s_wg);
        s_wg_live = false;
    }
    s.direct = false;
    s.rtt_ms = 0;
    s.have_peer_addr = false;
    s_ping_outstanding = false;
    if (s.state == WAN_DIRECT || s.state == WAN_RELAY || s.state == WAN_PAIRING) {
        set_state_locked(WAN_RECONNECTING, reason);
    }
    xSemaphoreGive(s_lock);
    rwk_line_release(RWK_LINE_WAN);
    close_fd(&s_tcp);
    ESP_LOGW(TAG, "session down: %s", reason != NULL ? reason : "");
}

// ------------------------------------------------------------------ inner packet -> edge applier

static void deliver_edge_liveness(const uint8_t *payload, size_t len)
{
    const rwk_edge_kind_t kind = rwk_edge_classify(payload, len);
    const int64_t now = now_ms();
    if (kind == RWK_EDGE_PING) {
        uint8_t pong[12];
        rwk_edge_build_pong(pong, payload);
        send_edge_payload(pong, 12);
        return;
    }
    if (kind == RWK_EDGE_PONG) {
        const uint64_t stamp = ((uint64_t)payload[4] << 56) | ((uint64_t)payload[5] << 48) |
                               ((uint64_t)payload[6] << 40) | ((uint64_t)payload[7] << 32) |
                               ((uint64_t)payload[8] << 24) | ((uint64_t)payload[9] << 16) |
                               ((uint64_t)payload[10] << 8) | payload[11];
        xSemaphoreTake(s_lock, portMAX_DELAY);
        if (s_ping_outstanding && stamp == s_pending_ping) {
            const int64_t rtt = now - (int64_t)(stamp / 1000000);
            s.rtt_ms = rtt > 0 && rtt < 60000 ? (int)rtt : 0;
            s_ping_outstanding = false;
        }
        xSemaphoreGive(s_lock);
        return;
    }
    if (kind == RWK_EDGE_HEARTBEAT) {
        return;  // liveness only; last-heard is already updated by the caller
    }
    if (kind != RWK_EDGE_FRAME) {
        return;
    }

    // A real edge frame. rwk_edge anchors the sender's clock to local time (plus the buffer delay
    // chosen at session start) and hands each edge an absolute play time; the playout task fires it
    // then. The network path never touches the key, so relay/TCP batching cannot smear the CW.
    const uint32_t newest_ts = (uint32_t)payload[8] | ((uint32_t)payload[9] << 8) |
                               ((uint32_t)payload[10] << 16) | ((uint32_t)payload[11] << 24);
    xSemaphoreTake(s_lock, portMAX_DELAY);
    rwk_jitter_observe(&s_jitter, newest_ts, now);  // kept only for the status jitter readout
    s.jitter_ms = rwk_jitter_estimate_ms(&s_jitter);
    const rwk_edge_status_t st = rwk_edge_feed(&s_edge, payload, len, now);
    // Keep the keyer's macro speed in step with the remote, so a locally played macro matches the
    // QSO speed rather than the station's own setting.
    const uint8_t wpm = rwk_edge_wpm(&s_edge);
    if (wpm > 0) {
        keyer_set_remote_speed(wpm);
    }
    char reason[WAN_REASON_LEN];
    bool end = false;
    if (st == RWK_EDGE_END_SESSION) {
        snprintf(reason, sizeof(reason), "%s", rwk_edge_end_reason(&s_edge));
        end = true;
    }
    xSemaphoreGive(s_lock);
    if (s_playout_paddle_lost && !end) {
        end = true;
        snprintf(reason, sizeof(reason), "local paddle has the line");
    }
    if (end) {
        teardown_session(reason);
    }
}

static void on_inner_packet(const uint8_t *pkt, size_t len)
{
    uint32_t sip, dip;
    uint16_t sp, dp;
    const uint8_t *pl;
    size_t pl_len;
    if (!rwk_inner_parse_udp(pkt, len, &sip, &sp, &dip, &dp, &pl, &pl_len)) {
        return;
    }
    // Only the keying flow from the Client inner address is of interest.
    if (sip != RWK_INNER_CLIENT_IP || dp != RWK_EDGE_PORT) {
        return;
    }
    deliver_edge_liveness(pl, pl_len);
}

// Feeds one received WireGuard datagram into the tunnel and acts on the result.
static void on_wg_datagram(const uint8_t *dg, size_t len, const struct sockaddr_in *src)
{
    uint8_t out[RWK_WG_INITIATION_LEN];
    uint8_t plain[RWK_WG_MAX_DATAGRAM];
    size_t out_len = 0, plain_len = 0;
    const rwk_wg_result_t r =
        rwk_wg_input(&s_wg, now_ms(), dg, len, out, &out_len, plain, &plain_len);

#if WAN_WG_TRACE
    unsigned idx = 0, ctr = 0;
    if (len >= 16 && dg[0] == 4) {
        idx = (unsigned)dg[4] | ((unsigned)dg[5] << 8) | ((unsigned)dg[6] << 16) |
              ((unsigned)dg[7] << 24);
        ctr = (unsigned)dg[8] | ((unsigned)dg[9] << 8) | ((unsigned)dg[10] << 16) |
              ((unsigned)dg[11] << 24);
    }
    ESP_LOGI(TAG, "wg rx: %s type=%u len=%u idx=%08x ctr=%u cur_idx=%08x cur_valid=%d -> result=%d",
             s.direct ? "udp" : "relay", len > 0 ? (unsigned)dg[0] : 0u, (unsigned)len, idx, ctr,
             (unsigned)s_wg.cur.local_index, (int)s_wg.cur.valid, (int)r);
#endif

    if (r == RWK_WG_REPLY || r == RWK_WG_DATA || r == RWK_WG_KEEPALIVE) {
        s_last_heard_ms = now_ms();
        // Adopt the authenticated source on the direct path (WireGuard roaming): a peer whose NAT
        // remapped it still reaches us.
        if (s.direct && src != NULL) {
            s_direct_peer = *src;
        }
    }
    if (r == RWK_WG_REPLY) {
        send_wg_datagram(out, out_len);
    } else if (r == RWK_WG_DATA) {
        on_inner_packet(plain, plain_len);
    }
}

// ------------------------------------------------------------------ connect sequence

// Brings a WireGuard session up on the current transport. `initiate_budget_ms` bounds how long we
// keep re-firing initiations. Returns true once established.
static bool bring_wg_up(int64_t budget_ms, bool punching)
{
    rwk_wg_free(&s_wg);
    if (!rwk_wg_init(&s_wg, s.sk, s.peer_pk)) {
        return false;
    }
    s_wg_live = true;
    // Playout buffer: a direct path is near-LAN, a relayed path adds a round trip through the relay
    // and bunches frames, so it needs a larger cushion to reorder them into clean CW. The buffer is
    // one-time latency added to the remote op's keying, not jitter, so it must be no larger than
    // needed to cover the path's bunching.
    playout_flush();
    const uint32_t delay = s.direct ? WAN_PLAYOUT_DIRECT_MS : WAN_PLAYOUT_RELAY_MS;
    rwk_edge_reset(&s_edge, delay, edge_key, edge_now, NULL);
    rwk_jitter_init(&s_jitter);

    const int64_t deadline = now_ms() + budget_ms;
    int64_t next_init = 0;
    struct timespec wall;
    while (now_ms() < deadline) {
        if (!rwk_wg_can_send(&s_wg, now_ms()) && now_ms() >= next_init) {
            // The Station initiates here too, both for the hole-punch (both ends must punch) and on
            // the relay, where the Client may be waiting. A missing wall clock skips this round.
            clock_gettime(CLOCK_REALTIME, &wall);
            if (wall.tv_sec > 1600000000) {
                uint8_t init[RWK_WG_INITIATION_LEN];
                if (rwk_wg_initiate(&s_wg, now_ms(), (uint64_t)wall.tv_sec, (uint32_t)wall.tv_nsec,
                                    init)) {
                    send_wg_datagram(init, sizeof(init));
                }
            }
            next_init = now_ms() + REINITIATE_MS;
        }
        if (rwk_wg_can_send(&s_wg, now_ms())) {
            return true;
        }

        // Poll both sockets briefly.
        fd_set r;
        FD_ZERO(&r);
        int maxfd = -1;
        if (s_udp >= 0 && (punching || s.direct)) {
            FD_SET(s_udp, &r);
            maxfd = s_udp;
        }
        if (!s.direct && s_tcp >= 0) {
            FD_SET(s_tcp, &r);
            if (s_tcp > maxfd) {
                maxfd = s_tcp;
            }
        }
        if (maxfd < 0) {
            return false;
        }
        struct timeval tv = {.tv_sec = 0, .tv_usec = 50000};
        if (select(maxfd + 1, &r, NULL, NULL, &tv) <= 0) {
            continue;
        }
        if (s_udp >= 0 && FD_ISSET(s_udp, &r)) {
            uint8_t buf[RWK_WG_MAX_DATAGRAM];
            struct sockaddr_in src;
            socklen_t sl = sizeof(src);
            const int n = recvfrom(s_udp, buf, sizeof(buf), 0, (struct sockaddr *)&src, &sl);
            if (n > 0) {
                on_wg_datagram(buf, (size_t)n, &src);
            }
        }
        if (!s.direct && s_tcp >= 0 && FD_ISSET(s_tcp, &r)) {
            uint8_t buf[512];
            const int n = recv(s_tcp, buf, sizeof(buf), 0);
            if (n <= 0) {
                return false;  // relay closed under us
            }
            // Parse relay frames; a Recv carries a WireGuard datagram.
            size_t pos = 0;
            while (pos < (size_t)n) {
                size_t used;
                rwk_frame_t ev;
                const rwk_parse_status_t st =
                    rwk_parser_feed(&s_relay_parser, buf + pos, (size_t)n - pos, &used, &ev);
                pos += used;
                if (st == RWK_PARSE_FRAME && ev.type == RWK_FRAME_RECV && ev.len > 32) {
                    on_wg_datagram(ev.body + 32, ev.len - 32, NULL);
                } else if (st == RWK_PARSE_ERR_TYPE || st == RWK_PARSE_ERR_LENGTH) {
                    return false;
                }
                if (used == 0) {
                    break;
                }
            }
        }
    }
    return false;
}

// The full rendezvous + connect. Returns true with a live session, false to retry.
static bool connect_once(void)
{
    char host[WAN_RELAY_MAX + 1];
    uint16_t port;
    char relay[WAN_RELAY_MAX + 1], code[WAN_CODE_MAX + 1];
    xSemaphoreTake(s_lock, portMAX_DELAY);
    snprintf(relay, sizeof(relay), "%s", s.relay);
    snprintf(code, sizeof(code), "%s", s.code);
    set_state_locked(WAN_RESOLVING, "resolving relay");
    xSemaphoreGive(s_lock);

    if (code[0] == '\0') {
        xSemaphoreTake(s_lock, portMAX_DELAY);
        set_state_locked(WAN_ERROR, "no pairing code set");
        xSemaphoreGive(s_lock);
        return false;
    }
    if (!split_host_port(relay, host, sizeof(host), &port, 9000)) {
        xSemaphoreTake(s_lock, portMAX_DELAY);
        set_state_locked(WAN_ERROR, "bad relay address");
        xSemaphoreGive(s_lock);
        return false;
    }
    struct sockaddr_in relay_addr;
    if (!resolve(host, port, &relay_addr)) {
        xSemaphoreTake(s_lock, portMAX_DELAY);
        set_state_locked(WAN_RECONNECTING, "relay DNS failed");
        xSemaphoreGive(s_lock);
        return false;
    }

    if (!open_udp()) {
        return false;
    }
    run_stun();

    // Rendezvous.
    xSemaphoreTake(s_lock, portMAX_DELAY);
    set_state_locked(WAN_RENDEZVOUS, "pairing via relay");
    xSemaphoreGive(s_lock);
    if (!relay_connect(&relay_addr)) {
        xSemaphoreTake(s_lock, portMAX_DELAY);
        set_state_locked(WAN_RECONNECTING, "relay unreachable");
        xSemaphoreGive(s_lock);
        return false;
    }
    uint8_t pair[RWK_FRAME_HEADER + 34 + WAN_CODE_MAX];
    const size_t pn = rwk_frame_enc_pair(pair, sizeof(pair), s.pk, s_mapped_port, code);
    if (pn == 0 || !tcp_write_all(pair, pn)) {
        return false;
    }

    // Wait for Paired on a fresh parser.
    rwk_parser_init(&s_relay_parser, s_relay_buf, sizeof(s_relay_buf));
    bool paired = false;
    const int64_t rdv_deadline = now_ms() + RENDEZVOUS_TIMEOUT_MS;
    while (now_ms() < rdv_deadline && !paired) {
        if (!s.enabled) {
            return false;
        }
        fd_set r;
        FD_ZERO(&r);
        FD_SET(s_tcp, &r);
        struct timeval tv = {.tv_sec = 0, .tv_usec = 200000};
        if (select(s_tcp + 1, &r, NULL, NULL, &tv) <= 0) {
            continue;
        }
        uint8_t buf[256];
        const int n = recv(s_tcp, buf, sizeof(buf), 0);
        if (n <= 0) {
            return false;
        }
        size_t pos = 0;
        while (pos < (size_t)n) {
            size_t used;
            rwk_frame_t ev;
            const rwk_parse_status_t st =
                rwk_parser_feed(&s_relay_parser, buf + pos, (size_t)n - pos, &used, &ev);
            pos += used;
            if (st == RWK_PARSE_FRAME && ev.type == RWK_FRAME_PAIRED) {
                rwk_paired_t pr;
                if (rwk_frame_dec_paired(ev.body, ev.len, &pr)) {
                    xSemaphoreTake(s_lock, portMAX_DELAY);
                    memcpy(s.peer_pk, pr.peer_key, 32);
                    s.peer_ip_be = pr.ip_be;
                    s.peer_port = pr.port;
                    s.have_peer_addr = pr.has_addr;
                    xSemaphoreGive(s_lock);
                    paired = true;
                }
            } else if (st == RWK_PARSE_ERR_TYPE || st == RWK_PARSE_ERR_LENGTH) {
                return false;
            }
            if (used == 0) {
                break;
            }
        }
    }
    if (!paired) {
        xSemaphoreTake(s_lock, portMAX_DELAY);
        set_state_locked(WAN_RECONNECTING, "no peer at the relay");
        xSemaphoreGive(s_lock);
        return false;
    }
    // The pairing TCP connection has done its job. The relay data path, if needed, is a fresh one.
    close_fd(&s_tcp);

    // Claim the shared line before bringing CW up, so a LAN session cannot also grab it.
    if (!rwk_line_claim(RWK_LINE_WAN)) {
        xSemaphoreTake(s_lock, portMAX_DELAY);
        set_state_locked(WAN_RECONNECTING, "a LAN Keyer holds the line");
        xSemaphoreGive(s_lock);
        return false;
    }

    xSemaphoreTake(s_lock, portMAX_DELAY);
    set_state_locked(WAN_PAIRING, "handshaking");
    xSemaphoreGive(s_lock);

    // Direct first, if the relay gave us an address.
    bool up = false;
    if (s.have_peer_addr) {
        s.direct = true;
        memset(&s_direct_peer, 0, sizeof(s_direct_peer));
        s_direct_peer.sin_family = AF_INET;
        s_direct_peer.sin_addr.s_addr = s.peer_ip_be;
        s_direct_peer.sin_port = htons(s.peer_port);
        up = bring_wg_up(PUNCH_TIMEOUT_MS, true);
    }
    if (!up) {
        // Relay fallback: a fresh Register connection, WireGuard over Send/Recv.
        s.direct = false;
        if (!relay_connect(&relay_addr)) {
            rwk_line_release(RWK_LINE_WAN);
            return false;
        }
        uint8_t reg[RWK_FRAME_HEADER + 32];
        const size_t rn = rwk_frame_enc_register(reg, sizeof(reg), s.pk);
        rwk_parser_init(&s_relay_parser, s_relay_buf, sizeof(s_relay_buf));
        if (rn == 0 || !tcp_write_all(reg, rn)) {
            rwk_line_release(RWK_LINE_WAN);
            return false;
        }
        up = bring_wg_up(RELAY_HANDSHAKE_TIMEOUT_MS, false);
    }
    if (!up) {
        rwk_line_release(RWK_LINE_WAN);
        xSemaphoreTake(s_lock, portMAX_DELAY);
        set_state_locked(WAN_RECONNECTING, "handshake failed");
        xSemaphoreGive(s_lock);
        close_fd(&s_tcp);
        return false;
    }

    s_last_heard_ms = now_ms();
    s_connected_at_ms = now_ms();
    xSemaphoreTake(s_lock, portMAX_DELAY);
    set_state_locked(s.direct ? WAN_DIRECT : WAN_RELAY, s.direct ? "direct" : "relay");
    s.reason[0] = '\0';
    xSemaphoreGive(s_lock);
    ESP_LOGI(TAG, "session up (%s)", s.direct ? "direct" : "relay");
    return true;
}

// Services an established session: polls for traffic, beacons, enforces liveness and the lease.
// Returns when the session ends.
static void run_session(void)
{
    int64_t next_beat = now_ms();
    int64_t next_tick = now_ms();
    while (s.enabled) {
        const int64_t now = now_ms();

        if (now - s_last_heard_ms >= PEER_SILENCE_MS) {
            teardown_session("peer went silent");
            return;
        }
        if (rwk_edge_lease_expired(&s_edge, now)) {
            teardown_session("key lease timed out");
            return;
        }
        if (now >= next_beat) {
            uint8_t hb[4];
            rwk_edge_build_heartbeat(hb);
            send_edge_payload(hb, 4);
            uint8_t ping[12];
            const uint64_t stamp = (uint64_t)esp_timer_get_time();
            rwk_edge_build_ping(ping, stamp);
            send_edge_payload(ping, 12);
            xSemaphoreTake(s_lock, portMAX_DELAY);
            s_pending_ping = stamp;
            s_ping_outstanding = true;
            xSemaphoreGive(s_lock);
            next_beat = now + HEARTBEAT_MS;
        }
        if (now >= next_tick) {
            // WireGuard timers: keepalives and rekey. A rekey past the time limit ends the session
            // cleanly and the supervisor reconnects.
            uint8_t dummy;
            size_t a, b;
            (void)dummy;
            (void)a;
            (void)b;
            next_tick = now + TUNNEL_TICK_MS;
            if (rwk_wg_needs_rekey(&s_wg, now)) {
                teardown_session("session key expired");
                return;
            }
        }

        fd_set r;
        FD_ZERO(&r);
        int maxfd = -1;
        if (s.direct && s_udp >= 0) {
            FD_SET(s_udp, &r);
            maxfd = s_udp;
        }
        if (!s.direct && s_tcp >= 0) {
            FD_SET(s_tcp, &r);
            if (s_tcp > maxfd) {
                maxfd = s_tcp;
            }
        }
        if (maxfd < 0) {
            teardown_session("transport closed");
            return;
        }
        struct timeval tv = {.tv_sec = 0, .tv_usec = 50000};
        if (select(maxfd + 1, &r, NULL, NULL, &tv) <= 0) {
            continue;
        }
        if (s.direct && FD_ISSET(s_udp, &r)) {
            uint8_t buf[RWK_WG_MAX_DATAGRAM];
            struct sockaddr_in src;
            socklen_t sl = sizeof(src);
            const int n = recvfrom(s_udp, buf, sizeof(buf), 0, (struct sockaddr *)&src, &sl);
            if (n > 0) {
                on_wg_datagram(buf, (size_t)n, &src);
            }
        }
        if (!s.direct && FD_ISSET(s_tcp, &r)) {
            uint8_t buf[512];
            const int n = recv(s_tcp, buf, sizeof(buf), 0);
            if (n <= 0) {
                teardown_session("relay closed");
                return;
            }
            size_t pos = 0;
            while (pos < (size_t)n) {
                size_t used;
                rwk_frame_t ev;
                const rwk_parse_status_t st =
                    rwk_parser_feed(&s_relay_parser, buf + pos, (size_t)n - pos, &used, &ev);
                pos += used;
                if (st == RWK_PARSE_FRAME && ev.type == RWK_FRAME_RECV && ev.len > 32) {
                    on_wg_datagram(ev.body + 32, ev.len - 32, NULL);
                } else if (st == RWK_PARSE_ERR_TYPE || st == RWK_PARSE_ERR_LENGTH) {
                    teardown_session("relay protocol error");
                    return;
                }
                if (used == 0) {
                    break;
                }
            }
        }
    }
    teardown_session("WAN turned off");
}

// ------------------------------------------------------------------ reconnect budget / backoff

static bool budget_expired(int64_t now)
{
    if (!s_budget_open) {
        s_budget_open = true;
        s_budget_start_ms = now;
    }
    return now - s_budget_start_ms >= RECONNECT_WINDOW_MS;
}

static void note_link_ended(void)
{
    // A link that stayed up long enough earns a fresh budget; a flap keeps consuming the old one.
    if (s_connected_at_ms != 0 && now_ms() - s_connected_at_ms >= LINK_STABLE_MS) {
        s_budget_open = false;
        s_backoff_ms = BACKOFF_MIN_MS;
    }
    s_connected_at_ms = 0;
}

// ------------------------------------------------------------------ task

static void wan_task(void *arg)
{
    (void)arg;
    s_backoff_ms = BACKOFF_MIN_MS;
    for (;;) {
        if (!s.enabled) {
            xSemaphoreTake(s_lock, portMAX_DELAY);
            if (s.state != WAN_DISABLED) {
                set_state_locked(WAN_DISABLED, "WAN off");
            }
            xSemaphoreGive(s_lock);
            close_fd(&s_udp);
            close_fd(&s_tcp);
            s_budget_open = false;
            s_backoff_ms = BACKOFF_MIN_MS;
            vTaskDelay(pdMS_TO_TICKS(200));
            continue;
        }
        if (station_ip() == 0) {
            xSemaphoreTake(s_lock, portMAX_DELAY);
            set_state_locked(WAN_WIFI_DOWN, "waiting for Wi-Fi");
            xSemaphoreGive(s_lock);
            vTaskDelay(pdMS_TO_TICKS(500));
            continue;
        }

        if (connect_once()) {
            s_budget_open = false;
            s_backoff_ms = BACKOFF_MIN_MS;
            run_session();
            note_link_ended();
        }

        close_fd(&s_tcp);
        if (!s.enabled) {
            continue;
        }
        if (budget_expired(now_ms()) && s.state != WAN_ERROR) {
            xSemaphoreTake(s_lock, portMAX_DELAY);
            if (s.state != WAN_ERROR) {
                set_state_locked(WAN_RECONNECTING, s.reason[0] ? s.reason : "retrying");
            }
            xSemaphoreGive(s_lock);
        }
        // Bounded exponential backoff between attempts.
        vTaskDelay(pdMS_TO_TICKS(s_backoff_ms));
        s_backoff_ms = s_backoff_ms * 2 > BACKOFF_MAX_MS ? BACKOFF_MAX_MS : s_backoff_ms * 2;
    }
}

// ------------------------------------------------------------------ public API

void wan_init(void)
{
    s_lock = xSemaphoreCreateMutex();
    if (s_lock == NULL) {
        ESP_LOGE(TAG, "no lock, WAN disabled");
        return;
    }
    memset(&s, 0, sizeof(s));
    load_settings();
    load_identity();
    s_sta_netif = esp_netif_get_handle_from_ifkey("WIFI_STA_DEF");
    s.state = s.enabled ? WAN_WIFI_DOWN : WAN_DISABLED;
    ESP_LOGI(TAG, "WAN %s, relay '%s', key %s", s.enabled ? "on" : "off", s.relay, s.pubkey_fp);

    // The playout queue and its task carry scheduled CW edges. Eight entries cover the deepest burst
    // a 1..4-edge frame history produces well inside one buffer delay. The playout task is priority
    // 6 — above the network task (3) so scheduled edges fire on time, below the keyer engine and its
    // esp_timer so it can never delay a local element.
    s_playout_q = xQueueCreate(8, sizeof(wan_edge_item_t));
    if (s_playout_q != NULL) {
        xTaskCreate(playout_task, "wan_play", 3072, NULL, 6, &s_playout_task);
    }
    // Priority 3: below the keyer (20), the LVGL loop and the LAN player (5).
    xTaskCreate(wan_task, "wan", 8192, NULL, 3, NULL);
}

wan_status_t wan_get_status(void)
{
    wan_status_t out = {0};
    if (s_lock == NULL) {
        return out;
    }
    xSemaphoreTake(s_lock, portMAX_DELAY);
    out.enabled = s.enabled;
    out.state = s.state;
    out.paired = s_wg_live && (s.state == WAN_DIRECT || s.state == WAN_RELAY);
    out.keying = rwk_edge_keying(&s_edge);
    out.active = out.paired && (now_ms() - s_last_heard_ms < PEER_SILENCE_MS);
    out.direct = s.direct;
    snprintf(out.relay, sizeof(out.relay), "%s", s.relay);
    snprintf(out.code, sizeof(out.code), "%s", s.code);
    snprintf(out.pubkey_fp, sizeof(out.pubkey_fp), "%s", s.pubkey_fp);
    out.peer_wpm = rwk_edge_wpm(&s_edge);
    out.rtt_ms = s.rtt_ms;
    out.jitter_ms = s.jitter_ms;
    snprintf(out.reason, sizeof(out.reason), "%s", s.reason);
    xSemaphoreGive(s_lock);
    return out;
}

void wan_set_enabled(bool on)
{
    if (s_lock == NULL) {
        return;
    }
    xSemaphoreTake(s_lock, portMAX_DELAY);
    s.enabled = on;
    if (!on) {
        set_state_locked(WAN_DISABLED, "WAN off");
    }
    xSemaphoreGive(s_lock);
    store_settings();
    ESP_LOGI(TAG, "WAN %s", on ? "on" : "off");
}

void wan_set_relay(const char *relay)
{
    if (s_lock == NULL || relay == NULL) {
        return;
    }
    xSemaphoreTake(s_lock, portMAX_DELAY);
    if (relay[0] == '\0') {
        snprintf(s.relay, sizeof(s.relay), "%s", WAN_DEFAULT_RELAY);
    } else {
        snprintf(s.relay, sizeof(s.relay), "%s", relay);
    }
    xSemaphoreGive(s_lock);
    store_settings();
    ESP_LOGI(TAG, "WAN relay set to '%s'", s.relay);
}

void wan_set_code(const char *code)
{
    if (s_lock == NULL || code == NULL) {
        return;
    }
    xSemaphoreTake(s_lock, portMAX_DELAY);
    if (code[0] != '\0') {
        snprintf(s.code, sizeof(s.code), "%s", code);
    } else {
        random_code(s.code);  // clearing the field rolls a fresh random code
    }
    xSemaphoreGive(s_lock);
    store_settings();
    ESP_LOGI(TAG, "pairing code set");
}

void wan_disconnect(void)
{
    teardown_session("local disconnect");
}

bool wan_owns_line(void)
{
    return rwk_line_owner() == RWK_LINE_WAN;
}
