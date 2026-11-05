// Macro slots: six canned exchanges, persisted in the same NVS namespace as the rest of the
// station settings.
#include "macro.h"

#include <stdio.h>
#include <string.h>

#include "keyer.h"

#include "esp_log.h"
#include "nvs.h"

static const char *TAG = "macro";

#define NVS_NS "w1ve"

typedef struct {
    char label[MACRO_LABEL_MAX];
    char text[MACRO_TEXT_MAX];
} macro_t;

static const macro_t k_defaults[MACRO_SLOTS] = {
    {"CQ", "CQ DE W1VE"},
    {"599", "5NN,"},
    {"TU", "TU"},
    {"73", "73"},
    {"W1VE", "W1VE"},
    {"(Empty)", ""},
};

static macro_t s_macros[MACRO_SLOTS];

// Bounded copy that GCC cannot mistake for a truncating format write.
static void copy_str(char *dst, size_t len, const char *src)
{
    if (len == 0) {
        return;
    }
    size_t n = strlen(src);
    if (n >= len) {
        n = len - 1;
    }
    memcpy(dst, src, n);
    dst[n] = '\0';
}

int macro_slots(void)
{
    return MACRO_SLOTS;
}

static void load_slot(nvs_handle_t nvs, int i, bool have_nvs)
{
    copy_str(s_macros[i].label, sizeof(s_macros[i].label), k_defaults[i].label);
    copy_str(s_macros[i].text, sizeof(s_macros[i].text), k_defaults[i].text);

    if (!have_nvs) {
        return;
    }

    char key[8];
    size_t len;

    snprintf(key, sizeof(key), "ml%d", i);
    len = sizeof(s_macros[i].label);
    if (nvs_get_str(nvs, key, s_macros[i].label, &len) != ESP_OK) {
        copy_str(s_macros[i].label, sizeof(s_macros[i].label), k_defaults[i].label);
    }

    snprintf(key, sizeof(key), "mt%d", i);
    len = sizeof(s_macros[i].text);
    if (nvs_get_str(nvs, key, s_macros[i].text, &len) != ESP_OK) {
        copy_str(s_macros[i].text, sizeof(s_macros[i].text), k_defaults[i].text);
    }
}

void macro_init(void)
{
    nvs_handle_t nvs;
    const bool have_nvs = nvs_open(NVS_NS, NVS_READONLY, &nvs) == ESP_OK;

    for (int i = 0; i < MACRO_SLOTS; i++) {
        load_slot(nvs, i, have_nvs);
    }
    if (have_nvs) {
        nvs_close(nvs);
    }

    ESP_LOGI(TAG, "%d slots: %s / %s / %s / %s / %s / %s", MACRO_SLOTS,
             s_macros[0].label, s_macros[1].label, s_macros[2].label,
             s_macros[3].label, s_macros[4].label, s_macros[5].label);
}

bool macro_get(int index, char *label, size_t label_len, char *text, size_t text_len)
{
    if (index < 0 || index >= MACRO_SLOTS) {
        return false;
    }
    if (label != NULL && label_len > 0) {
        copy_str(label, label_len, s_macros[index].label);
    }
    if (text != NULL && text_len > 0) {
        copy_str(text, text_len, s_macros[index].text);
    }
    return true;
}

bool macro_set(int index, const char *label, const char *text)
{
    if (index < 0 || index >= MACRO_SLOTS || label == NULL || text == NULL) {
        return false;
    }

    // A blank label would leave an unlabelled key on the panel; show a placeholder instead.
    copy_str(s_macros[index].label, sizeof(s_macros[index].label),
             label[0] != '\0' ? label : "(Empty)");
    copy_str(s_macros[index].text, sizeof(s_macros[index].text), text);

    nvs_handle_t nvs;
    if (nvs_open(NVS_NS, NVS_READWRITE, &nvs) != ESP_OK) {
        ESP_LOGW(TAG, "slot %d kept in RAM only, NVS open failed", index);
        return false;
    }

    char key[8];
    snprintf(key, sizeof(key), "ml%d", index);
    nvs_set_str(nvs, key, s_macros[index].label);
    snprintf(key, sizeof(key), "mt%d", index);
    nvs_set_str(nvs, key, s_macros[index].text);
    const esp_err_t err = nvs_commit(nvs);
    nvs_close(nvs);

    if (err != ESP_OK) {
        ESP_LOGW(TAG, "slot %d save failed: %s", index, esp_err_to_name(err));
        return false;
    }
    ESP_LOGI(TAG, "slot %d saved: %s -> \"%s\"", index, s_macros[index].label,
             s_macros[index].text);
    return true;
}

bool macro_play(int index)
{
    if (index < 0 || index >= MACRO_SLOTS || s_macros[index].text[0] == '\0') {
        return false;
    }
    return keyer_send_text(s_macros[index].text);
}
