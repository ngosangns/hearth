// Ported from rust/crates/hearth-cli/src/update.rs: release parsing, verification, and the
// staged `--version` smoke test.
package updater

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"regexp"
	"strconv"
	"strings"
	"time"

	"golang.org/x/mod/semver"
)

// Release is a validated GitHub release asset.
type Release struct {
	version string
	asset   string
	size    uint64
	sha256  string
	url     string
}

// FetchError mirrors the Rust FetchError enum: an HTTP status or a message.
type FetchError struct {
	Status  int
	Message string
}

func (e *FetchError) Error() string {
	if e.Status != 0 {
		return fmt.Sprintf("HTTP %d", e.Status)
	}
	return e.Message
}

// strictSemver matches the Rust semver crate's strict X.Y.Z: no leading zeros, no prerelease or
// build metadata.
var strictSemver = regexp.MustCompile(`^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$`)

// parseSemver accepts strict X.Y.Z only — release tags and versions never carry prerelease or
// build metadata. It returns the canonical version text.
func parseSemver(text string) (string, bool) {
	if !strictSemver.MatchString(text) {
		return "", false
	}
	if !semver.IsValid("v" + text) {
		return "", false
	}
	return text, true
}

func parseRelease(document string) (Release, error) {
	decoder := json.NewDecoder(strings.NewReader(document))
	decoder.UseNumber()
	var parsedAny any
	if err := decoder.Decode(&parsedAny); err != nil {
		return Release{}, errors.New("GitHub release payload is not valid JSON")
	}
	parsed, _ := parsedAny.(map[string]any)
	tag, _ := asString(parsed["tag_name"])
	version, ok := parseSemver(strings.TrimPrefix(tag, "v"))
	if !strings.HasPrefix(tag, "v") || !ok {
		return Release{}, fmt.Errorf("release tag %q is not vX.Y.Z", tag)
	}
	if draft, ok := asBool(parsed["draft"]); !ok || draft {
		return Release{}, errors.New("latest release is a draft")
	}
	if prerelease, ok := asBool(parsed["prerelease"]); !ok || prerelease {
		return Release{}, errors.New("latest release is a prerelease")
	}
	name := "hearth-" + tag
	assets, ok := asArray(parsed["assets"])
	if !ok {
		return Release{}, errors.New("GitHub release payload is missing assets")
	}
	var matching []map[string]any
	for _, entry := range assets {
		asset, ok := entry.(map[string]any)
		if !ok {
			continue
		}
		assetName, _ := asString(asset["name"])
		if assetName == name {
			matching = append(matching, asset)
		}
	}
	if len(matching) == 0 {
		return Release{}, fmt.Errorf("release %s has no asset named %s", tag, name)
	}
	if len(matching) > 1 {
		return Release{}, fmt.Errorf("release %s has more than one asset named %s", tag, name)
	}
	asset := matching[0]
	if state, _ := asString(asset["state"]); state != "uploaded" {
		return Release{}, fmt.Errorf("asset %s is not uploaded", name)
	}
	size, ok := asU64(asset["size"])
	if !ok || size == 0 {
		return Release{}, fmt.Errorf("asset %s has an empty size", name)
	}
	digest, ok := asString(asset["digest"])
	if !ok {
		return Release{}, fmt.Errorf("asset %s digest is missing", name)
	}
	sha256, ok := parseDigest(digest)
	if !ok {
		return Release{}, errors.New("asset digest must be sha256 and 64 hex characters")
	}
	expectedURL := fmt.Sprintf("https://github.com/ngosangns/hearth/releases/download/%s/%s", tag, name)
	url, _ := asString(asset["browser_download_url"])
	if url != expectedURL {
		return Release{}, fmt.Errorf("download url must be %s", expectedURL)
	}
	return Release{
		version: version,
		asset:   name,
		size:    size,
		sha256:  sha256,
		url:     url,
	}, nil
}

func parseDigest(digest string) (string, bool) {
	hexDigest, ok := strings.CutPrefix(digest, "sha256:")
	if !ok || len(hexDigest) != 64 {
		return "", false
	}
	for i := range len(hexDigest) {
		c := hexDigest[i]
		if !(c >= '0' && c <= '9' || c >= 'a' && c <= 'f' || c >= 'A' && c <= 'F') {
			return "", false
		}
	}
	return strings.ToLower(hexDigest), true
}

func fetchMessage(err error, prefix string) string {
	var fetch *FetchError
	if errors.As(err, &fetch) {
		switch {
		case fetch.Status == 403 || fetch.Status == 429:
			return "GitHub rate limit reached; set GITHUB_TOKEN or GH_TOKEN"
		case fetch.Status != 0:
			return fmt.Sprintf("%s: HTTP %d", prefix, fetch.Status)
		default:
			return fetch.Message
		}
	}
	return err.Error()
}

func verifyFile(path string, expectedSize uint64, expectedSHA string) error {
	file, err := os.Open(path)
	if err != nil {
		return fmt.Errorf("cannot read %s: %v", path, err)
	}
	defer file.Close()
	hasher := sha256.New()
	buffer := make([]byte, 64*1024)
	var size uint64
	for {
		read, err := file.Read(buffer)
		if read > 0 {
			size += uint64(read)
			hasher.Write(buffer[:read])
		}
		if err == io.EOF {
			break
		}
		if err != nil {
			return fmt.Errorf("cannot read %s: %v", path, err)
		}
	}
	if size != expectedSize {
		return fmt.Errorf("download size mismatch: expected %d bytes, got %d", expectedSize, size)
	}
	got := hex.EncodeToString(hasher.Sum(nil))
	if got != expectedSHA {
		return fmt.Errorf("sha256 mismatch: expected %s, got %s", expectedSHA, got)
	}
	return nil
}

func ensureExecutable(path string) error {
	if err := os.Chmod(path, 0o755); err != nil {
		return fmt.Errorf("cannot chmod %s: %v", path, err)
	}
	info, err := os.Stat(path)
	if err != nil {
		return fmt.Errorf("cannot stat %s: %v", path, err)
	}
	if info.Mode().Perm()&0o111 == 0 {
		return errors.New("downloaded binary is not executable")
	}
	return nil
}

// smokeBinary runs `--version` on the staged binary. A non-zero exit or any other text keeps the
// previous install.
func smokeBinary(path string) (string, error) {
	cmd := exec.Command(path, "--version")
	var stdout bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = io.Discard
	if err := cmd.Start(); err != nil {
		return "", fmt.Errorf("could not run %s: %v", path, err)
	}
	done := make(chan error, 1)
	go func() { done <- cmd.Wait() }()
	select {
	case err := <-done:
		if err != nil {
			var exit *exec.ExitError
			if errors.As(err, &exit) {
				code := "signal"
				if c := exit.ExitCode(); c >= 0 {
					code = strconv.Itoa(c)
				}
				return "", fmt.Errorf("smoke test failed (exit %s)", code)
			}
			return "", fmt.Errorf("smoke test failed: %v", err)
		}
	case <-time.After(30 * time.Second):
		_ = cmd.Process.Kill()
		<-done
		return "", errors.New("smoke test timed out")
	}
	line := stdout.String()
	if index := strings.IndexByte(line, '\n'); index >= 0 {
		line = line[:index]
	}
	line = strings.TrimSpace(line)
	version, ok := strings.CutPrefix(line, "hearth ")
	if !ok {
		return "", fmt.Errorf("smoke test printed %q", line)
	}
	version = strings.TrimSpace(version)
	if _, ok := parseSemver(version); !ok {
		return "", fmt.Errorf("smoke test printed %q", line)
	}
	return version, nil
}

func asString(value any) (string, bool) {
	s, ok := value.(string)
	return s, ok
}

func asBool(value any) (bool, bool) {
	b, ok := value.(bool)
	return b, ok
}

func asU64(value any) (uint64, bool) {
	number, ok := value.(json.Number)
	if !ok {
		return 0, false
	}
	parsed, err := strconv.ParseUint(number.String(), 10, 64)
	if err != nil {
		return 0, false
	}
	return parsed, true
}

func asArray(value any) ([]any, bool) {
	items, ok := value.([]any)
	return items, ok
}

// marshalJSON encodes a value without HTML escaping, matching serde_json's output.
func marshalJSON(value any) string {
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	_ = encoder.Encode(value)
	return strings.TrimRight(buffer.String(), "\n")
}
