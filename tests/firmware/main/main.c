#include "esp_log.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"

void app_main(void) {
    // One-time marker: repeated heartbeats cannot conceal a lost startup line.
    ESP_LOGI("idf_remote", "IDF_REMOTE_BOOT smoke-v1");
    unsigned tick = 0;
    while (1) {
        vTaskDelay(pdMS_TO_TICKS(1000));
        ESP_LOGI("idf_remote", "IDF_REMOTE_TICK %u", ++tick);
    }
}
