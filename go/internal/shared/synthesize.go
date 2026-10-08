// Builds the smp daemon's in-memory ServiceCatalog from the instance
// registry — the daemon's catalog is derived state, not a yaml file.
package shared

import (
	"fmt"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/render"
)

// First boot of a JVM or WiredTiger can outlast the supervisor's 10s default.
// Install itself happens before `start`, so this bound is only the process
// becoming ready.
const SharedInstanceReadinessTimeoutMs uint64 = 120_000

// SharedReadinessTimeoutMs is the project-side `hearth shared probe` budget.
// Must out-wait a script artifact pack (45 min) plus extract (120s) plus the
// instance readiness budget.
const SharedReadinessTimeoutMs uint64 = 45*60*1000 + 120_000 + SharedInstanceReadinessTimeoutMs

// DetachStopIfUnusedFlag — the flag `manager stop` appends to a `shared:`
// service's `hearth shared detach` stop command, so the smp instance is
// stopped too once no project is attached to it.
const DetachStopIfUnusedFlag = "--stop-if-unused"

// InstanceVars — available to every field of a recipe. Extra listeners are
// `{port2}`, `{port3}`, … in allocation order.
func InstanceVars(instance *SharedInstance, root string) map[string]string {
	vars := map[string]string{
		"installDir": instance.InstallDir(root),
		"dataDir":    instance.DataDir(root),
		"port":       fmt.Sprintf("%d", instance.Port),
		"name":       instance.Name,
		"version":    instance.Version,
		"instanceId": instance.ID(),
	}
	for i, port := range instance.ExtraPorts {
		vars[fmt.Sprintf("port%d", i+2)] = fmt.Sprintf("%d", port)
	}
	return vars
}

// ProjectVars adds the per-project vars used by provision/deprovision/
// connection.
func ProjectVars(vars map[string]string, projectID string) {
	vars["projectId"] = projectID
	vars["projectDb"] = ProjectDbName(projectID)
	vars["projectUser"] = ProjectUserName(projectID)
	vars["projectBucket"] = ProjectBucketName(projectID)
}

// Instances in installing/failed state still synthesize a definition: a failed
// install is retried by the next attach, and the service must exist in the
// catalog for the supervisor to start it once install succeeds.
func SynthesizeCatalog(root string, instances []SharedInstance) (*catalog.ServiceCatalog, error) {
	var services []catalog.ServiceDefinition
	for i := range instances {
		svc, err := SynthesizeService(root, &instances[i])
		if err != nil {
			return nil, err
		}
		services = append(services, *svc)
	}
	all := make([]string, len(instances))
	for i := range instances {
		all[i] = instances[i].ID()
	}
	runtimeDir := SharedRuntimeDirectoryName
	return &catalog.ServiceCatalog{
		Services:           services,
		Groups:             map[string][]string{"all": all},
		RuntimeDirectory:   &runtimeDir,
		StartFailurePolicy: catalog.StartFailureStopOnFirstFailureKeepStarted,
	}, nil
}

func SynthesizeService(root string, instance *SharedInstance) (*catalog.ServiceDefinition, error) {
	vars := InstanceVars(instance, root)
	resolved := instance.Recipe.Readiness.Resolve(instance.Port)
	readiness, err := render.RenderReadiness(&resolved, vars)
	if err != nil {
		return nil, &SharedError{err.Error()}
	}
	var environment map[string]string
	if instance.Recipe.Env != nil {
		for _, value := range instance.Recipe.Env {
			if err := render.CheckVars(value, vars, "env"); err != nil {
				return nil, &SharedError{err.Error()}
			}
		}
		if e := render.Env(instance.Recipe.Env, vars); len(e) > 0 {
			environment = e
		}
	}
	var preparationCommand *catalog.PreparationCommand
	if instance.Recipe.Prepare != nil {
		cmd, err := render.RenderCommandChecked(instance.Recipe.Prepare, vars, "prepare")
		if err != nil {
			return nil, &SharedError{err.Error()}
		}
		preparationCommand = &catalog.PreparationCommand{Command: cmd}
	}
	var dockerStop *catalog.CommandSpec
	if instance.Recipe.Stop != nil {
		spec, err := render.RenderCommandChecked(instance.Recipe.Stop, vars, "stop")
		if err != nil {
			return nil, &SharedError{err.Error()}
		}
		dockerStop = &spec
	}
	runCommand, err := render.RenderCommandChecked(&instance.Recipe.Run, vars, "run")
	if err != nil {
		return nil, &SharedError{err.Error()}
	}
	ports := []catalog.ServicePort{{Port: instance.Port, Label: "shared"}}
	for i, port := range instance.ExtraPorts {
		label := fmt.Sprintf("port%d", i+2)
		if i < len(instance.Recipe.ExtraPortLabels) && instance.Recipe.ExtraPortLabels[i] != "" {
			label = instance.Recipe.ExtraPortLabels[i]
		}
		ports = append(ports, catalog.ServicePort{Port: port, Label: label})
	}
	var readinessTimeout *uint64
	if readiness.Kind != "exit" {
		t := SharedInstanceReadinessTimeoutMs
		readinessTimeout = &t
	}
	kind := catalog.KindInfrastructure
	ownership := catalog.OwnershipDaemon
	label := fmt.Sprintf("%s@%s (shared)", instance.Name, instance.Version)
	return &catalog.ServiceDefinition{
		ID:        instance.ID(),
		Label:     &label,
		Kind:      &kind,
		Ownership: &ownership,
		Profiles: catalog.ServiceProfiles{
			Run: catalog.ServiceRunProfile{
				CommandStatus: "verified",
				Command: &catalog.ServiceCommand{
					Command:           runCommand,
					Cwd:               instance.DataDir(root), // recipe binaries write state under {dataDir}
					Environment:       environment,
					DockerStopCommand: dockerStop,
				},
				Readiness:          readiness,
				ReadinessTimeoutMs: readinessTimeout,
				PreparationCommand: preparationCommand,
			},
		},
		Ports: ports,
	}, nil
}

// ShutdownStopCommand is the stop command a `stop-services` shutdown
// (`hearth manager stop`) runs for an external service: the catalog's own,
// plus DetachStopIfUnusedFlag when it is the `hearth shared detach <id>` of a
// `shared:` entry.
func ShutdownStopCommand(command *catalog.CommandSpec) catalog.CommandSpec {
	if command.IsArgv() {
		hasDetach := false
		for i := 0; i+1 < len(command.Argv); i++ {
			if command.Argv[i] == "shared" && command.Argv[i+1] == "detach" {
				hasDetach = true
				break
			}
		}
		hasFlag := false
		for _, a := range command.Argv {
			if a == DetachStopIfUnusedFlag {
				hasFlag = true
				break
			}
		}
		if hasDetach && !hasFlag {
			argv := append(append([]string{}, command.Argv...), DetachStopIfUnusedFlag)
			return catalog.CommandSpec{Argv: argv}
		}
	}
	return *command
}

// ProjectServiceEntry is the project-side service a `shared:` yaml entry
// expands to. `ownership: external` + `command` readiness makes its run
// command a one-shot task (`hearth shared attach`) and its probe
// (`hearth shared probe`, exit 0 iff the instance is ready AND this project
// is attached) the adoption signal syncExternalServices polls. `stop` is
// `hearth shared detach` — released via the probe going false, never by
// killing the singleton.
func ProjectServiceEntry(id, instance, exe string, preparation *catalog.PreparationCommand, attachArgs []string, urls []catalog.ServiceURL) catalog.ServiceDefinition {
	attachArgv := append([]string{exe, "shared", "attach", instance}, attachArgs...)
	label := fmt.Sprintf("%s (shared)", instance)
	kind := catalog.KindInfrastructure
	ownership := catalog.OwnershipExternal
	timeout := SharedReadinessTimeoutMs
	return catalog.ServiceDefinition{
		Label:     &label,
		ID:        id,
		Kind:      &kind,
		Ownership: &ownership,
		Profiles: catalog.ServiceProfiles{
			Run: catalog.ServiceRunProfile{
				CommandStatus: "verified",
				Command: &catalog.ServiceCommand{
					Command: catalog.CommandSpec{Argv: attachArgv},
					Cwd:     ".",
					DockerStopCommand: &catalog.CommandSpec{
						Argv: []string{exe, "shared", "detach", instance},
					},
				},
				Readiness: catalog.ReadinessSpec{
					Kind:    "command",
					Command: &catalog.CommandSpec{Argv: []string{exe, "shared", "probe", instance}},
				},
				ReadinessTimeoutMs: &timeout,
				PreparationCommand: preparation,
			},
		},
		URLs: urls,
	}
}
