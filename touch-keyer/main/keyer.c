// Iambic keyer engine.
//
// Timing model: element boundaries are scheduled with a one-shot esp_timer. The timer
// callback only signals a high-priority task, which then writes the key line and arms the
// next deadline. That keeps every state transition in task context (nothing but flag
// maintenance happens in the GPIO ISR) while keeping element timing off the LVGL path.
//
// Local paddle keying has no lease and no watchdog: the operator is standing at the rig.
#include "keyer.h"

#include <ctype.h>
#include <stdlib.h>
#include <string.h>

#include "board.h"

#include "driver/gpio.h"
#include "esp_log.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "freertos/task.h"

static const char *TAG = "keyer";

#define EL_NONE 0
#define EL_DIT 1
#define EL_DAH 2

#define PHASE_IDLE 0
#define PHASE_ON 1
#define PHASE_GAP 2

static portMUX_TYPE s_lock = portMUX_INITIALIZER_UNLOCKED;

static SemaphoreHandle_t s_step;
static esp_timer_handle_t s_elem_timer;
static esp_timer_handle_t s_ptt_timer;

static volatile bool s_dit, s_dah;          // paddle state (physical OR touch)
static volatile bool s_dit_mem, s_dah_mem;  // iambic memory
static volatile uint32_t s_wpm = KEYER_WPM_DEFAULT;
static volatile keyer_mode_t s_mode = KEYER_MODE_IAMBIC_A;
static volatile bool s_key, s_ptt;
static volatile uint32_t s_weight = KEYER_WEIGHT_DEFAULT;
static volatile uint32_t s_deadline_us;
static volatile uint32_t s_max_late_us;
static volatile uint8_t s_phase = PHASE_IDLE;
static volatile uint8_t s_last = EL_NONE;
static volatile bool s_timer_fired;
static volatile bool s_dry_run;  // time the elements, leave the pins alone
static keyer_edge_cb_t s_edge_cb;
static volatile bool s_keytest_running;
// Speed a remote keyer is running at while it owns the line, 0 when no remote is active. A locally
// played macro uses this so it matches the QSO speed instead of the station's own setting. Set by
// whichever transport owns the line (remote.c / wan.c); the paddle engine ignores it.
static volatile uint8_t s_remote_wpm;

static inline uint32_t dit_us(void)
{
    const uint32_t wpm = s_wpm ? s_wpm : KEYER_WPM_DEFAULT;
    return 1200000u / wpm;  // 1200 ms per dit at 1 WPM
}

// Key-down duration of an element, scaled by the weight setting. Gaps stay at one dit, which
// is how weight behaves on a conventional keyer: the duty cycle changes, the gap does not.
static inline uint32_t element_us(int element)
{
    const uint32_t base = (element == EL_DIT) ? dit_us() : 3u * dit_us();
    const uint32_t scaled = (base * s_weight) / 50u;
    return scaled < 1000u ? 1000u : scaled;
}

static inline int key_level(bool down)
{
    return down == (KEYER_KEY_ACTIVE_HIGH != 0) ? 1 : 0;
}

static inline int ptt_level(bool down)
{
    return down == (KEYER_PTT_ACTIVE_HIGH != 0) ? 1 : 0;
}

static void set_key_at(bool down, uint32_t ms)
{
    portENTER_CRITICAL(&s_lock);
    const bool changed = (s_key != down);
    s_key = down;
    portEXIT_CRITICAL(&s_lock);

    if (!s_dry_run) {
        gpio_set_level(KEYER_PIN_KEY, key_level(down));
    }

    // Runs in the keyer task, never in the ISR, so the sink may do real work.
    if (changed && s_edge_cb != NULL) {
        s_edge_cb(down, ms);
    }
}

static void set_key(bool down)
{
    set_key_at(down, (uint32_t)(esp_timer_get_time() / 1000));
}

static void ptt_assert(void)
{
    esp_timer_stop(s_ptt_timer);
    if (!s_ptt) {
        portENTER_CRITICAL(&s_lock);
        s_ptt = true;
        portEXIT_CRITICAL(&s_lock);
        if (!s_dry_run) {
            gpio_set_level(KEYER_PIN_PTT, ptt_level(true));
        }
    }
}

static void ptt_release_after_tail(void)
{
    esp_timer_stop(s_ptt_timer);
    esp_timer_start_once(s_ptt_timer, (uint64_t)KEYER_PTT_TAIL_MS * 1000);
}

static void ptt_timer_cb(void *arg)
{
    (void)arg;
    portENTER_CRITICAL(&s_lock);
    s_ptt = false;
    portEXIT_CRITICAL(&s_lock);
    if (!s_dry_run) {
        gpio_set_level(KEYER_PIN_PTT, ptt_level(false));
    }
}

// Runs in the esp_timer task, not an ISR: plain give, and raise the flag that tells the
// keyer task this wake-up is a timing boundary rather than a paddle change.
static void elem_timer_cb(void *arg)
{
    (void)arg;

    // How late did this boundary actually fire? Exposed over the web page so timing can be
    // judged as a number instead of by ear.
    const uint32_t late = (uint32_t)esp_timer_get_time() - s_deadline_us;
    if (late != 0 && late < 1000000u && late > s_max_late_us) {
        s_max_late_us = late;
    }

    s_timer_fired = true;
    xSemaphoreGive(s_step);
}

static void arm(uint32_t us)
{
    esp_timer_stop(s_elem_timer);
    s_deadline_us = (uint32_t)(esp_timer_get_time() + us);
    esp_timer_start_once(s_elem_timer, us);
}

static int next_element(void)
{
    int next = EL_NONE;

    portENTER_CRITICAL(&s_lock);
    const bool dit = s_dit;
    const bool dah = s_dah;
    const bool mem_dit = s_dit_mem;
    const bool mem_dah = s_dah_mem;

    if (dit && dah) {
        next = (s_last == EL_DIT) ? EL_DAH : EL_DIT;  // squeeze alternates
    } else if (dit) {
        next = EL_DIT;
    } else if (dah) {
        next = EL_DAH;
    } else if (s_mode == KEYER_MODE_IAMBIC_B) {
        if (mem_dit) {
            next = EL_DIT;
        } else if (mem_dah) {
            next = EL_DAH;
        }
    }

    if (next == EL_DIT) {
        s_dit_mem = false;
    } else if (next == EL_DAH) {
        s_dah_mem = false;
    }
    portEXIT_CRITICAL(&s_lock);

    return next;
}

static void start_element(int element)
{
    s_phase = PHASE_ON;
    s_last = (uint8_t)element;
    ptt_assert();
    set_key(true);
    arm(element_us(element));
}

static void keyer_task(void *arg)
{
    (void)arg;

    for (;;) {
        xSemaphoreTake(s_step, portMAX_DELAY);

        // A wake-up is either the element timer or a paddle change. Only the timer may
        // advance the element timeline: a paddle edge arriving mid-element used to be
        // mistaken for the end of that element, which is what chopped the keying up.
        const bool timed = s_timer_fired;
        s_timer_fired = false;

        if (s_mode == KEYER_MODE_STRAIGHT) {
            bool down;
            portENTER_CRITICAL(&s_lock);
            down = s_dit || s_dah;
            portEXIT_CRITICAL(&s_lock);

            set_key(down);
            if (down) {
                ptt_assert();
            } else {
                ptt_release_after_tail();
            }
            continue;
        }

        if (!timed) {
            // Input changed while idle: that is how a paddle press starts a train. Mid
            // element and mid gap there is nothing to do; the timer will decide.
            if (s_phase == PHASE_IDLE) {
                const int first = next_element();
                if (first != EL_NONE) {
                    start_element(first);
                }
            }
            continue;
        }

        if (s_phase == PHASE_ON) {
            set_key(false);
            s_phase = PHASE_GAP;
            arm(dit_us());
            continue;
        }

        // PHASE_GAP or PHASE_IDLE: decide whether another element follows.
        const int element = next_element();
        if (element == EL_NONE) {
            s_phase = PHASE_IDLE;
            s_last = EL_NONE;
            ptt_release_after_tail();
            continue;
        }
        start_element(element);
    }
}

static void IRAM_ATTR paddle_isr(void *arg)
{
    (void)arg;

    static int64_t last_edge_us;
    const int64_t now = esp_timer_get_time();
    if (now - last_edge_us < KEYER_DEBOUNCE_US) {
        return;
    }
    last_edge_us = now;

    // Resample both contacts on every accepted edge: the paddle is a pair, and an edge on
    // one contact can coincide with a squeeze on the other.
    const bool dit = gpio_get_level(KEYER_PIN_DIT) == 0;
    const bool dah = gpio_get_level(KEYER_PIN_DAH) == 0;

    portENTER_CRITICAL_ISR(&s_lock);
    s_dit = dit;
    s_dah = dah;
    if (dit) {
        s_dit_mem = true;
    }
    if (dah) {
        s_dah_mem = true;
    }
    portEXIT_CRITICAL_ISR(&s_lock);

    BaseType_t woken = pdFALSE;
    xSemaphoreGiveFromISR(s_step, &woken);
    if (woken) {
        portYIELD_FROM_ISR(woken);
    }
}

void keyer_init(void)
{
    // INPUT_OUTPUT, not OUTPUT: the input buffer has to stay enabled for gpio_get_level() to
    // report what the pad is really doing. With plain OUTPUT every read returned 0, which is
    // how a pin fault could hide behind a self-test that "passed".
    const gpio_config_t out_cfg = {
        .pin_bit_mask = (1ULL << KEYER_PIN_KEY) | (1ULL << KEYER_PIN_PTT),
        .mode = GPIO_MODE_INPUT_OUTPUT,
        .pull_up_en = GPIO_PULLUP_DISABLE,
        .pull_down_en = GPIO_PULLDOWN_DISABLE,
        .intr_type = GPIO_INTR_DISABLE,
    };
    ESP_ERROR_CHECK(gpio_config(&out_cfg));

    // KEY and PTT start inactive, whatever the polarity.
    gpio_set_level(KEYER_PIN_KEY, key_level(false));
    gpio_set_level(KEYER_PIN_PTT, ptt_level(false));

    const gpio_config_t in_cfg = {
        .pin_bit_mask = (1ULL << KEYER_PIN_DIT) | (1ULL << KEYER_PIN_DAH),
        .mode = GPIO_MODE_INPUT,
        .pull_up_en = GPIO_PULLUP_ENABLE,
        .pull_down_en = GPIO_PULLDOWN_DISABLE,
        .intr_type = GPIO_INTR_ANYEDGE,
    };
    ESP_ERROR_CHECK(gpio_config(&in_cfg));

    s_step = xSemaphoreCreateBinary();
    configASSERT(s_step);

    const esp_timer_create_args_t elem_args = {
        .callback = elem_timer_cb,
        .name = "keyer_elem",
    };
    ESP_ERROR_CHECK(esp_timer_create(&elem_args, &s_elem_timer));

    const esp_timer_create_args_t ptt_args = {
        .callback = ptt_timer_cb,
        .name = "keyer_ptt",
    };
    ESP_ERROR_CHECK(esp_timer_create(&ptt_args, &s_ptt_timer));

    ESP_ERROR_CHECK(gpio_install_isr_service(0));
    ESP_ERROR_CHECK(gpio_isr_handler_add(KEYER_PIN_DIT, paddle_isr, NULL));
    ESP_ERROR_CHECK(gpio_isr_handler_add(KEYER_PIN_DAH, paddle_isr, NULL));

    // Deliberately below the esp_timer task (22). esp_timer's update path holds a spinlock
    // inside that task, so a higher-priority task calling esp_timer_start/stop can spin
    // forever on a single core — which hung this board outright at priority 24.
    BaseType_t ok = xTaskCreate(keyer_task, "keyer", 3072, NULL, 20, NULL);
    configASSERT(ok == pdPASS);

    ESP_LOGI(TAG, "keyer up: dit=%d dah=%d key=%d ptt=%d, %u wpm",
             KEYER_PIN_DIT, KEYER_PIN_DAH, KEYER_PIN_KEY, KEYER_PIN_PTT, (unsigned)s_wpm);
}

void keyer_set_edge_cb(keyer_edge_cb_t cb)
{
    s_edge_cb = cb;
}

// A remote edge is applied straight to the line: LAN jitter is far smaller than an element,
// and the frame's timestamps order staleness rather than scheduling ahead. Priority is 4, well
// below the esp_timer task, so the PTT tail timers are safe to touch from here.
bool keyer_remote_key(bool down)
{
    portENTER_CRITICAL(&s_lock);
    const bool local = s_dit || s_dah;
    portEXIT_CRITICAL(&s_lock);
    if (local) {
        return false;
    }

    if (down) {
        ptt_assert();
    }
    set_key(down);
    if (!down) {
        ptt_release_after_tail();
    }
    return true;
}

// The timestamped form, used by history frames: the edge reaches the sidetone with the moment
// the operator actually keyed it, so a frame holding several edges keeps its element spacing.
bool keyer_remote_edge(bool down, uint32_t ms)
{
    portENTER_CRITICAL(&s_lock);
    const bool local = s_dit || s_dah;
    portEXIT_CRITICAL(&s_lock);
    if (local) {
        return false;
    }

    if (down) {
        ptt_assert();
    }
    set_key_at(down, ms);
    if (!down) {
        ptt_release_after_tail();
    }
    return true;
}

// ------------------------------------------------------------------ macro sender

// A text macro is expanded into key-down/key-up segments at the current speed, then played by a
// task that sleeps out each segment. Timing comes from esp_timer_get_time(), so a slow tick does
// not accumulate: every boundary is measured against the clock, not against the number of sleeps.
#define MORSE_SEG_MAX 320

typedef struct {
    uint16_t ms;
    bool down;
} morse_seg_t;

static morse_seg_t *s_segs;
static int s_seg_count;
static int s_seg_index;
static uint32_t s_seg_end_ms;
static volatile bool s_macro_running;
static uint32_t s_macro_deadline_ms;  // hard stop, so a stalled sender cannot hold KEY

static const char *morse_code(char c)
{
    switch (c) {
    case 'A': return ".-";
    case 'B': return "-...";
    case 'C': return "-.-.";
    case 'D': return "-..";
    case 'E': return ".";
    case 'F': return "..-.";
    case 'G': return "--.";
    case 'H': return "....";
    case 'I': return "..";
    case 'J': return ".---";
    case 'K': return "-.-";
    case 'L': return ".-..";
    case 'M': return "--";
    case 'N': return "-.";
    case 'O': return "---";
    case 'P': return ".--.";
    case 'Q': return "--.-";
    case 'R': return ".-.";
    case 'S': return "...";
    case 'T': return "-";
    case 'U': return "..-";
    case 'V': return "...-";
    case 'W': return ".--";
    case 'X': return "-..-";
    case 'Y': return "-.--";
    case 'Z': return "--..";
    case '0': return "-----";
    case '1': return ".----";
    case '2': return "..---";
    case '3': return "...--";
    case '4': return "....-";
    case '5': return ".....";
    case '6': return "-....";
    case '7': return "--...";
    case '8': return "---..";
    case '9': return "----.";
    case '.': return ".-.-.-";
    case ',': return "--..--";
    case '?': return "..--..";
    case '/': return "-..-.";
    case '=': return "-...-";
    case '+': return ".-.-.";
    case '-': return "-....-";
    case ':': return "---...";
    case '\'': return ".----.";
    case '"': return ".-..-.";
    case '@': return ".--.-.";
    default: return NULL;
    }
}

static bool seg_push(int *count, bool down, uint32_t ms)
{
    if (*count >= MORSE_SEG_MAX) {
        return false;
    }
    s_segs[*count].down = down;
    s_segs[*count].ms = (uint16_t)(ms > 60000u ? 60000u : ms);
    (*count)++;
    return true;
}

static void macro_apply(int index)
{
    const uint32_t now = (uint32_t)(esp_timer_get_time() / 1000);

    if (index >= s_seg_count) {
        set_key_at(false, now);
        ptt_release_after_tail();
        free(s_segs);
        s_segs = NULL;
        s_macro_running = false;
        s_keytest_running = false;
        ESP_LOGI(TAG, "macro sent: %d segments", s_seg_count);
        return;
    }

    s_seg_index = index;
    set_key_at(s_segs[index].down, now);
    s_seg_end_ms = now + s_segs[index].ms;
}

static void macro_task(void *arg)
{
    (void)arg;

    while (s_macro_running) {
        const uint32_t now = (uint32_t)(esp_timer_get_time() / 1000);
        if ((int32_t)(now - s_macro_deadline_ms) > 0) {
            ESP_LOGW(TAG, "macro overran its deadline, releasing the line");
            macro_apply(s_seg_count);
            break;
        }
        if ((int32_t)(s_seg_end_ms - now) <= 0) {
            macro_apply(s_seg_index + 1);
        }
        vTaskDelay(pdMS_TO_TICKS(1));
    }
    vTaskDelete(NULL);
}

bool keyer_send_text(const char *text)
{
    if (text == NULL || text[0] == '\0' || s_macro_running) {
        return false;
    }

    portENTER_CRITICAL(&s_lock);
    bool busy;
    if (s_remote_wpm) {
        // A remote keyer owns the line. The operator fires a macro only in the gaps between the
        // remote's characters, so the lingering PTT tail (key up, PTT still asserted) must not
        // block it — only a local paddle or the remote actually keying right now does. The macro
        // plays at the remote's speed (set below) to match the QSO.
        busy = s_key || s_dit || s_dah;
    } else {
        // No remote: the original strict check — any asserted line or paddle blocks a macro.
        busy = s_key || s_ptt || s_dit || s_dah;
    }
    portEXIT_CRITICAL(&s_lock);
    if (busy || s_keytest_running) {
        return false;
    }

    s_segs = calloc(MORSE_SEG_MAX, sizeof(morse_seg_t));
    if (s_segs == NULL) {
        return false;
    }

    // When a remote keyer owns the line, send macros at its speed so they match the QSO, not the
    // station's own setting. Otherwise use the local speed.
    const uint8_t remote_wpm = s_remote_wpm;
    const uint32_t macro_wpm = remote_wpm ? remote_wpm : (s_wpm ? s_wpm : KEYER_WPM_DEFAULT);
    const uint32_t dit = 1200u / macro_wpm;  // ms per dit
    const uint32_t on_unit = (dit * s_weight) / 50u;  // weight moves the key-down share only

    int count = 0;
    bool ok = true;
    const size_t len = strlen(text);
    for (size_t i = 0; i < len && ok; i++) {
        const char c = (char)toupper((unsigned char)text[i]);
        if (c == ' ') {
            continue;  // a space is spent as the gap after the previous character
        }
        const char *code = morse_code(c);
        if (code == NULL) {
            continue;  // punctuation we do not know is skipped rather than keyed wrong
        }
        for (int e = 0; code[e] != '\0' && ok; e++) {
            ok = seg_push(&count, true, code[e] == '-' ? on_unit * 3u : on_unit);
            if (ok && code[e + 1] != '\0') {
                ok = seg_push(&count, false, dit);
            }
        }
        if (!ok || i + 1 >= len) {
            break;
        }
        // Seven units between words, three between characters, measured from the end of the
        // last element — not a character gap with a word gap stacked on top of it.
        if (text[i + 1] == ' ') {
            ok = seg_push(&count, false, dit * 7u);
            while (i + 1 < len && text[i + 1] == ' ') {
                i++;
            }
        } else {
            ok = seg_push(&count, false, dit * 3u);
        }
    }

    if (!ok || count == 0) {
        free(s_segs);
        s_segs = NULL;
        return false;
    }

    uint32_t total_ms = 0;
    for (int i = 0; i < count; i++) {
        total_ms += s_segs[i].ms;
    }

    s_seg_count = count;
    s_seg_index = 0;
    s_macro_deadline_ms = (uint32_t)(esp_timer_get_time() / 1000) + total_ms + 2000u;
    s_macro_running = true;
    ESP_LOGI(TAG, "macro start: %d segments, %lu ms at %u wpm%s", count, (unsigned long)total_ms,
             (unsigned)macro_wpm, remote_wpm ? " (remote)" : "");
    s_keytest_running = true;  // keep the other senders out for the duration
    ptt_assert();
    macro_apply(0);

    if (xTaskCreate(macro_task, "macro", 3072, NULL, 4, NULL) != pdPASS) {
        macro_apply(s_seg_count);  // releases the line, the PTT tail and the buffer
        return false;
    }
    return true;
}

// Runs the element machine for a while with the pins left alone, so boundary lateness can be
// measured on hardware nobody has to touch — and without putting a rig on the air.
void keyer_selftest(uint16_t wpm, uint16_t elements)
{
    if (elements == 0 || wpm == 0) {
        return;
    }

    const keyer_mode_t saved_mode = s_mode;
    const uint8_t saved_wpm = (uint8_t)s_wpm;

    s_mode = KEYER_MODE_IAMBIC_A;
    s_wpm = wpm;
    s_max_late_us = 0;
    s_dry_run = true;

    portENTER_CRITICAL(&s_lock);
    s_dit = true;
    s_dit_mem = true;
    portEXIT_CRITICAL(&s_lock);
    xSemaphoreGive(s_step);

    const uint32_t span_us = (1200000u / wpm) * 2u;  // element on, then one dit of gap
    vTaskDelay(pdMS_TO_TICKS((span_us * elements) / 1000u + 80u));

    portENTER_CRITICAL(&s_lock);
    s_dit = false;
    portEXIT_CRITICAL(&s_lock);
    xSemaphoreGive(s_step);
    vTaskDelay(pdMS_TO_TICKS(150));

    s_dry_run = false;
    s_mode = saved_mode;
    s_wpm = saved_wpm;

    ESP_LOGI(TAG, "timing self-test: %u elements at %u wpm, worst boundary lateness %lu us",
             elements, wpm, (unsigned long)s_max_late_us);
}

bool keyer_keytest(uint16_t elements)
{
    if (elements == 0 || s_keytest_running) {
        return false;
    }
    if (elements > 30) {
        elements = 30;
    }

    portENTER_CRITICAL(&s_lock);
    const bool busy = s_key || s_ptt || s_dit || s_dah;
    portEXIT_CRITICAL(&s_lock);
    if (busy) {
        return false;
    }

    s_keytest_running = true;

    // Straight mode would hold the line down for the whole span, so test the iambic machine
    // that actually produces elements, and put the operator's mode back afterwards.
    const keyer_mode_t saved_mode = s_mode;
    s_mode = KEYER_MODE_IAMBIC_A;

    portENTER_CRITICAL(&s_lock);
    s_dit = true;
    s_dit_mem = true;
    portEXIT_CRITICAL(&s_lock);
    xSemaphoreGive(s_step);

    // Poll the KEY pad while the line runs. The element machine can be perfect and the pin
    // still wrong; reading the pad back is the only way to know the GPIO actually followed.
    const uint32_t span_us = (1200000u / s_wpm) * 2u;  // element on, then one dit of gap
    const uint32_t total_ms = (span_us * elements) / 1000u + 120u;
    int pad_changes = 0;
    int pad_last = gpio_get_level(KEYER_PIN_KEY);
    for (uint32_t elapsed = 0; elapsed < total_ms; elapsed += 4) {
        const int pad = gpio_get_level(KEYER_PIN_KEY);
        if (pad != pad_last) {
            pad_changes++;
            pad_last = pad;
        }
        vTaskDelay(pdMS_TO_TICKS(4));
    }

    portENTER_CRITICAL(&s_lock);
    s_dit = false;
    s_dit_mem = false;
    portEXIT_CRITICAL(&s_lock);
    xSemaphoreGive(s_step);
    vTaskDelay(pdMS_TO_TICKS(200));

    s_mode = saved_mode;
    s_keytest_running = false;

    ESP_LOGI(TAG, "key test: %u elements at %u wpm, KEY pad followed %d times (expected %u)",
             elements, (unsigned)s_wpm, pad_changes, (unsigned)(elements * 2));
    return true;
}

void keyer_set_wpm(uint8_t wpm)
{
    if (wpm < KEYER_WPM_MIN) {
        wpm = KEYER_WPM_MIN;
    }
    if (wpm > KEYER_WPM_MAX) {
        wpm = KEYER_WPM_MAX;
    }
    s_wpm = wpm;
}

uint8_t keyer_get_wpm(void)
{
    return (uint8_t)s_wpm;
}

void keyer_set_remote_speed(uint8_t wpm)
{
    // No clamping to the station range: a remote can key outside 5..50 and the macro should still
    // match it. 0 means no remote owns the line.
    s_remote_wpm = wpm;
}

void keyer_set_mode(keyer_mode_t mode)
{
    if (mode >= KEYER_MODE_COUNT) {
        return;
    }
    s_mode = mode;
}

keyer_mode_t keyer_get_mode(void)
{
    return s_mode;
}

void keyer_set_weight(uint8_t percent)
{
    if (percent < KEYER_WEIGHT_MIN) {
        percent = KEYER_WEIGHT_MIN;
    }
    if (percent > KEYER_WEIGHT_MAX) {
        percent = KEYER_WEIGHT_MAX;
    }
    s_weight = percent;
}

uint8_t keyer_get_weight(void)
{
    return (uint8_t)s_weight;
}

uint32_t keyer_take_max_jitter_us(void)
{
    const uint32_t worst = s_max_late_us;
    s_max_late_us = 0;
    return worst;
}

const char *keyer_mode_name(keyer_mode_t mode)
{
    switch (mode) {
    case KEYER_MODE_IAMBIC_B:
        return "Iambic B";
    case KEYER_MODE_STRAIGHT:
        return "Straight";
    case KEYER_MODE_IAMBIC_A:
    default:
        return "Iambic A";
    }
}

static void touch_paddle(bool dit, bool down)
{
    portENTER_CRITICAL(&s_lock);
    if (dit) {
        s_dit = down;
        if (down) {
            s_dit_mem = true;
        }
    } else {
        s_dah = down;
        if (down) {
            s_dah_mem = true;
        }
    }
    portEXIT_CRITICAL(&s_lock);

    xSemaphoreGive(s_step);
}

void keyer_paddle_dit(bool down)
{
    touch_paddle(true, down);
}

void keyer_paddle_dah(bool down)
{
    touch_paddle(false, down);
}

keyer_status_t keyer_get_status(void)
{
    keyer_status_t status;
    portENTER_CRITICAL(&s_lock);
    status.key_down = s_key;
    status.ptt_down = s_ptt;
    status.wpm = (uint8_t)s_wpm;
    status.mode = s_mode;
    status.weight = (uint8_t)s_weight;
    portEXIT_CRITICAL(&s_lock);
    return status;
}
