#pragma once

#include "esp_err.h"

esp_err_t player_web_start(void);
void player_web_publish_status(void);
/** Defers a status WebSocket update out of a small system event-handler stack. */
void player_web_request_status_update(void);
void player_web_publish_log(const char *level, const char *message);
