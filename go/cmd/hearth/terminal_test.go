package main

import (
	"bytes"
	"io"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestDevNullIsNotATerminal(t *testing.T) {
	f, err := os.Open(os.DevNull)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = f.Close() })
	info, err := f.Stat()
	if err != nil {
		t.Fatal(err)
	}
	if info.Mode()&os.ModeCharDevice == 0 {
		t.Fatal("os.DevNull is not a character device")
	}
	if isTerminal(f) {
		t.Fatal("os.DevNull opened as *os.File is a terminal")
	}
}

func TestGatesExit6WhenStdinIsDevNull(t *testing.T) {
	t.Setenv("HOME", t.TempDir())
	root := t.TempDir()
	catalog := []byte("version: 1\nservices:\n  api:\n    run: { argv: [echo, hi] }\n    readiness: { kind: process }\n")
	if err := os.WriteFile(filepath.Join(root, "hearth.yaml"), catalog, 0o644); err != nil {
		t.Fatal(err)
	}

	devNull, err := os.Open(os.DevNull)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = devNull.Close() })
	oldStdin := os.Stdin
	os.Stdin = devNull
	t.Cleanup(func() { os.Stdin = oldStdin })

	prevProject, prevShared := spawnProjectDaemon, spawnSharedDaemon
	t.Cleanup(func() {
		spawnProjectDaemon = prevProject
		spawnSharedDaemon = prevShared
	})
	spawnProjectDaemon = func(string) { t.Error("spawned a project daemon") }
	spawnSharedDaemon = func(string) { t.Error("spawned smp") }

	cases := []struct {
		name string
		args []string
	}{
		{"manager stop", []string{"manager", "stop", "--json"}},
		{"kill-unowned", []string{"start", "api", "--kill-unowned", "--json"}},
		{"shared remove", []string{"shared", "remove", "postgres@16.4", "--json"}},
		{"shared detach", []string{"shared", "detach", "postgres@16.4", "--stop-if-unused", "--json"}},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			code, out := runCLICaptured(t, root, tc.args)
			if code != 6 {
				t.Fatalf("exit = %d\n%s", code, out)
			}
			if !strings.Contains(out, `"code":"needs-confirm"`) || !strings.Contains(out, `"hint":"pass --yes"`) {
				t.Fatalf("stdout = %s", out)
			}
		})
	}
}

func runCLICaptured(t *testing.T, root string, args []string) (int, string) {
	t.Helper()
	reader, writer, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	old := os.Stdout
	os.Stdout = writer
	code := runCLI(root, args)
	_ = writer.Close()
	os.Stdout = old
	var buf bytes.Buffer
	if _, err := io.Copy(&buf, reader); err != nil {
		t.Fatal(err)
	}
	return code, buf.String()
}
