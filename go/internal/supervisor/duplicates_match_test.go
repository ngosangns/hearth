package supervisor

import (
	"os"
	"os/exec"
	"path/filepath"
	"syscall"
	"testing"
	"time"
)

func TestQueryMatchesCommandByFingerprintExecutableOrInstallMarker(t *testing.T) {
	command := "/Users/me/viclass/portal/metadata/build/install/portal.metadata/bin/portal.metadata"
	if !queryMatchesCommand(command, serviceProcessQuery{marker: "portal/metadata/build/install/portal.metadata"}) {
		t.Fatal("install-dir marker did not match")
	}
	if queryMatchesCommand(command, serviceProcessQuery{marker: "portal/user/build/install/portal.user"}) {
		t.Fatal("another service's install dir matched")
	}
	if queryMatchesCommand(command, serviceProcessQuery{}) {
		t.Fatal("empty query matched")
	}
	if !queryMatchesCommand("/bin/sleep 30", serviceProcessQuery{executables: []string{"/bin/sleep"}}) {
		t.Fatal("absolute executable did not match")
	}
}

func TestMatchServiceProcessesAssignsOneSnapshotByCwdAndMarker(t *testing.T) {
	root := t.TempDir()
	here := filepath.Join(root, "here")
	there := filepath.Join(root, "there")
	if err := os.MkdirAll(here, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(there, 0o755); err != nil {
		t.Fatal(err)
	}
	hereCmd := exec.Command("/bin/sleep", "30")
	hereCmd.Dir = here
	hereCmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	thereCmd := exec.Command("/bin/sleep", "30")
	thereCmd.Dir = there
	thereCmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	if err := hereCmd.Start(); err != nil {
		t.Fatal(err)
	}
	if err := thereCmd.Start(); err != nil {
		_ = syscall.Kill(-hereCmd.Process.Pid, syscall.SIGKILL)
		t.Fatal(err)
	}
	t.Cleanup(func() {
		_ = syscall.Kill(-hereCmd.Process.Pid, syscall.SIGKILL)
		_ = syscall.Kill(-thereCmd.Process.Pid, syscall.SIGKILL)
		_ = hereCmd.Wait()
		_ = thereCmd.Wait()
	})
	herePid := int64(hereCmd.Process.Pid)
	therePid := int64(thereCmd.Process.Pid)
	deadline := time.Now().Add(3 * time.Second)
	var matched map[string][]CommandMatch
	var ok bool
	for {
		matched, ok = matchServiceProcesses(root, []serviceProcessQuery{
			{id: "here", executables: []string{"/bin/sleep"}, cwd: "here"},
			{id: "there", marker: "/bin/sleep", cwd: "there"},
			{id: "empty", cwd: "here"},
		})
		if ok && matchHasPid(matched["here"], herePid) && matchHasPid(matched["there"], therePid) {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("ok=%v here=%v there=%v want here %d there %d", ok, matched["here"], matched["there"], herePid, therePid)
		}
		time.Sleep(50 * time.Millisecond)
	}
	if matchHasPid(matched["here"], therePid) || matchHasPid(matched["there"], herePid) {
		t.Fatalf("cwd crossed: here=%v there=%v", matched["here"], matched["there"])
	}
	if len(matched["empty"]) != 0 {
		t.Fatalf("empty query returned %v", matched["empty"])
	}
}

func matchHasPid(rows []CommandMatch, pid int64) bool {
	for _, row := range rows {
		if row.Pid == pid {
			return true
		}
	}
	return false
}
