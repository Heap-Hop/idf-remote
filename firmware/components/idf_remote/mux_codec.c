// SPDX-License-Identifier: Apache-2.0
#include "mux_codec.h"
#include <string.h>
uint32_t mux_crc32(const uint8_t *data, size_t size) {
    uint32_t crc = ~0u;
    for (size_t i = 0; i < size; i++) {
        crc ^= data[i];
        for (int j = 0; j < 8; j++)
            crc = (crc >> 1) ^ (0xedb88320u & (0u - (crc & 1)));
    }
    return ~crc;
}
static uint64_t get(const uint8_t *p, int n) {
    uint64_t value = 0;
    for (int i = 0; i < n; i++)
        value |= (uint64_t)p[i] << (8 * i);
    return value;
}
static void put(uint8_t *p, uint64_t value, int n) {
    for (int i = 0; i < n; i++)
        p[i] = value >> (8 * i);
}
size_t mux_encode(const mux_frame_t *f, uint8_t *wire) {
    if (f->size > MUX_MAX_PAYLOAD || !f->session || f->kind < 1 || f->kind > 7)
        return 0;
    uint8_t body[MUX_MAX_BODY];
    body[0] = 1;
    body[1] = f->kind;
    put(body + 2, f->session, 8);
    put(body + 10, f->request, 4);
    memcpy(body + 14, f->data, f->size);
    size_t size = 14 + f->size;
    put(body + size, mux_crc32(body, size), 4);
    size += 4;
    size_t out = 0;
    wire[out++] = 0x7e;
    for (size_t i = 0; i < size; i++) {
        if (body[i] == 0x7e || body[i] == 0x7d) {
            wire[out++] = 0x7d;
            wire[out++] = body[i] ^ 0x20;
        } else
            wire[out++] = body[i];
    }
    wire[out++] = 0x7e;
    return out;
}
int mux_feed(mux_decoder_t *d, uint8_t b, mux_frame_t *f) {
    if (b == 0x7e) {
        int result = 0;
        if (d->inside && (d->size || d->overflow || d->escaped)) {
            size_t n = d->size;
            result = -1;
            if (!d->overflow && !d->escaped && n >= 18 && d->data[0] == 1 && d->data[1] >= 1 &&
                d->data[1] <= 7 && get(d->data + 2, 8) &&
                mux_crc32(d->data, n - 4) == get(d->data + n - 4, 4)) {
                f->kind = d->data[1];
                f->session = get(d->data + 2, 8);
                f->request = get(d->data + 10, 4);
                f->size = n - 18;
                memcpy(f->data, d->data + 14, f->size);
                result = 1;
            }
        }
        d->inside = true;
        d->size = 0;
        d->escaped = false;
        d->overflow = false;
        return result;
    }
    if (!d->inside || (!d->size && !d->escaped && !d->overflow && b != 1)) {
        d->inside = false;
        return 2;
    }
    if (d->overflow)
        return 0;
    if (!d->escaped && b == 0x7d) {
        d->escaped = true;
        return 0;
    }
    if (d->escaped) {
        b ^= 0x20;
        d->escaped = false;
    }
    if (d->size == MUX_MAX_BODY) {
        d->overflow = true;
        d->size = 0;
    } else
        d->data[d->size++] = b;
    return 0;
}
