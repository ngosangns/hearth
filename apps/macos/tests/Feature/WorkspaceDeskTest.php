<?php

namespace Tests\Feature;

use App\Livewire\WorkspaceDesk;
use App\Support\CommandResult;
use App\Support\DaemonMemory;
use App\Support\RunsHearth;
use App\Support\WorkspaceStore;
use Illuminate\Support\Facades\Http;
use Livewire\Livewire;
use Tests\Support\FakeHearth;
use Tests\TestCase;

class WorkspaceDeskTest extends TestCase
{
    private FakeHearth $fake;

    protected function setUp(): void
    {
        parent::setUp();
        $this->fake = new FakeHearth;
        $this->fake->on('status', fn () => new CommandResult(false, 3, '', 'hearth manager is unavailable', null));
        $this->fake->on('ensure', function () {
            throw new \RuntimeException('ensure must not run');
        });
        $this->fake->on('stop', function () {
            throw new \RuntimeException('stop must not run');
        });
        $this->app->instance(RunsHearth::class, $this->fake);
    }

    public function test_select_and_refresh_discover_and_do_not_ensure(): void
    {
        $dir = $this->makeDir();
        $added = WorkspaceStore::instance()->add($dir);
        WorkspaceStore::instance()->trust($added['record']['id']);

        Livewire::test(WorkspaceDesk::class)
            ->assertSee('Start runs the daemon')
            ->call('refreshList')
            ->assertSee('Start runs the daemon');

        $this->assertSame(0, $this->fake->count('ensure'));
        $this->assertGreaterThan(0, $this->fake->count('status'));
    }

    public function test_trust_is_two_steps_and_then_keeps_the_token_out_of_the_page(): void
    {
        $dir = $this->makeDir();
        WorkspaceStore::instance()->add($dir);
        $token = 'secret-'.bin2hex(random_bytes(8));
        $this->fake->on('ensure', fn () => new CommandResult(true, 0, '', '', [
            'port' => 59999,
            'token' => $token,
            'protocolVersion' => 3,
            'runtimeDirectory' => '/tmp/hearth-runtime',
            'root' => $dir,
        ]));
        Http::fake([
            'http://127.0.0.1:59999/healthz' => Http::response('ok', 200),
        ]);

        $component = Livewire::test(WorkspaceDesk::class)
            ->assertSee('untrusted')
            ->call('trust')
            ->assertSee('Press again to trust');

        $this->assertSame(0, $this->fake->count('ensure'));
        $this->assertFalse(WorkspaceStore::instance()->rows()[0]['trusted']);

        $component->call('trust')
            ->assertSee('Daemon is up on port 59999')
            ->assertSee('protocol 3')
            ->assertDontSee($token);

        $this->assertSame(1, $this->fake->count('ensure'));
        $this->assertTrue(WorkspaceStore::instance()->rows()[0]['trusted']);
        $this->assertSame($token, DaemonMemory::token($dir));
        $this->assertStringNotContainsString($token, $component->html());
    }

    public function test_start_on_an_untrusted_folder_does_not_ensure(): void
    {
        $dir = $this->makeDir();
        WorkspaceStore::instance()->add($dir);

        Livewire::test(WorkspaceDesk::class)
            ->call('startDaemon')
            ->assertSee('Trust the folder');

        $this->assertSame(0, $this->fake->count('ensure'));
    }

    public function test_stop_then_refresh_does_not_ensure(): void
    {
        $dir = $this->makeDir();
        $added = WorkspaceStore::instance()->add($dir);
        WorkspaceStore::instance()->trust($added['record']['id']);
        $this->fake->on('stop', fn () => new CommandResult(true, 0, '', '', ['ok' => true]));

        Livewire::test(WorkspaceDesk::class)
            ->call('stopDaemon')
            ->assertSee('Press again to stop')
            ->call('stopDaemon')
            ->assertSee('Daemon stopped')
            ->call('refreshList')
            ->assertSee('Daemon stopped');

        $this->assertSame(0, $this->fake->count('ensure'));
        $this->assertSame(1, $this->fake->count('stop'));
        $this->assertTrue(DaemonMemory::isStopped($added['record']['id']));
    }

    public function test_start_after_stop_ensures_again(): void
    {
        $dir = $this->makeDir();
        $added = WorkspaceStore::instance()->add($dir);
        WorkspaceStore::instance()->trust($added['record']['id']);
        $this->fake->on('stop', fn () => new CommandResult(true, 0, '', '', null));
        $this->fake->on('ensure', fn () => new CommandResult(true, 0, '', '', [
            'port' => 59998,
            'token' => 'again-token',
            'protocolVersion' => 3,
        ]));
        Http::fake([
            'http://127.0.0.1:59998/healthz' => Http::response('ok', 200),
        ]);

        Livewire::test(WorkspaceDesk::class)
            ->call('stopDaemon')
            ->call('stopDaemon')
            ->call('startDaemon')
            ->assertSee('Daemon is up on port 59998');

        $this->assertSame(1, $this->fake->count('ensure'));
        $this->assertFalse(DaemonMemory::isStopped($added['record']['id']));
    }

    public function test_forget_does_not_stop_the_daemon(): void
    {
        $dir = $this->makeDir();
        $added = WorkspaceStore::instance()->add($dir);

        Livewire::test(WorkspaceDesk::class)
            ->call('forget')
            ->assertSee('Press again to forget')
            ->assertSee('services keep running')
            ->call('forget')
            ->assertSee('Forgot the workspace');

        $this->assertSame(0, $this->fake->count('stop'));
        $this->assertSame(0, $this->fake->count('ensure'));
        $this->assertNull(WorkspaceStore::instance()->get($added['record']['id']));
    }

    public function test_selecting_another_folder_clears_a_pending_trust(): void
    {
        $first = $this->makeDir();
        $second = $this->makeDir();
        WorkspaceStore::instance()->add($first);
        $other = WorkspaceStore::instance()->add($second);

        Livewire::test(WorkspaceDesk::class)
            ->call('trust')
            ->assertSee('Press again to trust')
            ->call('select', $other['record']['id'])
            ->assertDontSee('Press again to trust')
            ->call('trust')
            ->assertSee('Press again to trust');

        $this->assertSame(0, $this->fake->count('ensure'));
        $this->assertFalse(WorkspaceStore::instance()->get($other['record']['id'])['trusted']);
    }

    private function makeDir(): string
    {
        $dir = sys_get_temp_dir().'/hearth-macos-desk-'.bin2hex(random_bytes(4));
        mkdir($dir);
        $this->beforeApplicationDestroyed(function () use ($dir) {
            @rmdir($dir);
        });

        return realpath($dir) ?: $dir;
    }
}
