package catalog

import json "encoding/json"

// kv is a key/value pair for ordered JSON emission.
type kv struct {
	k string
	v any
}

// marshalOrdered writes a JSON object from ordered key/value pairs.
func marshalOrdered(fields []kv) []byte {
	out := []byte{'{'}
	for i, f := range fields {
		if i > 0 {
			out = append(out, ',')
		}
		kb, _ := json.Marshal(f.k)
		vb, _ := json.Marshal(f.v)
		out = append(out, kb...)
		out = append(out, ':')
		out = append(out, vb...)
	}
	return append(out, '}')
}
