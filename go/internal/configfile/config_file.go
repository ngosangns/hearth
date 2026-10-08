// Package configfile ports the declarative catalog authoring: a `hearth.yaml`
// (or `.yml`/`.json`) in a project root maps onto the same ServiceCatalog every
// other entry point takes. TypeScript catalogs are not accepted.
package configfile

import (
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strings"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/env"
	"github.com/ngosangns/hearth/go/internal/omap"
	"github.com/ngosangns/hearth/go/internal/paths"
	"github.com/ngosangns/hearth/go/internal/render"
	"github.com/ngosangns/hearth/go/internal/shared"
)

var ConfigFileNames = []string{"hearth.yaml", "hearth.yml", "hearth.json"}

type LoadedCatalog struct {
	Catalog *catalog.ServiceCatalog
	Path    string
}

type LoadError struct {
	Path   string
	Errors []string
}

func (e *LoadError) Error() string { return strings.Join(e.Errors, "; ") }

// FindConfigFile returns the first existing candidate in CONFIG_FILE_NAMES order.
func FindConfigFile(root string) string {
	for _, name := range ConfigFileNames {
		p := filepath.Join(root, name)
		if _, err := os.Stat(p); err == nil {
			return p
		}
	}
	return ""
}

func LoadCatalog(root string) (*LoadedCatalog, error) {
	path := FindConfigFile(root)
	if path == "" {
		message := fmt.Sprintf("no config file found in %s (looked for %s)",
			root, strings.Join(ConfigFileNames, ", "))
		if _, err := os.Stat(filepath.Join(root, "hearth.config.ts")); err == nil {
			message += "; hearth.config.ts is no longer accepted — author a hearth.yaml instead"
		}
		return nil, &LoadError{Errors: []string{message}}
	}
	return LoadCatalogFromFile(path, root)
}

func LoadCatalogFromFile(path, root string) (*LoadedCatalog, error) {
	if filepath.Ext(path) == ".ts" {
		return nil, &LoadError{Path: path, Errors: []string{
			fmt.Sprintf("%s is a TypeScript catalog; only hearth.yaml, .yml, or .json are accepted", path),
		}}
	}
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, &LoadError{Path: path, Errors: []string{fmt.Sprintf("failed to read %s: %v", path, err)}}
	}
	ext := filepath.Ext(path)
	isYAML := ext == ".yaml" || ext == ".yml"
	var raw any
	if isYAML {
		raw, err = omap.ParseYAML(string(data))
	} else {
		raw, err = omap.ParseJSON(string(data))
	}
	if err != nil {
		return nil, &LoadError{Path: path, Errors: []string{fmt.Sprintf("failed to parse %s: %v", path, err)}}
	}
	cat, errors := mapConfigFile(raw, root)
	if len(errors) > 0 {
		return nil, &LoadError{Path: path, Errors: errors}
	}
	validation := catalog.ValidateCatalog(cat)
	if len(validation.Errors) > 0 {
		return nil, &LoadError{Path: path, Errors: validation.Errors}
	}
	return &LoadedCatalog{Catalog: cat, Path: path}, nil
}

var readinessKinds = []string{"process", "tcp", "http", "container", "tailnet", "command", "exit", "log"}
var serviceKinds = []string{"application", "infrastructure"}
var ownerships = []string{"daemon", "external"}

// expandGroupMembers expands group members depth-first; visiting is the chain
// currently being expanded — re-entering one is a cycle and a load error.
func expandGroupMembers(group string, members []string, declared map[string][]string, serviceIDs map[string]bool, visiting []string, out *[]string, errors *[]string) {
	for _, member := range members {
		// Service ids win over same-named groups, matching target resolution.
		if serviceIDs[member] {
			found := false
			for _, s := range *out {
				if s == member {
					found = true
					break
				}
			}
			if !found {
				*out = append(*out, member)
			}
			continue
		}
		sub, ok := declared[member]
		if !ok {
			*errors = append(*errors, fmt.Sprintf("group %s references unknown service or group %s", group, member))
			continue
		}
		cycle := false
		for _, v := range visiting {
			if v == member {
				cycle = true
				break
			}
		}
		if cycle {
			chain := append(append([]string{}, visiting...), member)
			*errors = append(*errors, "group cycle: "+strings.Join(chain, " -> "))
			continue
		}
		expandGroupMembers(member, sub, declared, serviceIDs, append(visiting, member), out, errors)
	}
}

type readCommand struct{ spec catalog.CommandSpec }

// readCommandSpec reads a `{argv}`/`{shell}` command at path; absent is valid
// (callers decide whether that means "no command" or "required").
func readCommandSpec(value any, path string, errors *[]string) *readCommand {
	obj := omap.AsObject(value)
	if obj == nil {
		*errors = append(*errors, fmt.Sprintf("%s must be an object with `argv` or `shell`", path))
		return nil
	}
	_, hasArgv := obj.Get("argv")
	_, hasShell := obj.Get("shell")
	if hasArgv == hasShell {
		*errors = append(*errors, fmt.Sprintf("%s must set exactly one of `argv` or `shell`", path))
		return nil
	}
	if hasArgv {
		argvValue, _ := obj.Get("argv")
		items := omap.AsArray(argvValue)
		var argv []string
		bad := items == nil
		for _, it := range items {
			s, ok := omap.ScalarToString(it)
			if !ok {
				bad = true
				break
			}
			argv = append(argv, s)
		}
		if bad || len(argv) == 0 {
			*errors = append(*errors, fmt.Sprintf("%s.argv must be a non-empty array of strings (numbers/booleans are coerced to strings)", path))
			return nil
		}
		return &readCommand{spec: catalog.CommandSpec{Argv: argv}}
	}
	shellV, _ := obj.Get("shell")
	shell, _ := omap.AsString(shellV)
	if strings.TrimSpace(shell) == "" {
		*errors = append(*errors, fmt.Sprintf("%s.shell must be a non-empty string", path))
		return nil
	}
	var exec *bool
	if ev, ok := obj.Get("exec"); ok {
		b, isBool := omap.AsBool(ev)
		if !isBool {
			*errors = append(*errors, fmt.Sprintf("%s.exec must be a boolean", path))
			return nil
		}
		exec = &b
	}
	return &readCommand{spec: catalog.CommandSpec{Shell: shell, Exec: exec}}
}

func tcpPort(value any) (uint16, bool) {
	n, ok := omap.AsInt64(value)
	if !ok || n < 1 || n > 65535 {
		return 0, false
	}
	return uint16(n), true
}

func validHealthPath(path string) bool {
	return strings.HasPrefix(path, "/") &&
		!strings.ContainsAny(path, " \t\n\r") &&
		!strings.Contains(path, "://") &&
		!strings.Contains(path, "{") &&
		!strings.Contains(path, "}")
}

func readHTTPReadiness(obj *omap.OMap, path string, primaryPort *uint16, errors *[]string) *catalog.ReadinessSpec {
	if obj.Contains("url") {
		if obj.Contains("path") || obj.Contains("port") {
			*errors = append(*errors, fmt.Sprintf("%s must not set url together with path or port", path))
			return nil
		}
		urlV, _ := obj.Get("url")
		url, _ := omap.AsString(urlV)
		if url == "" {
			*errors = append(*errors, fmt.Sprintf("%s.url must be a non-empty string", path))
			return nil
		}
		return &catalog.ReadinessSpec{Kind: "http", URL: url}
	}
	var requestPath string
	pv, hasPath := obj.Get("path")
	if !hasPath {
		requestPath = "/health"
	} else {
		s, isStr := omap.AsString(pv)
		if !isStr {
			*errors = append(*errors, fmt.Sprintf("%s.path must be a string", path))
			return nil
		}
		if !validHealthPath(s) {
			*errors = append(*errors, fmt.Sprintf("%s.path must be an absolute path", path))
			return nil
		}
		requestPath = s
	}
	var port *uint16
	if pv, ok := obj.Get("port"); ok {
		p, ok := tcpPort(pv)
		if !ok {
			*errors = append(*errors, fmt.Sprintf("%s.port must be a positive integer", path))
			return nil
		}
		port = &p
	} else {
		port = primaryPort
	}
	if port == nil {
		*errors = append(*errors, fmt.Sprintf("%s needs a port", path))
		return nil
	}
	return &catalog.ReadinessSpec{Kind: "http", URL: fmt.Sprintf("http://127.0.0.1:%d%s", *port, requestPath)}
}

func readReadiness(value any, path string, primaryPort *uint16, errors *[]string) *catalog.ReadinessSpec {
	obj := omap.AsObject(value)
	var kind string
	if obj != nil {
		kv, _ := obj.Get("kind")
		kind, _ = omap.AsString(kv)
	}
	if kind == "" {
		*errors = append(*errors, fmt.Sprintf("%s.kind is required (one of %s)", path, strings.Join(readinessKinds, ", ")))
		return nil
	}
	valid := false
	for _, k := range readinessKinds {
		if k == kind {
			valid = true
			break
		}
	}
	if !valid {
		*errors = append(*errors, fmt.Sprintf("%s.kind must be one of %s, got %q", path, strings.Join(readinessKinds, ", "), kind))
		return nil
	}
	switch kind {
	case "process":
		return &catalog.ReadinessSpec{Kind: "process"}
	case "container":
		return &catalog.ReadinessSpec{Kind: "container"}
	case "tailnet":
		return &catalog.ReadinessSpec{Kind: "tailnet"}
	case "tcp":
		if pv, ok := obj.Get("port"); ok {
			p, ok := tcpPort(pv)
			if !ok {
				*errors = append(*errors, fmt.Sprintf("%s.port must be a positive integer", path))
				return nil
			}
			return &catalog.ReadinessSpec{Kind: "tcp", Port: &p}
		}
		if primaryPort == nil {
			*errors = append(*errors, fmt.Sprintf("%s needs a port", path))
			return nil
		}
		return &catalog.ReadinessSpec{Kind: "tcp", Port: primaryPort}
	case "http":
		return readHTTPReadiness(obj, path, primaryPort, errors)
	case "exit":
		return &catalog.ReadinessSpec{Kind: "exit"}
	case "log":
		pv, _ := obj.Get("pattern")
		pattern, isStr := omap.AsString(pv)
		if !isStr || pattern == "" {
			*errors = append(*errors, fmt.Sprintf("%s.pattern must be a non-empty string", path))
			return nil
		}
		if _, err := regexp.Compile(pattern); err != nil {
			*errors = append(*errors, fmt.Sprintf("%s.pattern is not a valid regex: %v", path, err))
			return nil
		}
		return &catalog.ReadinessSpec{Kind: "log", Pattern: pattern}
	default: // "command"
		var cv any
		if v, ok := obj.Get("command"); ok {
			cv = v
		}
		command := readCommandSpec(cv, path+".command", errors)
		if command == nil {
			return nil
		}
		var cwd *string
		if v, ok := obj.Get("cwd"); ok {
			s, isStr := omap.AsString(v)
			if !isStr {
				*errors = append(*errors, fmt.Sprintf("%s.cwd must be a string", path))
				return nil
			}
			cwd = &s
		}
		return &catalog.ReadinessSpec{Kind: "command", Command: &command.spec, Cwd: cwd}
	}
}

func readPreparationCommand(value any, path string, errors *[]string) *catalog.PreparationCommand {
	obj := omap.AsObject(value)
	if obj == nil {
		*errors = append(*errors, fmt.Sprintf("%s must be an object with `command` (and optional `cwd`)", path))
		return nil
	}
	var cv any
	if v, ok := obj.Get("command"); ok {
		cv = v
	}
	command := readCommandSpec(cv, path+".command", errors)
	var cwd *string
	var serKey *string
	if v, ok := obj.Get("cwd"); ok {
		s, isStr := omap.AsString(v)
		if !isStr {
			*errors = append(*errors, fmt.Sprintf("%s.cwd must be a string", path))
		} else {
			cwd = &s
		}
	}
	if v, ok := obj.Get("serializationKey"); ok {
		s, isStr := omap.AsString(v)
		if !isStr {
			*errors = append(*errors, fmt.Sprintf("%s.serializationKey must be a string", path))
		} else {
			serKey = &s
		}
	}
	if command == nil {
		return nil
	}
	return &catalog.PreparationCommand{Command: command.spec, Cwd: cwd, SerializationKey: serKey}
}

func readURLs(value any, present bool, path string, errors *[]string) []catalog.ServiceURL {
	if !present {
		return []catalog.ServiceURL{}
	}
	array := omap.AsArray(value)
	if array == nil {
		*errors = append(*errors, fmt.Sprintf("%s must be an array", path))
		return nil
	}
	var urls []catalog.ServiceURL
	for index, entry := range array {
		entryPath := fmt.Sprintf("%s[%d]", path, index)
		if s, isStr := omap.AsString(entry); isStr {
			urls = append(urls, catalog.ServiceURL{URL: s})
			continue
		}
		obj := omap.AsObject(entry)
		var urlV any
		var hasURL bool
		if obj != nil {
			urlV, hasURL = obj.Get("url")
		}
		url, isStr := omap.AsString(urlV)
		if !hasURL || !isStr {
			*errors = append(*errors, fmt.Sprintf("%s must be a URL string or { url: string, label?: string, requiresRunning?: boolean }", entryPath))
			continue
		}
		var label *string
		var requiresRunning *bool
		bad := false
		if lv, ok := obj.Get("label"); ok {
			s, isStr := omap.AsString(lv)
			if !isStr {
				*errors = append(*errors, fmt.Sprintf("%s.label must be a string", entryPath))
				bad = true
			} else {
				label = &s
			}
		}
		if rv, ok := obj.Get("requiresRunning"); ok {
			b, isBool := omap.AsBool(rv)
			if !isBool {
				*errors = append(*errors, fmt.Sprintf("%s.requiresRunning must be a boolean", entryPath))
				bad = true
			} else {
				requiresRunning = &b
			}
		}
		if bad {
			continue
		}
		urls = append(urls, catalog.ServiceURL{URL: url, Label: label, RequiresRunning: requiresRunning})
	}
	return urls
}

func readPorts(value any, present bool, path string, errors *[]string) []catalog.ServicePort {
	if !present {
		return []catalog.ServicePort{}
	}
	array := omap.AsArray(value)
	if array == nil {
		*errors = append(*errors, fmt.Sprintf("%s must be an array", path))
		return nil
	}
	var ports []catalog.ServicePort
	for index, entry := range array {
		entryPath := fmt.Sprintf("%s[%d]", path, index)
		obj := omap.AsObject(entry)
		var portRaw, labelRaw any
		var hasPort, hasLabel bool
		if obj != nil {
			portRaw, hasPort = obj.Get("port")
			labelRaw, hasLabel = obj.Get("label")
		}
		portN, _ := omap.AsInt64(portRaw)
		label, labelIsStr := omap.AsString(labelRaw)
		if !hasPort || !hasLabel || !labelIsStr || label == "" {
			*errors = append(*errors, fmt.Sprintf("%s must be { port: number, label: string }", entryPath))
			continue
		}
		if portN < 1 || portN > 65535 {
			*errors = append(*errors, fmt.Sprintf("%s.port must be between 1 and 65535, got %d", entryPath, portN))
			continue
		}
		var requiresRunning *bool
		bad := false
		if rv, ok := obj.Get("requiresRunning"); ok {
			b, isBool := omap.AsBool(rv)
			if !isBool {
				*errors = append(*errors, fmt.Sprintf("%s.requiresRunning must be a boolean", entryPath))
				bad = true
			} else {
				requiresRunning = &b
			}
		}
		if bad {
			continue
		}
		ports = append(ports, catalog.ServicePort{Port: uint16(portN), Label: label, RequiresRunning: requiresRunning})
	}
	return ports
}

// resolveServiceCwd checks `cwd` stays lexically inside the project root.
func resolveServiceCwd(cwd *string, path string, errors *[]string) *string {
	relativeCwd := "."
	if cwd != nil {
		relativeCwd = *cwd
	}
	if filepath.IsAbs(relativeCwd) {
		*errors = append(*errors, fmt.Sprintf("%s.cwd must be a relative path, got %s", path, relativeCwd))
		return nil
	}
	depth := 0
	for _, component := range strings.Split(filepath.ToSlash(relativeCwd), "/") {
		switch component {
		case "..":
			depth--
		case "", ".":
		default:
			depth++
		}
		if depth < 0 {
			*errors = append(*errors, fmt.Sprintf("%s.cwd escapes the project root: %s", path, relativeCwd))
			return nil
		}
	}
	return &relativeCwd
}

var topLevelKeys = []string{"version", "env", "envFile", "runtimeDirectory", "privateFileGuard", "groups", "services", "shared"}
var serviceKeys = []string{"label", "kind", "ownership", "disabled", "env", "container", "cwd", "run", "stop", "build", "readiness", "readinessTimeoutMs", "preparationCommand", "ports", "urls", "artifact", "restart"}
var restartKeys = []string{"on", "maxRestarts", "delayMs"}
var restartTriggers = []string{"never", "on-failure", "always"}
var artifactKeys = []string{"version", "url", "script", "scriptArgs", "sha256"}
var sharedEntryKeys = []string{"version", "preparationCommand", "attachArgs", "urls"}

func readRestartPolicy(value any, path string, errors *[]string) *catalog.ServiceRestartPolicy {
	obj := omap.AsObject(value)
	if obj == nil {
		*errors = append(*errors, fmt.Sprintf("%s must be an object", path))
		return nil
	}
	checkKnownKeys(obj, restartKeys, path, errors)
	var on catalog.RestartTrigger
	switch ov, ok := obj.Get("on"); {
	case !ok:
		on = catalog.RestartOnFailure
	default:
		s, isStr := omap.AsString(ov)
		switch {
		case isStr && s == "never":
			on = catalog.RestartNever
		case isStr && s == "always":
			on = catalog.RestartAlways
		case isStr && s == "on-failure":
			on = catalog.RestartOnFailure
		default:
			*errors = append(*errors, fmt.Sprintf("%s.on must be one of %s, got %q", path, strings.Join(restartTriggers, ", "), omap.Describe(ov)))
			return nil
		}
	}
	var maxRestarts *uint32
	if mv, ok := obj.Get("maxRestarts"); ok {
		n, isInt := omap.AsInt64(mv)
		if !isInt || n < 0 || n > int64(^uint32(0)) {
			*errors = append(*errors, fmt.Sprintf("%s.maxRestarts must be a non-negative integer", path))
		} else {
			u := uint32(n)
			maxRestarts = &u
		}
	}
	delayMs := readDurationMs(func() (any, bool) { return obj.Get("delayMs") }, path+".delayMs", errors)
	return &catalog.ServiceRestartPolicy{On: on, MaxRestarts: maxRestarts, DelayMs: delayMs}
}

func readArtifact(value any, path string, errors *[]string) *catalog.ServiceArtifact {
	obj := omap.AsObject(value)
	if obj == nil {
		*errors = append(*errors, fmt.Sprintf("%s must be an object", path))
		return nil
	}
	checkKnownKeys(obj, artifactKeys, path, errors)
	var version string
	if v, ok := obj.Get("version"); ok {
		version, _ = omap.AsString(v)
	}
	if version == "" {
		*errors = append(*errors, fmt.Sprintf("%s.version must be a non-empty string", path))
	}
	var url, script *string
	if v, ok := obj.Get("url"); ok {
		if s, isStr := omap.AsString(v); isStr && s != "" {
			url = &s
		}
	}
	if v, ok := obj.Get("script"); ok {
		if s, isStr := omap.AsString(v); isStr && s != "" {
			script = &s
		}
	}
	if (url != nil) == (script != nil) {
		*errors = append(*errors, fmt.Sprintf("%s needs exactly one of url or script", path))
	}
	var sha256 *string
	if v, ok := obj.Get("sha256"); ok {
		if s, isStr := omap.AsString(v); isStr && s != "" {
			sha256 = &s
		}
	}
	if url != nil && sha256 == nil {
		*errors = append(*errors, fmt.Sprintf("%s.sha256 is required for a url artifact", path))
	}
	var scriptArgs []string
	if v, ok := obj.Get("scriptArgs"); ok {
		if omap.IsStringArray(v) {
			scriptArgs = omap.StringArray(v)
		} else {
			*errors = append(*errors, fmt.Sprintf("%s.scriptArgs must be an array of strings", path))
		}
	}
	return &catalog.ServiceArtifact{Version: version, URL: url, Script: script, ScriptArgs: scriptArgs, Sha256: sha256}
}

// artifactVars returns the `{var}` values available to a service declaring
// `artifact:` — `{port}` is the first declared port (or the tcp readiness port
// when no `ports:` entries exist); `{port2}…` walk the rest.
func artifactVars(root, runtimeDirectory, id, version string, ports []catalog.ServicePort, readiness *catalog.ReadinessSpec) map[string]string {
	vars := map[string]string{
		"installDir":  paths.InstallDir(runtimeDirectory, id, version),
		"dataDir":     paths.ServiceDataDir(runtimeDirectory, id),
		"serviceId":   id,
		"projectRoot": root,
	}
	for i, entry := range ports {
		name := "port"
		if i > 0 {
			name = fmt.Sprintf("port%d", i+1)
		}
		vars[name] = fmt.Sprintf("%d", entry.Port)
	}
	if _, ok := vars["port"]; !ok && readiness != nil && readiness.Kind == "tcp" && readiness.Port != nil {
		vars["port"] = fmt.Sprintf("%d", *readiness.Port)
	}
	return vars
}

func isArtifactVar(name string) bool {
	switch name {
	case "installDir", "dataDir", "serviceId", "projectRoot":
		return true
	}
	if rest, ok := strings.CutPrefix(name, "port"); ok {
		if rest == "" {
			return true
		}
		for _, c := range rest {
			if c < '0' || c > '9' {
				return false
			}
		}
		return true
	}
	return false
}

func checkTemplate(text string, vars map[string]string, path string, errors *[]string) {
	if v := catalog.UnknownTemplateVar(text, func(name string) bool {
		_, ok := vars[name]
		return !isArtifactVar(name) || ok
	}); v != nil {
		*errors = append(*errors, fmt.Sprintf("%s: template var {%s} has no value — declare it in ports (or a tcp readiness port for {port})", path, *v))
	}
}

func renderArtifactCommand(spec *catalog.CommandSpec, vars map[string]string, path string, errors *[]string) catalog.CommandSpec {
	for _, text := range render.CommandStrings(spec) {
		checkTemplate(text, vars, path, errors)
	}
	return render.RenderCommand(spec, vars)
}

// renderServiceArtifact substitutes artifact vars everywhere they can appear,
// in place on the finished definition.
func renderServiceArtifact(root, runtimeDirectory string, def *catalog.ServiceDefinition, artifact *catalog.ServiceArtifact, path string, errors *[]string) {
	readinessForPort := def.Profiles.Run.Readiness
	vars := artifactVars(root, runtimeDirectory, def.ID, artifact.Version, def.Ports, &readinessForPort)
	artifact.InstallDir = strPtr(vars["installDir"])
	artifact.DataDir = strPtr(vars["dataDir"])
	if def.Profiles.Run.IsVerified() {
		cmd := def.Profiles.Run.Command
		cmd.Command = renderArtifactCommand(&cmd.Command, vars, path+".run", errors)
		checkTemplate(cmd.Cwd, vars, path+".cwd", errors)
		cmd.Cwd = render.Str(cmd.Cwd, vars)
		if cmd.Environment != nil {
			for _, v := range cmd.Environment {
				checkTemplate(v, vars, path+".env", errors)
			}
			cmd.Environment = render.Env(cmd.Environment, vars)
		}
		if cmd.DockerStopCommand != nil {
			stop := renderArtifactCommand(cmd.DockerStopCommand, vars, path+".stop", errors)
			cmd.DockerStopCommand = &stop
		}
		r := &def.Profiles.Run.Readiness
		switch r.Kind {
		case "http":
			checkTemplate(r.URL, vars, path+".readiness.url", errors)
			r.URL = render.Str(r.URL, vars)
		case "command":
			if r.Command != nil {
				rc := renderArtifactCommand(r.Command, vars, path+".readiness.command", errors)
				r.Command = &rc
			}
			if r.Cwd != nil {
				checkTemplate(*r.Cwd, vars, path+".readiness.cwd", errors)
				*r.Cwd = render.Str(*r.Cwd, vars)
			}
		case "log":
			checkTemplate(r.Pattern, vars, path+".readiness.pattern", errors)
			r.Pattern = render.Str(r.Pattern, vars)
		}
		if prep := def.Profiles.Run.PreparationCommand; prep != nil {
			prep.Command = renderArtifactCommand(&prep.Command, vars, path+".preparationCommand", errors)
			if prep.Cwd != nil {
				checkTemplate(*prep.Cwd, vars, path+".preparationCommand.cwd", errors)
				*prep.Cwd = render.Str(*prep.Cwd, vars)
			}
		}
	}
	if build := def.Profiles.Build; build != nil {
		build.Command.Command = renderArtifactCommand(&build.Command.Command, vars, path+".build", errors)
		build.Command.Cwd = render.Str(build.Command.Cwd, vars)
		if build.Command.Environment != nil {
			build.Command.Environment = render.Env(build.Command.Environment, vars)
		}
	}
	for i := range def.URLs {
		def.URLs[i].URL = render.Str(def.URLs[i].URL, vars)
	}
}

func readPositiveU64(value any, present bool, path string, errors *[]string) *uint64 {
	if !present {
		return nil
	}
	n, ok := omap.AsInt64(value)
	if !ok || n <= 0 {
		*errors = append(*errors, fmt.Sprintf("%s must be a positive integer", path))
		return nil
	}
	u := uint64(n)
	return &u
}

// readDurationMs accepts a bare positive integer (ms) or a humantime string
// like "30s"/"1m30s"/"500ms".
func readDurationMs(get func() (any, bool), path string, errors *[]string) *uint64 {
	value, present := get()
	if !present {
		return nil
	}
	if s, isStr := omap.AsString(value); isStr {
		ms, ok := parseHumantime(s)
		if !ok || ms == 0 {
			*errors = append(*errors, fmt.Sprintf("%s must be a positive integer or a duration like \"30s\"", path))
			return nil
		}
		return &ms
	}
	return readPositiveU64(value, true, path, errors)
}

// parseHumantime parses a subset of humantime durations: units ns, us, ms, s,
// m, h, d (and common combos like "1m30s").
func parseHumantime(s string) (uint64, bool) {
	if s == "" {
		return 0, false
	}
	var total uint64
	i := 0
	sawAny := false
	for i < len(s) {
		start := i
		for i < len(s) && s[i] >= '0' && s[i] <= '9' {
			i++
		}
		if start == i {
			return 0, false
		}
		var n uint64
		fmt.Sscanf(s[start:i], "%d", &n)
		unitStart := i
		for i < len(s) && (s[i] < '0' || s[i] > '9') && s[i] != ' ' {
			i++
		}
		unit := s[unitStart:i]
		var mult uint64
		switch unit {
		case "ns":
			mult = 1
		case "us", "µs":
			mult = 1000
		case "ms":
			mult = 1_000_000
		case "s":
			mult = 1_000_000_000
		case "m":
			mult = 60_000_000_000
		case "h":
			mult = 3_600_000_000_000
		case "d":
			mult = 86_400_000_000_000
		default:
			return 0, false
		}
		total += n * mult
		sawAny = true
		// consume a single space between components ("1m 30s")
		for i < len(s) && s[i] == ' ' {
			i++
		}
	}
	if !sawAny {
		return 0, false
	}
	return total / 1_000_000, true
}

func checkKnownKeys(obj *omap.OMap, known []string, path string, errors *[]string) {
	obj.Each(func(key string, _ any) {
		found := false
		for _, k := range known {
			if k == key {
				found = true
				break
			}
		}
		if !found && !strings.HasPrefix(key, "x-") {
			*errors = append(*errors, fmt.Sprintf("%s has unknown key %q (known: %s)", path, key, strings.Join(known, ", ")))
		}
	})
}

// isValidSharedName restricts `shared:` keys to the charset both service ids
// and smp instance names carry safely (also a valid filename under installs/).
func isValidSharedName(name string) bool {
	if name == "" {
		return false
	}
	if c := name[0]; !(c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9') {
		return false
	}
	for i := 1; i < len(name); i++ {
		c := name[i]
		if !(c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' || c == '.' || c == '_' || c == '-') {
			return false
		}
	}
	return true
}

func strPtr(s string) *string { return &s }

func mapConfigFile(raw any, root string) (*catalog.ServiceCatalog, []string) {
	var errors []string
	top := omap.AsObject(raw)
	if top == nil {
		return nil, []string{"config file must contain a YAML/JSON object"}
	}

	checkKnownKeys(top, topLevelKeys, "config file", &errors)
	versionV, hasVersion := top.Get("version")
	vi, vIsInt := omap.AsInt64(versionV)
	if !hasVersion || !vIsInt || vi != 1 {
		errors = append(errors, fmt.Sprintf("version must be 1, got %s", omap.Describe(versionV)))
	}
	if v, ok := top.Get("env"); ok && !omap.IsStringRecord(v) {
		errors = append(errors, "env must be a map of string to string")
	}
	if v, ok := top.Get("envFile"); ok && !omap.IsString(v) {
		errors = append(errors, "envFile must be a string")
	}
	if v, ok := top.Get("runtimeDirectory"); ok && !omap.IsString(v) {
		errors = append(errors, "runtimeDirectory must be a string")
	}
	if v, ok := top.Get("privateFileGuard"); ok && !omap.IsBool(v) {
		errors = append(errors, "privateFileGuard must be a boolean")
	}
	if v, ok := top.Get("groups"); ok {
		groupsObj := omap.AsObject(v)
		ok := groupsObj != nil
		if ok {
			groupsObj.Each(func(_ string, e any) {
				if !omap.IsStringArray(e) {
					ok = false
				}
			})
		}
		if !ok {
			errors = append(errors, "groups must be a map of string to string[]")
		}
	}
	sharedNotObject := false
	if sv, ok := top.Get("shared"); ok {
		entries := omap.AsObject(sv)
		if entries == nil {
			sharedNotObject = true
			errors = append(errors, "shared must be a map of service name to version (or { version })")
		} else {
			entries.Each(func(name string, value any) {
				path := "shared." + name
				if !isValidSharedName(name) {
					errors = append(errors, fmt.Sprintf("%s has an invalid service name (expected [a-zA-Z0-9][a-zA-Z0-9._-]*)", path))
				}
				switch v := value.(type) {
				case string:
				case *omap.OMap:
					v.Each(func(key string, _ any) {
						found := false
						for _, k := range sharedEntryKeys {
							if k == key {
								found = true
								break
							}
						}
						if !found {
							errors = append(errors, fmt.Sprintf("%s has unknown key %q (known: version, preparationCommand, attachArgs, urls)", path, key))
						}
					})
					verV, hasVer := v.Get("version")
					verS, verIsStr := omap.AsString(verV)
					if !hasVer || !verIsStr || verS == "" {
						errors = append(errors, fmt.Sprintf("%s.version must be a non-empty string", path))
					}
					if aa, ok := v.Get("attachArgs"); ok && !omap.IsStringArray(aa) {
						errors = append(errors, fmt.Sprintf("%s.attachArgs must be an array of strings", path))
					}
				default:
					errors = append(errors, fmt.Sprintf("%s must be a version string or an object with version", path))
				}
			})
		}
	}
	servicesNotObject := false
	var servicesRaw *omap.OMap
	if sv, ok := top.Get("services"); ok {
		servicesRaw = omap.AsObject(sv)
		if servicesRaw == nil {
			errors = append(errors, "services must be a map of service id to service definition")
			servicesNotObject = true
		}
	}
	var sharedRaw *omap.OMap
	if sv, ok := top.Get("shared"); ok {
		sharedRaw = omap.AsObject(sv)
	}
	if !servicesNotObject && !sharedNotObject &&
		(servicesRaw == nil || servicesRaw.Len() == 0) &&
		(sharedRaw == nil || sharedRaw.Len() == 0) {
		errors = append(errors, "services must be a non-empty map of service id to service definition")
	}
	if len(errors) > 0 {
		return nil, errors
	}

	runtimeDirectoryPath := paths.ResolveRuntimeDirectory(root, func() string {
		if v, ok := top.Get("runtimeDirectory"); ok {
			s, _ := omap.AsString(v)
			return s
		}
		return ""
	}())

	globalEnv := map[string]string{}
	if v, ok := top.Get("env"); ok {
		globalEnv = omap.StringRecord(v)
	}
	fileEnv := map[string]string{}
	if v, ok := top.Get("envFile"); ok {
		if ef, isStr := omap.AsString(v); isStr {
			p := ef
			if !filepath.IsAbs(p) {
				p = filepath.Join(root, p)
			}
			fileEnv = env.LoadEnvFile(p)
		}
	}
	baseEnv := fileEnv
	for k, v := range globalEnv {
		baseEnv[k] = v
	}

	var services []catalog.ServiceDefinition
	if servicesRaw != nil {
		servicesRaw.Each(func(id string, value any) {
			svcPath := "services." + id
			obj := omap.AsObject(value)
			if obj == nil {
				errors = append(errors, fmt.Sprintf("%s must be an object", svcPath))
				return
			}
			checkKnownKeys(obj, serviceKeys, svcPath, &errors)

			var kind *catalog.ServiceKind
			if kv, ok := obj.Get("kind"); ok {
				s, isStr := omap.AsString(kv)
				switch {
				case isStr && s == "application":
					k := catalog.KindApplication
					kind = &k
				case isStr && s == "infrastructure":
					k := catalog.KindInfrastructure
					kind = &k
				default:
					errors = append(errors, fmt.Sprintf("%s.kind must be one of %s", svcPath, strings.Join(serviceKinds, ", ")))
				}
			}
			var ownership *catalog.ServiceOwnership
			if ov, ok := obj.Get("ownership"); ok {
				s, isStr := omap.AsString(ov)
				switch {
				case isStr && s == "daemon":
					o := catalog.OwnershipDaemon
					ownership = &o
				case isStr && s == "external":
					o := catalog.OwnershipExternal
					ownership = &o
				default:
					errors = append(errors, fmt.Sprintf("%s.ownership must be one of %s", svcPath, strings.Join(ownerships, ", ")))
				}
			}
			if ev, ok := obj.Get("env"); ok && !omap.IsStringRecord(ev) {
				errors = append(errors, fmt.Sprintf("%s.env must be a map of string to string", svcPath))
			}
			var container *string
			if cv, ok := obj.Get("container"); ok {
				if s, isStr := omap.AsString(cv); isStr {
					container = &s
				} else {
					errors = append(errors, fmt.Sprintf("%s.container must be a string", svcPath))
				}
			}
			var artifact *catalog.ServiceArtifact
			if av, ok := obj.Get("artifact"); ok {
				artifact = readArtifact(av, svcPath+".artifact", &errors)
			}
			if artifact != nil {
				if ownership != nil && *ownership == catalog.OwnershipExternal {
					errors = append(errors, fmt.Sprintf("%s.artifact needs a daemon-owned service — external units never spawn, so never install", svcPath))
				}
				if _, ok := obj.Get("run"); !ok {
					errors = append(errors, fmt.Sprintf("%s.artifact needs a run command to install for", svcPath))
				}
			}

			var cwdV *string
			if cv, ok := obj.Get("cwd"); ok {
				if s, isStr := omap.AsString(cv); isStr {
					cwdV = &s
				}
			}
			cwd := resolveServiceCwd(cwdV, svcPath, &errors)
			var portsRaw any
			var portsPresent bool
			if pv, ok := obj.Get("ports"); ok {
				portsRaw, portsPresent = pv, true
			}
			ports := readPorts(portsRaw, portsPresent, svcPath+".ports", &errors)
			var primaryPort *uint16
			if len(ports) > 0 {
				p := ports[0].Port
				primaryPort = &p
			}
			var readinessV any
			if rv, ok := obj.Get("readiness"); ok {
				readinessV = rv
			}
			readiness := readReadiness(readinessV, svcPath+".readiness", primaryPort, &errors)
			var urlsRaw any
			var urlsPresent bool
			if uv, ok := obj.Get("urls"); ok {
				urlsRaw, urlsPresent = uv, true
			}
			urls := readURLs(urlsRaw, urlsPresent, svcPath+".urls", &errors)
			readinessTimeoutMs := readDurationMs(func() (any, bool) { return obj.Get("readinessTimeoutMs") }, svcPath+".readinessTimeoutMs", &errors)
			var prepCmd *catalog.PreparationCommand
			if pv, ok := obj.Get("preparationCommand"); ok {
				prepCmd = readPreparationCommand(pv, svcPath+".preparationCommand", &errors)
			}

			var profileRun *catalog.ServiceRunProfile
			runV, hasRun := obj.Get("run")
			if !hasRun {
				if readiness != nil {
					profileRun = &catalog.ServiceRunProfile{
						CommandStatus:      "unresolved",
						Readiness:          *readiness,
						ReadinessTimeoutMs: readinessTimeoutMs,
						PreparationCommand: prepCmd,
					}
				}
			} else {
				run := readCommandSpec(runV, svcPath+".run", &errors)
				var stop *catalog.CommandSpec
				if sv, ok := obj.Get("stop"); ok {
					if s := readCommandSpec(sv, svcPath+".stop", &errors); s != nil {
						stop = &s.spec
					}
				}
				if run != nil && readiness != nil && cwd != nil {
					serviceEnv := map[string]string{}
					if ev, ok := obj.Get("env"); ok {
						serviceEnv = omap.StringRecord(ev)
					}
					environment := map[string]string{}
					for k, v := range baseEnv {
						environment[k] = v
					}
					for k, v := range serviceEnv {
						environment[k] = v
					}
					var envMap map[string]string
					if len(environment) > 0 {
						envMap = environment
					}
					profileRun = &catalog.ServiceRunProfile{
						CommandStatus: "verified",
						Command: &catalog.ServiceCommand{
							Command:           run.spec,
							Cwd:               *cwd,
							Environment:       envMap,
							ContainerName:     container,
							DockerStopCommand: stop,
						},
						Readiness:          *readiness,
						ReadinessTimeoutMs: readinessTimeoutMs,
						PreparationCommand: prepCmd,
					}
				}
			}

			var profileBuild *catalog.ServiceBuildProfile
			if bv, ok := obj.Get("build"); ok {
				if buildObj := omap.AsObject(bv); buildObj != nil {
					build := readCommandSpec(bv, svcPath+".build", &errors)
					timeoutMs := readDurationMs(func() (any, bool) { return buildObj.Get("timeoutMs") }, svcPath+".build.timeoutMs", &errors)
					var serKey *string
					if sk, ok := buildObj.Get("serializationKey"); ok {
						if s, isStr := omap.AsString(sk); isStr {
							serKey = &s
						} else {
							errors = append(errors, fmt.Sprintf("%s.build.serializationKey must be a string", svcPath))
						}
					}
					if build != nil && cwd != nil {
						profileBuild = &catalog.ServiceBuildProfile{
							Command:          catalog.ServiceCommand{Command: build.spec, Cwd: *cwd},
							TimeoutMs:        timeoutMs,
							SerializationKey: serKey,
						}
					}
				} else {
					errors = append(errors, fmt.Sprintf("%s.build must be an object", svcPath))
				}
			}

			if profileRun == nil {
				return // a more specific error was already recorded
			}
			disabled := false
			if dv, ok := obj.Get("disabled"); ok {
				if b, isBool := omap.AsBool(dv); isBool {
					disabled = b
				} else {
					errors = append(errors, fmt.Sprintf("%s.disabled must be a boolean", svcPath))
				}
			}
			var restart *catalog.ServiceRestartPolicy
			if rv, ok := obj.Get("restart"); ok {
				restart = readRestartPolicy(rv, svcPath+".restart", &errors)
			}
			var label *string
			if lv, ok := obj.Get("label"); ok {
				if s, isStr := omap.AsString(lv); isStr {
					label = &s
				}
			}
			definition := catalog.ServiceDefinition{
				ID:        id,
				Label:     label,
				Kind:      kind,
				Ownership: ownership,
				Disabled:  disabled,
				Restart:   restart,
				Profiles: catalog.ServiceProfiles{
					Run:   *profileRun,
					Build: profileBuild,
				},
				Ports: ports,
				URLs:  urls,
			}
			if artifact != nil {
				renderServiceArtifact(root, runtimeDirectoryPath, &definition, artifact, svcPath, &errors)
				definition.Artifact = artifact
			}
			services = append(services, definition)
		})
	}

	// `shared:` entries expand into generated external services, in document
	// order after the project's own services.
	var sharedIDs []string
	if sharedRaw != nil {
		exe, err := os.Executable()
		if err != nil {
			exe = "hearth"
		}
		sharedRaw.Each(func(name string, value any) {
			var version string
			switch v := value.(type) {
			case string:
				version = v
			case *omap.OMap:
				if verV, ok := v.Get("version"); ok {
					version, _ = omap.AsString(verV)
				}
			default:
				return
			}
			if !isValidSharedName(name) || version == "" {
				return
			}
			sharedVars := map[string]string{
				"dataDir":     paths.ServiceDataDir(runtimeDirectoryPath, name),
				"serviceId":   name,
				"projectRoot": root,
			}
			var preparationCommand *catalog.PreparationCommand
			var attachArgs []string
			if o, isObj := value.(*omap.OMap); isObj {
				svcPath := "shared." + name
				if pv, ok := o.Get("preparationCommand"); ok {
					if prep := readPreparationCommand(pv, svcPath+".preparationCommand", &errors); prep != nil {
						prep.Command = render.RenderCommand(&prep.Command, sharedVars)
						if prep.Cwd != nil {
							s := render.Str(*prep.Cwd, sharedVars)
							prep.Cwd = &s
						}
						preparationCommand = prep
					}
				}
				if av, ok := o.Get("attachArgs"); ok {
					for _, a := range omap.StringArray(av) {
						attachArgs = append(attachArgs, render.Str(a, sharedVars))
					}
				}
			}
			var urls []catalog.ServiceURL
			if o, isObj := value.(*omap.OMap); isObj {
				if uv, ok := o.Get("urls"); ok {
					for _, entry := range readURLs(uv, true, "shared."+name+".urls", &errors) {
						entry.URL = render.Str(entry.URL, sharedVars)
						urls = append(urls, entry)
					}
				}
			}
			instance := shared.InstanceID(name, version)
			services = append(services, shared.ProjectServiceEntry(name, instance, exe, preparationCommand, attachArgs, urls))
			sharedIDs = append(sharedIDs, name)
		})
	}

	var declaredGroups []catalog.CatalogGroup
	if gv, ok := top.Get("groups"); ok {
		if gobj := omap.AsObject(gv); gobj != nil {
			gobj.Each(func(name string, members any) {
				declaredGroups = append(declaredGroups, catalog.CatalogGroup{Name: name, Members: omap.StringArray(members)})
			})
		}
	}
	declaredMap := map[string][]string{}
	for _, g := range declaredGroups {
		declaredMap[g.Name] = g.Members
	}
	serviceIDs := map[string]bool{}
	for _, s := range services {
		serviceIDs[s.ID] = true
	}
	groups := map[string][]string{}
	for _, g := range declaredGroups {
		var resolved []string
		expandGroupMembers(g.Name, g.Members, declaredMap, serviceIDs, []string{g.Name}, &resolved, &errors)
		groups[g.Name] = resolved
	}
	if all, ok := groups["all"]; ok {
		for _, id := range sharedIDs {
			found := false
			for _, m := range all {
				if m == id {
					found = true
					break
				}
			}
			if !found {
				groups["all"] = append(all, id)
			}
		}
	}
	disabledIDs := map[string]bool{}
	for _, s := range services {
		if s.Disabled {
			disabledIDs[s.ID] = true
		}
	}
	for name, members := range groups {
		var kept []string
		for _, id := range members {
			if !disabledIDs[id] {
				kept = append(kept, id)
			}
		}
		groups[name] = kept
	}
	if len(errors) > 0 {
		return nil, errors
	}

	var runtimeDir *string
	if v, ok := top.Get("runtimeDirectory"); ok {
		if s, isStr := omap.AsString(v); isStr {
			runtimeDir = &s
		}
	}
	var privateFileGuard *bool
	if v, ok := top.Get("privateFileGuard"); ok {
		if b, isBool := omap.AsBool(v); isBool {
			privateFileGuard = &b
		}
	}
	return &catalog.ServiceCatalog{
		StartFailurePolicy: catalog.StartFailureStopOnFirstFailureKeepStarted,
		Services:           services,
		Groups:             groups,
		GroupTree:          declaredGroups,
		RuntimeDirectory:   runtimeDir,
		PrivateFileGuard:   privateFileGuard,
	}, nil
}
