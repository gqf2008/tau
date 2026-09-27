#include <stdint.h>
#include <stddef.h>

/* 1 MiB arena; header = {size, next, free-flag}. First fit, no coalescing —
 * an example-grade allocator, not a general one. */
static uint8_t arena[1024 * 1024] __attribute__((aligned(16)));

typedef struct Block {
    size_t size;
    struct Block *next;
    int free;
} Block;

static Block *head = 0;

void *malloc(size_t n) {
    n = (n + 15) & ~(size_t)15;
    if (!head) {
        head = (Block *)arena;
        head->size = sizeof(arena) - sizeof(Block);
        head->next = 0;
        head->free = 1;
    }
    for (Block *b = head; b; b = b->next) {
        if (b->free && b->size >= n) {
            if (b->size >= n + sizeof(Block) + 16) {
                Block *rest = (Block *)((uint8_t *)(b + 1) + n);
                rest->size = b->size - n - sizeof(Block);
                rest->next = b->next;
                rest->free = 1;
                b->next = rest;
                b->size = n;
            }
            b->free = 0;
            return b + 1;
        }
    }
    return 0;
}

void free(void *p) {
    if (!p) return;
    ((Block *)p - 1)->free = 1;
}

void *realloc(void *p, size_t n) {
    if (!p) return malloc(n);
    Block *b = (Block *)p - 1;
    if (b->size >= n) return p;
    void *q = malloc(n);
    if (!q) return 0;
    uint8_t *d = q, *s = p;
    for (size_t i = 0; i < b->size; i++) d[i] = s[i];
    free(p);
    return q;
}

void abort(void) { __builtin_trap(); }

void *memcpy(void *d, const void *s, size_t n) {
    uint8_t *dd = d; const uint8_t *ss = s;
    for (size_t i = 0; i < n; i++) dd[i] = ss[i];
    return d;
}

void *memmove(void *d, const void *s, size_t n) {
    uint8_t *dd = d; const uint8_t *ss = s;
    if (dd < ss) for (size_t i = 0; i < n; i++) dd[i] = ss[i];
    else for (size_t i = n; i > 0; i--) dd[i-1] = ss[i-1];
    return d;
}

void *memset(void *d, int c, size_t n) {
    uint8_t *dd = d;
    for (size_t i = 0; i < n; i++) dd[i] = (uint8_t)c;
    return d;
}

size_t strlen(const char *s) {
    size_t n = 0;
    while (s[n]) n++;
    return n;
}

int memcmp(const void *a, const void *b, size_t n) {
    const uint8_t *x = a, *y = b;
    for (size_t i = 0; i < n; i++)
        if (x[i] != y[i]) return x[i] < y[i] ? -1 : 1;
    return 0;
}
