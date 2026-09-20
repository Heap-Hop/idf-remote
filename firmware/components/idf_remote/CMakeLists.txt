idf_component_register(SRCS "idf_remote.c" "mux_codec.c" INCLUDE_DIRS "include"
    REQUIRES json PRIV_REQUIRES esp_driver_usb_serial_jtag vfs esp_system)
