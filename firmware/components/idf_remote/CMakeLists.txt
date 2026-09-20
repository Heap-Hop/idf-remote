if(NOT IDF_TARGET STREQUAL "esp32s3")
    message(FATAL_ERROR "idf_remote MVP currently supports ESP32-S3 USB Serial/JTAG only")
endif()
idf_component_register(SRCS "idf_remote.c" "mux_codec.c" INCLUDE_DIRS "include"
    REQUIRES json PRIV_REQUIRES esp_driver_usb_serial_jtag vfs esp_system)
