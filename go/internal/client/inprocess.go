package client

import (
	"context"
	"encoding/json"
	"fmt"
)

// InProcessRunner executes hearth shared attach/detach/probe via smp HTTP
// instead of re-execing the binary. SpawnSMP starts the smp daemon when absent.
type InProcessRunner struct {
	ProjectRoot string
	SpawnSMP    func(root string)
}

func (r *InProcessRunner) ensureClient(ctx context.Context) (*Client, error) {
	if r.SpawnSMP == nil {
		return nil, &Error{Kind: KindUnavailable, Message: "shared in-process runner has no SpawnSMP hook", ExitCode: ExitUnavailable}
	}
	return EnsureSMP(ctx, r.SpawnSMP)
}

// RunAttach calls POST /v1/shared/attach. onLine receives connection JSON for logs.
func (r *InProcessRunner) RunAttach(ctx context.Context, instanceID string, attachArgs []string, onLine func(string)) (int, error) {
	c, err := r.ensureClient(ctx)
	if err != nil {
		return 1, err
	}
	result, err := SharedAttach(ctx, c, instanceID, r.ProjectRoot, attachArgs)
	if err != nil {
		return 1, err
	}
	if onLine != nil {
		text, _ := json.Marshal(result)
		onLine(string(text) + "\n")
	}
	return 0, nil
}

// RunDetach calls POST /v1/shared/detach.
func (r *InProcessRunner) RunDetach(ctx context.Context, instanceID string, stopIfUnused, yes bool) (int, error) {
	if stopIfUnused && !yes {
		return 6, &Error{Kind: KindUsage, Message: "detach --stop-if-unused requires --yes off a TTY", ExitCode: 6}
	}
	c, err := r.ensureClient(ctx)
	if err != nil {
		return 1, err
	}
	_, err = SharedDetach(ctx, c, instanceID, r.ProjectRoot, stopIfUnused)
	if err != nil {
		return 1, err
	}
	return 0, nil
}

// ProbeReady is true when GET /v1/shared/probe reports ready for this project.
func (r *InProcessRunner) ProbeReady(ctx context.Context, instanceID string) bool {
	c, err := r.ensureClient(ctx)
	if err != nil {
		return false
	}
	return SharedProbe(ctx, c, instanceID, r.ProjectRoot)
}

// FormatAttachLogLine is a short human line for service logs.
func FormatAttachLogLine(result map[string]any) string {
	if result == nil {
		return ""
	}
	service, _ := result["service"].(string)
	port, _ := result["port"].(float64)
	return fmt.Sprintf("attached %s (port %.0f)\n", service, port)
}
