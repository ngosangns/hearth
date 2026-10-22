package updater

import (
	"bytes"
	"context"
	"encoding/json"
	"path/filepath"
	"strings"
	"testing"
)

func TestUpdateHelpAndUsageJSON(t *testing.T) {
	home := t.TempDir()
	exe := filepath.Join(home, "hearth")
	var stdout, stderr bytes.Buffer
	err := Run(context.Background(), Options{
		Args:       []string{"--check", "--help"},
		Version:    "0.1.0",
		Stdout:     &stdout,
		Stderr:     &stderr,
		Home:       home,
		CurrentExe: exe,
	})
	if err != nil || stderr.Len() != 0 || !strings.Contains(stdout.String(), "usage: hearth update") {
		t.Fatalf("help: %v\n%s\n%s", err, stdout.String(), stderr.String())
	}

	stdout.Reset()
	stderr.Reset()
	err = Run(context.Background(), Options{
		Args:       []string{"--bogus", "--json"},
		Version:    "0.1.0",
		Stdout:     &stdout,
		Stderr:     &stderr,
		Home:       home,
		CurrentExe: exe,
	})
	if ExitCode(err) != 2 || stderr.Len() != 0 {
		t.Fatalf("usage json: %v\n%s\n%s", err, stdout.String(), stderr.String())
	}
	var doc struct {
		Error struct {
			Code    string `json:"code"`
			Exit    int    `json:"exit"`
			Message string `json:"message"`
		} `json:"error"`
	}
	if jsonErr := json.Unmarshal(stdout.Bytes(), &doc); jsonErr != nil {
		t.Fatal(jsonErr)
	}
	if doc.Error.Code != "usage" || doc.Error.Exit != 2 || doc.Error.Message == "" {
		t.Fatalf("error = %#v", doc.Error)
	}

	stdout.Reset()
	stderr.Reset()
	err = Run(context.Background(), Options{
		Args:       []string{"--", "--help"},
		Version:    "0.1.0",
		Stdout:     &stdout,
		Stderr:     &stderr,
		Home:       home,
		CurrentExe: exe,
	})
	if ExitCode(err) != 2 || stdout.Len() != 0 || !strings.Contains(stderr.String(), "unknown flag") {
		t.Fatalf("help after --: %v\n%s\n%s", err, stdout.String(), stderr.String())
	}
}
