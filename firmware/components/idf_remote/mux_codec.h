// SPDX-License-Identifier: Apache-2.0
#pragma once
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#define MUX_MAX_PAYLOAD 1024
#define MUX_MAX_BODY (14 + MUX_MAX_PAYLOAD + 4)
#define MUX_MAX_WIRE (MUX_MAX_BODY * 2 + 2)
enum { MUX_HELLO = 1, MUX_HELLO_ACK, MUX_CONSOLE, MUX_REQUEST, MUX_RESPONSE, MUX_EVENT, MUX_ERROR };
typedef struct {
    uint8_t kind;
    uint64_t session;
    uint32_t request;
    size_t size;
    uint8_t data[MUX_MAX_PAYLOAD];
} mux_frame_t;
typedef struct {
    bool inside, escaped, overflow;
    size_t size;
    uint8_t data[MUX_MAX_BODY];
} mux_decoder_t;
uint32_t mux_crc32(const uint8_t *data, size_t size);
size_t mux_encode(const mux_frame_t *frame, uint8_t *wire);
// 1=complete frame, 2=raw byte, 0=incomplete, -1=corrupt.
int mux_feed(mux_decoder_t *decoder, uint8_t byte, mux_frame_t *frame);
