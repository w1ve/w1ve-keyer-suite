idf_component_register(
  SRCS
    "app_main.c"
    "player_config.c"
    "player_gpio.c"
    "player_json.c"
    "player_log.c"
    "player_transport.c"
    "player_web.c"
    "player_wifi.c"
  INCLUDE_DIRS "include"
  PRIV_REQUIRES
    app_update
    driver
    esp_driver_gpio
    esp_driver_rmt
    esp_event
    esp_http_server
    esp_netif
    esp_timer
    esp_wifi
    mbedtls
    nvs_flash
  EMBED_TXTFILES "web/index.html"
)
