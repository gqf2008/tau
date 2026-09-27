/* Minimal freestanding string shim: mem and str symbols that clang and the
 * generated glue call. Plain byte loops, size over speed. */
#ifndef SHIM_STRING_H
#define SHIM_STRING_H
#ifdef __cplusplus
extern "C" {
#endif
#include <stddef.h>
void *memcpy(void *d, const void *s, size_t n);
void *memmove(void *d, const void *s, size_t n);
void *memset(void *d, int c, size_t n);
size_t strlen(const char *s);
int memcmp(const void *a, const void *b, size_t n);
#ifdef __cplusplus
}
#endif
#endif
