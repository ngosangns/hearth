// Package catalog ports the public config/catalog API: the types a
// `hearth.yaml`/`.json` config file parses into, plus validation and URL
// placeholder resolution. Field names match the Rust serde output.
package catalog

import (
	json "encoding/json"
	"fmt"
	"sort"
	"strings"
)

// CommandSpec is the serde-untagged union: exactly one of Argv/Shell is set.
// Argv is injection-safe (spawned directly, no shell); Shell supports chaining
// — set Exec when the shell command execs into the long-running process.
type CommandSpec struct {
	Argv  []string `json:"argv,omitempty"`
	Shell string   `json:"shell,omitempty"`
	Exec  *bool    `json:"exec,omitempty"`
}

func (c *CommandSpec) IsArgv() bool { return c != nil && c.Argv != nil }
func (c *CommandSpec) IsShell() bool { return c != nil && c.Shell != "" }

// MarshalJSON emits the untagged shape: `{"argv":[...]}` or
// `{"shell":"...","exec":true}`.
func (c CommandSpec) MarshalJSON() ([]byte, error) {
	if c.IsArgv() {
		return marshalOrdered([]kv{{"argv", c.Argv}}), nil
	}
	fields := []kv{{"shell", c.Shell}}
	if c.Exec != nil {
		fields = append(fields, kv{"exec", *c.Exec})
	}
	return marshalOrdered(fields), nil
}

// UnmarshalJSON accepts either `{"argv":[...]}` or `{"shell":"...","exec"?}`.
func (c *CommandSpec) UnmarshalJSON(data []byte) error {
	var raw map[string]json.RawMessage
	if err := json.Unmarshal(data, &raw); err != nil {
		return err
	}
	if v, ok := raw["argv"]; ok {
		var argv []string
		if err := json.Unmarshal(v, &argv); err != nil {
			return err
		}
		c.Argv = argv
		c.Shell = ""
		c.Exec = nil
		return nil
	}
	if v, ok := raw["shell"]; ok {
		var shell string
		if err := json.Unmarshal(v, &shell); err != nil {
			return err
		}
		c.Shell = shell
		c.Argv = nil
		if v, ok := raw["exec"]; ok {
			var exec bool
			if err := json.Unmarshal(v, &exec); err != nil {
				return err
			}
			c.Exec = &exec
		}
		return nil
	}
	return fmt.Errorf("command must have argv or shell")
}

type ServiceCommand struct {
	Command           CommandSpec         `json:"command"`
	Cwd               string              `json:"cwd"`
	Environment       map[string]string   `json:"environment,omitempty"`
	ContainerName     *string             `json:"containerName,omitempty"`
	DockerStopCommand *CommandSpec        `json:"dockerStopCommand,omitempty"`
}

// ReadinessSpec is `{"kind":"..."}`-tagged. Exactly one kind's fields are set.
type ReadinessSpec struct {
	Kind string `json:"kind"`
	// tcp
	Port *uint16 `json:"port,omitempty"`
	// http
	URL string `json:"url,omitempty"`
	// command
	Command *CommandSpec `json:"command,omitempty"`
	Cwd     *string      `json:"cwd,omitempty"`
	// log
	Pattern string `json:"pattern,omitempty"`
}

func (r *ReadinessSpec) IsExit() bool    { return r.Kind == "exit" }
func (r *ReadinessSpec) IsProcess() bool { return r.Kind == "process" }

// RestartTrigger is kebab-case on the wire.
type RestartTrigger string

const (
	RestartNever     RestartTrigger = "never"
	RestartOnFailure RestartTrigger = "on-failure"
	RestartAlways    RestartTrigger = "always"
)

type ServiceRestartPolicy struct {
	On          RestartTrigger `json:"on"`
	MaxRestarts *uint32        `json:"maxRestarts,omitempty"`
	DelayMs     *uint64        `json:"delayMs,omitempty"`
}

type ServiceOwnership string

const (
	OwnershipDaemon   ServiceOwnership = "daemon"
	OwnershipExternal ServiceOwnership = "external"
)

type ServiceKind string

const (
	KindApplication    ServiceKind = "application"
	KindInfrastructure ServiceKind = "infrastructure"
)

type PreparationCommand struct {
	Command          CommandSpec `json:"command"`
	Cwd              *string     `json:"cwd,omitempty"`
	SerializationKey *string     `json:"serializationKey,omitempty"`
}

// ServiceRunProfile is `{"commandStatus":"verified"|"unresolved"}`-tagged.
type ServiceRunProfile struct {
	CommandStatus       string              `json:"commandStatus"`
	Command             *ServiceCommand     `json:"command,omitempty"`
	Readiness           ReadinessSpec       `json:"readiness"`
	ReadinessTimeoutMs  *uint64             `json:"readinessTimeoutMs,omitempty"`
	Preparation         []string            `json:"preparation,omitempty"`
	PreparationCommand  *PreparationCommand `json:"preparationCommand,omitempty"`
}

func (p *ServiceRunProfile) IsVerified() bool { return p.CommandStatus == "verified" }

type ServiceBuildProfile struct {
	Command          ServiceCommand `json:"command"`
	TimeoutMs        *uint64        `json:"timeoutMs,omitempty"`
	SerializationKey *string        `json:"serializationKey,omitempty"`
}

type ServiceArtifact struct {
	Version    string   `json:"version"`
	URL        *string  `json:"url,omitempty"`
	Script     *string  `json:"script,omitempty"`
	ScriptArgs []string `json:"scriptArgs,omitempty"`
	Sha256     *string  `json:"sha256,omitempty"`
	InstallDir *string  `json:"installDir,omitempty"`
	DataDir    *string  `json:"dataDir,omitempty"`
}

type ServicePort struct {
	Port            uint16 `json:"port"`
	Label           string `json:"label"`
	RequiresRunning *bool  `json:"requiresRunning,omitempty"`
}

type ServiceURL struct {
	URL             string  `json:"url"`
	Label           *string `json:"label,omitempty"`
	RequiresRunning *bool   `json:"requiresRunning,omitempty"`
}

var ServiceURLPlaceholders = []string{"tailnetHost"}

// BraceSpan is one innermost `{…}` pair: Start is the byte offset of '{',
// Name the body.
type BraceSpan struct {
	Start int
	Name  string
}

// braceSpans returns every innermost `{…}` pair in text, in order of
// appearance — the one brace tokenizer behind URL placeholders and command
// template vars.
func braceSpans(text string) []BraceSpan {
	var spans []BraceSpan
	offset := 0
	for offset < len(text) {
		open := strings.Index(text[offset:], "{")
		if open < 0 {
			break
		}
		start := offset + open
		after := text[start+1:]
		closeIdx := strings.IndexAny(after, "{}")
		if closeIdx < 0 {
			break
		}
		if after[closeIdx] == '}' {
			spans = append(spans, BraceSpan{start, after[:closeIdx]})
			offset = start + 1 + closeIdx + 1
		} else {
			offset = start + 1 + closeIdx
		}
	}
	return spans
}

// ServiceURLPlaceholdersIn returns every `{placeholder}` name in a URL, in
// order of appearance.
func ServiceURLPlaceholdersIn(url string) []string {
	var names []string
	for _, s := range braceSpans(url) {
		names = append(names, s.Name)
	}
	return names
}

// TemplateVarSpans returns every `{name}` var in text as (offset, name). Only
// identifier-shaped bodies count, and `${NAME}` shell expansions never do.
func TemplateVarSpans(text string) []BraceSpan {
	var out []BraceSpan
	for _, s := range braceSpans(text) {
		if s.Start > 0 && text[s.Start-1] == '$' {
			continue
		}
		name := s.Name
		if name == "" {
			continue
		}
		c := name[0]
		if !(c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c == '_') {
			continue
		}
		ok := true
		for i := 1; i < len(name); i++ {
			c := name[i]
			if !(c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' || c == '_' || c == '.') {
				ok = false
				break
			}
		}
		if ok {
			out = append(out, s)
		}
	}
	return out
}

// TemplateVars returns every `{name}` template var in text.
func TemplateVars(text string) []string {
	var names []string
	for _, s := range TemplateVarSpans(text) {
		names = append(names, s.Name)
	}
	return names
}

// UnknownTemplateVar returns the first var in text that isKnown rejects.
func UnknownTemplateVar(text string, isKnown func(string) bool) *string {
	for _, s := range TemplateVarSpans(text) {
		if !isKnown(s.Name) {
			name := s.Name
			return &name
		}
	}
	return nil
}

type ResolvedServiceURL struct {
	ServiceID       string  `json:"serviceId"`
	Label           *string `json:"label,omitempty"`
	URL             string  `json:"url"`
	RequiresRunning bool    `json:"requiresRunning"`
}

type UnresolvedServiceURL struct {
	ServiceID   string `json:"serviceId"`
	URL         string `json:"url"`
	Placeholder string `json:"placeholder"`
}

// ResolveServiceURLs substitutes every placeholder in every service URL, in
// catalog order. lookup answers one placeholder name.
func ResolveServiceURLs(catalog *ServiceCatalog, lookup func(string) *string) ([]ResolvedServiceURL, []UnresolvedServiceURL) {
	var resolved []ResolvedServiceURL
	var unresolved []UnresolvedServiceURL
	for _, service := range catalog.Services {
		for _, entry := range service.URLs {
			url := entry.URL
			skip := false
			for _, name := range ServiceURLPlaceholdersIn(entry.URL) {
				value := lookup(name)
				if value == nil {
					unresolved = append(unresolved, UnresolvedServiceURL{
						ServiceID:   service.ID,
						URL:         entry.URL,
						Placeholder: name,
					})
					skip = true
					break
				}
				url = strings.ReplaceAll(url, "{"+name+"}", *value)
			}
			if skip {
				continue
			}
			requiresRunning := true
			if entry.RequiresRunning != nil {
				requiresRunning = *entry.RequiresRunning
			}
			resolved = append(resolved, ResolvedServiceURL{
				ServiceID:       service.ID,
				Label:           entry.Label,
				URL:             url,
				RequiresRunning: requiresRunning,
			})
		}
	}
	return resolved, unresolved
}

type ServiceProfiles struct {
	Run   ServiceRunProfile    `json:"run"`
	Build *ServiceBuildProfile `json:"build,omitempty"`
}

type ServiceDefinition struct {
	ID        string            `json:"id"`
	Label     *string           `json:"label,omitempty"`
	Kind      *ServiceKind      `json:"kind,omitempty"`
	Ownership *ServiceOwnership `json:"ownership,omitempty"`
	Disabled  bool              `json:"disabled"`
	Restart   *ServiceRestartPolicy `json:"restart,omitempty"`
	Profiles  ServiceProfiles   `json:"profiles"`
	Ports     []ServicePort     `json:"ports,omitempty"`
	URLs      []ServiceURL      `json:"urls,omitempty"`
	Artifact  *ServiceArtifact  `json:"artifact,omitempty"`
}

type CatalogGroup struct {
	Name    string   `json:"name"`
	Members []string `json:"members"`
}

type StartFailurePolicy string

const (
	StartFailureStopOnFirstFailureKeepStarted StartFailurePolicy = "stop-on-first-failure-keep-started"
)

type ServiceCatalog struct {
	Services           []ServiceDefinition     `json:"services"`
	Groups             map[string][]string     `json:"groups"`
	GroupTree          []CatalogGroup          `json:"groupTree,omitempty"`
	ComposeFile        *string                 `json:"composeFile,omitempty"`
	RuntimeDirectory   *string                 `json:"runtimeDirectory,omitempty"`
	StartFailurePolicy StartFailurePolicy      `json:"startFailurePolicy"`
	PrivateFileGuard   *bool                   `json:"privateFileGuard,omitempty"`
}

type CatalogValidation struct {
	Errors   []string
	Warnings []string
}

func ValidateCatalog(catalog *ServiceCatalog) CatalogValidation {
	var errors, warnings []string
	services := map[string]*ServiceDefinition{}
	for i := range catalog.Services {
		s := &catalog.Services[i]
		if _, dup := services[s.ID]; dup {
			errors = append(errors, "duplicate service "+s.ID)
		}
		services[s.ID] = s
	}

	groupNames := make([]string, 0, len(catalog.Groups))
	for name := range catalog.Groups {
		groupNames = append(groupNames, name)
	}
	sort.Strings(groupNames)
	for _, name := range groupNames {
		for _, member := range catalog.Groups[name] {
			if _, ok := services[member]; !ok {
				errors = append(errors, fmt.Sprintf("group %s references unknown service %s", name, member))
			}
		}
	}

	verifiedPorts := map[uint16]string{}
	for i := range catalog.Services {
		service := &catalog.Services[i]
		profile := &service.Profiles.Run
		if !profile.IsVerified() {
			warnings = append(warnings, service.ID+":run command is unresolved")
		}
		if build := service.Profiles.Build; build != nil {
			if build.TimeoutMs != nil && *build.TimeoutMs == 0 {
				errors = append(errors, service.ID+":build has an invalid timeout")
			}
		}
		for index, entry := range service.URLs {
			place := fmt.Sprintf("%s:urls[%d]", service.ID, index)
			wellFormed := (strings.HasPrefix(entry.URL, "http://") || strings.HasPrefix(entry.URL, "https://")) &&
				len(entry.URL) > strings.Index(entry.URL, "://")+3 &&
				!strings.ContainsAny(entry.URL, " \t\n\r")
			if !wellFormed {
				errors = append(errors, place+" must be an http:// or https:// URL")
				continue
			}
			for _, name := range ServiceURLPlaceholdersIn(entry.URL) {
				known := false
				for _, p := range ServiceURLPlaceholders {
					if p == name {
						known = true
						break
					}
				}
				if !known {
					var names []string
					for _, p := range ServiceURLPlaceholders {
						names = append(names, "{"+p+"}")
					}
					errors = append(errors, fmt.Sprintf("%s has unknown placeholder {%s} (known: %s)", place, name, strings.Join(names, ", ")))
				}
			}
			if entry.Label != nil && strings.TrimSpace(*entry.Label) == "" {
				errors = append(errors, place+".label must be a non-empty string")
			}
		}
		if profile.Readiness.IsExit() {
			if service.Ownership != nil && *service.Ownership == OwnershipExternal {
				errors = append(errors, service.ID+": readiness exit requires a daemon-owned service")
			}
			if profile.Command != nil && IsContainerCommand(profile.Command) {
				errors = append(errors, service.ID+": readiness exit cannot run a container command")
			}
		}
		if profile.Readiness.Kind == "tcp" && profile.Readiness.Port != nil {
			port := *profile.Readiness.Port
			if existing, ok := verifiedPorts[port]; ok && existing != service.ID {
				errors = append(errors, fmt.Sprintf("port %d is shared by %s and %s", port, existing, service.ID))
			}
			verifiedPorts[port] = service.ID
		}
	}
	return CatalogValidation{Errors: errors, Warnings: warnings}
}

func IsContainerCommand(command *ServiceCommand) bool {
	return command != nil && command.ContainerName != nil
}
