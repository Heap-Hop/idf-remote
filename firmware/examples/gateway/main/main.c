// SPDX-License-Identifier: Apache-2.0
#include "esp_log.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "idf_remote.h"
#include <stdio.h>
#include <string.h>
static esp_err_t command(const char *method, const cJSON *params, cJSON **result, void *ctx) {
    if (!strcmp(method, "echo")) {
        *result = params ? cJSON_Duplicate(params, true) : cJSON_CreateNull();
        return ESP_OK;
    }
    if (!strcmp(method, "status")) {
        *result = cJSON_CreateObject();
        cJSON_AddNumberToObject(*result, "uptime_ms", esp_timer_get_time() / 1000);
        cJSON_AddNumberToObject(*result, "dropped_console", idf_remote_dropped_console());
        cJSON_AddNumberToObject(*result, "dropped_control", idf_remote_dropped_control());
        return ESP_OK;
    }
    if (!strcmp(method, "log_burst")) {
        for (int i = 0; i < 500; i++)
            printf("burst %d: abcdefghijklmnopqrstuvwxyz\n", i);
        *result = cJSON_CreateString("done");
        return ESP_OK;
    }
    if (!strcmp(method, "delay")) {
        int ms = cJSON_IsNumber(params) ? params->valueint : 0;
        if (ms < 0 || ms > 5000)
            return ESP_ERR_INVALID_ARG;
        vTaskDelay(pdMS_TO_TICKS(ms));
        *result = cJSON_CreateNumber(ms);
        return ESP_OK;
    }
    return ESP_ERR_NOT_SUPPORTED;
}
static void console_task(void *arg) {
    while (1) {
        int c = getchar();
        if (c != EOF)
            printf("stdin:%02x\n", c);
        else
            vTaskDelay(1);
    }
}
void app_main(void) {
    idf_remote_config_t config = {.application = "gateway-demo",
                                  .version = "0.1.0",
                                  .methods_json = "[\"echo\",\"status\",\"log_burst\",\"delay\"]",
                                  .handler = command};
    ESP_ERROR_CHECK(idf_remote_attach_console(&config));
    xTaskCreate(console_task, "console_demo", 4096, NULL, 4, NULL);
    unsigned tick = 0;
    while (1) {
        ESP_LOGI("gateway", "tick %u", tick);
        printf("printf tick %u\n", tick);
        fprintf(stderr, "stderr tick %u\n", tick);
        cJSON *event = cJSON_CreateObject();
        cJSON_AddStringToObject(event, "event", "tick");
        cJSON_AddNumberToObject(event, "count", tick++);
        idf_remote_publish(event);
        cJSON_Delete(event);
        vTaskDelay(pdMS_TO_TICKS(1000));
    }
}
