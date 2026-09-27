"""Post-generation patch for TinyGo: wit-bindgen 0.62's Go output and
go.bytecodealliance.org/pkg use runtime.Pinner, runtime.AddCleanup, a
//go:linkname to runtime.sbrk, and a duplicate cabi_realloc export, none
of which work under TinyGo. Replace/drop them (conservative GC keeps
pointers alive; cleanups are irrelevant for one-shot calls; TinyGo's own
runtime already exports cabi_realloc). One canonical Pinner lives in
wit/runtime; other patched packages alias it so types stay identical."""
import os
import re

CANON = """package runtime

// Pinner is a no-op replacement for runtime.Pinner (absent in TinyGo).
type Pinner struct{}

func (*Pinner) Pin(any) {}
func (*Pinner) Unpin()  {}
"""

ALIAS = """package {pkg}

import witRuntimePinner "go.bytecodealliance.org/pkg/wit/runtime"

// Pinner aliases the no-op shim in wit/runtime (absent in TinyGo).
type Pinner = witRuntimePinner.Pinner

// AddCleanup is a no-op replacement for runtime.AddCleanup (absent in
// TinyGo). Handle teardown is irrelevant for one-shot extension calls.
func AddCleanup[T, S any](ptr *T, cleanup func(S), arg S) func() {{
	return func() {{}}
}}
"""

SBRK_DECL = "//go:linkname sbrk runtime.sbrk\nfunc sbrk(n uintptr) unsafe.Pointer"
SBRK_BODY = ("// Permanent bump allocator over a static buffer. TinyGo runs initRand\n"
             "// BEFORE initHeap (scheduler_none.go), and initRand reaches cabi_realloc\n"
             "// through the adapter stack allocation, so the GC path can never be\n"
             "// enabled safely; every cabi_realloc leaks a few KB into this buffer.\n"
             "var sbrkBuf [16 << 20]byte\n"
             "var sbrkOff uintptr\n"
             "\n"
             "func sbrk(n uintptr) unsafe.Pointer {\n"
             "\tif sbrkOff+n > uintptr(len(sbrkBuf)) {\n"
             "\t\tpanic(\"sbrk: static buffer exhausted\")\n"
             "\t}\n"
             "\tp := unsafe.Pointer(&sbrkBuf[sbrkOff])\n"
             "\tsbrkOff += n\n"
             "\treturn p\n"
             "}")

REALLOC_PANIC = ("\tif oldPointer != nil || oldSize != 0 {\n"
             "\t\tpanic(\"todo\")\n"
             "\t}")

USE_GC_INIT = "func init() {\n\tuseGCAllocations = true\n}"

PAUSED_TRUE = "\tadapterMonotonicClockSetPaused(true)\n"
PAUSED_FALSE = "\tadapterMonotonicClockSetPaused(false)\n"

REALLOC_COPY = ("\tif oldPointer != nil || oldSize != 0 {\n"
             "\t\tgrown := cabiRealloc(nil, 0, align, newSize)\n"
             "\t\tif oldPointer != nil && oldSize > 0 {\n"
             "\t\t\tn := oldSize\n"
             "\t\t\tif newSize < n {\n"
             "\t\t\t\tn = newSize\n"
             "\t\t\t}\n"
             "\t\t\tcopy(unsafe.Slice((*byte)(grown), n),\n"
             "\t\t\t\tunsafe.Slice((*byte)(oldPointer), n))\n"
             "\t\t}\n"
             "\t\treturn grown\n"
             "\t}")

DROPS = ()


def package_name(path):
    with open(path, encoding="utf-8") as f:
        for line in f:
            m = re.match(r"package\s+(\w+)", line)
            if m:
                return m.group(1)
    raise AssertionError(f"no package clause in {path}")


def fix(path):
    s = open(path, encoding="utf-8").read()
    if ("runtime.Pinner" not in s and "runtime.AddCleanup" not in s
            and SBRK_DECL not in s and REALLOC_PANIC not in s
            and PAUSED_TRUE not in s and USE_GC_INIT not in s
            and not any(d in s for d in DROPS)):
        return False
    s = s.replace("runtime.Pinner", "Pinner")
    s = s.replace("runtime.AddCleanup", "AddCleanup")
    s = s.replace(SBRK_DECL, SBRK_BODY)
    s = s.replace(REALLOC_PANIC, REALLOC_COPY)
    s = s.replace(PAUSED_TRUE, '')
    s = s.replace(PAUSED_FALSE, '')
    s = s.replace(USE_GC_INIT, '')
    for d in DROPS:
        s = s.replace(d, "")
    code = "\n".join(l for l in s.split("\n") if not l.lstrip().startswith("//"))
    if not re.search(r"(?<![A-Za-z])runtime\.", code):
        s = "\n".join(l for l in s.split("\n") if l.strip().rstrip("\r") != '"runtime"')
    open(path, "w", encoding="utf-8", newline="\n").write(s)
    return True


RUNTIME_DIR = os.path.normpath("vendor/go.bytecodealliance.org/pkg/wit/runtime")
count = 0
for root, _dirs, files in os.walk("vendor/go.bytecodealliance.org"):
    go_files = [f for f in files if f.endswith(".go")]
    results = [fix(os.path.join(root, f)) for f in go_files]
    if not any(results):
        continue
    count += 1
    if os.path.normpath(root) == RUNTIME_DIR:
        with open(os.path.join(root, "pinner_shim.go"), "w", encoding="utf-8", newline="\n") as f:
            f.write(CANON)
    else:
        pkg = package_name(os.path.join(root, go_files[0]))
        with open(os.path.join(root, "pinner_shim.go"), "w", encoding="utf-8", newline="\n") as f:
            f.write(ALIAS.format(pkg=pkg))

assert fix("wit_exports.go"), "generated Pinner site drifted"
with open("pinner_shim.go", "w", encoding="utf-8", newline="\n") as f:
    f.write(ALIAS.format(pkg="main"))
print(f"patched {count + 1} packages")
