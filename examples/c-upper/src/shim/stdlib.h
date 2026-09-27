/* Minimal freestanding stdlib shim for the wasm32 bare target:
 * just enough for wit-bindgen's generated glue (malloc/realloc/free/abort).
 * First-fit free-list allocator over a fixed arena, no WASI, no sysroot. */
#ifndef SHIM_STDLIB_H
#define SHIM_STDLIB_H
#ifdef __cplusplus
extern "C" {
#endif
#include <stddef.h>
void *malloc(size_t n);
void *realloc(void *p, size_t n);
void free(void *p);
void abort(void) __attribute__((noreturn));
#ifdef __cplusplus
}
#endif
#endif
