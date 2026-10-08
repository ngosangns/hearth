// Package paths defines the runtime directory layout shared with the Rust daemon.
package paths

import (
	"path/filepath"

	"github.com/ngosangns/hearth/go/internal/state"
)

const DefaultRuntimeDirectoryName = state.DefaultRuntimeDirName

func ResolveRuntimeDirectory(root, catalogRuntimeDirectory string) string {
	if catalogRuntimeDirectory == "" {
		catalogRuntimeDirectory = DefaultRuntimeDirectoryName
	}
	return filepath.Join(root, catalogRuntimeDirectory)
}

func LockDir(runtimeDirectory string) string {
	return filepath.Join(runtimeDirectory, "manager.lock")
}

func TokenPath(runtimeDirectory string) string {
	return filepath.Join(LockDir(runtimeDirectory), "token")
}

func MetadataPath(runtimeDirectory string) string {
	return filepath.Join(LockDir(runtimeDirectory), "metadata.json")
}

func ProofPath(runtimeDirectory string) string {
	return filepath.Join(LockDir(runtimeDirectory), state.LockOwnershipProofName)
}

func OwnershipKeyPath(runtimeDirectory string) string {
	return filepath.Join(runtimeDirectory, state.OwnershipKeyName)
}

func StatePath(runtimeDirectory string) string {
	return filepath.Join(runtimeDirectory, "state.json")
}

func LogsDir(runtimeDirectory string) string {
	return filepath.Join(runtimeDirectory, "logs")
}

func LogPath(runtimeDirectory, serviceID string) string {
	return filepath.Join(LogsDir(runtimeDirectory), serviceID+".log")
}

func RawLogPath(runtimeDirectory, serviceID string) string {
	return filepath.Join(LogsDir(runtimeDirectory), serviceID+".raw")
}

func InstallDir(runtimeDirectory, serviceID, version string) string {
	return filepath.Join(runtimeDirectory, "installs", serviceID, version)
}

func ServiceDataDir(runtimeDirectory, serviceID string) string {
	return filepath.Join(runtimeDirectory, "data", serviceID)
}

func DownloadsDir(runtimeDirectory string) string {
	return filepath.Join(runtimeDirectory, "downloads")
}
