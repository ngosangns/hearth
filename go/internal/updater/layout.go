// Ported from rust/crates/hearth-cli/src/update.rs: the install layout, atomic swap, and lock.
package updater

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"syscall"

	"github.com/google/uuid"
)

type layout struct {
	link string
	dir  string
}

// layoutFromHome is kept local rather than reusing internal/paths: paths covers the per-project
// runtime directory, not the ~/.local install layout.
func layoutFromHome(home string) layout {
	return layout{
		link: filepath.Join(home, ".local", "bin", "hearth"),
		dir:  filepath.Join(home, ".local", "share", "hearth", "bin"),
	}
}

func (l layout) versioned(version string) string {
	return filepath.Join(l.dir, "hearth-"+version)
}

func installOwned(currentExe string, l layout) bool {
	if pathIsInside(currentExe, l.dir) {
		return true
	}
	info, err := os.Lstat(l.link)
	if err == nil && info.Mode().IsRegular() {
		exe := canonicalOr(currentExe)
		link := canonicalOr(l.link)
		return exe == link
	}
	return false
}

func pathIsInside(path, dir string) bool {
	p := splitComponents(canonicalOr(path))
	d := splitComponents(canonicalOr(dir))
	if len(p) <= len(d) {
		return false
	}
	for i := range d {
		if p[i] != d[i] {
			return false
		}
	}
	return true
}

func canonicalOr(path string) string {
	resolved, err := filepath.EvalSymlinks(path)
	if err != nil {
		return path
	}
	return resolved
}

func splitComponents(path string) []string {
	return strings.Split(filepath.Clean(path), string(filepath.Separator))
}

type fileIdentity struct {
	dev uint64
	ino uint64
}

func fileID(path string) (*fileIdentity, error) {
	info, err := os.Lstat(path)
	if err != nil {
		if os.IsNotExist(err) {
			return nil, nil
		}
		return nil, err
	}
	stat, ok := info.Sys().(*syscall.Stat_t)
	if !ok {
		return nil, fmt.Errorf("cannot stat %s: unsupported file info", path)
	}
	return &fileIdentity{dev: uint64(stat.Dev), ino: uint64(stat.Ino)}, nil
}

func sameFileID(a, b *fileIdentity) bool {
	if a == nil || b == nil {
		return a == nil && b == nil
	}
	return a.dev == b.dev && a.ino == b.ino
}

func ensureLinkUnchanged(link string, expected *fileIdentity) error {
	now, err := fileID(link)
	if err != nil {
		return fmt.Errorf("cannot stat %s: %v", link, err)
	}
	if !sameFileID(now, expected) {
		return errors.New("install path changed while updating")
	}
	return nil
}

// undoRotation puts a rotated install back. previous is the file rotateIntoPlace moved aside;
// the new bytes return to partial so the partial guard can delete them.
func undoRotation(partial, dest string, previous *string, sameVersion bool) {
	if previous != nil {
		_ = os.Rename(dest, partial)
		_ = os.Rename(*previous, dest)
	} else if !sameVersion {
		_ = os.Remove(dest)
	}
}

type partialFile struct{ path string }

func (p *partialFile) remove() {
	_ = os.Remove(p.path)
}

// rotateIntoPlace moves source onto dest by renaming. An existing dest is renamed aside first so
// a process that mapped it keeps that inode.
func rotateIntoPlace(source, dest string) (*string, error) {
	parent := filepath.Dir(dest)
	if err := os.MkdirAll(parent, 0o755); err != nil {
		return nil, fmt.Errorf("cannot create %s: %v", parent, err)
	}
	var previous *string
	if _, err := os.Lstat(dest); err == nil {
		backup := filepath.Join(filepath.Dir(dest), filepath.Base(dest)+".previous")
		if _, err := os.Lstat(backup); err == nil {
			if err := os.Remove(backup); err != nil {
				return nil, fmt.Errorf("cannot replace %s: %v", backup, err)
			}
		}
		if err := os.Rename(dest, backup); err != nil {
			return nil, fmt.Errorf("cannot move %s aside: %v", dest, err)
		}
		previous = &backup
	}
	if err := os.Rename(source, dest); err != nil {
		if previous != nil {
			_ = os.Rename(*previous, dest)
		}
		return nil, fmt.Errorf("cannot install %s: %v", dest, err)
	}
	return previous, nil
}

type linkBackupKind int

const (
	linkBackupMissing linkBackupKind = iota
	linkBackupSymlink
	linkBackupRenamedFile
)

type linkBackup struct {
	kind   linkBackupKind
	target string
	aside  string
}

func publishLink(l layout, versioned string) (linkBackup, error) {
	parent := filepath.Dir(l.link)
	if err := os.MkdirAll(parent, 0o755); err != nil {
		return linkBackup{}, fmt.Errorf("cannot create %s: %v", parent, err)
	}
	relative := relativeFrom(parent, versioned)
	var backup linkBackup
	info, err := os.Lstat(l.link)
	switch {
	case os.IsNotExist(err):
		backup = linkBackup{kind: linkBackupMissing}
	case err != nil:
		return linkBackup{}, fmt.Errorf("cannot stat %s: %v", l.link, err)
	case info.Mode()&os.ModeSymlink != 0:
		target, err := os.Readlink(l.link)
		if err != nil {
			return linkBackup{}, fmt.Errorf("cannot read %s: %v", l.link, err)
		}
		backup = linkBackup{kind: linkBackupSymlink, target: target}
	case info.Mode().IsRegular():
		aside := uniqueAside(l.dir)
		if err := os.Rename(l.link, aside); err != nil {
			return linkBackup{}, fmt.Errorf("cannot move %s aside: %v", l.link, err)
		}
		backup = linkBackup{kind: linkBackupRenamedFile, aside: aside}
	default:
		return linkBackup{}, fmt.Errorf("%s is not a file or a symlink", l.link)
	}
	if err := placeSymlink(l.link, relative); err != nil {
		_ = restoreLink(l.link, backup)
		return linkBackup{}, err
	}
	return backup, nil
}

func restoreLink(link string, backup linkBackup) error {
	switch backup.kind {
	case linkBackupMissing:
		if _, err := os.Lstat(link); err == nil {
			if err := os.Remove(link); err != nil {
				return fmt.Errorf("cannot remove %s: %v", link, err)
			}
		}
		return nil
	case linkBackupSymlink:
		return placeSymlink(link, backup.target)
	default:
		if _, err := os.Lstat(link); err == nil {
			if err := os.Remove(link); err != nil {
				return fmt.Errorf("cannot remove %s: %v", link, err)
			}
		}
		if err := os.Rename(backup.aside, link); err != nil {
			return fmt.Errorf("cannot restore %s: %v", link, err)
		}
		return nil
	}
}

func removeLegacyCommandLink(l layout) {
	parent := filepath.Dir(l.link)
	legacy := filepath.Join(parent, "hearthd")
	if legacy == l.link {
		return
	}
	info, err := os.Lstat(legacy)
	if err != nil {
		return
	}
	if info.Mode()&os.ModeSymlink != 0 {
		_ = os.Remove(legacy)
	}
}

func placeSymlink(link, target string) error {
	parent := filepath.Dir(link)
	if err := os.MkdirAll(parent, 0o755); err != nil {
		return fmt.Errorf("cannot create %s: %v", parent, err)
	}
	tmp := filepath.Join(parent, ".hearth-link-"+uuid.NewString())
	if err := os.Symlink(target, tmp); err != nil {
		return fmt.Errorf("cannot symlink %s: %v", link, err)
	}
	if err := os.Rename(tmp, link); err != nil {
		_ = os.Remove(tmp)
		return fmt.Errorf("cannot replace %s: %v", link, err)
	}
	return nil
}

func uniqueAside(dir string) string {
	path := filepath.Join(dir, fmt.Sprintf("hearth-previous-%d", os.Getpid()))
	if _, err := os.Lstat(path); err == nil {
		return filepath.Join(dir, fmt.Sprintf("hearth-previous-%d-%s", os.Getpid(), uuid.NewString()))
	}
	return path
}

func relativeFrom(base, target string) string {
	b := splitComponents(base)
	t := splitComponents(target)
	shared := 0
	for shared < len(b) && shared < len(t) && b[shared] == t[shared] {
		shared++
	}
	out := make([]string, 0, len(b)-shared+len(t)-shared)
	for i := shared; i < len(b); i++ {
		out = append(out, "..")
	}
	out = append(out, t[shared:]...)
	if len(out) == 0 {
		return "."
	}
	return filepath.Join(out...)
}

func versionFromName(path string) (string, bool) {
	name := filepath.Base(path)
	version, ok := strings.CutPrefix(name, "hearth-")
	if !ok {
		return "", false
	}
	if _, ok := parseSemver(version); !ok {
		return "", false
	}
	return version, true
}

func pruneOldVersions(dir string, keep []string) {
	entries, err := os.ReadDir(dir)
	if err != nil {
		return
	}
	for _, entry := range entries {
		name := entry.Name()
		version, ok := strings.CutPrefix(name, "hearth-")
		if !ok {
			continue
		}
		if _, ok := parseSemver(version); ok && !containsString(keep, version) {
			_ = os.Remove(filepath.Join(dir, name))
		}
	}
}

func containsString(items []string, want string) bool {
	for _, item := range items {
		if item == want {
			return true
		}
	}
	return false
}

type updateLock struct {
	file *os.File
}

func acquireUpdateLock(dir string) (*updateLock, error) {
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return nil, fmt.Errorf("cannot create %s: %v", dir, err)
	}
	path := filepath.Join(dir, ".update.lock")
	file, err := os.OpenFile(path, os.O_RDWR|os.O_CREATE, 0o644)
	if err != nil {
		return nil, fmt.Errorf("cannot open %s: %v", path, err)
	}
	if err := syscall.Flock(int(file.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		_ = file.Close()
		if errors.Is(err, syscall.EWOULDBLOCK) || errors.Is(err, syscall.EAGAIN) {
			return nil, errors.New("hearth update is already running")
		}
		return nil, fmt.Errorf("cannot lock %s: %v", path, err)
	}
	return &updateLock{file: file}, nil
}

func (l *updateLock) release() {
	_ = syscall.Flock(int(l.file.Fd()), syscall.LOCK_UN)
	_ = l.file.Close()
}
