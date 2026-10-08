// Tarball installer — shared services and per-service `artifact:` blocks.
//
// The flow is the same for both: materialize the archive (url download or script
// packager), verify sha256 for url artifacts only, extract under downloads/, then
// rename the payload into the install dir. `.hearth-installed` is written last, so
// a partial install is never treated as done.
//
// Helper commands run in their own process group. They finish when the leader
// exits and its output pipes close. A pipe still open one second later is a
// leftover child: the group is SIGKILLed. A timeout kills the group too, so a
// grandchild does not outlive the wait just because the leader was already reaped.
package shared

import (
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"syscall"
	"time"

	"github.com/ngosangns/hearth/go/internal/catalog"
	"github.com/ngosangns/hearth/go/internal/paths"
)

const (
	installedMarker     = ".hearth-installed"
	extractTimeout      = 120 * time.Second
	packScriptTimeout   = 45 * time.Minute
	downloadTimeout     = 600 * time.Second
	outputDrain         = time.Second
	outputDrainKillWait = 500 * time.Millisecond
)

// ArtifactSpec is the normalized artifact both callers lower into.
type ArtifactSpec struct {
	URL        string
	Script     string
	ScriptArgs []string
	// Sha256 is enforced only for downloaded url artifacts. Script output is not
	// byte-reproducible; the script itself is the trust boundary.
	Sha256 string
}

func artifactFromService(a *catalog.ServiceArtifact) ArtifactSpec {
	spec := ArtifactSpec{ScriptArgs: a.ScriptArgs}
	if a.URL != nil {
		spec.URL = *a.URL
	}
	if a.Script != nil {
		spec.Script = *a.Script
	}
	if a.Sha256 != nil {
		spec.Sha256 = *a.Sha256
	}
	return spec
}

// TarballInstaller installs one archive into an install dir.
// Exactly one of CatalogURL or ProjectDir selects how a relative script resolves.
type TarballInstaller struct {
	DownloadsDir string
	CatalogURL   string
	ProjectDir   string
}

// ProjectInstaller resolves relative scripts under projectRoot. HEARTH_CATALOG_ORIGIN
// handed to the script is that root as a file:// URL.
func ProjectInstaller(projectRoot, downloadsDir string) *TarballInstaller {
	return &TarballInstaller{DownloadsDir: downloadsDir, ProjectDir: projectRoot}
}

// InstallServiceArtifact installs a service `artifact:` block. installDir and dataDir
// come from the resolved definition, or fall back to the runtime-dir convention.
// The data dir is created on every call — wiping it must not wedge the next start.
func InstallServiceArtifact(projectRoot, runtimeDirectory string, service *catalog.ServiceDefinition, artifact *catalog.ServiceArtifact, onProgress func(string)) error {
	if service == nil || artifact == nil {
		return Errorf("artifact install: missing service or artifact")
	}
	installDir := strings.TrimSpace(derefString(artifact.InstallDir))
	if installDir == "" {
		installDir = paths.InstallDir(runtimeDirectory, service.ID, artifact.Version)
	}
	dataDir := strings.TrimSpace(derefString(artifact.DataDir))
	if dataDir == "" {
		dataDir = paths.ServiceDataDir(runtimeDirectory, service.ID)
	}
	if err := os.MkdirAll(dataDir, 0o755); err != nil {
		return Errorf("cannot create data dir %s: %v", dataDir, err)
	}
	_, err := ProjectInstaller(projectRoot, paths.DownloadsDir(runtimeDirectory)).Install(service.ID, installDir, artifactFromService(artifact), onProgress)
	return err
}

func derefString(s *string) string {
	if s == nil {
		return ""
	}
	return *s
}

// Install is idempotent: a present marker returns the install dir immediately.
func (t *TarballInstaller) Install(name, installDir string, artifact ArtifactSpec, onProgress func(string)) (string, error) {
	if installDir == "" {
		return "", Errorf("%s: install dir is empty", name)
	}
	marker := filepath.Join(installDir, installedMarker)
	if info, err := os.Stat(marker); err == nil && info.Mode().IsRegular() {
		return installDir, nil
	}
	if err := os.MkdirAll(t.DownloadsDir, 0o755); err != nil {
		return "", Errorf("cannot create downloads dir: %v", err)
	}
	archivePath := filepath.Join(t.DownloadsDir, name+".tar.gz")
	digest, err := t.materialize(name, artifact, archivePath, onProgress)
	if err != nil {
		return "", err
	}
	if artifact.URL != "" {
		if digest == "" || !strings.EqualFold(digest, artifact.Sha256) {
			_ = os.Remove(archivePath)
			got := digest
			if got == "" {
				got = ""
			}
			return "", Errorf("sha256 mismatch: expected %s, got %s", artifact.Sha256, got)
		}
	}
	staging := filepath.Join(t.DownloadsDir, name+".extract")
	_ = os.RemoveAll(staging)
	if err := os.MkdirAll(staging, 0o755); err != nil {
		return "", Errorf("cannot create extract dir: %v", err)
	}
	if err := extractArchive(archivePath, staging); err != nil {
		return "", err
	}
	installed, err := finishInstall(staging, archivePath, installDir)
	if err != nil {
		return "", err
	}
	progress(onProgress, "installed "+name)
	return installed, nil
}

func (t *TarballInstaller) materialize(name string, artifact ArtifactSpec, dest string, onProgress func(string)) (string, error) {
	if artifact.URL != "" {
		return download(artifact.URL, dest, onProgress)
	}
	if artifact.Script == "" {
		return "", Errorf("%s: artifact needs a url or a script", name)
	}
	if err := t.runPackScript(name, artifact.Script, artifact.ScriptArgs, dest, onProgress); err != nil {
		return "", err
	}
	return "", nil
}

func (t *TarballInstaller) runPackScript(name, script string, args []string, dest string, onProgress func(string)) error {
	scriptPath, err := t.stageScript(name, script)
	if err != nil {
		return err
	}
	origin := t.catalogOrigin()
	progress(onProgress, fmt.Sprintf("packaging %s with %s", name, script))
	argv := append([]string{scriptPath}, args...)
	argv = append(argv, dest)
	cmd := exec.Command("bash", argv...)
	cmd.Env = append(os.Environ(), "HEARTH_CATALOG_ORIGIN="+origin)
	if dir := filepath.Dir(scriptPath); dir != "" {
		cmd.Dir = dir
	}
	out, err := outputWithTimeout(cmd, packScriptTimeout)
	if err != nil {
		return Errorf("%s: failed to run packaging script: %v", name, err)
	}
	if out == nil {
		return Errorf("%s: packaging script timed out", name)
	}
	if out.Code != 0 {
		return Errorf("%s: packaging script failed: %s%s", name, string(out.Stderr), string(out.Stdout))
	}
	info, err := os.Stat(dest)
	if err != nil || !info.Mode().IsRegular() {
		return Errorf("%s: packaging script did not write %s", name, dest)
	}
	return nil
}

func (t *TarballInstaller) catalogOrigin() string {
	if t.ProjectDir != "" {
		return "file://" + t.ProjectDir
	}
	url := t.CatalogURL
	if i := strings.LastIndex(url, "/"); i >= 0 {
		return url[:i]
	}
	return url
}

func (t *TarballInstaller) stageScript(name, script string) (string, error) {
	if t.ProjectDir != "" {
		if strings.Contains(script, "://") {
			dest := filepath.Join(t.DownloadsDir, name+".pack.sh")
			if _, err := download(script, dest, nil); err != nil {
				return "", err
			}
			return dest, nil
		}
		path := filepath.Join(t.ProjectDir, script)
		if info, err := os.Stat(path); err == nil && info.Mode().IsRegular() {
			return path, nil
		}
		return "", Errorf("%s: packaging script not found: %s", name, path)
	}
	if path, ok := localCatalogScript(t.CatalogURL, script); ok {
		return path, nil
	}
	url := script
	if !strings.Contains(script, "://") {
		url = t.catalogOrigin() + "/" + script
	}
	dest := filepath.Join(t.DownloadsDir, name+".pack.sh")
	if _, err := download(url, dest, nil); err != nil {
		return "", err
	}
	return dest, nil
}

func localCatalogScript(catalogURL, script string) (string, bool) {
	rest, ok := strings.CutPrefix(catalogURL, "file://")
	if !ok || strings.Contains(script, "://") {
		return "", false
	}
	path := filepath.Join(filepath.Dir(rest), script)
	info, err := os.Stat(path)
	if err != nil || !info.Mode().IsRegular() {
		return "", false
	}
	return path, true
}

// download streams url into dest and returns the lowercase-hex sha256 of the bytes written.
func download(url, dest string, onProgress func(string)) (string, error) {
	progress(onProgress, "downloading "+url)
	if rest, ok := strings.CutPrefix(url, "file://"); ok {
		digest, err := copyHashing(rest, dest)
		if err != nil {
			return "", Errorf("failed to copy %s: %v", url, err)
		}
		return digest, nil
	}
	req, err := http.NewRequest(http.MethodGet, url, nil)
	if err != nil {
		return "", Errorf("download failed for %s: %v", url, err)
	}
	client := &http.Client{Timeout: downloadTimeout}
	resp, err := client.Do(req)
	if err != nil {
		return "", Errorf("download failed for %s: %v", url, err)
	}
	defer resp.Body.Close()
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return "", Errorf("download failed for %s: HTTP %d", url, resp.StatusCode)
	}
	file, err := os.Create(dest)
	if err != nil {
		return "", Errorf("cannot create %s: %v", dest, err)
	}
	defer file.Close()
	hasher := sha256.New()
	if _, err := io.Copy(io.MultiWriter(file, hasher), resp.Body); err != nil {
		return "", Errorf("download failed for %s: %v", url, err)
	}
	if err := file.Close(); err != nil {
		return "", Errorf("cannot write %s: %v", dest, err)
	}
	return hex.EncodeToString(hasher.Sum(nil)), nil
}

func copyHashing(src, dest string) (string, error) {
	input, err := os.Open(src)
	if err != nil {
		return "", err
	}
	defer input.Close()
	output, err := os.Create(dest)
	if err != nil {
		return "", err
	}
	defer output.Close()
	hasher := sha256.New()
	buf := make([]byte, 64*1024)
	for {
		n, err := input.Read(buf)
		if n > 0 {
			if _, werr := hasher.Write(buf[:n]); werr != nil {
				return "", werr
			}
			if _, werr := output.Write(buf[:n]); werr != nil {
				return "", werr
			}
		}
		if err == io.EOF {
			break
		}
		if err != nil {
			return "", err
		}
	}
	if err := output.Close(); err != nil {
		return "", err
	}
	return hex.EncodeToString(hasher.Sum(nil)), nil
}

func extractArchive(archive, dest string) error {
	cmd := exec.Command("tar", "-xzf", archive, "-C", dest)
	out, err := outputWithTimeout(cmd, extractTimeout)
	if err != nil {
		return Errorf("failed to spawn tar: %v", err)
	}
	if out == nil {
		return Errorf("tar extract timed out")
	}
	if out.Code != 0 {
		return Errorf("tar extract failed: %s", strings.TrimSpace(string(out.Stderr)))
	}
	return nil
}

// payloadDir is the single top-level directory inside staging, or staging itself
// when the archive is not wrapped that way. Dotfiles are ignored.
func payloadDir(staging string) (string, error) {
	entries, err := os.ReadDir(staging)
	if err != nil {
		return "", Errorf("%v", err)
	}
	var visible []string
	for _, entry := range entries {
		if strings.HasPrefix(entry.Name(), ".") {
			continue
		}
		visible = append(visible, filepath.Join(staging, entry.Name()))
	}
	if len(visible) == 1 {
		info, err := os.Stat(visible[0])
		if err == nil && info.IsDir() {
			return visible[0], nil
		}
	}
	return staging, nil
}

// finishInstall renames the payload into place and writes the marker last.
// A leftover target from a failed attempt is quarantined aside, never deleted.
func finishInstall(staging, archivePath, installDir string) (string, error) {
	payload, err := payloadDir(staging)
	if err != nil {
		return "", err
	}
	if _, err := os.Stat(installDir); err == nil {
		quarantined := withExtension(installDir, "quarantine-"+randomHex())
		if err := os.Rename(installDir, quarantined); err != nil {
			return "", Errorf("cannot quarantine previous install: %v", err)
		}
	}
	if parent := filepath.Dir(installDir); parent != "" {
		if err := os.MkdirAll(parent, 0o755); err != nil {
			return "", Errorf("%v", err)
		}
	}
	if err := os.Rename(payload, installDir); err != nil {
		return "", Errorf("cannot move install into place: %v", err)
	}
	if err := os.WriteFile(filepath.Join(installDir, installedMarker), []byte("ok\n"), 0o644); err != nil {
		return "", Errorf("%v", err)
	}
	_ = os.Remove(archivePath)
	_ = os.RemoveAll(staging)
	return installDir, nil
}

// withExtension matches Path::with_extension: replace the last dot-suffix, or append one.
func withExtension(path, ext string) string {
	dir := filepath.Dir(path)
	base := filepath.Base(path)
	if i := strings.LastIndex(base, "."); i > 0 {
		base = base[:i]
	}
	return filepath.Join(dir, base+"."+ext)
}

func randomHex() string {
	var b [16]byte
	if _, err := rand.Read(b[:]); err != nil {
		return fmt.Sprintf("%d", time.Now().UnixNano())
	}
	return hex.EncodeToString(b[:])
}

func progress(fn func(string), line string) {
	if fn != nil {
		fn(line)
	}
}

type capturedOutput struct {
	Code   int
	Stdout []byte
	Stderr []byte
}

// outputWithTimeout runs cmd in its own process group.
// A nil result with a nil error means the limit elapsed and the group was killed.
//
// Stdout and stderr are our own pipes, not Cmd.StdoutPipe. Wait closes the
// StdoutPipe read end as soon as the leader exits, which looks like a finished
// drain while a grandchild still holds the write end — and then the group is
// never killed. An *os.File assigned to Stdout is not closed by Wait.
func outputWithTimeout(cmd *exec.Cmd, limit time.Duration) (*capturedOutput, error) {
	outR, outW, err := os.Pipe()
	if err != nil {
		return nil, err
	}
	defer outR.Close()
	errR, errW, err := os.Pipe()
	if err != nil {
		outW.Close()
		return nil, err
	}
	defer errR.Close()
	cmd.Stdout = outW
	cmd.Stderr = errW
	cmd.Stdin = nil
	if cmd.SysProcAttr == nil {
		cmd.SysProcAttr = &syscall.SysProcAttr{}
	}
	cmd.SysProcAttr.Setpgid = true
	if err := cmd.Start(); err != nil {
		outW.Close()
		errW.Close()
		return nil, err
	}
	// Only the child (and whatever it forks) may hold the write ends.
	_ = outW.Close()
	_ = errW.Close()
	pgid := cmd.Process.Pid
	disarmed := false
	defer func() {
		if !disarmed {
			killGroup(pgid)
		}
	}()

	outCh := readPipe(outR)
	errCh := readPipe(errR)

	waitCh := make(chan error, 1)
	go func() { waitCh <- cmd.Wait() }()

	timer := time.NewTimer(limit)
	defer timer.Stop()
	select {
	case werr := <-waitCh:
		pipes := pipePair{outCh: outCh, errCh: errCh}
		if !pipes.wait(outputDrain) {
			killGroup(pgid)
			pipes.wait(outputDrainKillWait)
		}
		disarmed = true
		code := 0
		if werr != nil {
			if ee, ok := werr.(*exec.ExitError); ok {
				code = ee.ExitCode()
			} else {
				return nil, werr
			}
		}
		return &capturedOutput{Code: code, Stdout: pipes.out, Stderr: pipes.errb}, nil
	case <-timer.C:
		killGroup(pgid)
		<-waitCh
		disarmed = true
		return nil, nil
	}
}

func readPipe(r io.Reader) <-chan []byte {
	ch := make(chan []byte, 1)
	go func() {
		var buf strings.Builder
		_, _ = io.Copy(&buf, r)
		ch <- []byte(buf.String())
	}()
	return ch
}

// pipePair collects stdout and stderr once each. A second wait does not drop
// bytes the first wait already took.
type pipePair struct {
	outCh  <-chan []byte
	errCh  <-chan []byte
	out    []byte
	errb   []byte
	gotOut bool
	gotErr bool
}

func (p *pipePair) wait(limit time.Duration) bool {
	if p.gotOut && p.gotErr {
		return true
	}
	timer := time.NewTimer(limit)
	defer timer.Stop()
	for !p.gotOut || !p.gotErr {
		var outCh, errCh <-chan []byte
		if !p.gotOut {
			outCh = p.outCh
		}
		if !p.gotErr {
			errCh = p.errCh
		}
		select {
		case b := <-outCh:
			p.out = b
			p.gotOut = true
		case b := <-errCh:
			p.errb = b
			p.gotErr = true
		case <-timer.C:
			return false
		}
	}
	return true
}

func killGroup(pgid int) {
	if pgid > 1 {
		_ = syscall.Kill(-pgid, syscall.SIGKILL)
	}
}
