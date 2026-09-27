//! Minimal tau extension in freestanding C++: the `upper` tool, aligned
//! with examples/upper (Rust). No WASI, no libc++ — the generated
//! bindings' std:: surface is shimmed in src/cxxshim/, the C runtime in
//! ../c-upper/src/shim.c.
//!
//! Build: ./build.sh  (wit-bindgen + clang++ wasm32 + wasm-tools only)

#include "extension_cpp.h"

namespace {

// 0.3.0: a tool result is a list of result-blocks; one text block here.
exports::tau::extension::tools::ToolResult error_result(wit::string msg) {
    namespace tools = exports::tau::extension::tools;
    tools::ToolResult ret;
    ret.content = wit::vector<::tau::extension::types::ResultBlock>::allocate(1);
    ret.content.initialize(0, ::tau::extension::types::ResultBlock{
        ::tau::extension::types::ResultBlock::Text{std::move(msg)}});
    ret.is_error = true;
    return ret;
}

// Naive extraction of the "text" field from a flat JSON object; see the C
// example for the contract. ASCII-only uppercase, by design.
wit::string extract_text(const wit::string& json, char* out, size_t cap) {
    const char* key = "\"text\"";
    const size_t klen = 6;
    const char* p = reinterpret_cast<const char*>(json.get_view().data());
    size_t len = json.get_view().size();
    for (size_t i = 0; i + klen <= len; i++) {
        if (memcmp(p + i, key, klen) == 0) {
            size_t j = i + klen;
            while (j < len && (p[j] == ' ' || p[j] == ':' || p[j] == '\t')) j++;
            if (j >= len || p[j] != '"') return wit::string::from_view("missing string argument 'text'");
            j++;
            size_t k = 0;
            while (j < len && p[j] != '"' && k + 1 < cap) {
                char c = p[j++];
                out[k++] = (c >= 'a' && c <= 'z') ? char(c - 32) : c;
            }
            out[k] = 0;
            return wit::string::from_view(out);
        }
    }
    return wit::string::from_view("missing string argument 'text'");
}

} // namespace

namespace exports::tau::extension {

wit::vector<tools::Definition> tools::Definitions() {
    auto defs = wit::vector<tools::Definition>::allocate(1);
    defs.initialize(0, tools::Definition{
        wit::string::from_view("upper"),
        wit::string::from_view("Convert text to UPPERCASE"),
        wit::string::from_view("{\"type\":\"object\",\"properties\":{\"text\":{\"type\":\"string\"}},\"required\":[\"text\"]}"),
    });
    return defs;
}

tools::ToolResult tools::Execute(wit::string name, wit::string arguments_json) {
    auto view = name.get_view();
    if (view.size() != 5 || memcmp(view.data(), "upper", 5) != 0) {
        return error_result(wit::string::from_view("unknown tool"));
    }
    static char text[4096];
    wit::string content = extract_text(arguments_json, text, sizeof(text));
    bool is_error = content.get_view().size() > 0 && content.get_view().data()[0] == 'm'; // "missing..."
    // 0.3.0 (docs/tool-media.md): content is a list of result-blocks;
    // this example returns one text block.
    tools::ToolResult ret;
    ret.content = wit::vector<::tau::extension::types::ResultBlock>::allocate(1);
    ret.content.initialize(0, ::tau::extension::types::ResultBlock{
        ::tau::extension::types::ResultBlock::Text{std::move(content)}});
    ret.is_error = is_error;
    return ret;
}

wit::vector<wit::string> probes::Points() {
    return wit::vector<wit::string>();
}

probes::Verdict probes::Probe(wit::string, wit::string) {
    return {probes::Action::kContinue, std::nullopt, std::nullopt};
}

} // namespace exports::tau::extension
