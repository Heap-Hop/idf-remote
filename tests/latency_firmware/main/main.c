#include "driver/usb_serial_jtag.h"
#include "esp_err.h"
#include "freertos/FreeRTOS.h"

// Disposable ESP32-S3 fixture: echo bytes immediately, without a line editor,
// stdio buffering, Wi-Fi, or periodic application logs in the measurement path.
void app_main(void) {
    usb_serial_jtag_driver_config_t config = {
        .tx_buffer_size = 4096,
        .rx_buffer_size = 4096,
    };
    ESP_ERROR_CHECK(usb_serial_jtag_driver_install(&config));
    char buffer[256];
    while (1) {
        int count = usb_serial_jtag_read_bytes(buffer, sizeof(buffer), portMAX_DELAY);
        int sent = 0;
        while (sent < count) {
            int written = usb_serial_jtag_write_bytes(buffer + sent, count - sent, portMAX_DELAY);
            if (written > 0) sent += written;
        }
    }
}
