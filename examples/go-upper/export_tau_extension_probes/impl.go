// Implementation of tau:extension/probes for the Go upper example: no probes.
//
// 0.7.0: `points` and `probe` are typed now — a `point` enum and a `payload`
// variant in, a `Verdict` variant out, instead of JSON strings. The pair
// stays synchronous: a probe is a decision on the run's hot path.
package export_tau_extension_probes

import (
	"wit_component/tau_extension_probes"
)

func Points() []tau_extension_probes.Point {
	return nil
}

func Probe(point tau_extension_probes.Point, payload tau_extension_probes.Payload) tau_extension_probes.Verdict {
	return tau_extension_probes.MakeVerdictContinue()
}
