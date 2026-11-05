// One WireGuard peer built on the portable RWK core, for interop tests against boringtun.
//   wg_peer respond  <my_sk_hex> <peer_pk_hex> <listen_port>
//   wg_peer initiate <my_sk_hex> <peer_pk_hex> <listen_port> <peer_port>
// Both use 127.0.0.1. Prints PASS on success. This is test tooling, not firmware.
#include <arpa/inet.h>
#include <errno.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

#include "../../main/rwk/rwk_crypto.h"
#include "../../main/rwk/rwk_inner.h"
#include "../../main/rwk/rwk_wg.h"

static int64_t now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

static void unhex(const char *s, uint8_t *out, size_t n)
{
    for (size_t i = 0; i < n; i++) {
        unsigned v = 0;
        sscanf(s + 2 * i, "%2x", &v);
        out[i] = (uint8_t)v;
    }
}

static int g_fd;
static struct sockaddr_in g_peer;
static bool g_have_peer;

static void send_dgram(const uint8_t *d, size_t n)
{
    if (g_have_peer) {
        sendto(g_fd, d, n, 0, (struct sockaddr *)&g_peer, sizeof(g_peer));
    }
}

int main(int argc, char **argv)
{
    if (argc < 5) {
        fprintf(stderr, "usage\n");
        return 2;
    }
    const bool initiate = strcmp(argv[1], "initiate") == 0;
    uint8_t sk[32], peer_pk[32];
    unhex(argv[2], sk, 32);
    unhex(argv[3], peer_pk, 32);
    rwk_wg_t w;
    if (!rwk_wg_init(&w, sk, peer_pk)) {
        fprintf(stderr, "bad keys\n");
        return 2;
    }

    g_fd = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in me = {.sin_family = AF_INET, .sin_port = htons((uint16_t)atoi(argv[4])),
                             .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    if (bind(g_fd, (struct sockaddr *)&me, sizeof(me)) != 0) {
        perror("bind");
        return 2;
    }
    if (initiate) {
        g_peer = (struct sockaddr_in){.sin_family = AF_INET, .sin_port = htons((uint16_t)atoi(argv[5])),
                                      .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
        g_have_peer = true;
    }

    const int64_t start = now_ms();
    int64_t next_init = start;
    int64_t last_data_sent = 0;
    int received = 0;
    bool sent_hello = false;
    printf("ready\n");
    fflush(stdout);

    while (now_ms() - start < 15000) {
        const int64_t now = now_ms();
        if (initiate && !rwk_wg_can_send(&w, now) && now >= next_init) {
            uint8_t init[RWK_WG_INITIATION_LEN];
            struct timespec wall;
            clock_gettime(CLOCK_REALTIME, &wall);
            if (rwk_wg_initiate(&w, now, (uint64_t)wall.tv_sec, (uint32_t)wall.tv_nsec, init)) {
                send_dgram(init, sizeof(init));
            }
            next_init = now + 250;
        }
        if (initiate && rwk_wg_can_send(&w, now) && now - last_data_sent > 200) {
            // Edge-shaped payload: a one-entry RWK-PADDLE frame, from the Client inner address, as the desktop Keyer sends it.
            uint8_t frame[16] = {0x01, 0x00, 0x01, 0x00, 0x01, 0, 0, 0, 0x10, 0, 0, 0, 1, 0, 0, 0};
            uint8_t pkt[128], dg[256];
            size_t n = rwk_inner_build_udp(pkt, sizeof(pkt), RWK_INNER_CLIENT_IP, RWK_EDGE_PORT,
                                           RWK_INNER_STATION_IP, RWK_EDGE_PORT, frame, sizeof(frame));
            size_t dn = rwk_wg_encrypt(&w, now, pkt, n, dg, sizeof(dg));
            send_dgram(dg, dn);
            sent_hello = true;
            last_data_sent = now;
        }

        struct pollfd pfd = {.fd = g_fd, .events = POLLIN};
        if (poll(&pfd, 1, 50) <= 0) {
            continue;
        }
        uint8_t in[2048], out[256], plain[2048];
        struct sockaddr_in src;
        socklen_t sl = sizeof(src);
        ssize_t len = recvfrom(g_fd, in, sizeof(in), 0, (struct sockaddr *)&src, &sl);
        if (len <= 0) {
            continue;
        }
        size_t out_len = 0, plain_len = 0;
        rwk_wg_result_t r = rwk_wg_input(&w, now_ms(), in, (size_t)len, out, &out_len, plain, &plain_len);
        if (r == RWK_WG_REPLY || r == RWK_WG_DATA || r == RWK_WG_KEEPALIVE) {
            // Adopt the authenticated source, as WireGuard does for a roaming peer.
            g_peer = src;
            g_have_peer = true;
        }
        if (r == RWK_WG_REPLY) {
            send_dgram(out, out_len);
        } else if (r == RWK_WG_DATA) {
            uint32_t sip, dip;
            uint16_t sp, dp;
            const uint8_t *pl;
            size_t pl_len;
            if (!rwk_inner_parse_udp(plain, plain_len, &sip, &sp, &dip, &dp, &pl, &pl_len)) {
                printf("FAIL unparseable inner packet\n");
                return 1;
            }
            received++;
            if (!initiate) {
                // Echo the payload back from the station address to the client address.
                uint8_t pkt[256], dg[512];
                size_t n = rwk_inner_build_udp(pkt, sizeof(pkt), dip, dp, sip, sp, pl, pl_len);
                size_t dn = rwk_wg_encrypt(&w, now_ms(), pkt, n, dg, sizeof(dg));
                send_dgram(dg, dn);
                if (received >= 1) {
                    printf("PASS responder echoed %d packets\n", received);
                    return 0;
                }
            } else if (sent_hello && sip == RWK_INNER_STATION_IP && pl_len >= 4) {
                printf("PASS initiator got %zu byte echo\n", pl_len);
                return 0;
            }
        }
    }
    printf("FAIL timeout (can_send=%d received=%d)\n", rwk_wg_can_send(&w, now_ms()), received);
    return 1;
}
