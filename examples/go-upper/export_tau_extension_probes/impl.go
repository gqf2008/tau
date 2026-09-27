// Implementation of tau:extension/probes for the Go upper example: no probes.
package export_tau_extension_probes

import (
	"wit_component/tau_extension_probes"
)

func Points() []string {
	return nil
}

func Probe(point string, payloadJson string) tau_extension_probes.Verdict {
	return tau_extension_probes.Verdict{Action: tau_extension_probes.ActionContinue}
}
