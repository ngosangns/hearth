package supervisor

import (
	"path/filepath"
	"testing"
)

func TestResolveCwd(t *testing.T) {
	root := filepath.Join(string(filepath.Separator), "home", "u", ".hearth", "shared")
	abs := filepath.Join(root, "instances", "mongodb@8.0.32")
	if got := resolveCwd(root, abs); got != abs {
		t.Fatalf("absolute cwd nested under root: %s", got)
	}
	if got := resolveCwd(root, "."); got != root {
		t.Fatalf("relative cwd: %s", got)
	}
	if got := resolveCwd(root, "sub/dir"); got != filepath.Join(root, "sub", "dir") {
		t.Fatalf("relative subdir: %s", got)
	}
}
