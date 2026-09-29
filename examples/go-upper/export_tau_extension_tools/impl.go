// Implementation of tau:extension/tools for the Go upper example.
//
// 0.7.0: `definitions` and `execute` are `async func`s, so the generated
// glue wraps each call in `witAsync.Run` (TinyGo's asyncify scheduler
// carries the suspension). This example awaits nothing, so the bodies
// below are ordinary functions.
package export_tau_extension_tools

import (
	"encoding/json"
	"strings"

	"wit_component/tau_extension_tools"
	"wit_component/tau_extension_types"
)

// 0.3.0 (docs/tool-media.md): content is a list of result-blocks;
// this example returns one text block.
func textResult(text string, isError bool) tau_extension_tools.ToolResult {
	return tau_extension_tools.ToolResult{
		Content: []tau_extension_types.ResultBlock{tau_extension_types.MakeResultBlockText(text)},
		IsError: isError,
	}
}

const parametersJSON = `{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}`

func Definitions() []tau_extension_tools.Definition {
	return []tau_extension_tools.Definition{{
		Name:           "upper",
		Description:    "Convert text to UPPERCASE",
		ParametersJson: parametersJSON,
	}}
}

func Execute(name string, argumentsJson string) tau_extension_tools.ToolResult {
	if name != "upper" {
		return textResult("unknown tool: "+name, true)
	}
	var args struct {
		Text string `json:"text"`
	}
	if err := json.Unmarshal([]byte(argumentsJson), &args); err != nil || args.Text == "" {
		return textResult("missing string argument 'text'", true)
	}
	return textResult(strings.ToUpper(args.Text), false)
}
