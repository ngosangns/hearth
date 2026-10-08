package shared

import (
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/paths"
)

func makeTarball(t *testing.T, dir string) (archive string, sha string) {
	t.Helper()
	payload := filepath.Join(dir, "pkg", "bin")
	if err := os.MkdirAll(payload, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(payload, "hello"), []byte("world"), 0o644); err != nil {
		t.Fatal(err)
	}
	archive = filepath.Join(dir, "pkg.tar.gz")
	cmd := exec.Command("tar", "-czf", archive, "-C", dir, "pkg")
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("tar: %v %s", err, out)
	}
	digest, err := copyHashing(archive, archive+".copy")
	if err != nil {
		t.Fatal(err)
	}
	_ = os.Remove(archive + ".copy")
	return archive, digest
}

func TestInstallServiceArtifactFromScript(t *testing.T) {
	src := t.TempDir()
	archive, _ := makeTarball(t, src)
	project := t.TempDir()
	script := "#!/bin/bash\nset -euo pipefail\ncp " + shellQuote(archive) + " \"$1\"\n"
	if err := os.WriteFile(filepath.Join(project, "pack.sh"), []byte(script), 0o755); err != nil {
		t.Fatal(err)
	}
	runtime := t.TempDir()
	service := &catalog.ServiceDefinition{ID: "db"}
	artifact := &catalog.ServiceArtifact{
		Version: "1.0",
		Script:  strPtr("pack.sh"),
	}
	if err := InstallServiceArtifact(project, runtime, service, artifact, func(string) {}); err != nil {
		t.Fatal(err)
	}
	installDir := paths.InstallDir(runtime, "db", "1.0")
	if _, err := os.Stat(filepath.Join(installDir, "bin", "hello")); err != nil {
		t.Fatalf("payload top-level dir should be unwrapped: %v", err)
	}
	if info, err := os.Stat(paths.ServiceDataDir(runtime, "db")); err != nil || !info.IsDir() {
		t.Fatalf("data dir: %v", err)
	}
	if err := InstallServiceArtifact(project, runtime, service, artifact, nil); err != nil {
		t.Fatal(err)
	}
}

func TestInstallRejectsSha256Mismatch(t *testing.T) {
	src := t.TempDir()
	archive, _ := makeTarball(t, src)
	root := t.TempDir()
	installDir := filepath.Join(root, "installs", "pkg")
	installer := ProjectInstaller(root, filepath.Join(root, "downloads"))
	_, err := installer.Install("pkg", installDir, ArtifactSpec{
		URL:    "file://" + archive,
		Sha256: strings.Repeat("0", 64),
	}, nil)
	if err == nil || !strings.Contains(err.Error(), "sha256 mismatch") {
		t.Fatalf("got %v", err)
	}
	if _, statErr := os.Stat(filepath.Join(installDir, ".hearth-installed")); !os.IsNotExist(statErr) {
		t.Fatalf("marker should be absent, stat=%v", statErr)
	}
}

func TestInstallFromFileURL(t *testing.T) {
	src := t.TempDir()
	archive, sha := makeTarball(t, src)
	root := t.TempDir()
	installDir := filepath.Join(root, "installs", "pkg")
	installer := ProjectInstaller(root, filepath.Join(root, "downloads"))
	dir, err := installer.Install("pkg", installDir, ArtifactSpec{
		URL:    "file://" + archive,
		Sha256: sha,
	}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(dir, "bin", "hello")); err != nil {
		t.Fatal(err)
	}
	if _, err := installer.Install("pkg", installDir, ArtifactSpec{URL: "file://" + archive, Sha256: sha}, nil); err != nil {
		t.Fatal(err)
	}
}

func TestOutputWithTimeoutKillsChildHoldingPipe(t *testing.T) {
	dir := t.TempDir()
	pidFile := filepath.Join(dir, "bg.pid")
	cmd := exec.Command("sh", "-c", "echo packed; sleep 300 & echo $! > "+shellQuote(pidFile)+"; exit 0")
	started := time.Now()
	out, err := outputWithTimeout(cmd, 60*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if out == nil {
		t.Fatal("the command itself finished")
	}
	if time.Since(started) >= 5*time.Second {
		t.Fatalf("waited %s; a leftover pipe holder must not run out the limit", time.Since(started))
	}
	if out.Code != 0 {
		t.Fatalf("code %d stderr %s", out.Code, out.Stderr)
	}
	if strings.TrimSpace(string(out.Stdout)) != "packed" {
		t.Fatalf("stdout %q", out.Stdout)
	}
	assertDead(t, pidFile)
}

func TestOutputWithTimeoutKillsGroupOnTimeout(t *testing.T) {
	dir := t.TempDir()
	pidFile := filepath.Join(dir, "bg.pid")
	cmd := exec.Command("sh", "-c", "sleep 300 >/dev/null 2>&1 & echo $! > "+shellQuote(pidFile)+"; sleep 300")
	out, err := outputWithTimeout(cmd, 800*time.Millisecond)
	if err != nil {
		t.Fatal(err)
	}
	if out != nil {
		t.Fatal("expected timeout")
	}
	assertDead(t, pidFile)
}

func assertDead(t *testing.T, pidFile string) {
	t.Helper()
	var bg int
	deadline := time.Now().Add(3 * time.Second)
	for {
		text, err := os.ReadFile(pidFile)
		if err != nil {
			if time.Now().After(deadline) {
				t.Fatal(err)
			}
			time.Sleep(25 * time.Millisecond)
			continue
		}
		bg, err = strconv.Atoi(strings.TrimSpace(string(text)))
		if err != nil {
			t.Fatal(err)
		}
		break
	}
	deadline = time.Now().Add(3 * time.Second)
	for pidAlive(bg) && time.Now().Before(deadline) {
		time.Sleep(25 * time.Millisecond)
	}
	if pidAlive(bg) {
		_ = syscall.Kill(bg, syscall.SIGKILL)
		t.Fatalf("pid %d still alive", bg)
	}
}

func pidAlive(pid int) bool {
	err := syscall.Kill(pid, 0)
	return err == nil
}

func shellQuote(s string) string {
	return "'" + strings.ReplaceAll(s, "'", "'\\''") + "'"
}

func strPtr(s string) *string { return &s }
