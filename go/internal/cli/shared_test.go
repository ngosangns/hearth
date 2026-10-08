package cli

import "testing"

func TestAttachArgsAfterTheIDPassThroughVerbatimEvenWhenTheyLookLikeFlags(t *testing.T) {
	argv := []string{"--json", "attach", "nginx@1.27", "--conf-dir", "/tmp/conf"}
	parsed, passthrough := splitPassthrough(argv)
	if len(parsed) != 3 || parsed[0] != argv[0] || parsed[1] != argv[1] || parsed[2] != argv[2] {
		t.Fatalf("unexpected parsed: %v", parsed)
	}
	if len(passthrough) != 2 || passthrough[0] != argv[3] || passthrough[1] != argv[4] {
		t.Fatalf("unexpected passthrough: %v", passthrough)
	}
	flags, err := ParseCommandFlags(parsed, []Flag{FlagJSON})
	if err != nil || !flags.JSON {
		t.Fatalf("unexpected flags: %+v (%v)", flags, err)
	}
}

// `manager stop` runs the synthesized `shared detach <id>` with
// `--stop-if-unused` appended after the id; it must parse as this command's
// flag, not pass through.
func TestDetachParsesStopIfUnusedAfterTheID(t *testing.T) {
	argv := []string{"detach", "postgres@16.4", "--stop-if-unused"}
	parsed, passthrough := splitPassthrough(argv)
	if len(passthrough) != 0 {
		t.Fatalf("unexpected passthrough: %v", passthrough)
	}
	flags, err := ParseCommandFlags(parsed, []Flag{FlagJSON, FlagForce, FlagStopIfUnused})
	if err != nil {
		t.Fatal(err)
	}
	if !flags.StopIfUnused {
		t.Fatal("expected stop-if-unused")
	}
	if len(flags.Positionals) != 2 || flags.Positionals[0] != "detach" || flags.Positionals[1] != "postgres@16.4" {
		t.Fatalf("unexpected positionals: %v", flags.Positionals)
	}
}

func TestADoubleDashEndsFlagParsingForAnySubcommand(t *testing.T) {
	argv := []string{"probe", "--", "--weird"}
	parsed, passthrough := splitPassthrough(argv)
	if len(parsed) != 1 || parsed[0] != "probe" {
		t.Fatalf("unexpected parsed: %v", parsed)
	}
	if len(passthrough) != 1 || passthrough[0] != "--weird" {
		t.Fatalf("unexpected passthrough: %v", passthrough)
	}
}

func TestOtherSubcommandsStillParseFlagsAfterTheID(t *testing.T) {
	argv := []string{"status", "--json"}
	parsed, passthrough := splitPassthrough(argv)
	if len(parsed) != len(argv) || len(passthrough) != 0 {
		t.Fatalf("unexpected split: %v / %v", parsed, passthrough)
	}
	argv = []string{"start", "pg@16", "--json"}
	parsed, _ = splitPassthrough(argv)
	if len(parsed) != len(argv) {
		t.Fatalf("unexpected parsed: %v", parsed)
	}
}

func TestParseSharedIDValidatesNameAtVersion(t *testing.T) {
	if _, err := parseSharedID(nil); err == nil {
		t.Fatal("expected an error for a missing id")
	}
	if _, err := parseSharedID(new("nginx")); err == nil {
		t.Fatal("expected an error for a missing version")
	}
	if _, err := parseSharedID(new("nginx@1.27@x")); err == nil {
		t.Fatal("expected an error for a second @")
	}
	if _, err := parseSharedID(new("@1.27")); err == nil {
		t.Fatal("expected an error for an empty name")
	}
	if _, err := parseSharedID(new("nginx@")); err == nil {
		t.Fatal("expected an error for an empty version")
	}
	id, err := parseSharedID(new("nginx@1.27"))
	if err != nil || id != "nginx@1.27" {
		t.Fatalf("unexpected id: %q (%v)", id, err)
	}
}
