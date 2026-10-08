<?php

namespace Tests\Support;

use App\Support\CommandResult;
use App\Support\HearthCommands;
use RuntimeException;

final class FakeCommands implements HearthCommands
{
    /** @var list<array{cwd: string, args: list<string>, timeout: ?int}> */
    public array $calls = [];

    /** @var array<string, callable> */
    public array $handlers = [];

    public function on(string $command, callable $handler): void
    {
        $this->handlers[$command] = $handler;
    }

    public function run(string $cwd, array $args, ?int $timeoutSeconds = 20): CommandResult
    {
        $this->calls[] = ['cwd' => $cwd, 'args' => $args, 'timeout' => $timeoutSeconds];
        $key = ($args[0] ?? '') === 'shared' ? (string) ($args[1] ?? '') : implode(' ', $args);
        $handler = $this->handlers[$key] ?? null;
        if ($handler === null) {
            throw new RuntimeException('unexpected hearth '.implode(' ', $args));
        }

        return $handler($cwd, $args, $timeoutSeconds);
    }

    public function countCommand(string $command): int
    {
        return count(array_filter(
            $this->calls,
            fn (array $call) => ($call['args'][1] ?? null) === $command,
        ));
    }
}
