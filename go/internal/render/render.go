// Package render ports the recipe/service template renderer: `{placeholder}`
// substitution over command specs, env maps, readiness specs, and connection
// blocks. `${NAME}` shell expansions are never template vars.
package render

import (
	"fmt"

	"github.com/ngosangns/hearth/go/internal/catalog"
)

// CommandStrings returns every template string inside a command — the argv
// entries, or the one shell string.
func CommandStrings(spec *catalog.CommandSpec) []string {
	if spec == nil {
		return nil
	}
	if spec.IsArgv() {
		return spec.Argv
	}
	return []string{spec.Shell}
}

// Str substitutes every `{name}` var vars defines in one pass; unknown vars
// and `${NAME}` expansions are left as written.
func Str(template string, vars map[string]string) string {
	spans := catalog.TemplateVarSpans(template)
	if len(spans) == 0 {
		return template
	}
	var out []byte
	last := 0
	for _, s := range spans {
		if value, ok := vars[s.Name]; ok {
			out = append(out, template[last:s.Start]...)
			out = append(out, value...)
			last = s.Start + len(s.Name) + 2
		}
	}
	out = append(out, template[last:]...)
	return string(out)
}

func Command(spec *catalog.CommandSpec, vars map[string]string) catalog.CommandSpec {
	if spec.IsArgv() {
		argv := make([]string, len(spec.Argv))
		for i, a := range spec.Argv {
			argv[i] = Str(a, vars)
		}
		return catalog.CommandSpec{Argv: argv}
	}
	return catalog.CommandSpec{Shell: Str(spec.Shell, vars), Exec: spec.Exec}
}

// RenderCommand is the exported alias used by configfile.
func RenderCommand(spec *catalog.CommandSpec, vars map[string]string) catalog.CommandSpec {
	return Command(spec, vars)
}

// Env renders every value of an env map.
func Env(env map[string]string, vars map[string]string) map[string]string {
	out := make(map[string]string, len(env))
	for k, v := range env {
		out[k] = Str(v, vars)
	}
	return out
}

// CheckVars returns an error naming the first `{var}` this context does not
// define — a recipe referencing something it can't have must surface rather
// than run with a literal `{projectDb}` in it.
func CheckVars(template string, vars map[string]string, what string) error {
	if v := catalog.UnknownTemplateVar(template, func(name string) bool {
		_, ok := vars[name]
		return ok
	}); v != nil {
		return fmt.Errorf("%s references unknown template var {%s}", what, *v)
	}
	return nil
}

// RenderCommandChecked renders a command and rejects unknown `{var}`s.
func RenderCommandChecked(spec *catalog.CommandSpec, vars map[string]string, what string) (catalog.CommandSpec, error) {
	for _, text := range CommandStrings(spec) {
		if err := CheckVars(text, vars, what); err != nil {
			return catalog.CommandSpec{}, err
		}
	}
	return Command(spec, vars), nil
}

// RenderReadiness renders a readiness spec; non-templated kinds pass through.
func RenderReadiness(spec *catalog.ReadinessSpec, vars map[string]string) (catalog.ReadinessSpec, error) {
	out := *spec
	switch spec.Kind {
	case "http":
		if err := CheckVars(spec.URL, vars, "readiness.url"); err != nil {
			return out, err
		}
		out.URL = Str(spec.URL, vars)
	case "command":
		if spec.Command != nil {
			cmd, err := RenderCommandChecked(spec.Command, vars, "readiness.command")
			if err != nil {
				return out, err
			}
			out.Command = &cmd
		}
	case "log":
		if err := CheckVars(spec.Pattern, vars, "readiness.pattern"); err != nil {
			return out, err
		}
		out.Pattern = Str(spec.Pattern, vars)
	}
	return out, nil
}

// RenderConnection renders the attach response's connection object:
// `{ url?, env? }` rendered per-project; `{url}` inside env values resolves to
// the rendered url.
func RenderConnection(conn *SharedConnectionLike, vars map[string]string) (map[string]any, error) {
	vars2 := map[string]string{}
	for k, v := range vars {
		vars2[k] = v
	}
	if conn.URL != nil {
		if err := CheckVars(*conn.URL, vars2, "connection.url"); err != nil {
			return nil, err
		}
		vars2["url"] = Str(*conn.URL, vars2)
	}
	out := map[string]any{}
	if url, ok := vars2["url"]; ok {
		out["url"] = url
	}
	if conn.Env != nil {
		rendered := map[string]any{}
		for key, value := range conn.Env {
			if err := CheckVars(value, vars2, "connection.env"); err != nil {
				return nil, err
			}
			rendered[key] = Str(value, vars2)
		}
		out["env"] = rendered
	}
	return out, nil
}

// SharedConnectionLike mirrors shared.SharedConnection without an import cycle.
type SharedConnectionLike struct {
	URL *string
	Env map[string]string
}
