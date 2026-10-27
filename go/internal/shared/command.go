package shared

import (
	"path/filepath"
	"strings"
)

// SharedSubcommand is a hearth shared CLI verb driven from a generated service.
type SharedSubcommand string

const (
	SharedAttach SharedSubcommand = "attach"
	SharedDetach SharedSubcommand = "detach"
	SharedProbe  SharedSubcommand = "probe"
)

// ParsedSharedCommand is argv after the hearth binary when it is a shared task.
type ParsedSharedCommand struct {
	Subcommand         SharedSubcommand
	InstanceID         string
	AttachArgs         []string
	DetachStopIfUnused bool
	DetachYes          bool
}

// ParseSharedCommand recognizes `[hearth, shared, attach|detach|probe, name@ver, …]`.
// exe must be the hearth binary (any path); returns false for anything else.
func ParseSharedCommand(exe string, argv []string) (ParsedSharedCommand, bool) {
	if len(argv) < 3 {
		return ParsedSharedCommand{}, false
	}
	if !isHearthBinaryName(filepath.Base(exe)) {
		return ParsedSharedCommand{}, false
	}
	if argv[0] != "shared" {
		return ParsedSharedCommand{}, false
	}
	out := ParsedSharedCommand{InstanceID: argv[2]}
	switch argv[1] {
	case "attach":
		out.Subcommand = SharedAttach
		if len(argv) > 3 {
			out.AttachArgs = append([]string{}, argv[3:]...)
		}
		return out, true
	case "detach":
		out.Subcommand = SharedDetach
		for i := 3; i < len(argv); i++ {
			switch argv[i] {
			case DetachStopIfUnusedFlag:
				out.DetachStopIfUnused = true
			case ConfirmYesFlag:
				out.DetachYes = true
			}
		}
		return out, true
	case "probe":
		out.Subcommand = SharedProbe
		return out, true
	default:
		return ParsedSharedCommand{}, false
	}
}

func isHearthBinaryName(name string) bool {
	if name == "hearth" {
		return true
	}
	rest, ok := strings.CutPrefix(name, "hearth-")
	if !ok {
		return false
	}
	for _, r := range rest {
		if r != '.' && (r < '0' || r > '9') {
			return false
		}
	}
	return rest != ""
}
