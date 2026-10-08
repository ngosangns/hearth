// Package omap is an insertion-ordered map used as the generic representation
// for YAML/JSON config values — the Rust side relies on serde_json's
// `preserve_order` so the `services:` map iterates in document order; a plain
// Go map would scramble user-visible service ordering.
package omap

import (
	"bytes"
	json "encoding/json"
	"fmt"
	"strconv"

	yaml "gopkg.in/yaml.v3"
)

// OMap preserves key insertion order.
type OMap struct {
	keys []string
	m    map[string]any
}

func New() *OMap { return &OMap{m: map[string]any{}} }

func (o *OMap) Get(key string) (any, bool) {
	v, ok := o.m[key]
	return v, ok
}

func (o *OMap) Set(key string, v any) {
	if _, ok := o.m[key]; !ok {
		o.keys = append(o.keys, key)
	}
	o.m[key] = v
}

func (o *OMap) Keys() []string { return o.keys }

func (o *OMap) Len() int { return len(o.keys) }

func (o *OMap) Contains(key string) bool {
	_, ok := o.m[key]
	return ok
}

// Each yields (key, value) in document order.
func (o *OMap) Each(fn func(string, any)) {
	for _, k := range o.keys {
		fn(k, o.m[k])
	}
}

// ---- generic value accessors (any = nil | bool | string | int64 | float64 | []any | *OMap) ----

func IsObject(v any) bool { _, ok := v.(*OMap); return ok }
func IsArray(v any) bool  { _, ok := v.([]any); return ok }

func AsObject(v any) *OMap {
	if o, ok := v.(*OMap); ok {
		return o
	}
	return nil
}

func AsArray(v any) []any {
	if a, ok := v.([]any); ok {
		return a
	}
	return nil
}

func AsString(v any) (string, bool) {
	if s, ok := v.(string); ok {
		return s, true
	}
	return "", false
}

func AsBool(v any) (bool, bool) {
	if b, ok := v.(bool); ok {
		return b, true
	}
	return false, false
}

// AsInt64 returns the integer for int64/float64-with-integral-value values.
func AsInt64(v any) (int64, bool) {
	switch n := v.(type) {
	case int64:
		return n, true
	case float64:
		if n == float64(int64(n)) {
			return int64(n), true
		}
	}
	return 0, false
}

func IsString(v any) bool { _, ok := v.(string); return ok }
func IsBool(v any) bool   { _, ok := v.(bool); return ok }

func IsStringArray(v any) bool {
	a, ok := v.([]any)
	if !ok {
		return false
	}
	for _, e := range a {
		if _, ok := e.(string); !ok {
			return false
		}
	}
	return true
}

func IsStringRecord(v any) bool {
	o, ok := v.(*OMap)
	if !ok {
		return false
	}
	ok = true
	o.Each(func(_ string, e any) {
		if _, isStr := e.(string); !isStr {
			ok = false
		}
	})
	return ok
}

func StringRecord(v any) map[string]string {
	out := map[string]string{}
	if o, ok := v.(*OMap); ok {
		o.Each(func(k string, e any) {
			if s, isStr := e.(string); isStr {
				out[k] = s
			}
		})
	}
	return out
}

func StringArray(v any) []string {
	var out []string
	if a, ok := v.([]any); ok {
		for _, e := range a {
			if s, isStr := e.(string); isStr {
				out = append(out, s)
			}
		}
	}
	return out
}

// ScalarToString is JS's String(x) for the scalar types argv-coercion accepts
// (string/number/bool).
func ScalarToString(v any) (string, bool) {
	switch t := v.(type) {
	case string:
		return t, true
	case bool:
		if t {
			return "true", true
		}
		return "false", true
	case int64:
		return strconv.FormatInt(t, 10), true
	case float64:
		return strconv.FormatFloat(t, 'g', -1, 64), true
	}
	return "", false
}

// Describe returns a serde_json-ish rendering used in "got X" error messages.
func Describe(v any) string {
	switch t := v.(type) {
	case nil:
		return "null"
	case string:
		b, _ := json.Marshal(t)
		return string(b)
	case bool:
		if t {
			return "true"
		}
		return "false"
	case int64:
		return strconv.FormatInt(t, 10)
	case float64:
		return strconv.FormatFloat(t, 'g', -1, 64)
	}
	b, _ := json.Marshal(toPlain(v))
	return string(b)
}

// ToPlain converts an OMap tree into plain map[string]any (order lost).
func ToPlain(v any) any {
	return toPlain(v)
}

func toPlain(v any) any {
	switch t := v.(type) {
	case *OMap:
		m := map[string]any{}
		t.Each(func(k string, e any) { m[k] = toPlain(e) })
		return m
	case []any:
		out := make([]any, len(t))
		for i, e := range t {
			out[i] = toPlain(e)
		}
		return out
	default:
		return v
	}
}

// ---- parsers ----

// ParseYAML parses YAML into the generic ordered representation.
func ParseYAML(text string) (any, error) {
	var node yaml.Node
	if err := yaml.Unmarshal([]byte(text), &node); err != nil {
		return nil, err
	}
	return yamlToValue(&node)
}

func yamlToValue(n *yaml.Node) (any, error) {
	if n.Kind == yaml.DocumentNode {
		if len(n.Content) == 0 {
			return nil, nil
		}
		return yamlToValue(n.Content[0])
	}
	switch n.Kind {
	case yaml.MappingNode:
		o := New()
		for i := 0; i+1 < len(n.Content); i += 2 {
			key := n.Content[i].Value
			v, err := yamlToValue(n.Content[i+1])
			if err != nil {
				return nil, err
			}
			o.Set(key, v)
		}
		return o, nil
	case yaml.SequenceNode:
		var out []any
		for _, c := range n.Content {
			v, err := yamlToValue(c)
			if err != nil {
				return nil, err
			}
			out = append(out, v)
		}
		return out, nil
	case yaml.ScalarNode:
		return yamlScalar(n), nil
	case yaml.AliasNode:
		return yamlToValue(n.Alias)
	default:
		return nil, nil
	}
}

func yamlScalar(n *yaml.Node) any {
	switch n.Tag {
	case "!!null":
		return nil
	case "!!bool":
		return n.Value == "true" || n.Value == "True" || n.Value == "TRUE"
	case "!!int":
		var i int64
		if err := n.Decode(&i); err == nil {
			return i
		}
		var f float64
		if err := n.Decode(&f); err == nil {
			return f
		}
		return n.Value
	case "!!float":
		var f float64
		if err := n.Decode(&f); err == nil {
			return f
		}
		return n.Value
	case "!!str":
		return n.Value
	default:
		return n.Value
	}
}

// ParseJSON parses JSON preserving object key order via a token walk.
func ParseJSON(text string) (any, error) {
	dec := json.NewDecoder(bytes.NewReader([]byte(text)))
	v, err := jsonValue(dec)
	if err != nil {
		return nil, err
	}
	return v, nil
}

func jsonValue(dec *json.Decoder) (any, error) {
	tok, err := dec.Token()
	if err != nil {
		return nil, err
	}
	switch t := tok.(type) {
	case json.Delim:
		switch t {
		case '{':
			o := New()
			for dec.More() {
				kTok, err := dec.Token()
				if err != nil {
					return nil, err
				}
				key, ok := kTok.(string)
				if !ok {
					return nil, fmt.Errorf("json: expected object key")
				}
				v, err := jsonValue(dec)
				if err != nil {
					return nil, err
				}
				o.Set(key, v)
			}
			if _, err := dec.Token(); err != nil { // consume '}'
				return nil, err
			}
			return o, nil
		case '[':
			var out []any
			for dec.More() {
				v, err := jsonValue(dec)
				if err != nil {
					return nil, err
				}
				out = append(out, v)
			}
			if _, err := dec.Token(); err != nil { // consume ']'
				return nil, err
			}
			return out, nil
		}
		return nil, fmt.Errorf("json: unexpected delimiter %v", t)
	case string:
		return t, nil
	case bool:
		return t, nil
	case float64:
		if t == float64(int64(t)) {
			return int64(t), nil
		}
		return t, nil
	case nil:
		return nil, nil
	case json.Number:
		return t.String(), nil
	default:
		return nil, fmt.Errorf("json: unexpected token %v", t)
	}
}
