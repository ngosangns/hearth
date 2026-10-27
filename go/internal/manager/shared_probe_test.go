package manager

import (
	"testing"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/fileio"
	"github.com/ngosangns/hearth/go/internal/shared"
	"github.com/ngosangns/hearth/go/internal/state"
)

func TestEvaluateSharedProbeRequiresReadyAndProvisionedAttachment(t *testing.T) {
	ctx := &shared.Context{
		Registry: mustRegistry(t, map[string]shared.SharedInstance{
			"redis@1": {
				Name: "redis", Version: "1", Port: 43100,
				Attachments: map[string]shared.SharedAttachment{
					"abc123": {Provisioned: true},
				},
			},
		}),
	}
	cat := catalog.ServiceCatalog{
		Services: []catalog.ServiceDefinition{{ID: "redis@1"}},
	}
	m := &HearthManager{
		catalog: &cat,
		state: &state.PersistedManagerState{
			Services: map[string]*state.ServiceLifecycleState{
				"redis@1": {ServiceID: "redis@1", ActualState: state.ActualReady},
			},
		},
	}
	if !evaluateSharedProbe(m, ctx, "redis@1", "abc123") {
		t.Fatal("expected ready")
	}
	m.state.Services["redis@1"].ActualState = state.ActualRunning
	if evaluateSharedProbe(m, ctx, "redis@1", "abc123") {
		t.Fatal("expected not ready when service not ready")
	}
}

func mustRegistry(t *testing.T, instances map[string]shared.SharedInstance) *shared.SharedRegistry {
	t.Helper()
	reg, err := shared.LoadRegistry(fileio.New(false), t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	if err := reg.Update(func(d *shared.SharedRegistryData) error {
		d.Instances = instances
		return nil
	}); err != nil {
		t.Fatal(err)
	}
	return reg
}
