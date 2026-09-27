// Implementation of tau:extension/tools for the Go upper example.
package export_tau_extension_tools

import (
	"encoding/json"
	"strings"

	"wit_component/tau_extension_tools"
)

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
		return tau_extension_tools.ToolResult{Content: "unknown tool: " + name, IsError: true}
	}
	var args struct {
		Text string `json:"text"`
	}
	if err := json.Unmarshal([]byte(argumentsJson), &args); err != nil || args.Text == "" {
		return tau_extension_tools.ToolResult{Content: "missing string argument 'text'", IsError: true}
	}
	return tau_extension_tools.ToolResult{Content: strings.ToUpper(args.Text), IsError: false}
}
