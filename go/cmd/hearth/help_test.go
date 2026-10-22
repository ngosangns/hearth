package main

import (
	"bytes"
	"io"
	"os"
	"strings"
	"testing"
)

func TestDaemonAndSmpHelpPrintUsage(t *testing.T) {
	stdout := captureStdout(t, func() int {
		return runDaemonSubcommand(t.TempDir(), []string{"--help"})
	})
	if !strings.Contains(stdout, "usage: hearth daemon") {
		t.Fatalf("daemon help = %q", stdout)
	}
	stdout = captureStdout(t, func() int {
		return runSmpSubcommand([]string{"--help"})
	})
	if !strings.Contains(stdout, "usage: hearth smp") {
		t.Fatalf("smp help = %q", stdout)
	}
	code := runDaemonSubcommand(t.TempDir(), []string{"extra"})
	if code != 2 {
		t.Fatalf("extra args exited %d", code)
	}
}

func captureStdout(t *testing.T, fn func() int) string {
	t.Helper()
	reader, writer, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	old := os.Stdout
	os.Stdout = writer
	code := fn()
	writer.Close()
	os.Stdout = old
	var buf bytes.Buffer
	if _, err := io.Copy(&buf, reader); err != nil {
		t.Fatal(err)
	}
	if code != 0 {
		t.Fatalf("exited %d\n%s", code, buf.String())
	}
	return buf.String()
}
