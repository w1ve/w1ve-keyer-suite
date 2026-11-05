#pragma once

#include <stdbool.h>
#include <stdint.h>

#define NET_SCAN_MAX 12

typedef struct {
    bool ap_active;      // the setup access point is running
    char ap_ssid[33];
    char ap_pass[65];    // shown on the Wi-Fi page so a phone can join
    bool connected;      // station associated and holding an address
    char ssid[33];       // network joined, empty if none
    char address[16];    // dotted quad to show on screen (station if joined, else the AP)
    int8_t rssi;
} net_status_t;

typedef struct {
    char ssid[33];
    int8_t rssi;
    bool secure;
} net_scan_entry_t;

typedef struct {
    int count;
    net_scan_entry_t entries[NET_SCAN_MAX];
} net_scan_result_t;

// Brings the access point up and joins a stored network if there is one.
void net_init(void);
net_status_t net_get_status(void);

// Blocks for a couple of seconds while the radio sweeps; call before building the list.
bool net_scan(net_scan_result_t *out);

// Applies, tries and persists the credentials. Blocks up to ~12 s waiting for an address.
bool net_join(const char *ssid, const char *pass);

// Drops the stored network and goes back to the access point alone.
void net_forget(void);
