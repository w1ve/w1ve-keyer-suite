// Boot-time proof that the WAN crypto works on this chip and a measurement of what it costs.
// Runs once on its own low-priority task, so it can never delay the keyer. Log lines only.
#include <string.h>

#include "esp_log.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "rwk_blake2s.h"
#include "rwk_crypto.h"
#include "rwk_selftest.h"
#include "rwk_wg.h"

static const char *TAG = "rwk-self";

static bool kat_x25519(void)
{
    // RFC 7748 section 6.1.
    static const uint8_t a[32] = {0x77, 0x07, 0x6d, 0x0a, 0x73, 0x18, 0xa5, 0x7d, 0x3c, 0x16, 0xc1,
                                  0x72, 0x51, 0xb2, 0x66, 0x45, 0xdf, 0x4c, 0x2f, 0x87, 0xeb, 0xc0,
                                  0x99, 0x2a, 0xb1, 0x77, 0xfb, 0xa5, 0x1d, 0xb9, 0x2c, 0x2a};
    static const uint8_t pa[32] = {0x85, 0x20, 0xf0, 0x09, 0x89, 0x30, 0xa7, 0x54, 0x74, 0x8b, 0x7d,
                                   0xdc, 0xb4, 0x3e, 0xf7, 0x5a, 0x0d, 0xbf, 0x3a, 0x0d, 0x26, 0x38,
                                   0x1a, 0xf4, 0xeb, 0xa4, 0xa9, 0x8e, 0xaa, 0x9b, 0x4e, 0x6a};
    uint8_t out[32];
    return rwk_x25519_pub(out, a) && memcmp(out, pa, 32) == 0;
}

static bool kat_blake2s(void)
{
    // BLAKE2s-256("abc") from RFC 7693 appendix B.
    static const uint8_t expect[32] = {0x50, 0x8c, 0x5e, 0x8c, 0x32, 0x7c, 0x14, 0xe2, 0xe1, 0xa7, 0x2b,
                                       0xa3, 0x4e, 0xeb, 0x45, 0x2f, 0x37, 0x45, 0x8b, 0x20, 0x9e, 0xd6,
                                       0x3a, 0x29, 0x4d, 0x99, 0x9b, 0x4c, 0x86, 0x67, 0x59, 0x82};
    uint8_t out[32];
    rwk_blake2s(out, 32, NULL, 0, (const uint8_t *)"abc", 3);
    return memcmp(out, expect, 32) == 0;
}

static void selftest_task(void *arg)
{
    (void)arg;
    bool ok = kat_x25519() && kat_blake2s();
    ESP_LOGI(TAG, "known answers x25519/blake2s: %s", ok ? "ok" : "FAILED");

    // Two in-memory peers shake hands and exchange a packet; also times each step.
    static rwk_wg_t ini, rsp;
    uint8_t ski[32], skr[32], pki[32], pkr[32];
    rwk_rng(ski, 32);
    rwk_rng(skr, 32);

    int64_t t0 = esp_timer_get_time();
    ok = ok && rwk_x25519_pub(pki, ski) && rwk_x25519_pub(pkr, skr);
    const int64_t one_mult_us = (esp_timer_get_time() - t0) / 2;

    ok = ok && rwk_wg_init(&ini, ski, pkr) && rwk_wg_init(&rsp, skr, pki);
    uint8_t init[RWK_WG_INITIATION_LEN], reply[RWK_WG_INITIATION_LEN], conf[64], plain[512];
    uint8_t dg[128], back[128];
    size_t reply_len = 0, plain_len = 0, conf_len = 0;

    t0 = esp_timer_get_time();
    ok = ok && rwk_wg_initiate(&ini, 0, 1700000000, 0, init);
    const int64_t initiate_us = esp_timer_get_time() - t0;

    t0 = esp_timer_get_time();
    ok = ok && rwk_wg_input(&rsp, 0, init, sizeof(init), reply, &reply_len, plain, &plain_len) ==
                   RWK_WG_REPLY;
    const int64_t respond_us = esp_timer_get_time() - t0;

    ok = ok && rwk_wg_input(&ini, 0, reply, reply_len, conf, &conf_len, plain, &plain_len) ==
                   RWK_WG_REPLY;
    ok = ok && rwk_wg_input(&rsp, 0, conf, conf_len, reply, &reply_len, plain, &plain_len) ==
                   RWK_WG_KEEPALIVE;

    uint8_t msg[28];
    memset(msg, 0x5a, sizeof(msg));
    t0 = esp_timer_get_time();
    const size_t n = rwk_wg_encrypt(&rsp, 10, msg, sizeof(msg), dg, sizeof(dg));
    ok = ok && n > 0 &&
         rwk_wg_input(&ini, 10, dg, n, back, &reply_len, plain, &plain_len) == RWK_WG_DATA &&
         memcmp(plain, msg, sizeof(msg)) == 0;
    const int64_t packet_us = esp_timer_get_time() - t0;

    ESP_LOGI(TAG, "wireguard loopback handshake + data: %s", ok ? "ok" : "FAILED");
    ESP_LOGI(TAG, "cost: x25519 %lld us, initiate %lld us, respond %lld us, packet seal+open %lld us",
             (long long)one_mult_us, (long long)initiate_us, (long long)respond_us,
             (long long)packet_us);
    ESP_LOGI(TAG, "stack high water %u bytes free", (unsigned)uxTaskGetStackHighWaterMark(NULL));
    rwk_wg_free(&ini);
    rwk_wg_free(&rsp);
    vTaskDelete(NULL);
}

void rwk_selftest_start(void)
{
    // Priority 1: below the keyer, the UI and the network tasks.
    xTaskCreate(selftest_task, "rwk-self", 8192, NULL, 1, NULL);
}
