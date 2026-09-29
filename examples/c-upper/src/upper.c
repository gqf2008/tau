//! Minimal tau extension in freestanding C: the `upper` tool, aligned with
//! examples/upper (Rust). No WASI, no libc — see src/shim.c.
//!
//! 0.7.0 shape: `tools.definitions` and `tools.execute` are `async func`s
//! (a synchronously lowered export cannot wait for anything), so the C
//! bindings hand the guest a callback ABI: the export computes its answer,
//! hands it over with `..._return(...)`, and returns `CALLBACK_CODE_EXIT`
//! to say it is done. This example never waits, so both `..._callback`
//! functions exist only to be the "I have nothing to resume on" answer —
//! they never run. The two `probes` exports stay synchronous (a probe is a
//! decision on the run's hot path), but their signature is typed now:
//! a `point` enum and a `payload` variant instead of JSON strings.
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

/* 0.3.0 (docs/tool-media.md): content is a list of result-blocks;
 * this example only ever returns one text block. */
static void set_result(exports_tau_extension_tools_tool_result_t *ret,
                       const char *content, int is_error) {
    ret->content.ptr = malloc(sizeof(exports_tau_extension_tools_result_block_t));
    ret->content.len = 1;
    ret->content.ptr[0].tag = TAU_EXTENSION_TYPES_RESULT_BLOCK_TEXT;
    extension_string_dup(&ret->content.ptr[0].val.text, content);
    ret->is_error = is_error;
}

extension_callback_code_t exports_tau_extension_tools_definitions(void) {
    exports_tau_extension_tools_list_definition_t ret;
    ret.ptr = malloc(sizeof(exports_tau_extension_tools_definition_t));
    ret.len = 1;
    extension_string_dup(&ret.ptr[0].name, "upper");
    extension_string_dup(&ret.ptr[0].description, "Convert text to UPPERCASE");
    extension_string_dup(&ret.ptr[0].parameters_json,
        "{\"type\":\"object\",\"properties\":{\"text\":{\"type\":\"string\"}},\"required\":[\"text\"]}");
    exports_tau_extension_tools_definitions_return(ret);
    return EXTENSION_CALLBACK_CODE_EXIT;
}

extension_callback_code_t exports_tau_extension_tools_definitions_callback(extension_event_t *event) {
    (void)event;
    return EXTENSION_CALLBACK_CODE_EXIT; /* definitions() answered immediately */
}

extension_callback_code_t exports_tau_extension_tools_execute(extension_string_t *name,
                                                              extension_string_t *arguments_json) {
    exports_tau_extension_tools_tool_result_t ret;
    if (name->len != 5 || memcmp(name->ptr, "upper", 5) != 0) {
        set_result(&ret, "unknown tool", 1);
        exports_tau_extension_tools_execute_return(ret);
        return EXTENSION_CALLBACK_CODE_EXIT;
    }
    static char text[4096];
    if (!extract_text(arguments_json, text, sizeof(text))) {
        set_result(&ret, "missing string argument 'text'", 1);
        exports_tau_extension_tools_execute_return(ret);
        return EXTENSION_CALLBACK_CODE_EXIT;
    }
    for (size_t i = 0; text[i]; i++)
        if (text[i] >= 'a' && text[i] <= 'z') text[i] -= 32;
    set_result(&ret, text, 0);
    exports_tau_extension_tools_execute_return(ret);
    return EXTENSION_CALLBACK_CODE_EXIT;
}

extension_callback_code_t exports_tau_extension_tools_execute_callback(extension_event_t *event) {
    (void)event;
    return EXTENSION_CALLBACK_CODE_EXIT; /* execute() answered immediately */
}

void exports_tau_extension_probes_points(exports_tau_extension_probes_list_point_t *ret) {
    ret->ptr = 0;
    ret->len = 0;
}

void exports_tau_extension_probes_probe(exports_tau_extension_probes_point_t point,
                                       exports_tau_extension_probes_payload_t *payload,
                                       exports_tau_extension_probes_verdict_t *ret) {
    (void)point; (void)payload;
    ret->tag = EXPORTS_TAU_EXTENSION_PROBES_VERDICT_CONTINUE;
}
