<?php

namespace Tests\Feature;

use App\Livewire\WorkspaceDesk;
use App\Support\CommandResult;
use App\Support\DaemonMemory;
use App\Support\HearthCommands;
use App\Support\RunsHearth;
use App\Support\WorkspaceStore;
use GuzzleHttp\Promise\PromiseInterface;
use Illuminate\Support\Facades\Http;
use Livewire\Features\SupportTesting\Testable;
use Livewire\Livewire;
use Tests\Support\FakeCommands;
use Tests\Support\FakeHearth;
use Tests\TestCase;

class ServiceDeskTest extends TestCase
{
    private FakeHearth $fake;

    private string $root = '';

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
        $this->fake->on('reload', function () {
            throw new \RuntimeException('reload must not run');
        });
        $this->fake->on('restart', function () {
            throw new \RuntimeException('restart must not run');
        });
        $this->app->instance(RunsHearth::class, $this->fake);
        $this->root = sys_get_temp_dir().'/hearth-macos-svc-'.bin2hex(random_bytes(4));
        mkdir($this->root);
        // WorkspaceStore stores the canonical path. /tmp is /private/tmp on macOS.
        $this->root = realpath($this->root) ?: $this->root;
    }

    protected function tearDown(): void
    {
        @unlink($this->root.'/hearth.yaml');
        @rmdir($this->root);
        parent::tearDown();
    }

    public function test_tick_lists_services_and_does_not_ensure(): void
    {
        $seen = [];
        $protocols = [];
        $this->fakeBoard(function ($request) use (&$seen, &$protocols) {
            $seen[] = $request->url();
            $header = $request->header('x-hearth-protocol');
            $protocols[] = is_array($header) ? ($header[0] ?? null) : $header;

            return null;
        });
        $component = $this->attached();
        DaemonMemory::put($this->root, ['token' => 'desk-token', 'port' => 59991, 'protocolVersion' => 3]);

        $component->call('tick')
            ->assertSee('web')
            ->assertSee('degraded')
            ->assertSee('daemon log')
            ->assertSee('1/1 ready')
            ->assertDontSee('desk-token');

        $this->assertSame(0, $this->fake->count('ensure'));
        $this->assertContains('3', $protocols);
        $this->assertTrue(collect($seen)->contains(fn (string $url) => str_contains($url, '/v1/services')));
        $this->assertTrue(collect($seen)->contains(fn (string $url) => str_contains($url, '/v1/daemon/log')));
    }

    public function test_service_log_echoes_the_composite_generation(): void
    {
        $logQueries = [];
        Http::fake(function ($request) use (&$logQueries) {
            $url = $request->url();
            if (str_contains($url, '/v1/logs/web')) {
                $logQueries[] = $request->data();
                $echo = array_key_exists('generation', $request->data());

                return Http::response([
                    'serviceId' => 'web',
                    'generation' => 1000001,
                    'nextCursor' => 5,
                    'data' => $echo ? 'more' : 'hello',
                    'reset' => ! $echo,
                ], 200);
            }

            return $this->boardResponse($url);
        });
        $component = $this->attached();
        DaemonMemory::put($this->root, ['token' => 'desk-token', 'port' => 59991, 'protocolVersion' => 3]);

        $component->call('selectService', 'web')
            ->assertSee('hello');
        $this->assertArrayNotHasKey('generation', $logQueries[0]);

        $component->call('tick')->assertSee('hellomore');
        $this->assertSame(1000001, $logQueries[1]['generation']);
        $this->assertSame(0, $this->fake->count('ensure'));
    }

    public function test_a_normal_start_clears_reclaim_and_does_not_send_kill_unowned(): void
    {
        $bodies = [];
        Http::fake(function ($request) use (&$bodies) {
            if ($request->method() === 'POST') {
                $bodies[] = $request->data();

                return Http::response(['operation' => ['id' => 'op-1', 'status' => 'succeeded']], 202);
            }
            if (str_contains($request->url(), '/v1/services')) {
                return Http::response(['services' => [[
                    'serviceId' => 'web',
                    'actualState' => 'externally-owned',
                    'generation' => 2,
                ]]], 200);
            }

            return $this->boardResponse($request->url());
        });
        $component = $this->attached()->set('sections', [$this->externalSection()]);
        DaemonMemory::put($this->root, ['token' => 'desk-token', 'port' => 59991, 'protocolVersion' => 3]);

        $component->call('reclaimPort', 'web')
            ->assertSee('Press again to reclaim the port for web')
            ->call('startService', 'web')
            ->assertSee('Started web.');

        $this->assertCount(1, $bodies);
        $this->assertSame('start', $bodies[0]['action']);
        $this->assertArrayNotHasKey('killUnowned', $bodies[0]);

        $component->call('reclaimPort', 'web')
            ->call('reclaimPort', 'web')
            ->assertSee('Started web.');
        $this->assertTrue($bodies[1]['killUnowned']);
        $this->assertSame('start', $bodies[1]['action']);
        $this->assertSame(0, $this->fake->count('ensure'));
    }

    public function test_stop_all_skips_stopped_and_succeeded_and_names_a_failure(): void
    {
        $bodies = [];
        Http::fake(function ($request) use (&$bodies) {
            if ($request->method() === 'POST') {
                $data = $request->data();
                $bodies[] = $data;
                $status = $data['serviceId'] === 'web' ? 'failed' : 'succeeded';

                return Http::response(['operation' => ['id' => 'op', 'status' => $status, 'error' => ['message' => 'busy']]], 202);
            }

            return $this->boardResponse($request->url());
        });
        $component = $this->attached()->set('sections', [[
            'name' => null,
            'services' => [
                $this->line('held', 'ready'),
                $this->line('web', 'running'),
                $this->line('done', 'succeeded'),
                $this->line('down', 'stopped'),
                $this->line('off', 'running', disabled: true),
            ],
        ]]);
        DaemonMemory::put($this->root, ['token' => 'desk-token', 'port' => 59991, 'protocolVersion' => 3]);

        $component->call('stopAll')->assertSee('Failed: web.');

        $this->assertSame(['held', 'web'], array_column($bodies, 'serviceId'));
        $this->assertSame(0, $this->fake->count('ensure'));
    }

    public function test_disabled_service_has_no_lifecycle_action(): void
    {
        $bodies = [];
        $this->fakeBoard(null, $bodies);
        $component = $this->attached()->set('sections', [[
            'name' => null,
            'services' => [$this->line('off', 'stopped', disabled: true)],
        ]]);
        DaemonMemory::put($this->root, ['token' => 'desk-token', 'port' => 59991, 'protocolVersion' => 3]);

        $component->call('selectService', 'off')
            ->assertSee('This service is disabled.')
            ->call('startService', 'off')
            ->call('stopService', 'off')
            ->call('restartService', 'off');

        $this->assertSame([], $bodies);
    }

    public function test_catalog_mtime_reloads_once_and_a_failure_does_not_retry(): void
    {
        file_put_contents($this->root.'/hearth.yaml', "version: 1\nservices: {}\n");
        $this->fake->on('reload', fn () => new CommandResult(false, 1, '', 'stop_failed', null));
        $this->fakeBoard();
        $component = $this->attached();
        DaemonMemory::put($this->root, ['token' => 'desk-token', 'port' => 59991, 'protocolVersion' => 3]);

        $component->call('tick');
        $this->assertSame(0, $this->fake->count('reload'));

        $component->set('catalogMtime', 1)->call('tick')->assertSee('stop_failed');
        $this->assertSame(1, $this->fake->count('reload'));
        $component->call('tick');
        $this->assertSame(1, $this->fake->count('reload'));
        $this->assertSame(0, $this->fake->count('ensure'));
    }

    public function test_daemon_restart_stores_the_new_token(): void
    {
        $this->fake->on('restart', fn () => new CommandResult(true, 0, '', '', [
            'port' => 59992,
            'token' => 'fresh-token',
            'protocolVersion' => 3,
        ]));
        $this->fakeBoard();
        $added = WorkspaceStore::instance()->add($this->root);
        WorkspaceStore::instance()->trust($added['record']['id']);
        $component = Livewire::test(WorkspaceDesk::class);
        DaemonMemory::put($this->root, ['token' => 'old-token', 'port' => 59991, 'protocolVersion' => 3]);
        DaemonMemory::markStopped($added['record']['id']);
        DaemonMemory::clearStopped($added['record']['id']);

        $component->call('restartDaemon')
            ->assertSee('Press again to restart the daemon')
            ->assertSee('services keep running');
        $this->assertSame('old-token', DaemonMemory::token($this->root));

        $component->call('restartDaemon')
            ->assertSee('Daemon is up on port 59992')
            ->assertDontSee('fresh-token');
        $this->assertSame('fresh-token', DaemonMemory::token($this->root));
        $this->assertSame(0, $this->fake->count('ensure'));
    }

    public function test_a_lost_session_forgets_the_token_and_does_not_ensure(): void
    {
        Http::fake(['*' => Http::response(['error' => ['message' => 'no']], 401)]);
        $component = $this->attached();
        DaemonMemory::put($this->root, ['token' => 'desk-token', 'port' => 59991, 'protocolVersion' => 3]);

        $component->call('tick')->assertSee('Daemon session ended. Start attaches this window.');

        $this->assertNull(DaemonMemory::token($this->root));
        $this->assertSame(0, $this->fake->count('ensure'));
    }

    public function test_project_shared_stop_names_the_other_workspace_before_it_runs(): void
    {
        $other = sys_get_temp_dir().'/hearth-macos-other-'.bin2hex(random_bytes(4));
        mkdir($other);
        $bodies = [];
        $this->fakeBoard(null, $bodies);
        $commands = $this->bindCommands();
        $commands->on('installed', fn () => new CommandResult(true, 0, '', '', [
            'instances' => [[
                'name' => 'postgres',
                'version' => '16.4',
                'attachments' => [
                    'a' => ['projectRoot' => $this->root],
                    'b' => ['projectRoot' => $other],
                ],
            ]],
        ]));
        $component = $this->attached();
        WorkspaceStore::instance()->add($other);
        $component->set('sections', [[
            'name' => 'data',
            'services' => [[
                'id' => 'db',
                'label' => 'Database',
                'ports' => '',
                'state' => 'ready',
                'display' => 'ready',
                'disabled' => false,
                'finite' => false,
                'shared' => true,
                'sharedInstance' => 'postgres@16.4',
                'error' => null,
                'up' => true,
            ]],
        ]]);
        DaemonMemory::put($this->root, ['token' => 'desk-token', 'port' => 59991, 'protocolVersion' => 3]);

        $component->call('stopService', 'db')
            ->assertSee('postgres@16.4')
            ->assertSee('only detaches')
            ->assertSee('Press again to stop.');
        $this->assertSame([], $bodies);

        $component->call('stopService', 'db')->assertSee('Stopped db.');
        $this->assertSame('stop', $bodies[0]['action']);
        $this->assertArrayNotHasKey('killUnowned', $bodies[0]);
        @rmdir($other);
    }

    public function test_shared_remove_sends_force_only_after_confirm_when_someone_is_attached(): void
    {
        $commands = $this->bindCommands();
        $commands->on('list', fn () => new CommandResult(true, 0, '', '', ['services' => []]));
        $commands->on('status', fn () => new CommandResult(false, 3, '', 'smp is not running', null));
        $commands->on('installed', fn () => new CommandResult(true, 0, '', '', [
            'instances' => [[
                'name' => 'redis',
                'version' => '8.2.10',
                'port' => 43100,
                'installState' => 'installed',
                'attachments' => ['a' => ['projectRoot' => '/work/viclass']],
            ]],
        ]));
        $commands->on('remove', fn () => new CommandResult(true, 0, '', '', ['removed' => true]));
        $this->attached();

        Livewire::test(WorkspaceDesk::class)
            ->call('showPane', 'shared')
            ->assertSee('redis@8.2.10')
            ->assertSee('smp is not running')
            ->call('removeInstance', 'redis@8.2.10')
            ->assertSee('Press again to remove')
            ->call('removeInstance', 'redis@8.2.10')
            ->assertSee('Removed redis@8.2.10.');

        $this->assertSame(0, $commands->countCommand('ensure'));
        $removes = array_values(array_filter($commands->calls, fn (array $call) => ($call['args'][1] ?? null) === 'remove'));
        $this->assertCount(1, $removes);
        $this->assertContains('--force', $removes[0]['args']);
        $this->assertNotContains('ensure', array_merge(...array_column($commands->calls, 'args')));
    }

    public function test_remove_without_attachments_omits_force(): void
    {
        $commands = $this->bindCommands();
        $commands->on('installed', fn () => new CommandResult(true, 0, '', '', [
            'instances' => [[
                'name' => 'redis',
                'version' => '8.2.10',
                'attachments' => [],
            ]],
        ]));
        $commands->on('list', fn () => new CommandResult(true, 0, '', '', ['services' => []]));
        $commands->on('status', fn () => new CommandResult(false, 3, '', 'smp is not running', null));
        $commands->on('remove', fn () => new CommandResult(true, 0, '', '', ['removed' => true]));
        $this->attached();

        Livewire::test(WorkspaceDesk::class)
            ->call('removeInstance', 'redis@8.2.10')
            ->call('removeInstance', 'redis@8.2.10');

        $removes = array_values(array_filter($commands->calls, fn (array $call) => ($call['args'][1] ?? null) === 'remove'));
        $this->assertCount(1, $removes);
        $this->assertNotContains('--force', $removes[0]['args']);
    }

    public function test_log_pane_is_a_dedicated_column_that_collapses(): void
    {
        $this->fakeBoard();
        $component = $this->attached();
        $id = WorkspaceStore::instance()->rows()[0]['id'];

        $component->call('select', $id)
            ->assertDontSeeHtml('class="logcol ');

        $this->fake->on('status', fn () => new CommandResult(true, 0, '', '', ['port' => 59991, 'protocolVersion' => 3]));
        DaemonMemory::put($this->root, ['token' => 'desk-token', 'port' => 59991, 'protocolVersion' => 3]);

        $component->call('select', $id)
            ->assertSeeHtml('class="desk with-log ')
            ->assertSeeHtml('class="logcol ');

        $component->call('toggleLog')
            ->assertSeeHtml('class="desk with-log log-closed"')
            ->assertSeeHtml('class="logcol collapsed"');
    }

    private function attached(): Testable
    {
        $added = WorkspaceStore::instance()->add($this->root);
        WorkspaceStore::instance()->trust($added['record']['id']);

        return Livewire::test(WorkspaceDesk::class);
    }

    private function bindCommands(): FakeCommands
    {
        $commands = new FakeCommands;
        $this->app->instance(HearthCommands::class, $commands);

        return $commands;
    }

    /**
     * @param  list<array<string, mixed>>  $bodies
     */
    private function fakeBoard(?callable $extra = null, array &$bodies = []): void
    {
        Http::fake(function ($request) use ($extra, &$bodies) {
            if ($extra !== null) {
                $handled = $extra($request);
                if ($handled !== null) {
                    return $handled;
                }
            }
            if ($request->method() === 'POST') {
                $bodies[] = $request->data();

                return Http::response(['operation' => ['id' => 'op-1', 'status' => 'succeeded']], 202);
            }

            return $this->boardResponse($request->url());
        });
    }

    private function boardResponse(string $url, int $generation = 2): PromiseInterface
    {
        if (str_contains($url, '/v1/services')) {
            return Http::response(['services' => [[
                'serviceId' => 'web',
                'actualState' => 'running-unready',
                'generation' => $generation,
            ]]], 200);
        }
        if (str_contains($url, '/v1/catalog')) {
            return Http::response(['catalog' => [
                'services' => [[
                    'id' => 'web',
                    'label' => 'Web',
                    'profiles' => ['run' => ['commandStatus' => 'verified', 'readiness' => ['kind' => 'http'], 'command' => ['command' => ['argv' => ['/bin/echo']]]]],
                ]],
                'groups' => ['all' => ['web']],
                'groupTree' => [],
            ]], 200);
        }
        if (str_contains($url, '/v1/urls')) {
            return Http::response(['urls' => []], 200);
        }

        return Http::response([
            'serviceId' => 'daemon',
            'generation' => 0,
            'nextCursor' => 3,
            'data' => 'log',
            'reset' => true,
        ], 200);
    }

    /**
     * @return array{name: null, services: list<array<string, mixed>>}
     */
    private function externalSection(): array
    {
        return [
            'name' => null,
            'services' => [$this->line('web', 'externally-owned')],
        ];
    }

    /**
     * @return array<string, mixed>
     */
    private function line(string $id, string $state, bool $disabled = false): array
    {
        return [
            'id' => $id,
            'label' => $id,
            'ports' => '',
            'state' => $state,
            'display' => $state === 'running-unready' ? 'degraded' : $state,
            'disabled' => $disabled,
            'finite' => false,
            'shared' => false,
            'sharedInstance' => null,
            'error' => null,
            'up' => in_array($state, ['ready', 'running', 'running-unready'], true),
        ];
    }
}
