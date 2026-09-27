// Implementation of tau:extension/hooks for the Go upper example: no probes.
package export_tau_extension_hooks

import (
	"wit_component/tau_extension_hooks"
)

func Points() []string {
	return nil
}

func Probe(point string, payloadJson string) tau_extension_hooks.Verdict {
	return tau_extension_hooks.Verdict{Action: tau_extension_hooks.ActionContinue}
}
