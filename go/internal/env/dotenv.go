package env

import (
	"os"
	"strings"
)

// parseDotenv implements the dotenvy semantics hearth relies on: comments,
// `export ` prefixes, single/double quoted values, escapes in double quotes,
// and ${VAR} interpolation resolved against earlier entries then the process
// environment.
func parseDotenv(text string) map[string]string {
	out := map[string]string{}
	var keyOrder []string
	lookup := func(name string) (string, bool) {
		if v, ok := out[name]; ok {
			return v, true
		}
		v, ok := os.LookupEnv(name)
		return v, ok
	}
	for _, raw := range strings.Split(text, "\n") {
		line := strings.TrimRight(raw, "\r")
		trimmed := strings.TrimSpace(line)
		if trimmed == "" || strings.HasPrefix(trimmed, "#") {
			continue
		}
		rest := strings.TrimPrefix(trimmed, "export ")
		eq := strings.IndexByte(rest, '=')
		if eq < 0 {
			continue
		}
		key := strings.TrimSpace(rest[:eq])
		val := strings.TrimSpace(rest[eq+1:])
		var parsed string
		switch {
		case strings.HasPrefix(val, "'") && strings.HasSuffix(val, "'") && len(val) >= 2:
			parsed = val[1 : len(val)-1]
		case strings.HasPrefix(val, `"`) && strings.HasSuffix(val, `"`) && len(val) >= 2:
			parsed = unescapeDoubleQuoted(val[1 : len(val)-1])
		default:
			// Unquoted: strip trailing comment after whitespace.
			if idx := strings.Index(val, " #"); idx >= 0 {
				val = val[:idx]
			}
			parsed = interpolate(strings.TrimSpace(val), lookup)
		}
		if _, exists := out[key]; !exists {
			keyOrder = append(keyOrder, key)
		}
		out[key] = parsed
	}
	return out
}

func unescapeDoubleQuoted(s string) string {
	var b strings.Builder
	for i := 0; i < len(s); i++ {
		c := s[i]
		if c == '\\' && i+1 < len(s) {
			i++
			switch s[i] {
			case 'n':
				b.WriteByte('\n')
			case 't':
				b.WriteByte('\t')
			case 'r':
				b.WriteByte('\r')
			case '\\':
				b.WriteByte('\\')
			case '"':
				b.WriteByte('"')
			case '$':
				b.WriteByte('$')
			default:
				b.WriteByte('\\')
				b.WriteByte(s[i])
			}
			continue
		}
		b.WriteByte(c)
	}
	return interpolate(b.String(), func(name string) (string, bool) {
		v, ok := os.LookupEnv(name)
		return v, ok
	})
}

func interpolate(s string, lookup func(string) (string, bool)) string {
	var b strings.Builder
	for i := 0; i < len(s); i++ {
		if s[i] == '$' && i+1 < len(s) && s[i+1] == '{' {
			if end := strings.IndexByte(s[i+2:], '}'); end >= 0 {
				name := s[i+2 : i+2+end]
				if v, ok := lookup(name); ok {
					b.WriteString(v)
				}
				i += 2 + end
				continue
			}
		}
		b.WriteByte(s[i])
	}
	return b.String()
}
