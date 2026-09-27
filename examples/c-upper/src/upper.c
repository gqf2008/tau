//! Minimal tau extension in freestanding C: the `upper` tool, aligned with
//! examples/upper (Rust). No WASI, no libc — see src/shim.c.
//!
//! Build: ./build.sh  (needs wit-bindgen, clang with the wasm32 target,
//! and wasm-tools — no wasi-sdk, no downloads)

#include "extension.h"
#include <stdlib.h>
#include <string.h>

/* Naive extraction of the "text" string field from a flat JSON object.
 * Handles the shape tau's models actually send ({"text":"..."}); escapes
 * and nesting are out of scope for a minimal example. */
static int extract_text(const extension_string_t *json, char *out, size_t cap) {
    static const char key[] = "\"text\"";
    size_t klen = sizeof(key) - 1;
    for (size_t i = 0; i + klen <= json->len; i++) {
        if (memcmp(json->ptr + i, key, klen) == 0) {
            size_t j = i + klen;
            while (j < json->len && (json->ptr[j] == ' ' || json->ptr[j] == ':' || json->ptr[j] == '\t')) j++;
            if (j >= json->len || json->ptr[j] != '"') return 0;
            j++;
            size_t k = 0;
            while (j < json->len && json->ptr[j] != '"' && k + 1 < cap)
                out[k++] = json->ptr[j++];
            out[k] = 0;
            return k > 0;
        }
    }
    return 0;
}

static void set_result(exports_tau_extension_tools_tool_result_t *ret,
                       const char *content, int is_error) {
    extension_string_dup(&ret->content, content);
    ret->is_error = is_error;
}

void exports_tau_extension_tools_definitions(exports_tau_extension_tools_list_definition_t *ret) {
    ret->ptr = malloc(sizeof(exports_tau_extension_tools_definition_t));
    ret->len = 1;
    extension_string_dup(&ret->ptr[0].name, "upper");
    extension_string_dup(&ret->ptr[0].description, "Convert text to UPPERCASE");
    extension_string_dup(&ret->ptr[0].parameters_json,
        "{\"type\":\"object\",\"properties\":{\"text\":{\"type\":\"string\"}},\"required\":[\"text\"]}");
}

void exports_tau_extension_tools_execute(extension_string_t *name,
                                         extension_string_t *arguments_json,
                                         exports_tau_extension_tools_tool_result_t *ret) {
    if (name->len != 5 || memcmp(name->ptr, "upper", 5) != 0) {
        set_result(ret, "unknown tool", 1);
        return;
    }
    static char text[4096];
    if (!extract_text(arguments_json, text, sizeof(text))) {
        set_result(ret, "missing string argument 'text'", 1);
        return;
    }
    for (size_t i = 0; text[i]; i++)
        if (text[i] >= 'a' && text[i] <= 'z') text[i] -= 32;
    set_result(ret, text, 0);
}

void exports_tau_extension_hooks_points(extension_list_string_t *ret) {
    ret->ptr = 0;
    ret->len = 0;
}

void exports_tau_extension_hooks_probe(extension_string_t *point,
                                       extension_string_t *payload_json,
                                       exports_tau_extension_hooks_verdict_t *ret) {
    (void)point; (void)payload_json;
    ret->action = EXPORTS_TAU_EXTENSION_HOOKS_ACTION_CONTINUE;
    ret->payload_json.is_some = 0;
    ret->reason.is_some = 0;
}
