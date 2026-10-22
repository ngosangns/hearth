// Package redact masks secret values in JSON-like trees.
//
// A secret is a string whose upper-case key contains SECRET, PASSWORD,
// PASSWD, TOKEN, API_KEY, ACCESS_KEY, or PRIVATE_KEY (a "-" in the key is
// treated as "_"), or the password in a URL's userinfo
// (scheme://user:password@host). Masked secret values become "••••••". A URL
// keeps its scheme, user, host, and path; only the password is replaced.
//
// Masking covers the whole tree: recipe.env, attachment connection blocks,
// provision argument maps, and nested objects. String elements of an array
// that look like KEY=value are masked when KEY matches. The value after a
// --password or --secret style flag is masked when it is not another flag.
package redact

import (
	"regexp"
	"strings"

	"github.com/ngosangns/hearth/go/internal/omap"
)

// Mask is the replacement for a secret value.
const Mask = "••••••"

// maxWalkDepth bounds recursion. Anything deeper is replaced so a hostile
// document cannot overflow the stack or leak a value the walk did not reach.
const maxWalkDepth = 32

// fragments are matched against the upper-case key. PASSWORD does not contain
// PASSWD; both are listed because the contract names them separately.
var fragments = []string{
	"SECRET",
	"PASSWORD",
	"PASSWD",
	"TOKEN",
	"API_KEY",
	"ACCESS_KEY",
	"PRIVATE_KEY",
}

// assignmentKeyPattern matches an env name or a short flag (KEY or --key).
var assignmentKeyPattern = regexp.MustCompile(`^-{0,2}[A-Za-z_][A-Za-z0-9_-]*$`)

// IsSecretKey reports whether name is a secret key. "-" and "_" are equivalent,
// so API_KEY also matches api-key and --api-key.
func IsSecretKey(name string) bool {
	upper := strings.ReplaceAll(strings.ToUpper(name), "-", "_")
	for _, fragment := range fragments {
		if strings.Contains(upper, fragment) {
			return true
		}
	}
	return false
}

// MaskURL replaces a userinfo password with Mask. A string that is not a URL
// with a password is returned unchanged. The user, scheme, host, path, query,
// and fragment stay as they were.
func MaskURL(raw string) string {
	scheme, rest, ok := strings.Cut(raw, "://")
	if !ok || !validScheme(scheme) {
		return raw
	}
	userinfo, tail, ok := strings.Cut(rest, "@")
	if !ok {
		return raw
	}
	user, password, ok := strings.Cut(userinfo, ":")
	if !ok || password == "" {
		return raw
	}
	return scheme + "://" + user + ":" + Mask + "@" + tail
}

func validScheme(scheme string) bool {
	if scheme == "" {
		return false
	}
	for i, r := range scheme {
		switch {
		case r >= 'A' && r <= 'Z', r >= 'a' && r <= 'z':
		case i > 0 && (r >= '0' && r <= '9' || r == '+' || r == '-' || r == '.'):
		default:
			return false
		}
	}
	return true
}

// MaskEnvValue masks one env value. A secret key becomes Mask. Any other
// string is passed through MaskURL, so a DATABASE_URL keeps its user and
// hides only the password.
func MaskEnvValue(key, value string) string {
	if IsSecretKey(key) {
		return Mask
	}
	return MaskURL(value)
}

// Values returns a copy of v with every secret masked. v is a JSON tree of
// map[string]any, *omap.OMap, []any, and scalars. The input is not modified.
// Connections is the historical name.
func Values(v any) any {
	return walk(v, 0)
}

// Connections masks secrets in the tree it is given.
func Connections(v any) any {
	return Values(v)
}

func walk(v any, depth int) any {
	if depth > maxWalkDepth {
		return "[truncated]"
	}
	switch t := v.(type) {
	case *omap.OMap:
		if t == nil {
			return nil
		}
		out := omap.New()
		t.Each(func(key string, entry any) {
			out.Set(key, maskKeyed(key, entry, depth))
		})
		return out
	case map[string]any:
		if t == nil {
			return nil
		}
		out := make(map[string]any, len(t))
		for key, entry := range t {
			out[key] = maskKeyed(key, entry, depth)
		}
		return out
	case []any:
		// A nil slice is how an empty JSON array arrives from omap. Keep the
		// slice type so the printer still emits [], not null.
		if t == nil {
			return t
		}
		return maskSlice(t, depth)
	case string:
		return MaskURL(t)
	default:
		return v
	}
}

func maskKeyed(key string, entry any, depth int) any {
	if text, ok := entry.(string); ok {
		return MaskEnvValue(key, text)
	}
	return walk(entry, depth+1)
}

func maskSlice(items []any, depth int) []any {
	if depth > maxWalkDepth {
		return []any{"[truncated]"}
	}
	out := make([]any, 0, len(items))
	for i := 0; i < len(items); i++ {
		text, ok := items[i].(string)
		if !ok {
			out = append(out, walk(items[i], depth+1))
			continue
		}
		if masked, ok := maskAssignment(text); ok {
			out = append(out, masked)
			continue
		}
		if isSecretFlag(text) && i+1 < len(items) {
			if next, ok := items[i+1].(string); ok && !strings.HasPrefix(next, "-") {
				out = append(out, text, Mask)
				i++
				continue
			}
		}
		out = append(out, MaskURL(text))
	}
	return out
}

// maskAssignment masks KEY=value. A secret KEY hides the whole value. Any
// other assignment still masks a URL password in the value.
func maskAssignment(text string) (string, bool) {
	key, value, ok := strings.Cut(text, "=")
	if !ok || !assignmentKeyPattern.MatchString(key) {
		return "", false
	}
	if IsSecretKey(key) {
		return key + "=" + Mask, true
	}
	return key + "=" + MaskURL(value), true
}

// isSecretFlag reports whether arg is a --password / --secret style flag
// with no inline value. The following array element is the secret.
func isSecretFlag(arg string) bool {
	if strings.Contains(arg, "=") || !strings.HasPrefix(arg, "-") {
		return false
	}
	name := strings.TrimLeft(arg, "-")
	if name == "" {
		return false
	}
	return IsSecretKey(name)
}
