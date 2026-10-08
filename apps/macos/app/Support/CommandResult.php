<?php

namespace App\Support;

final class CommandResult
{
    /**
     * @param  ?array<string, mixed>  $json
     */
    public function __construct(
        public bool $ok,
        public ?int $exit,
        public string $stdout,
        public string $stderr,
        public ?array $json,
    ) {}

    public static function fromStreams(bool $ok, ?int $exit, string $stdout, string $stderr): self
    {
        return new self($ok, $exit, $stdout, $stderr, self::lastJson($stdout));
    }

    public static function missingBinary(): self
    {
        return new self(false, null, '', 'bundled hearth is missing or not executable', null);
    }

    public function visibleMessage(): string
    {
        $text = trim($this->stderr);
        if ($text === '') {
            $text = trim($this->stdout);
        }

        return self::redact($text);
    }

    /**
     * @return ?array<string, mixed>
     */
    public static function lastJson(string $stdout): ?array
    {
        // Pretty-printed documents contain nested one-line objects. Those must
        // not win over the whole document. A compact payload, or a JSON line
        // after a log line, still decodes.
        $whole = json_decode(trim($stdout), true);
        if (is_array($whole)) {
            return $whole;
        }

        $decoded = null;
        foreach (preg_split("/\r\n|\n|\r/", $stdout) ?: [] as $line) {
            $line = trim($line);
            if ($line === '' || ! str_starts_with($line, '{')) {
                continue;
            }
            $candidate = json_decode($line, true);
            if (is_array($candidate)) {
                $decoded = $candidate;
            }
        }

        return $decoded;
    }

    public static function redact(string $text): string
    {
        return preg_replace('/("token"\s*:\s*")[^"]+/', '$1[redacted]', $text) ?? $text;
    }
}
