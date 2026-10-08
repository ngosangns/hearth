<?php

namespace App\Support;

final class ApiResult
{
    /**
     * @param  ?array<string, mixed>  $json
     */
    public function __construct(
        public bool $ok,
        public ?int $status,
        public ?array $json,
        public bool $unauthorized,
        public string $message,
    ) {}

    public static function unauthorized(): self
    {
        return new self(false, 401, null, true, 'Daemon session ended. Start attaches this window.');
    }

    public static function transport(string $message): self
    {
        return new self(false, null, null, false, $message);
    }
}
