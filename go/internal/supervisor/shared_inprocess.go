package supervisor

import (
	"context"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/shared"
)

// SharedInProcessRunner executes hearth shared attach/detach/probe via smp HTTP.
type SharedInProcessRunner interface {
	RunAttach(ctx context.Context, instanceID string, attachArgs []string, onLine func(string)) (int, error)
	RunDetach(ctx context.Context, instanceID string, stopIfUnused, yes bool) (int, error)
	ProbeReady(ctx context.Context, instanceID string) bool
}

// WithSharedInProcess wraps process and probe adapters so generated `shared:`
// services use runner instead of re-execing hearth.
func WithSharedInProcess(opts SupervisorOptions, runner SharedInProcessRunner) SupervisorOptions {
	if runner == nil {
		return opts
	}
	if proc, ok := opts.Process.(*DefaultProcessAdapter); ok {
		opts.Process = &sharedProcessAdapter{DefaultProcessAdapter: proc, runner: runner}
	}
	if probe, ok := opts.Probes.(*DefaultProbeAdapter); ok {
		opts.Probes = &sharedProbeAdapter{inner: probe, runner: runner}
	}
	return opts
}

type sharedProcessAdapter struct {
	*DefaultProcessAdapter
	runner SharedInProcessRunner
}

func (a *sharedProcessAdapter) Spawn(input SpawnInput, onOutput OnOutput) (*ManagedProcess, error) {
	argv, _ := CommandArgv(&input.Command.Command)
	if len(argv) >= 3 {
		parsed, ok := shared.ParseSharedCommand(argv[0], argv[1:])
		if ok {
			switch parsed.Subcommand {
			case shared.SharedAttach:
				exit, err := a.runner.RunAttach(context.Background(), parsed.InstanceID, parsed.AttachArgs, onOutput)
				if err != nil {
					if exit == 0 {
						exit = 1
					}
					return nil, Errorf("shared attach failed: %v", err)
				}
				if exit != 0 {
					return nil, Errorf("shared attach exited with %d", exit)
				}
				return a.spawnSettledTask(input)
			case shared.SharedDetach:
				exit, err := a.runner.RunDetach(context.Background(), parsed.InstanceID, parsed.DetachStopIfUnused, parsed.DetachYes)
				if err != nil {
					return nil, Errorf("shared detach failed: %v", err)
				}
				if exit != 0 {
					return nil, Errorf("shared detach exited with %d", exit)
				}
				return a.spawnSettledTask(input)
			}
		}
	}
	return a.DefaultProcessAdapter.Spawn(input, onOutput)
}

func (a *sharedProcessAdapter) spawnSettledTask(input SpawnInput) (*ManagedProcess, error) {
	settled := input
	settled.Command.Command = catalog.CommandSpec{Argv: []string{"/usr/bin/true"}}
	return a.DefaultProcessAdapter.Spawn(settled, nil)
}

type sharedProbeAdapter struct {
	inner  *DefaultProbeAdapter
	runner SharedInProcessRunner
}

func (a *sharedProbeAdapter) TCP(port uint16) bool                  { return a.inner.TCP(port) }
func (a *sharedProbeAdapter) HTTP(url string) bool                  { return a.inner.HTTP(url) }
func (a *sharedProbeAdapter) Container(name string) bool            { return a.inner.Container(name) }
func (a *sharedProbeAdapter) Tailnet() bool                         { return a.inner.Tailnet() }
func (a *sharedProbeAdapter) PortInUse(port uint16) *bool           { return a.inner.PortInUse(port) }
func (a *sharedProbeAdapter) PortHolders(port uint16) *[]PortHolder { return a.inner.PortHolders(port) }

func (a *sharedProbeAdapter) Command(ctx CommandContext, command *catalog.CommandSpec, cwd *string) *bool {
	argv, _ := CommandArgv(command)
	if len(argv) >= 3 {
		if parsed, ok := shared.ParseSharedCommand(argv[0], argv[1:]); ok && parsed.Subcommand == shared.SharedProbe {
			ready := a.runner.ProbeReady(context.Background(), parsed.InstanceID)
			return &ready
		}
	}
	return a.inner.Command(ctx, command, cwd)
}
