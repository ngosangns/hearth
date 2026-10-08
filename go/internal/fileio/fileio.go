// Package fileio ports Rust's PlainFileIo/GuardedFileIo strategies.
// Guarded adds O_NOFOLLOW + dev/ino identity tracking for private files.
package fileio

import (
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"syscall"
	"time"
)

type UnsafeError struct{ Path string }

func (e *UnsafeError) Error() string { return "Unsafe private file: " + e.Path }

type UnsafeDirectoryError struct{ Path string }

func (e *UnsafeDirectoryError) Error() string {
	return "Refusing unsafe private directory: " + e.Path
}

// FileIO abstracts every lock/state/log file operation.
type FileIO interface {
	Guarded() bool
	EnsureDirectory(path string) error
	IsPrivateDirectory(path string) bool
	ReadFile(path string) (*string, error)
	// WriteFile writes atomically via temp file + rename + fsync.
	WriteFile(path, content string) error
	// CreateExclusive creates-if-absent; returns true if created.
	CreateExclusive(path, content string) (bool, error)
	RemoveFile(path string) error
	Quarantine(path, suffix string) error
	AgeMs(path string) uint64
}

func quarantineSuffixPath(path, suffix string) string {
	return fmt.Sprintf("%s.%s-%d-%s", path, suffix, time.Now().UnixMilli(), randomHex8())
}

func writeDurable(f *os.File, content string) error {
	if _, err := f.WriteString(content); err != nil {
		return err
	}
	return f.Sync()
}

func tempPath(path string) string {
	return fmt.Sprintf("%s.tmp-%d-%s", path, os.Getpid(), randomHex8())
}

func ageMsOf(path string) uint64 {
	info, err := os.Lstat(path)
	if err != nil {
		return 0
	}
	d := time.Since(info.ModTime())
	if d < 0 {
		return 0
	}
	return uint64(d.Milliseconds())
}

// ---------------------------------------------------------------------------
// Plain strategy
// ---------------------------------------------------------------------------

type PlainFileIO struct{}

func (PlainFileIO) Guarded() bool { return false }

func (PlainFileIO) EnsureDirectory(path string) error {
	if err := os.MkdirAll(path, 0o700); err != nil {
		return err
	}
	return os.Chmod(path, 0o700)
}

func (PlainFileIO) IsPrivateDirectory(path string) bool {
	info, err := os.Lstat(path)
	return err == nil && info.IsDir()
}

func (PlainFileIO) ReadFile(path string) (*string, error) {
	data, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	s := string(data)
	return &s, nil
}

func (PlainFileIO) WriteFile(path, content string) error {
	parent := filepath.Dir(path)
	if parent != "" && parent != "." {
		if err := os.MkdirAll(parent, 0o700); err != nil {
			return err
		}
		if err := os.Chmod(parent, 0o700); err != nil {
			return err
		}
	}
	tmp := tempPath(path)
	f, err := os.OpenFile(tmp, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, 0o600)
	if err != nil {
		return err
	}
	if err := writeDurable(f, content); err != nil {
		f.Close()
		os.Remove(tmp)
		return err
	}
	f.Close()
	if err := os.Rename(tmp, path); err != nil {
		os.Remove(tmp)
		return err
	}
	return nil
}

func (PlainFileIO) CreateExclusive(path, content string) (bool, error) {
	parent := filepath.Dir(path)
	if parent != "" && parent != "." {
		if err := os.MkdirAll(parent, 0o700); err != nil {
			return false, err
		}
		if err := os.Chmod(parent, 0o700); err != nil {
			return false, err
		}
	}
	f, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if errors.Is(err, fs.ErrExist) {
		return false, nil
	}
	if err != nil {
		return false, err
	}
	if _, err := f.WriteString(content); err != nil {
		f.Close()
		return false, err
	}
	return true, f.Close()
}

func (PlainFileIO) RemoveFile(path string) error {
	err := os.Remove(path)
	if errors.Is(err, fs.ErrNotExist) {
		return nil
	}
	return err
}

func (PlainFileIO) Quarantine(path, suffix string) error {
	_ = os.Rename(path, quarantineSuffixPath(path, suffix))
	return nil
}

func (PlainFileIO) AgeMs(path string) uint64 { return ageMsOf(path) }

// ---------------------------------------------------------------------------
// Guarded strategy — O_NOFOLLOW + dev/ino identity tracking
// ---------------------------------------------------------------------------

type GuardedFileIO struct{}

func (GuardedFileIO) Guarded() bool { return true }

type identity struct {
	dev uint64
	ino uint64
}

func isPrivateMode(mode fs.FileMode) bool { return mode&0o077 == 0 }

func isPrivateRegular(info fs.FileInfo) bool {
	return info.Mode().IsRegular() &&
		info.Mode()&fs.ModeSymlink == 0 &&
		isPrivateMode(info.Mode()) &&
		info.Sys().(*syscall.Stat_t).Nlink == 1
}

func identityOf(path string) (*identity, error) {
	info, err := os.Lstat(path)
	if errors.Is(err, fs.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	if !isPrivateRegular(info) {
		return nil, &UnsafeError{Path: path}
	}
	st := info.Sys().(*syscall.Stat_t)
	return &identity{dev: uint64(st.Dev), ino: st.Ino}, nil
}

func validateHandle(path string, f *os.File, expected *identity) (*identity, error) {
	info, err := f.Stat()
	if err != nil {
		return nil, err
	}
	st := info.Sys().(*syscall.Stat_t)
	found := &identity{dev: uint64(st.Dev), ino: st.Ino}
	if !isPrivateRegular(info) || (expected != nil && *expected != *found) {
		return nil, &UnsafeError{Path: path}
	}
	return found, nil
}

func openExisting(path string, write bool) (*os.File, error) {
	before, err := identityOf(path)
	if err != nil {
		return nil, err
	}
	if before == nil {
		return nil, &UnsafeError{Path: path}
	}
	flag := os.O_RDONLY
	if write {
		flag = os.O_RDWR
	}
	f, err := os.OpenFile(path, flag|syscall.O_NOFOLLOW, 0)
	if err != nil {
		return nil, err
	}
	if _, err := validateHandle(path, f, before); err != nil {
		f.Close()
		return nil, err
	}
	return f, nil
}

func createExclusiveHandle(path string) (*os.File, error) {
	f, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_EXCL|syscall.O_NOFOLLOW, 0o600)
	if errors.Is(err, fs.ErrExist) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	if _, err := validateHandle(path, f, nil); err != nil {
		f.Close()
		return nil, err
	}
	return f, nil
}

func openRegular(path string, write, create bool) (*os.File, error) {
	id, err := identityOf(path)
	if err != nil {
		return nil, err
	}
	if id != nil {
		return openExisting(path, write)
	}
	if !create {
		return nil, &UnsafeError{Path: path}
	}
	f, err := createExclusiveHandle(path)
	if err != nil {
		return nil, err
	}
	if f != nil {
		return f, nil
	}
	return openExisting(path, write)
}

func isPrivateDirectoryGuarded(path string) bool {
	info, err := os.Lstat(path)
	return err == nil && info.IsDir() && info.Mode()&fs.ModeSymlink == 0 && isPrivateMode(info.Mode())
}

func (GuardedFileIO) EnsureDirectory(path string) error {
	if isPrivateDirectoryGuarded(path) {
		return nil
	}
	if err := os.MkdirAll(path, 0o700); err != nil && !errors.Is(err, fs.ErrExist) {
		return err
	}
	if err := os.Chmod(path, 0o700); err != nil {
		return err
	}
	if !isPrivateDirectoryGuarded(path) {
		return &UnsafeDirectoryError{Path: path}
	}
	return nil
}

func (GuardedFileIO) IsPrivateDirectory(path string) bool {
	return isPrivateDirectoryGuarded(path)
}

func (GuardedFileIO) ReadFile(path string) (*string, error) {
	id, err := identityOf(path)
	if err != nil {
		return nil, err
	}
	if id == nil {
		return nil, nil
	}
	f, err := openRegular(path, false, false)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	data, err := io.ReadAll(f)
	if err != nil {
		return nil, err
	}
	s := string(data)
	return &s, nil
}

func (GuardedFileIO) WriteFile(path, content string) error {
	parent := filepath.Dir(path)
	if parent != "" && parent != "." {
		if err := (GuardedFileIO{}).EnsureDirectory(parent); err != nil {
			return err
		}
	}
	tmp := tempPath(path)
	f, err := openRegular(tmp, true, true)
	if err != nil {
		return err
	}
	werr := writeDurable(f, content)
	f.Close()
	if werr != nil {
		os.Remove(tmp)
		return werr
	}
	if err := os.Rename(tmp, path); err != nil {
		os.Remove(tmp)
		return err
	}
	id, err := identityOf(path)
	if err != nil || id == nil {
		os.Remove(tmp)
		if err == nil {
			err = &UnsafeError{Path: path}
		}
		return err
	}
	return nil
}

func (GuardedFileIO) CreateExclusive(path, content string) (bool, error) {
	parent := filepath.Dir(path)
	if parent != "" && parent != "." {
		if err := (GuardedFileIO{}).EnsureDirectory(parent); err != nil {
			return false, err
		}
	}
	f, err := createExclusiveHandle(path)
	if err != nil {
		return false, err
	}
	if f == nil {
		return false, nil
	}
	if _, err := f.WriteString(content); err != nil {
		f.Close()
		return false, err
	}
	return true, f.Close()
}

func (GuardedFileIO) RemoveFile(path string) error {
	id, err := identityOf(path)
	if err != nil {
		return err
	}
	if id == nil {
		return nil
	}
	return os.Remove(path)
}

func (GuardedFileIO) Quarantine(path, suffix string) error {
	id, err := identityOf(path)
	if err != nil {
		return err
	}
	if id == nil {
		return nil
	}
	return os.Rename(path, quarantineSuffixPath(path, suffix))
}

func (GuardedFileIO) AgeMs(path string) uint64 { return ageMsOf(path) }

func New(guarded bool) FileIO {
	if guarded {
		return GuardedFileIO{}
	}
	return PlainFileIO{}
}

func RemoveDirectory(path string) error {
	err := os.RemoveAll(path)
	if errors.Is(err, fs.ErrNotExist) {
		return nil
	}
	return err
}
