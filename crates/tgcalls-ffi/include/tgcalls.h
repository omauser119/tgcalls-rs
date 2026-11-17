#ifndef TGCALLS_RUST_H
#define TGCALLS_RUST_H
#include <stdint.h>
#include <stddef.h>
#ifdef __cplusplus
extern "C" {
#endif
uint64_t tgcalls_client_new(uint32_t mode, const uint8_t *module, size_t length);
uint64_t tgcalls_client_request(uint64_t client, const uint8_t *bytes, size_t length);
uint64_t tgcalls_client_generation(uint64_t client);
void tgcalls_client_free(uint64_t client);
const uint8_t *tgcalls_output_data(uint64_t ticket);
size_t tgcalls_output_len(uint64_t ticket);
void tgcalls_output_free(uint64_t ticket);
const char *tgcalls_last_error(void);
#ifdef __cplusplus
}
#endif
#endif
