// SPDX-License-Identifier: Apache-2.0
#pragma once
#include "cJSON.h"
#include "esp_err.h"
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif
// Runs on a dedicated command task; never in the USB RX or TX task.
// On success transfer ownership of *result to the component. Do not block forever.
typedef esp_err_t (*idf_remote_handler_t)(const char *method, const cJSON *params, cJSON **result,
                                          void *context);
typedef struct {
    const char *application; // copied during attach
    const char *version;
    const char *methods_json; // JSON array, e.g. ["echo","status"]
    idf_remote_handler_t handler;
    void *context;
} idf_remote_config_t;
// Call once, early in app_main, before application console readers/writers.
// Captures the default shared FILE objects via VFS, including ESP_LOGx and printf.
// Returns without waiting for a host. ESP32-S3 USB Serial/JTAG, IDF 5.5.3 MVP.
// Attachment is process-lifetime; failure is fatal to startup (use ESP_ERROR_CHECK).
esp_err_t idf_remote_attach_console(const idf_remote_config_t *config);
// Nonblocking; ESP_ERR_INVALID_STATE before negotiation, ESP_ERR_TIMEOUT if full.
esp_err_t idf_remote_publish(const cJSON *event);
uint32_t idf_remote_dropped_console(void);
uint32_t idf_remote_dropped_control(void);
#ifdef __cplusplus
}
#endif
