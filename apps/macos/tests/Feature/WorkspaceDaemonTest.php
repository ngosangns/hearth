<?php

namespace Tests\Feature;

use App\Livewire\WorkspaceDesk;
use App\Support\DaemonMemory;
use App\Support\WorkspaceStore;
use Illuminate\Support\Facades\Process;
use Livewire\Livewire;
use Tests\TestCase;

class WorkspaceDaemonTest extends TestCase
{
    public function test_trust_ensures_with_the_bundled_binary_and_forget_leaves_it_running(): void
    {
        if (! is_executable(base_path('extras/hearth'))) {
            $this->markTestSkipped('extras/hearth is missing');
        }

        $root = $this->fixture();
        try {
            $component = Livewire::test(WorkspaceDesk::class)
                ->set('folder', $root)
                ->call('addFolder')
                ->call('trust')
                ->assertSee('Press again to trust')
                ->call('trust');

            $path = WorkspaceStore::instance()->rows()[0]['path'];
            $component->assertSee('Daemon is up on port')
                ->assertSee('protocol 3')
                ->assertSee('spike')
                ->assertSee('daemon log')
                ->assertDontSee('healthz did not answer');

            $component->call('startService', 'spike');
            $this->waitUntil(function () use ($component) {
                $component->call('tick');

                return str_contains($component->html(), '>succeeded<');
            }, 'service did not succeed');
            $component->assertSee('http://127.0.0.1:9/spike')
                ->assertDontSee('healthz did not answer');

            $token = DaemonMemory::token($path);
            $this->assertNotNull($token);
            $this->assertStringNotContainsString($token, $component->html());
            $this->waitUntil(fn () => $this->daemonAlive($path), 'daemon did not stay up after ensure');

            $component->call('forget')
                ->assertSee('Press again to forget')
                ->call('forget')
                ->assertSee('Forgot the workspace');

            $this->assertTrue($this->daemonAlive($path), 'forget must leave the daemon running');
            $this->assertNull(DaemonMemory::token($path));
            $this->stop($path);
            $this->waitUntil(fn () => ! $this->daemonAlive($path), 'daemon did not exit after stop');
        } finally {
            $this->cleanup($root);
        }
    }

    public function test_a_stopped_daemon_stays_down_when_the_list_refreshes(): void
    {
        if (! is_executable(base_path('extras/hearth'))) {
            $this->markTestSkipped('extras/hearth is missing');
        }

        $root = $this->fixture();
        try {
            $component = Livewire::test(WorkspaceDesk::class)
                ->set('folder', $root)
                ->call('addFolder')
                ->call('trust')
                ->call('trust');

            $path = WorkspaceStore::instance()->rows()[0]['path'];
            $this->waitUntil(fn () => $this->daemonAlive($path), 'daemon did not start');

            $component->call('stopDaemon')
                ->assertSee('Press again to stop')
                ->call('stopDaemon')
                ->assertSee('Daemon stopped')
                ->call('refreshList')
                ->assertSee('Daemon stopped');

            $this->waitUntil(fn () => ! $this->daemonAlive($path), 'refresh started the daemon again');
        } finally {
            $this->cleanup($root);
        }
    }

    private function fixture(): string
    {
        $root = sys_get_temp_dir().'/hearth-macos-daemon-'.bin2hex(random_bytes(4));
        mkdir($root);
        file_put_contents($root.'/hearth.yaml', <<<'YAML'
version: 1
services:
  spike:
    run: { argv: ["/bin/echo", "hearth-macos-desk"] }
    readiness: { kind: exit }
    urls:
      - { url: "http://127.0.0.1:9/spike", label: Spike }
YAML);

        return $root;
    }

    private function cleanup(string $root): void
    {
        $path = realpath($root) ?: $root;
        if ($this->daemonAlive($path)) {
            $this->stop($path);
            $deadline = microtime(true) + 15;
            while ($this->daemonAlive($path) && microtime(true) < $deadline) {
                usleep(100000);
            }
        }
        @unlink($root.'/hearth.yaml');
        @rmdir($root);
    }

    private function stop(string $root): void
    {
        Process::timeout(120)->run([
            base_path('extras/hearth'),
            '--root',
            $root,
            'manager',
            'stop',
        ]);
    }

    private function daemonAlive(string $root): bool
    {
        $result = Process::timeout(10)->run(['ps', '-axo', 'command=']);
        foreach (preg_split("/\r\n|\n|\r/", $result->output()) ?: [] as $line) {
            if (str_contains($line, 'hearth daemon') && str_contains($line, $root)) {
                return true;
            }
        }

        return false;
    }

    private function waitUntil(callable $ready, string $message): void
    {
        $deadline = microtime(true) + 15;
        while (microtime(true) < $deadline) {
            if ($ready()) {
                return;
            }
            usleep(100000);
        }
        $this->fail($message);
    }
}
