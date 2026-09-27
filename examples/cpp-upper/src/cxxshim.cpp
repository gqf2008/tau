// Global replacement new/delete for the freestanding build: bump-arena
// backed (c-upper shim), abort on OOM. Defined once, here.
#include <stddef.h>
#include <stdlib.h>
void* operator new(size_t n) { void* p = malloc(n); if (!p) abort(); return p; }
void operator delete(void* p) noexcept { free(p); }
void operator delete(void* p, size_t) noexcept { free(p); }
