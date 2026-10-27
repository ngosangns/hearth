package shared

import "testing"

func TestParseSharedCommandAttach(t *testing.T) {
	parsed, ok := ParseSharedCommand("/usr/local/bin/hearth", []string{"shared", "attach", "redis@8.2.10", "--extra"})
	if !ok {
		t.Fatal("expected match")
	}
	if parsed.Subcommand != SharedAttach || parsed.InstanceID != "redis@8.2.10" || len(parsed.AttachArgs) != 1 || parsed.AttachArgs[0] != "--extra" {
		t.Fatalf("%+v", parsed)
	}
}

func TestParseSharedCommandDetachFlags(t *testing.T) {
	parsed, ok := ParseSharedCommand("hearth", []string{"shared", "detach", "postgres@16.4", DetachStopIfUnusedFlag, ConfirmYesFlag})
	if !ok {
		t.Fatal("expected match")
	}
	if !parsed.DetachStopIfUnused || !parsed.DetachYes {
		t.Fatalf("%+v", parsed)
	}
}

func TestParseSharedCommandRejectsNonHearth(t *testing.T) {
	if _, ok := ParseSharedCommand("node", []string{"shared", "probe", "x@1"}); ok {
		t.Fatal("expected no match")
	}
}
