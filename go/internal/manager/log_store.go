// CursorLogStore — per-service append-only log files with crash-safe rotation
// and a cursor/generation tailing protocol for `GET /v1/logs/:id`.
//
// Sharp edges (AGENTS.md):
//  1. Log rotation is crash-safe via a two-phase journal (pending → renamed →
//     committed → generation-counter updated → journal deleted), reconciled on
//     next load by checking whether the rotated file already exists even if
//     the journal never reached committed.
//  2. Cursor/generation log-tailing must never split a UTF-8 sequence across a
//     read boundary, and must signal reset=true on a stale generation or an
//     invalid/out-of-range cursor.
package manager

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync"

	"github.com/ngosangns/hearth/go/internal/fileio"
	"github.com/ngosangns/hearth/go/internal/state"
	"github.com/ngosangns/hearth/go/internal/syncx"
)

const (
	DefaultLogTailBytes     uint64 = 16 * 1024
	DefaultLogMaxBytes      uint64 = 256 * 1024
	DefaultLogRotationCount        = 2
	logStreamStateVersion          = 1
)

var ErrLogTooLarge = errors.New("log append exceeds byte limit")

// LogCursorGeneration packs lifecycle (restart count) and rotation (file roll
// count) into the single generation clients echo — a follower resets when
// either changes.
func LogCursorGeneration(lifecycle, rotation uint64) uint64 {
	g := lifecycle * 1_000_000
	if g < lifecycle { // saturated
		g = ^uint64(0)
	}
	sum := g + rotation
	if sum < g {
		sum = ^uint64(0)
	}
	return sum
}

type logStreamState struct {
	Version     uint32            `json:"version"`
	Generations map[string]uint64 `json:"generations"`
}

type logRotationJournal struct {
	Version        uint32 `json:"version"`
	ServiceID      string `json:"serviceId"`
	FromGeneration uint64 `json:"fromGeneration"`
	ToGeneration   uint64 `json:"toGeneration"`
	Phase          string `json:"phase"`
}

type CursorLogStore struct {
	io              fileio.FileIO
	directory       string
	latestTailBytes uint64
	maxBytes        uint64
	rotationCount   int
	streamStatePath string

	genMu               sync.Mutex
	rotationGenerations map[string]uint64
	appendQueues        *syncx.KeyedLock[string]
	metadataSerial      sync.Mutex
	loadOnce            sync.Once
}

func NewCursorLogStore(io fileio.FileIO, directory string, latestTailBytes, maxBytes uint64, rotationCount int) *CursorLogStore {
	if latestTailBytes == 0 {
		latestTailBytes = DefaultLogTailBytes
	}
	if maxBytes == 0 {
		maxBytes = DefaultLogMaxBytes
	}
	if rotationCount <= 0 {
		rotationCount = DefaultLogRotationCount
	}
	return &CursorLogStore{
		io:                  io,
		directory:           directory,
		latestTailBytes:     latestTailBytes,
		maxBytes:            maxBytes,
		rotationCount:       rotationCount,
		streamStatePath:     filepath.Join(directory, "streams.json"),
		rotationGenerations: map[string]uint64{},
		appendQueues:        syncx.NewKeyedLock[string](),
	}
}

func (s *CursorLogStore) Append(serviceID, data string) error {
	unlock := s.appendQueues.Lock(serviceID)
	defer unlock()
	s.ensureLoaded()
	encoded := []byte(data)
	if uint64(len(encoded)) > s.maxBytes {
		return fmt.Errorf("%w: %d", ErrLogTooLarge, s.maxBytes)
	}
	if err := s.io.EnsureDirectory(s.directory); err != nil {
		return err
	}
	path := s.pathFor(serviceID)
	currentSize := s.size(path)
	if currentSize > 0 && currentSize+uint64(len(encoded)) > s.maxBytes {
		if err := s.rotate(serviceID); err != nil {
			return err
		}
	}
	// Appends bypass FileIO (which always rewrites atomically) — an
	// append-in-place is safe because only this store's own serialized
	// per-service queue ever writes this path.
	f, err := os.OpenFile(path, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o600)
	if err != nil {
		return err
	}
	defer f.Close()
	_ = f.Chmod(0o600)
	_, err = f.Write(encoded)
	return err
}

func (s *CursorLogStore) Read(serviceID string, cursor *uint64, limit *uint64, lifecycleGeneration uint64, requestedGeneration *uint64) state.LogSlice {
	unlock := s.appendQueues.Lock(serviceID)
	defer unlock()
	s.ensureLoaded()
	path := s.pathFor(serviceID)
	safeLimit := s.latestTailBytes
	if limit != nil {
		safeLimit = *limit
		if safeLimit < 1 {
			safeLimit = 1
		}
		if safeLimit > s.latestTailBytes {
			safeLimit = s.latestTailBytes
		}
	}
	s.genMu.Lock()
	rotation := s.rotationGenerations[serviceID]
	s.genMu.Unlock()
	if rotation == 0 {
		rotation = 1
	}
	generation := LogCursorGeneration(lifecycleGeneration, rotation)
	staleGeneration := requestedGeneration != nil && *requestedGeneration != generation
	// One handle for the whole request: the cursor check, and a single window
	// read. Appends and rotations hold this same per-service lock, so its view
	// of the file cannot shift.
	var file *os.File
	if f, err := os.Open(path); err == nil {
		file = f
		defer file.Close()
	}
	var size uint64
	if file != nil {
		if info, err := file.Stat(); err == nil {
			size = uint64(info.Size())
		}
	}
	invalidCursor := cursor != nil && (*cursor > size || !isUTF8Boundary(file, *cursor, size))
	reset := staleGeneration || invalidCursor
	var start, bytesRead uint64
	var data string
	switch {
	case cursor != nil && file != nil && !reset && *cursor < size:
		start = *cursor
		data, bytesRead = readFramed(file, *cursor, size, safeLimit)
	case cursor != nil && !reset:
		start = *cursor
	case file != nil:
		start, data, bytesRead = readTail(file, size, safeLimit)
	}
	return state.LogSlice{
		ServiceID:  serviceID,
		Generation: generation,
		Cursor:     start,
		NextCursor: start + bytesRead,
		Data:       data,
		Reset:      reset,
		Truncated:  start > 0,
	}
}

func (s *CursorLogStore) ensureLoaded() { s.loadOnce.Do(s.load) }

func (s *CursorLogStore) load() {
	if raw, err := s.io.ReadFile(s.streamStatePath); err == nil && raw != nil {
		var value struct {
			Version     uint32            `json:"version"`
			Generations map[string]uint64 `json:"generations"`
		}
		if err := json.Unmarshal([]byte(*raw), &value); err != nil {
			_ = s.io.Quarantine(s.streamStatePath, "corrupt")
		} else if value.Version == logStreamStateVersion {
			s.genMu.Lock()
			for k, v := range value.Generations {
				s.rotationGenerations[k] = v
			}
			s.genMu.Unlock()
		}
	}
	s.reconcileRotationJournals()
}

func (s *CursorLogStore) reconcileRotationJournals() {
	entries, err := os.ReadDir(s.directory)
	if err != nil {
		return
	}
	changed := false
	var journalsToDelete []string
	for _, e := range entries {
		name := e.Name()
		if !strings.HasSuffix(name, ".log.rotation.json") {
			continue
		}
		path := filepath.Join(s.directory, name)
		var journal logRotationJournal
		valid := false
		if raw, rerr := s.io.ReadFile(path); rerr == nil && raw != nil {
			if jerr := json.Unmarshal([]byte(*raw), &journal); jerr == nil {
				valid = true
			}
		}
		if valid && journal.Version == logStreamStateVersion {
			rotatedPath := fmt.Sprintf("%s.%d", s.pathFor(journal.ServiceID), journal.FromGeneration)
			_, statErr := os.Stat(rotatedPath)
			renamed := statErr == nil
			if journal.Phase == "committed" || renamed {
				s.genMu.Lock()
				current := s.rotationGenerations[journal.ServiceID]
				if current == 0 {
					current = 1
				}
				if current < journal.ToGeneration {
					s.rotationGenerations[journal.ServiceID] = journal.ToGeneration
					changed = true
				}
				s.genMu.Unlock()
			}
			journalsToDelete = append(journalsToDelete, path)
		} else {
			_ = s.io.Quarantine(path, "corrupt")
		}
	}
	if changed {
		s.writeStreamState()
	}
	for _, path := range journalsToDelete {
		_ = s.io.RemoveFile(path)
	}
}

func (s *CursorLogStore) writeStreamState() {
	s.genMu.Lock()
	generations := make(map[string]uint64, len(s.rotationGenerations))
	for k, v := range s.rotationGenerations {
		generations[k] = v
	}
	s.genMu.Unlock()
	if text, err := json.Marshal(&logStreamState{Version: logStreamStateVersion, Generations: generations}); err == nil {
		_ = s.io.WriteFile(s.streamStatePath, string(text))
	}
}

func (s *CursorLogStore) saveGenerationAfterRotation(serviceID string, generation uint64) {
	s.metadataSerial.Lock()
	defer s.metadataSerial.Unlock()
	s.genMu.Lock()
	s.rotationGenerations[serviceID] = generation
	s.genMu.Unlock()
	s.writeStreamState()
}

func (s *CursorLogStore) pathFor(serviceID string) string {
	return filepath.Join(s.directory, serviceID+".log")
}

func (s *CursorLogStore) size(path string) uint64 {
	if info, err := os.Stat(path); err == nil {
		return uint64(info.Size())
	}
	return 0
}

func (s *CursorLogStore) rotate(serviceID string) error {
	path := s.pathFor(serviceID)
	s.genMu.Lock()
	fromGeneration := s.rotationGenerations[serviceID]
	if fromGeneration == 0 {
		fromGeneration = 1
	}
	s.genMu.Unlock()
	journalPath := path + ".rotation.json"
	journal := logRotationJournal{
		Version:        logStreamStateVersion,
		ServiceID:      serviceID,
		FromGeneration: fromGeneration,
		ToGeneration:   fromGeneration + 1,
		Phase:          "pending",
	}
	rotatedPath := fmt.Sprintf("%s.%d", path, fromGeneration)
	if text, err := json.Marshal(&journal); err == nil {
		if err := s.io.WriteFile(journalPath, string(text)); err != nil {
			return err
		}
	}
	// A second writer (another daemon sharing this runtime directory) may have
	// rotated the same file first; the log is simply already gone, so appending
	// recreates it. Erroring here would reject the append that triggered the
	// rotation.
	if err := os.Rename(path, rotatedPath); err != nil && !os.IsNotExist(err) {
		return err
	}
	journal.Phase = "committed"
	if text, err := json.Marshal(&journal); err == nil {
		if err := s.io.WriteFile(journalPath, string(text)); err != nil {
			return err
		}
	}

	prefix := filepath.Base(s.pathFor(serviceID)) + "."
	var rotated []string
	if entries, err := os.ReadDir(s.directory); err == nil {
		for _, e := range entries {
			name := e.Name()
			if strings.HasPrefix(name, prefix) && !strings.HasSuffix(name, ".rotation.json") {
				rotated = append(rotated, name)
			}
		}
	} else {
		return err
	}
	// Sort by the NUMERIC generation suffix, not lexicographically — a plain
	// string sort orders `.1, .10, .11, .2, …` and would delete the newest
	// generations once a service passed 9 rotations.
	sort.Slice(rotated, func(i, j int) bool {
		return genSuffix(rotated[i]) < genSuffix(rotated[j])
	})
	excess := len(rotated) - s.rotationCount
	for i := 0; i < excess && i < len(rotated); i++ {
		_ = s.io.RemoveFile(filepath.Join(s.directory, rotated[i]))
	}
	s.saveGenerationAfterRotation(serviceID, journal.ToGeneration)
	_ = s.io.RemoveFile(journalPath)
	return nil
}

func genSuffix(name string) uint64 {
	idx := strings.LastIndex(name, ".")
	if idx < 0 {
		return 0
	}
	n, _ := strconv.ParseUint(name[idx+1:], 10, 64)
	return n
}

func readAt(file *os.File, offset uint64, buffer []byte) int {
	if _, err := file.Seek(int64(offset), 0); err != nil {
		return 0
	}
	filled := 0
	for filled < len(buffer) {
		n, err := file.Read(buffer[filled:])
		if n > 0 {
			filled += n
		}
		if n == 0 || err != nil {
			break
		}
	}
	return filled
}

func isUTF8Continuation(b byte) bool { return (b & 0xc0) == 0x80 }

func isUTF8Boundary(file *os.File, cursor, size uint64) bool {
	if cursor == 0 || cursor == size {
		return true
	}
	if file == nil {
		return false
	}
	var b [1]byte
	return readAt(file, cursor, b[:]) == 1 && !isUTF8Continuation(b[0])
}

// readTail reads the last `limit` bytes, moved forward past any leading
// continuation bytes so the slice starts on a character boundary.
// Returns (start, data, bytesRead).
func readTail(file *os.File, size, limit uint64) (uint64, string, uint64) {
	var windowStart uint64
	if size > limit {
		windowStart = size - limit
	}
	buffer := make([]byte, size-windowStart)
	read := readAt(file, windowStart, buffer)
	skip := 0
	for skip < read && isUTF8Continuation(buffer[skip]) {
		skip++
	}
	data := strings.ToValidUTF8(string(buffer[skip:read]), "\uFFFD")
	return windowStart + uint64(skip), data, uint64(read - skip)
}

// readFramed reads up to `limit` bytes starting at `start`, never splitting a
// multi-byte UTF-8 sequence across the end of the returned slice.
func readFramed(file *os.File, start, size, limit uint64) (string, uint64) {
	maximum := size - start
	if limit+3 < maximum {
		maximum = limit + 3
	}
	buffer := make([]byte, maximum)
	bytesRead := uint64(readAt(file, start, buffer))
	end := bytesRead
	if limit < end {
		end = limit
	}
	for end < bytesRead && isUTF8Continuation(buffer[end]) {
		end++
	}
	if end < bytesRead && end > 0 && (buffer[end-1]&0xe0) == 0xc0 {
		end = min64(bytesRead, end+1)
	}
	if end < bytesRead && end > 0 && (buffer[end-1]&0xf0) == 0xe0 {
		end = min64(bytesRead, end+2)
	}
	if end < bytesRead && end > 0 && (buffer[end-1]&0xf8) == 0xf0 {
		end = min64(bytesRead, end+3)
	}
	for end < bytesRead && isUTF8Continuation(buffer[end]) {
		end++
	}
	return strings.ToValidUTF8(string(buffer[:end]), "\uFFFD"), end
}

func min64(a, b uint64) uint64 {
	if a < b {
		return a
	}
	return b
}
