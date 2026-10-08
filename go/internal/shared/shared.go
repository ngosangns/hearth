// Package shared is the smp (shared services) state layer: the instance
// registry persisted under ~/.hearth/shared, deterministic port allocation,
// recipe template rendering hooks, and catalog synthesis.
package shared

import (
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"os"
	"path/filepath"
)

// SharedRoot is the smp daemon's root directory: ~/.hearth/shared. Unlike
// project daemons this root has no hearth.yaml — the manager's catalog is
// synthesized from registry.json instead.
func SharedRoot() string {
	if home, err := os.UserHomeDir(); err == nil {
		return filepath.Join(home, ".hearth", "shared")
	}
	return filepath.Join("/", ".hearth", "shared")
}

// The smp runtime directory lives directly under its root (not the default
// .hearth/runtime-v1 nested inside it), because ~/.hearth/shared already IS
// the hearth-namespaced directory.
const SharedRuntimeDirectoryName = "runtime-v1"

// Shared instances bind inside their own range, away from both well-known dev
// ports and macOS's ephemeral range (49152–65535). hash(name@version) picks
// the base slot deterministically.
const SharedPortRangeStart uint16 = 43100
const SharedPortRangeSize uint16 = 900

// MaxSharedPorts caps a recipe's contiguous port reservation.
const MaxSharedPorts uint16 = 4

// The pinned remote registry URL.
const SharedCatalogURL = "https://raw.githubusercontent.com/ngosangns/hearth/main/catalog.json"

// v1 supports exactly one artifact platform.
const SharedArtifactPlatform = "darwin-arm64"

// SharedError is the module's error type.
type SharedError struct{ Message string }

func (e *SharedError) Error() string { return e.Message }

func Errorf(format string, args ...any) *SharedError {
	return &SharedError{Message: fmt.Sprintf(format, args...)}
}

// InstanceID — `name@version` — the instance id everywhere: registry key, smp
// catalog service id, CLI arg.
func InstanceID(name, version string) string { return name + "@" + version }

// ProjectID is the stable identity of a registering project: sha256 of the
// canonicalized project root, truncated.
func ProjectID(root string) string {
	canonical, err := filepath.EvalSymlinks(root)
	if err != nil {
		if abs, absErr := filepath.Abs(root); absErr == nil {
			canonical = abs
		} else {
			canonical = root
		}
	}
	digest := sha256.Sum256([]byte(canonical))
	return hex.EncodeToString(digest[:8])
}

// Hex is the lowercase hex of bytes — sha256 digests and project ids.
func Hex(b []byte) string { return hex.EncodeToString(b) }

// ProjectDbName / ProjectUserName / ProjectBucketName are the per-project
// logical resource names handed to a recipe's provision/connection templates.
// The h_/u_ prefixes keep the values valid as database identifiers (leading
// letter, <=63 bytes, lowercase). S3 buckets reject `_`, so the bucket uses a
// hyphen.
func ProjectDbName(project string) string     { return "h_" + project }
func ProjectUserName(project string) string   { return "u_" + project }
func ProjectBucketName(project string) string { return "h-" + project }
