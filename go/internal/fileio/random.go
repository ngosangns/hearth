package fileio

import (
	"crypto/rand"
	"encoding/hex"
)

func randomHex8() string {
	var b [8]byte
	if _, err := rand.Read(b[:]); err != nil {
		panic(err)
	}
	return hex.EncodeToString(b[:])
}
