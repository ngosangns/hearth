// The SSE half of the daemon client.
//
// Ported from rust/crates/hearth-tui/src/client.rs: the `watch()` reconnect
// loop, the `WatchEvent`/`EventReplay` frames, the hand-rolled
// `eventsource-stream` decode (multi-line `data:` joined, comments dropped,
// UTF-8 split across chunks intact) and the `SseFrameCap` 64 KiB frame bound
// the crate does not enforce.
package client

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strconv"
	"strings"
	"time"

	"github.com/ngosangns/hearth/go/internal/state"
)

const (
	// MaxSSEFrameBytes bounds one SSE frame's bytes. A frame that outgrows it
	// fails the stream.
	MaxSSEFrameBytes = 64 * 1024
	// LogTailBytes is the default log tail clients request.
	LogTailBytes uint64 = 16 * 1024
	// DaemonLogTailBytes is the daemon-log tail clients request.
	DaemonLogTailBytes uint64 = 16 * 1024
	// reconnectDelay is the SSE reconnect delay.
	reconnectDelay = 250 * time.Millisecond
	// eventChannelBuffer is the client-side frame queue depth (the server's
	// backpressure is 64 frames too).
	eventChannelBuffer = 64
)

// errFrameExceedsLimit fails a stream whose pending frame outgrows the cap.
var errFrameExceedsLimit = errors.New("event stream frame exceeds limit")

// errEventPayloadMalformed mirrors the old client's "event stream payload is malformed".
var errEventPayloadMalformed = errors.New("event stream payload is malformed")

// EventStream opens GET /v1/events/stream (SSE) and returns the response for
// the caller to read. It is never bounded by a transport timeout.
func (c *Client) EventStream(ctx context.Context, after *uint64, epoch *string) (*http.Response, error) {
	url := fmt.Sprintf("http://127.0.0.1:%d/v1/events/stream%s", c.Metadata.Port, eventsQuery(after, epoch))
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return nil, unavailableError("manager unavailable")
	}
	req.Header.Set("Authorization", "Bearer "+c.Token)
	req.Header.Set("x-hearth-protocol", strconv.FormatUint(uint64(state.ProtocolVersion), 10))
	req.Header.Set("Accept", "text/event-stream")
	resp, err := c.httpDoer().Do(req)
	if err != nil {
		return nil, unavailableError("manager unavailable")
	}
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		resp.Body.Close()
		return nil, unavailableError("event stream failed: " + resp.Status)
	}
	return resp, nil
}

// Events opens the SSE stream and returns a channel of decoded frames. The
// channel is closed when the stream ends or ctx is cancelled; a connection or
// decode failure is delivered as an EventKindUnavailable frame first.
//
// A replay frame with Reset true is the resync signal: the caller's cursor is
// unusable, so it must refetch /v1/services and resume from
// Replay.LatestSequence.
func (c *Client) Events(ctx context.Context, after *uint64, epoch *string) <-chan Event {
	out := make(chan Event, eventChannelBuffer)
	go func() {
		defer close(out)
		resp, err := c.EventStream(ctx, after, epoch)
		if err != nil {
			_ = sendEvent(ctx, out, Event{Kind: EventKindUnavailable, Message: err.Error()})
			return
		}
		defer resp.Body.Close()
		if err := decodeSSE(ctx, resp.Body, func(ev Event) error {
			return sendEvent(ctx, out, ev)
		}); err != nil {
			if ctx.Err() != nil {
				return
			}
			_ = sendEvent(ctx, out, Event{Kind: EventKindUnavailable, Message: err.Error()})
		}
	}()
	return out
}

// Watch runs the reconnect loop until ctx is done: on each attempt it emits
// BeginConnection, fetches a fresh /v1/services snapshot, then streams
// /v1/events/stream until it errors, emitting every step in order. It never
// returns early on a transport error — only ctx stops it.
func (c *Client) Watch(ctx context.Context, after *uint64, epoch *string) <-chan Event {
	out := make(chan Event, eventChannelBuffer)
	go func() {
		defer close(out)
		var cursor *uint64
		if after != nil {
			v := *after
			cursor = &v
		}
		var epochVal *string
		if epoch != nil {
			v := *epoch
			epochVal = &v
		}
		for {
			if ctx.Err() != nil {
				return
			}
			if sendEvent(ctx, out, Event{Kind: EventKindBeginConnection}) != nil {
				return
			}
			if err := c.watchOnce(ctx, &cursor, &epochVal, out); err != nil {
				if ctx.Err() != nil {
					return
				}
				if sendEvent(ctx, out, Event{Kind: EventKindUnavailable, Message: err.Error()}) != nil {
					return
				}
			}
			if ctx.Err() != nil {
				return
			}
			if err := sleepCtx(ctx, reconnectDelay); err != nil {
				return
			}
		}
	}()
	return out
}

func (c *Client) watchOnce(ctx context.Context, after **uint64, epoch **string, out chan<- Event) error {
	services, err := c.Services(ctx)
	if err != nil {
		return err
	}
	if err := sendEvent(ctx, out, Event{Kind: EventKindSnapshot, Snapshot: services}); err != nil {
		return err
	}
	return c.streamEvents(ctx, after, epoch, out)
}

func (c *Client) streamEvents(ctx context.Context, after **uint64, epoch **string, out chan<- Event) error {
	resp, err := c.EventStream(ctx, *after, *epoch)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	return decodeSSE(ctx, resp.Body, func(ev Event) error {
		switch ev.Kind {
		case EventKindReplay:
			epochValue := ev.Replay.Epoch
			*epoch = &epochValue
			if ev.Replay.Reset {
				sequence := ev.Replay.LatestSequence
				*after = &sequence
			}
		case EventKindManagerEvent:
			sequence := ev.Manager.Sequence
			*after = &sequence
		}
		return sendEvent(ctx, out, ev)
	})
}

func eventsQuery(after *uint64, epoch *string) string {
	var parts []string
	if after != nil {
		parts = append(parts, "after="+strconv.FormatUint(*after, 10))
	}
	if epoch != nil {
		parts = append(parts, "epoch="+EncodePathSegment(*epoch))
	}
	if len(parts) == 0 {
		return ""
	}
	return "?" + strings.Join(parts, "&")
}

func sendEvent(ctx context.Context, out chan<- Event, ev Event) error {
	select {
	case out <- ev:
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}

// decodeSSE reads an SSE byte stream and hands each decoded frame to sink. It
// fails as soon as a single frame's bytes pass MaxSSEFrameBytes.
func decodeSSE(ctx context.Context, r io.Reader, sink func(Event) error) error {
	frameCap := &sseFrameCap{}
	parser := &sseParser{}
	buf := make([]byte, 32*1024)
	for {
		if err := ctx.Err(); err != nil {
			return err
		}
		n, err := r.Read(buf)
		if n > 0 {
			chunk := buf[:n]
			if capErr := frameCap.feed(chunk, MaxSSEFrameBytes); capErr != nil {
				return capErr
			}
			if perr := parser.feed(chunk, func(ev sseEvent) error {
				frame, ferr := mapSSE(ev)
				if ferr != nil {
					return ferr
				}
				return sink(frame)
			}); perr != nil {
				return perr
			}
		}
		if err != nil {
			if errors.Is(err, io.EOF) {
				return nil
			}
			return err
		}
	}
}

func mapSSE(ev sseEvent) (Event, error) {
	if ev.Type == "replay" {
		var replay EventReplay
		if err := json.Unmarshal([]byte(ev.Data), &replay); err != nil {
			return Event{}, errEventPayloadMalformed
		}
		return Event{Kind: EventKindReplay, Replay: &replay}, nil
	}
	var managerEvent state.ManagerEvent
	if err := json.Unmarshal([]byte(ev.Data), &managerEvent); err != nil {
		return Event{}, errEventPayloadMalformed
	}
	return Event{Kind: EventKindManagerEvent, Manager: &managerEvent}, nil
}

// ---------------------------------------------------------------------------
// SSE frame cap
// ---------------------------------------------------------------------------

// sseFrameCap counts bytes in the current (not yet terminated) SSE frame
// across chunks, so a stream whose pending frame outgrows the cap fails —
// the limit lives on the raw bytes because the parser buffers internally.
type sseFrameCap struct {
	// pending is the bytes consumed since the last completed frame boundary.
	pending int
	// lastTerm is whether the last parsed unit was a complete line terminator.
	lastTerm bool
	// pendingCR is whether the last byte was a CR whose CRLF may continue into
	// the next byte.
	pendingCR bool
}

// feed returns errFrameExceedsLimit once the pending frame exceeds limit. A
// frame boundary is two adjacent line terminators, where each terminator is
// \n, \r, or \r\n — possibly split across chunks.
func (c *sseFrameCap) feed(chunk []byte, limit int) error {
	// Index after the last boundary completed inside this chunk, if any.
	boundary := -1
	for i, b := range chunk {
		switch b {
		case '\r':
			if c.lastTerm {
				boundary = i + 1
			}
			c.lastTerm = true
			c.pendingCR = true
		case '\n':
			if c.pendingCR {
				// Completes the CRLF; if that CR had just finished a boundary,
				// the boundary ends here instead.
				if boundary == i {
					boundary = i + 1
				}
				c.pendingCR = false
			} else {
				if c.lastTerm {
					boundary = i + 1
				}
				c.lastTerm = true
			}
		default:
			c.lastTerm = false
			c.pendingCR = false
		}
	}
	if boundary >= 0 {
		c.pending = len(chunk) - boundary
	} else {
		c.pending += len(chunk)
	}
	if c.pending > limit {
		return errFrameExceedsLimit
	}
	return nil
}

// ---------------------------------------------------------------------------
// SSE parser
// ---------------------------------------------------------------------------

// sseEvent is one decoded SSE frame.
type sseEvent struct {
	Type string
	Data string
	ID   string
}

// sseParser decodes the SSE wire format: lines terminated by \n, \r or \r\n,
// `field: value` pairs, comments (a leading `:`) dropped, multi-line `data:`
// joined with \n, and a frame dispatched on a blank line. A frame with no
// `data:` field emits nothing (SSE spec).
type sseParser struct {
	buf       []byte
	eventType string
	data      []string
	haveData  bool
	id        string
}

func (p *sseParser) feed(chunk []byte, emit func(sseEvent) error) error {
	p.buf = append(p.buf, chunk...)
	consumed := 0
	for consumed < len(p.buf) {
		rest := p.buf[consumed:]
		i := bytes.IndexAny(rest, "\r\n")
		if i < 0 {
			break
		}
		if rest[i] == '\r' && i == len(rest)-1 {
			// A CR at the end of the buffer may be the first half of a CRLF;
			// wait for the next chunk so the terminator is not split.
			break
		}
		line := rest[:i]
		j := i + 1
		if rest[i] == '\r' && j < len(rest) && rest[j] == '\n' {
			j++
		}
		consumed += j
		if err := p.line(string(line), emit); err != nil {
			return err
		}
	}
	if consumed > 0 {
		n := copy(p.buf, p.buf[consumed:])
		p.buf = p.buf[:n]
	}
	return nil
}

func (p *sseParser) line(line string, emit func(sseEvent) error) error {
	if line == "" {
		return p.dispatch(emit)
	}
	if strings.HasPrefix(line, ":") {
		return nil // comment / keep-alive
	}
	field, value, found := strings.Cut(line, ":")
	if found && strings.HasPrefix(value, " ") {
		value = value[1:]
	}
	switch field {
	case "event":
		p.eventType = value
	case "data":
		p.data = append(p.data, value)
		p.haveData = true
	case "id":
		p.id = value
	}
	return nil
}

func (p *sseParser) dispatch(emit func(sseEvent) error) error {
	if !p.haveData {
		p.eventType = ""
		p.data = nil
		return nil
	}
	ev := sseEvent{Type: p.eventType, Data: strings.Join(p.data, "\n"), ID: p.id}
	if ev.Type == "" {
		ev.Type = "message"
	}
	p.eventType = ""
	p.data = nil
	p.haveData = false
	return emit(ev)
}
