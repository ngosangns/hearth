package doctor

import (
	"testing"

	"github.com/ngosangns/hearth/go/internal/catalog"
)

type fakeAdapter struct {
	commandOK bool
	pathOK    bool
	portOK    bool
}

func (f fakeAdapter) Command(command string, args []string) CommandResult {
	return CommandResult{OK: f.commandOK, Output: "output"}
}

func (f fakeAdapter) Path(path string) bool { return f.pathOK }
func (f fakeAdapter) Port(port uint16) bool { return f.portOK }
func (f fakeAdapter) Platform() string      { return "darwin" }

func emptyCatalog() *catalog.ServiceCatalog {
	return &catalog.ServiceCatalog{
		Groups:             map[string][]string{},
		StartFailurePolicy: catalog.StartFailureStopOnFirstFailureKeepStarted,
	}
}

func TestOKReportWhenEveryCheckPasses(t *testing.T) {
	adapter := fakeAdapter{commandOK: true, pathOK: true, portOK: true}
	checks := &DoctorChecks{
		Commands: []DoctorCommandCheck{{Name: "docker", Command: "docker"}},
		Paths:    []DoctorPathCheck{{Name: "bin", Path: "/usr/bin"}},
		Ports:    []DoctorPortCheck{{Name: "db", Port: 5432}},
	}
	report := RunDoctor(emptyCatalog(), checks, adapter)
	if !report.OK {
		t.Fatal("expected an ok report")
	}
	if len(report.Checks) != 4 { // platform + 3
		t.Fatalf("expected 4 checks, got %d", len(report.Checks))
	}
}

func TestNotOKWhenACheckFails(t *testing.T) {
	adapter := fakeAdapter{commandOK: false, pathOK: true, portOK: true}
	checks := &DoctorChecks{
		Commands: []DoctorCommandCheck{{Name: "docker", Command: "docker"}},
	}
	report := RunDoctor(emptyCatalog(), checks, adapter)
	if report.OK {
		t.Fatal("expected a failing report")
	}
}

func TestReportsUnresolvedProfiles(t *testing.T) {
	adapter := fakeAdapter{commandOK: true, pathOK: true, portOK: true}
	cat := &catalog.ServiceCatalog{
		Services: []catalog.ServiceDefinition{{
			ID: "wip",
			Profiles: catalog.ServiceProfiles{
				Run: catalog.ServiceRunProfile{CommandStatus: "unresolved"},
			},
		}},
		Groups:             map[string][]string{},
		StartFailurePolicy: catalog.StartFailureStopOnFirstFailureKeepStarted,
	}
	report := RunDoctor(cat, &DoctorChecks{}, adapter)
	if len(report.UnresolvedProfiles) != 1 || report.UnresolvedProfiles[0] != "wip:run command is unresolved" {
		t.Fatalf("unexpected unresolved profiles: %v", report.UnresolvedProfiles)
	}
}

func TestDefaultDoctorChecksCoverTheHostTools(t *testing.T) {
	checks := DefaultDoctorChecks()
	want := []string{"docker", "tailscale", "nc", "ps", "sh"}
	if len(checks.Commands) != len(want) {
		t.Fatalf("expected %d checks, got %d", len(want), len(checks.Commands))
	}
	for i, name := range want {
		check := checks.Commands[i]
		if check.Name != name || check.Command != "sh" {
			t.Fatalf("check %d: got %+v", i, check)
		}
		if len(check.Args) != 2 || check.Args[0] != "-c" || check.Args[1] != "command -v "+name {
			t.Fatalf("check %d args: %v", i, check.Args)
		}
	}
}
