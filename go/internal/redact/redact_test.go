package redact

import (
	"encoding/json"
	"reflect"
	"strings"
	"testing"

	"github.com/ngosangns/hearth/go/internal/omap"
)

func TestIsSecretKey(t *testing.T) {
	secret := []string{
		"SECRET", "MY_SECRET", "password", "DB_PASSWORD", "PASSWD",
		"GITHUB_TOKEN", "api_key", "api-key", "--api-key", "AWS_ACCESS_KEY_ID",
		"AWS_SECRET_ACCESS_KEY", "PRIVATE_KEY", "client_private_key",
		"MINIO_ROOT_PASSWORD", "--password",
	}
	for _, key := range secret {
		if !IsSecretKey(key) {
			t.Errorf("IsSecretKey(%q) = false, want true", key)
		}
	}
	plain := []string{"USER", "PORT", "HOST", "URL", "S3_BUCKET", "APIKEY", "KEY", "MINIO_ROOT_USER"}
	for _, key := range plain {
		if IsSecretKey(key) {
			t.Errorf("IsSecretKey(%q) = true, want false", key)
		}
	}
}

func TestMaskURL(t *testing.T) {
	cases := []struct {
		in, want string
	}{
		{"http://127.0.0.1:43100/bucket", "http://127.0.0.1:43100/bucket"},
		{"postgres://app@127.0.0.1:5432/db", "postgres://app@127.0.0.1:5432/db"},
		{"postgres://app:fake-secret-123@127.0.0.1:5432/db", "postgres://app:" + Mask + "@127.0.0.1:5432/db"},
		{"https://user:p%40ss@host/a?x=1#f", "https://user:" + Mask + "@host/a?x=1#f"},
		{"not a url", "not a url"},
		{"http://user:@host", "http://user:@host"},
		{"postgres://app:" + Mask + "@127.0.0.1/db", "postgres://app:" + Mask + "@127.0.0.1/db"},
		{"see postgres://app:fake-secret-123@127.0.0.1/db", "see postgres://app:fake-secret-123@127.0.0.1/db"},
	}
	for _, tc := range cases {
		if got := MaskURL(tc.in); got != tc.want {
			t.Errorf("MaskURL(%q) = %q, want %q", tc.in, got, tc.want)
		}
	}
}

func TestMaskEnvValuePrefersTheKey(t *testing.T) {
	got := MaskEnvValue("DATABASE_URL", "postgres://app:fake-secret-123@127.0.0.1/db")
	want := "postgres://app:" + Mask + "@127.0.0.1/db"
	if got != want {
		t.Fatalf("DATABASE_URL = %q, want %q", got, want)
	}
	if MaskEnvValue("AWS_SECRET_ACCESS_KEY", "postgres://app:fake-secret-123@h/db") != Mask {
		t.Fatal("a secret key must hide the whole value")
	}
}

func TestValuesMasksSecretsEverywhere(t *testing.T) {
	in := map[string]any{
		"token":    "fake-secret-123",
		"note":     "keep",
		"endpoint": "http://user:fake-secret-123@127.0.0.1:9/b",
		"env": map[string]any{
			"MINIO_ROOT_PASSWORD": "fake-root-pass-123",
			"MINIO_ROOT_USER":     "hearth",
		},
		"provisionArgs": map[string]any{
			"AWS_SECRET_ACCESS_KEY": "fake-secret-123",
			"S3_BUCKET":             "photos",
		},
		"argv": []any{
			"minio",
			"MINIO_ROOT_PASSWORD=fake-root-pass-123",
			"--password", "fake-root-pass-123",
			"--user", "hearth",
			"DATABASE_URL=postgres://app:fake-secret-123@127.0.0.1/db",
			"--password", "--verbose",
		},
		"instances": []any{
			map[string]any{
				"attachments": []any{
					map[string]any{
						"connection": map[string]any{
							"url": "http://user:fake-secret-123@127.0.0.1:9/b",
							"env": map[string]any{
								"AWS_SECRET_ACCESS_KEY": "fake-secret-123",
								"DATABASE_URL":          "postgres://app:fake-secret-123@127.0.0.1/db",
								"S3_BUCKET":             "photos",
							},
							"note": "keep",
						},
					},
				},
			},
		},
	}
	got := Values(in).(map[string]any)
	if got["token"] != Mask {
		t.Fatalf("token = %#v", got["token"])
	}
	if got["note"] != "keep" {
		t.Fatalf("note = %#v", got["note"])
	}
	if got["endpoint"] != "http://user:"+Mask+"@127.0.0.1:9/b" {
		t.Fatalf("endpoint = %#v", got["endpoint"])
	}
	env := got["env"].(map[string]any)
	if env["MINIO_ROOT_PASSWORD"] != Mask || env["MINIO_ROOT_USER"] != "hearth" {
		t.Fatalf("env = %#v", env)
	}
	args := got["provisionArgs"].(map[string]any)
	if args["AWS_SECRET_ACCESS_KEY"] != Mask || args["S3_BUCKET"] != "photos" {
		t.Fatalf("provisionArgs = %#v", args)
	}
	argv := got["argv"].([]any)
	wantArgv := []any{
		"minio",
		"MINIO_ROOT_PASSWORD=" + Mask,
		"--password", Mask,
		"--user", "hearth",
		"DATABASE_URL=postgres://app:" + Mask + "@127.0.0.1/db",
		"--password", "--verbose",
	}
	if !reflect.DeepEqual(argv, wantArgv) {
		t.Fatalf("argv = %#v", argv)
	}
	conn := got["instances"].([]any)[0].(map[string]any)["attachments"].([]any)[0].(map[string]any)["connection"].(map[string]any)
	if conn["url"] != "http://user:"+Mask+"@127.0.0.1:9/b" || conn["note"] != "keep" {
		t.Fatalf("connection = %#v", conn)
	}
	cenv := conn["env"].(map[string]any)
	if cenv["AWS_SECRET_ACCESS_KEY"] != Mask || cenv["S3_BUCKET"] != "photos" {
		t.Fatalf("connection env = %#v", cenv)
	}
	if cenv["DATABASE_URL"] != "postgres://app:"+Mask+"@127.0.0.1/db" {
		t.Fatalf("DATABASE_URL = %#v", cenv["DATABASE_URL"])
	}
	encoded, err := json.Marshal(got)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(encoded), "fake-secret-123") || strings.Contains(string(encoded), "fake-root-pass-123") {
		t.Fatalf("masked tree still has a secret: %s", encoded)
	}
	origEnv := in["env"].(map[string]any)
	if origEnv["MINIO_ROOT_PASSWORD"] != "fake-root-pass-123" {
		t.Fatal("input was mutated")
	}
}

func TestValuesIsIdempotentAndKeepsNull(t *testing.T) {
	in := map[string]any{
		"connection": nil,
		"items":      []any{map[string]any{"connection": map[string]any{"url": "redis://127.0.0.1:1/0"}}},
		"argv":       []any{"--password", "fake-root-pass-123"},
	}
	once := Values(in)
	twice := Values(once)
	if !reflect.DeepEqual(once, twice) {
		t.Fatalf("not idempotent:\n%#v\n%#v", once, twice)
	}
	if once.(map[string]any)["connection"] != nil {
		t.Fatal("null connection changed")
	}
	if Values(nil) != nil || Values(map[string]any(nil)) != nil {
		t.Fatal("nil changed")
	}
	slice, ok := Values([]any(nil)).([]any)
	if !ok || len(slice) != 0 {
		t.Fatalf("nil slice = %#v", slice)
	}
}

func TestValuesPreservesOMapOrder(t *testing.T) {
	in := omap.New()
	in.Set("MINIO_ROOT_USER", "hearth")
	in.Set("MINIO_ROOT_PASSWORD", "fake-root-pass-123")
	in.Set("url", "http://user:fake-secret-123@127.0.0.1:9/b")
	got := Values(in).(*omap.OMap)
	if !reflect.DeepEqual(got.Keys(), []string{"MINIO_ROOT_USER", "MINIO_ROOT_PASSWORD", "url"}) {
		t.Fatalf("keys = %#v", got.Keys())
	}
	password, _ := got.Get("MINIO_ROOT_PASSWORD")
	if password != Mask {
		t.Fatalf("password = %#v", password)
	}
	user, _ := got.Get("MINIO_ROOT_USER")
	if user != "hearth" {
		t.Fatalf("user = %#v", user)
	}
}

func TestValuesTruncatesDeepTrees(t *testing.T) {
	var value any = map[string]any{"PASSWORD": "fake-root-pass-123"}
	for range maxWalkDepth + 8 {
		value = map[string]any{"nested": value}
	}
	encoded, err := json.Marshal(Values(value))
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(encoded), "fake-root-pass-123") {
		t.Fatalf("deep secret leaked: %s", encoded)
	}
}
