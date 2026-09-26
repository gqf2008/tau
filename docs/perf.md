# Performance baseline

Measured 2026-09-26 on the maintainer's Windows machine (release binary,
`lto + codegen-units 1 + strip`). Absolute numbers move with hardware;
the *ratios* are the contract.

## Startup (whole CLI, `tau --demo -p hi`)

| scenario | min | median |
|---|---|---|
| bare | 23 ms | 24 ms |
| one extension, cold compile cache | 113 ms (first run ever) | — |
| one extension, warm cache | 26 ms | 28 ms |

Pre-cache the warm figure was 42 ms: the module compile cache
(`~/.tau/cache/wasmtime`, wasmtime's content-keyed machine-code cache)
cuts per-extension startup overhead from ~21 ms to ~4 ms. The cache is
best-effort: an unwritable config dir disables it silently in effect but
`tau_ext::compile_cache_dir()` + the `compile_cache_populates` test keep
that from going unnoticed.

## Component load (library level, fresh Engine per round)

```
round 0 (cold): 202 ms
round 1+ (warm): ~9 ms      ← 22x from the disk cache
```

Reproduce: `cargo test -p tau-ext --test perf_load -- --ignored --nocapture`

## Session store

10,000-entry JSONL session: full parse + index on open in **66 ms**
(~150k entries/s; the file is loaded once, `active_branch` is then
O(branch) hash lookups). Gate: `open_parses_large_sessions_quickly`
asserts < 10 s — deliberately generous; it guards pathological
(quadratic-style) regressions, not micro-perf.

## Gates that run by default

- `perf_load.rs::compile_cache_populates_and_loads_stay_correct` —
  proves the compile cache is active and loads stay correct.
- `session.rs::open_parses_large_sessions_quickly` — pathological
  regression tripwire with the real time printed.
