<?php

namespace Tests\Unit;

use App\Support\BundledHearth;
use App\Support\CommandResult;
use Illuminate\Support\Facades\Process;
use Tests\TestCase;

class CommandResultTest extends TestCase
{
    public function test_pretty_json_keeps_the_document_when_a_nested_object_is_one_line(): void
    {
        $stdout = <<<'JSON'
{
    "instances": [
        {
            "name": "redis",
            "version": "8.2.10",
            "attachments": {
                "abc": {"projectRoot": "/work/viclass", "provisioned": true}
            }
        }
    ]
}
JSON;

        $decoded = CommandResult::lastJson($stdout);

        $this->assertSame('redis', $decoded['instances'][0]['name']);
    }

    public function test_a_json_line_after_a_log_line_still_decodes(): void
    {
        $decoded = CommandResult::lastJson("listening\n".'{"port": 1, "token": "secret"}'."\n");

        $this->assertSame(1, $decoded['port']);
    }

    public function test_a_spawned_hearth_keeps_home(): void
    {
        $binary = base_path('extras/hearth');
        if (! is_file($binary) || ! is_executable($binary)) {
            $this->markTestSkipped('bundled hearth is absent');
        }

        Process::fake([
            '*' => Process::result('{"instances":[]}'),
        ]);

        app(BundledHearth::class)->run(base_path(), ['shared', 'installed', '--json'], 5);

        $home = getenv('HOME');
        Process::assertRan(function ($process) use ($home) {
            return ($process->environment['HOME'] ?? null) === $home
                && is_string($process->environment['PATH'] ?? null)
                && $process->environment['PATH'] !== '';
        });
    }
}
