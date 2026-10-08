<?php

namespace Tests\Support;

use App\Support\CommandResult;
use App\Support\RunsHearth;
use RuntimeException;

final class FakeHearth implements RunsHearth
{
    /** @var list<array{0: string, 1: string}> */
    public array $calls = [];

    /** @var array<string, callable> */
    public array $handlers = [];

    public function on(string $subcommand, callable $handler): void
    {
        $this->handlers[$subcommand] = $handler;
    }

    public function manager(string $root, string $subcommand): CommandResult
    {
        $this->calls[] = [$subcommand, $root];
        $handler = $this->handlers[$subcommand] ?? null;
        if ($handler === null) {
            throw new RuntimeException("unexpected hearth manager {$subcommand}");
        }

        return $handler($root, $subcommand);
    }

    public function count(string $subcommand): int
    {
        return count(array_filter($this->calls, fn (array $call) => $call[0] === $subcommand));
    }
}
