<?php

namespace App\Livewire;

use App\Support\DaemonMemory;
use App\Support\HearthBinary;
use App\Support\HearthCommands;
use App\Support\HearthLink;
use App\Support\ManagerHttp;
use App\Support\RunsHearth;
use App\Support\ServiceBoard;
use App\Support\WorkspaceStore;
use Illuminate\Support\Facades\Http;
use InvalidArgumentException;
use Livewire\Attributes\Layout;
use Livewire\Component;
use RuntimeException;

#[Layout('layouts::app')]
class WorkspaceDesk extends Component
{
    public string $folder = '';

    public ?string $selectedId = null;

    public ?string $notice = null;

    public ?string $pendingKind = null;

    public ?string $pendingId = null;

    public string $binaryLine = '';

    public string $pane = 'workspaces';

    /** @var list<array{name: ?string, services: list<array<string, mixed>>}> */
    public array $sections = [];

    public string $summary = '';

    /** @var list<array{serviceId: string, label: string, url: string}> */
    public array $urls = [];

    /** @var array<string, mixed> */
    public array $catalogGroups = [];

    public string $selectedService = '$daemon';

    public string $logText = '';

    public ?int $logCursor = null;

    public ?int $logGeneration = null;

    public bool $logOpen = true;

    public int $logLimit = 16384;

    public bool $logHasMore = false;

    public ?int $serviceGeneration = null;

    public ?int $catalogMtime = null;

    /** @var list<array{id: string, name: string, version: string}> */
    public array $recipes = [];

    /** @var list<array<string, mixed>> */
    public array $instances = [];

    public bool $smpLive = false;

    public function mount(HearthBinary $binary): void
    {
        if (! app()->runningUnitTests()) {
            HearthLink::ensure($binary->path(), (string) config('nativephp.version'));
        }
        $version = $binary->version();
        $this->binaryLine = $version['output'];
        $rows = WorkspaceStore::instance()->rows();
        if ($this->selectedId === null && $rows !== []) {
            $this->selectedId = $rows[0]['id'];
        }
        $this->discoverSelected();
    }

    public function addFolder(): void
    {
        $this->clearPending();
        try {
            $added = WorkspaceStore::instance()->add(trim($this->folder));
        } catch (InvalidArgumentException|RuntimeException $error) {
            $this->notice = $error->getMessage();

            return;
        }
        $this->folder = '';
        $this->selectedId = $added['record']['id'];
        $this->resetBoard();
        $this->discoverSelected();
    }

    public function select(string $id): void
    {
        if ($this->pendingId !== $id) {
            $this->clearPending();
        }
        $changed = $this->selectedId !== $id;
        $this->selectedId = $id;
        if ($changed) {
            $this->resetBoard();
        }
        $this->discoverSelected();
    }

    /**
     * Re-read the list and discover the selection. Never calls `manager ensure`.
     */
    public function refreshList(): void
    {
        $this->clearPending();
        $error = WorkspaceStore::instance()->reload();
        if ($error !== null) {
            $this->notice = $error;
        }
        $this->discoverSelected();
    }

    public function tick(): void
    {
        $row = $this->selectedRow();
        if ($row === null || DaemonMemory::isStopped($row['id']) || ! DaemonMemory::hasToken($row['path'])) {
            return;
        }
        $this->loadBoard($row['path']);
    }

    public function showPane(string $pane): void
    {
        if (! in_array($pane, ['workspaces', 'shared'], true)) {
            return;
        }
        $this->pane = $pane;
        $this->clearPending();
        if ($pane === 'shared') {
            $this->notice = null;
            $this->loadShared();
        }
    }

    public function refreshShared(): void
    {
        $this->clearPending();
        $this->notice = null;
        $this->loadShared();
    }

    public function trust(): void
    {
        $row = $this->selectedRow();
        if ($row === null) {
            return;
        }
        if (! is_dir($row['path'])) {
            $this->notice = $this->missingNotice($row);

            return;
        }
        if ($row['trusted']) {
            $this->startDaemon();

            return;
        }
        if (! $this->armed('trust', $row['id'])) {
            $this->pendingKind = 'trust';
            $this->pendingId = $row['id'];
            $name = WorkspaceStore::folderName($row['path']);
            $path = WorkspaceStore::displayPath($row['path']);
            $this->notice = "Press again to trust {$name} ({$path}) and start its daemon.";

            return;
        }
        $this->clearPending();
        $store = WorkspaceStore::instance();
        $store->reload();
        try {
            $fresh = $store->trust($row['id']);
        } catch (RuntimeException $error) {
            $this->notice = $error->getMessage();

            return;
        }
        DaemonMemory::clearStopped($fresh['id']);
        $this->ensure($fresh);
    }

    public function startDaemon(): void
    {
        $row = $this->selectedRow();
        if ($row === null) {
            return;
        }
        if (! is_dir($row['path'])) {
            $this->notice = $this->missingNotice($row);

            return;
        }
        if (! $row['trusted']) {
            $this->notice = 'Trust the folder before starting its daemon.';

            return;
        }
        $this->clearPending();
        DaemonMemory::clearStopped($row['id']);
        $this->ensure($row);
    }

    public function stopDaemon(): void
    {
        $row = $this->selectedRow();
        if ($row === null || ! is_dir($row['path'])) {
            return;
        }
        if (! $row['trusted']) {
            return;
        }
        if (! $this->armed('stop', $row['id'])) {
            $this->pendingKind = 'stop';
            $this->pendingId = $row['id'];
            $this->notice = 'Press again to stop the daemon for '.WorkspaceStore::folderName($row['path']).'. Its services stop.';

            return;
        }
        $this->clearPending();
        DaemonMemory::markStopped($row['id']);
        DaemonMemory::forgetRoot($row['path']);
        $this->resetBoard();
        $result = app(RunsHearth::class)->manager($row['path'], 'stop');
        $this->notice = $result->ok
            ? 'Daemon stopped. Start runs it again.'
            : ($result->visibleMessage() !== '' ? $result->visibleMessage() : 'stop daemon failed');
    }

    public function restartDaemon(): void
    {
        $row = $this->selectedRow();
        if ($row === null || ! is_dir($row['path']) || ! $row['trusted']) {
            return;
        }
        if (DaemonMemory::isStopped($row['id'])) {
            return;
        }
        if (! $this->armed('restart-daemon', $row['id'])) {
            $this->pendingKind = 'restart-daemon';
            $this->pendingId = $row['id'];
            $this->notice = 'Press again to restart the daemon for '.WorkspaceStore::folderName($row['path']).'. Its services keep running.';

            return;
        }
        $this->clearPending();
        $result = app(RunsHearth::class)->manager($row['path'], 'restart');
        $payload = $result->json;
        if (! $result->ok || ! is_array($payload) || ! isset($payload['token'], $payload['port'])) {
            $this->notice = $result->visibleMessage() !== '' ? $result->visibleMessage() : 'restart daemon failed';

            return;
        }
        DaemonMemory::put($row['path'], $payload);
        DaemonMemory::clearStopped($row['id']);
        $port = (int) $payload['port'];
        $protocol = $payload['protocolVersion'] ?? null;
        $this->notice = 'Daemon is up on port '.$port
            .($protocol !== null ? ', protocol '.$protocol : '')
            .'.';
        $this->logCursor = null;
        $this->logGeneration = null;
        $this->loadBoard($row['path']);
    }

    public function forget(): void
    {
        $row = $this->selectedRow();
        if ($row === null) {
            return;
        }
        if (! $this->armed('forget', $row['id'])) {
            $this->pendingKind = 'forget';
            $this->pendingId = $row['id'];
            $this->notice = 'Press again to forget '.WorkspaceStore::folderName($row['path']).'. Its services keep running.';

            return;
        }
        $this->clearPending();
        $store = WorkspaceStore::instance();
        $store->reload();
        try {
            $store->remove($row['id']);
        } catch (RuntimeException $error) {
            $this->notice = $error->getMessage();

            return;
        }
        DaemonMemory::forgetId($row['id']);
        DaemonMemory::forgetRoot($row['path']);
        $this->resetBoard();
        $this->selectedId = $store->rows()[0]['id'] ?? null;
        if ($this->selectedId !== null) {
            $this->discoverSelected();
        }
        $this->notice = 'Forgot the workspace. Its services keep running.';
    }

    public function selectService(string $id): void
    {
        $this->selectedService = $id;
        $this->logText = '';
        $this->logCursor = null;
        $this->logGeneration = null;
        $this->serviceGeneration = null;
        $this->logOpen = true;
        $this->logLimit = 16384;
        $this->logHasMore = false;
        $row = $this->selectedRow();
        if ($row === null || DaemonMemory::isStopped($row['id']) || ! DaemonMemory::hasToken($row['path'])) {
            return;
        }
        $api = ManagerHttp::open($row['path']);
        if ($api !== null) {
            $this->loadLog($api);
        }
    }

    public function startService(string $id): void
    {
        $line = $this->line($id);
        if ($line === null || $line['disabled']) {
            return;
        }
        $this->clearPending();
        $this->finishOne($id, 'start', false);
    }

    public function stopService(string $id): void
    {
        $line = $this->line($id);
        if ($line === null || $line['disabled']) {
            return;
        }
        $this->keepArmed('project-stop', $id);
        if (! $this->allowProjectShared('project-stop', 'stop', $id, [$id])) {
            return;
        }
        $this->finishOne($id, 'stop', false);
    }

    public function restartService(string $id): void
    {
        $line = $this->line($id);
        if ($line === null || $line['disabled']) {
            return;
        }
        $this->keepArmed('project-restart', $id);
        if (! $this->allowProjectShared('project-restart', 'restart', $id, [$id])) {
            return;
        }
        $this->finishOne($id, 'restart', false);
    }

    public function reclaimPort(string $id): void
    {
        $line = $this->line($id);
        if ($line === null || $line['disabled'] || $line['state'] !== 'externally-owned') {
            return;
        }
        $this->keepArmed('kill', $id);
        if (! $this->armed('kill', $id)) {
            $this->pendingKind = 'kill';
            $this->pendingId = $id;
            $this->notice = "Press again to reclaim the port for {$id} and start it.";

            return;
        }
        $this->clearPending();
        $this->finishOne($id, 'start', true);
    }

    public function startAll(): void
    {
        $this->clearPending();
        $ids = ServiceBoard::startAllTargets($this->catalogGroups, $this->sections);
        $ids = array_values(array_filter($ids, function (string $id): bool {
            $line = $this->line($id);

            return $line === null || ! $line['disabled'];
        }));
        $this->runMany($ids, 'start', 'Started.');
    }

    public function stopAll(): void
    {
        $this->keepArmed('stop-all', 'all');
        $ids = ServiceBoard::stopAllTargets($this->sections);
        if (! $this->allowProjectShared('stop-all', 'stop', 'all', $ids)) {
            return;
        }
        $this->runMany($ids, 'stop', 'Stopped.');
    }

    public function startGroup(string $name): void
    {
        $this->clearPending();
        $this->runMany(ServiceBoard::groupTargets($this->sections, $name), 'start', "Started {$name}.");
    }

    public function stopGroup(string $name): void
    {
        $this->keepArmed('stop-group', $name);
        $ids = array_values(array_filter(
            ServiceBoard::groupTargets($this->sections, $name),
            function (string $id): bool {
                $line = $this->line($id);

                return $line !== null && ! in_array($line['state'], ['stopped', 'succeeded'], true);
            },
        ));
        if (! $this->allowProjectShared('stop-group', 'stop', $name, $ids)) {
            return;
        }
        $this->runMany($ids, 'stop', "Stopped {$name}.");
    }

    public function restartGroup(string $name): void
    {
        $this->keepArmed('restart-group', $name);
        $ids = ServiceBoard::groupTargets($this->sections, $name);
        if (! $this->allowProjectShared('restart-group', 'restart', $name, $ids)) {
            return;
        }
        $this->runMany($ids, 'restart', "Restarted {$name}.");
    }

    public function installRecipe(string $id): void
    {
        $this->clearPending();
        $result = app(HearthCommands::class)->run(base_path(), ['shared', 'install', $id, '--json'], null);
        $this->notice = $result->ok
            ? "Installed {$id}."
            : ($result->visibleMessage() !== '' ? $result->visibleMessage() : 'install failed');
        $this->loadShared();
    }

    public function startInstance(string $id): void
    {
        $this->clearPending();
        $result = app(HearthCommands::class)->run(base_path(), ['shared', 'start', $id, '--json'], null);
        $this->notice = $result->ok
            ? "Started {$id}."
            : ($result->visibleMessage() !== '' ? $result->visibleMessage() : 'start failed');
        $this->loadShared();
    }

    public function stopInstance(string $id): void
    {
        $this->keepArmed('instance-stop', $id);
        if (! $this->allowInstance($id, 'stop')) {
            return;
        }
        $result = app(HearthCommands::class)->run(base_path(), ['shared', 'stop', $id, '--json'], 120);
        $this->notice = $result->ok
            ? "Stopped {$id}."
            : ($result->visibleMessage() !== '' ? $result->visibleMessage() : 'stop failed');
        $this->loadShared();
    }

    public function restartInstance(string $id): void
    {
        $this->keepArmed('instance-restart', $id);
        if (! $this->allowInstance($id, 'restart')) {
            return;
        }
        $commands = app(HearthCommands::class);
        $stop = $commands->run(base_path(), ['shared', 'stop', $id, '--json'], 120);
        if (! $stop->ok) {
            $this->notice = $stop->visibleMessage() !== '' ? $stop->visibleMessage() : 'stop failed';
            $this->loadShared();

            return;
        }
        $start = $commands->run(base_path(), ['shared', 'start', $id, '--json'], null);
        $this->notice = $start->ok
            ? "Restarted {$id}."
            : ($start->visibleMessage() !== '' ? $start->visibleMessage() : 'start failed');
        $this->loadShared();
    }

    public function removeInstance(string $id): void
    {
        $this->keepArmed('shared-remove', $id);
        [$unchecked, $affected, $force] = $this->removeImpact($id);
        if (! $this->armed('shared-remove', $id)) {
            $this->pendingKind = 'shared-remove';
            $this->pendingId = $id;
            $this->notice = ServiceBoard::removeNotice($id, $affected, $unchecked);

            return;
        }
        $this->clearPending();
        $args = ['shared', 'remove', $id, '--json'];
        if ($force) {
            $args[] = '--force';
        }
        $result = app(HearthCommands::class)->run(base_path(), $args, 120);
        $this->notice = $result->ok
            ? "Removed {$id}."
            : ($result->visibleMessage() !== '' ? $result->visibleMessage() : 'remove failed');
        $this->loadShared();
    }

    public function render()
    {
        $store = WorkspaceStore::instance();
        $binary = app(HearthBinary::class)->report();
        $rows = [];
        foreach ($store->rows() as $row) {
            $rows[] = $this->present($row);
        }
        $selected = null;
        if ($this->selectedId !== null) {
            $row = $store->get($this->selectedId);
            $selected = $row === null ? null : $this->present($row);
        }

        return view('livewire.workspace-desk', [
            'rows' => $rows,
            'selected' => $selected,
            'loadError' => $store->loadError,
            'binary' => $binary,
            'selectedLine' => $this->line($this->selectedService),
        ]);
    }

    /**
     * @param  array{id: string, path: string, trusted: bool, addedAt: string}  $row
     * @return array{id: string, name: string, path: string, fullPath: string, trusted: bool, missing: bool, stopped: bool, hasToken: bool}
     */
    private function present(array $row): array
    {
        return [
            'id' => $row['id'],
            'name' => WorkspaceStore::folderName($row['path']),
            'path' => WorkspaceStore::displayPath($row['path']),
            'fullPath' => $row['path'],
            'trusted' => $row['trusted'],
            'missing' => ! is_dir($row['path']),
            'stopped' => DaemonMemory::isStopped($row['id']),
            'hasToken' => DaemonMemory::hasToken($row['path']),
        ];
    }

    /**
     * @return ?array{id: string, path: string, trusted: bool, addedAt: string}
     */
    private function selectedRow(): ?array
    {
        if ($this->selectedId === null) {
            return null;
        }

        return WorkspaceStore::instance()->get($this->selectedId);
    }

    private function discoverSelected(): void
    {
        $row = $this->selectedRow();
        if ($row === null) {
            return;
        }
        if (! is_dir($row['path'])) {
            $this->resetBoard();
            $this->notice = $this->missingNotice($row);

            return;
        }

        $result = app(RunsHearth::class)->manager($row['path'], 'status');
        $stopped = DaemonMemory::isStopped($row['id']);
        if (! $result->ok) {
            DaemonMemory::forgetRoot($row['path']);
        }
        if ($stopped) {
            $this->resetBoard();
            $this->notice = $result->ok
                ? 'Daemon is stopping. Start runs it again.'
                : 'Daemon stopped. Start runs it again.';

            return;
        }
        if ($result->ok) {
            $port = $result->json['port'] ?? null;
            $protocol = $result->json['protocolVersion'] ?? null;
            $this->notice = 'Daemon is up'
                .(is_numeric($port) ? ' on port '.$port : '')
                .($protocol !== null ? ', protocol '.$protocol : '')
                .'.';
            if (! DaemonMemory::hasToken($row['path'])) {
                $this->resetBoard();
                $this->notice .= ' Start attaches this window.';

                return;
            }
            $this->loadBoard($row['path']);

            return;
        }

        $this->resetBoard();
        $message = $result->visibleMessage();
        $name = WorkspaceStore::folderName($row['path']);
        $path = WorkspaceStore::displayPath($row['path']);
        if ($message === '' || $message === 'hearth manager is unavailable') {
            $this->notice = $row['trusted']
                ? "{$name} ({$path}). Start runs the daemon."
                : "{$name} ({$path}) is untrusted. Trust starts the daemon.";

            return;
        }
        $this->notice = $message;
    }

    /**
     * @param  array{id: string, path: string, trusted: bool, addedAt: string}  $row
     */
    private function ensure(array $row): void
    {
        $result = app(RunsHearth::class)->manager($row['path'], 'ensure');
        $payload = $result->json;
        if (! $result->ok || ! is_array($payload) || ! isset($payload['token'], $payload['port'])) {
            $this->notice = $result->visibleMessage() !== '' ? $result->visibleMessage() : 'manager ensure failed';

            return;
        }
        DaemonMemory::put($row['path'], $payload);
        if (! DaemonMemory::hasToken($row['path'])) {
            $this->notice = 'manager ensure failed';

            return;
        }
        $port = (int) $payload['port'];
        $protocol = $payload['protocolVersion'] ?? null;
        $healthy = $this->healthz($port);
        $this->notice = 'Daemon is up on port '.$port
            .($protocol !== null ? ', protocol '.$protocol : '')
            .'.';
        if (! $healthy) {
            $this->notice .= ' healthz did not answer.';
        }
        $this->loadBoard($row['path']);
    }

    private function loadBoard(string $root): void
    {
        $this->maybeReload($root);
        $api = ManagerHttp::open($root);
        if ($api === null) {
            $this->resetBoard();
            $this->notice = 'Daemon session ended. Start attaches this window.';

            return;
        }
        $services = $api->get('/v1/services');
        if ($services->unauthorized) {
            $this->resetBoard();
            $this->notice = 'Daemon session ended. Start attaches this window.';

            return;
        }
        if (! $services->ok) {
            return;
        }
        $catalog = $api->get('/v1/catalog');
        $body = ['services' => [], 'groups' => [], 'groupTree' => []];
        if ($catalog->unauthorized) {
            $this->resetBoard();
            $this->notice = 'Daemon session ended. Start attaches this window.';

            return;
        }
        if ($catalog->ok && is_array($catalog->json['catalog'] ?? null)) {
            $body = $catalog->json['catalog'];
            $this->catalogGroups = is_array($body['groups'] ?? null) ? $body['groups'] : [];
        }
        $live = is_array($services->json['services'] ?? null) ? $services->json['services'] : [];
        $this->sections = ServiceBoard::sections(is_array($body) ? $body : [], $live);
        $this->summary = ServiceBoard::summary($this->sections);
        $urls = $api->get('/v1/urls');
        if ($urls->unauthorized) {
            $this->resetBoard();
            $this->notice = 'Daemon session ended. Start attaches this window.';

            return;
        }
        $this->urls = ServiceBoard::visibleUrls(
            $urls->ok && is_array($urls->json['urls'] ?? null) ? $urls->json['urls'] : [],
            $this->sections,
        );
        $this->syncLifecycle($live);
        if ($this->logOpen) {
            $this->loadLog($api);
        }
    }

    public function toggleLog(): void
    {
        $this->logOpen = ! $this->logOpen;
        if (! $this->logOpen) {
            return;
        }
        $row = $this->selectedRow();
        if ($row === null || DaemonMemory::isStopped($row['id']) || ! DaemonMemory::hasToken($row['path'])) {
            return;
        }
        $api = ManagerHttp::open($row['path']);
        if ($api !== null) {
            $this->loadLog($api);
        }
    }

    public function expandLog(): void
    {
        if (! $this->logOpen || ! $this->logHasMore) {
            return;
        }
        $this->logLimit = min($this->logLimit * 4, 262144);
        $this->logText = '';
        $this->logCursor = null;
        $this->logGeneration = null;
        $this->logHasMore = false;
        $row = $this->selectedRow();
        if ($row === null || DaemonMemory::isStopped($row['id']) || ! DaemonMemory::hasToken($row['path'])) {
            return;
        }
        $api = ManagerHttp::open($row['path']);
        if ($api !== null) {
            $this->loadLog($api);
        }
    }

    /**
     * @param  list<array<string, mixed>>  $live
     */
    private function syncLifecycle(array $live): void
    {
        if ($this->selectedService === '$daemon') {
            return;
        }
        $next = null;
        foreach ($live as $row) {
            if (($row['serviceId'] ?? null) === $this->selectedService && isset($row['generation']) && is_numeric($row['generation'])) {
                $next = (int) $row['generation'];
            }
        }
        if ($this->serviceGeneration !== null && $next !== $this->serviceGeneration) {
            $this->logCursor = null;
            $this->logGeneration = null;
        }
        $this->serviceGeneration = $next;
    }

    private function loadLog(ManagerHttp $api): void
    {
        if ($this->selectedService === '$daemon') {
            $result = $api->daemonLog($this->logLimit);
        } else {
            $result = $api->serviceLog($this->selectedService, $this->logCursor, $this->logGeneration, $this->logLimit);
        }
        if ($result->unauthorized) {
            $this->resetBoard();
            $this->notice = 'Daemon session ended. Start attaches this window.';

            return;
        }
        if (! $result->ok || ! is_array($result->json)) {
            return;
        }
        $this->applyLog($result->json);
    }

    /**
     * @param  array<string, mixed>  $slice
     */
    private function applyLog(array $slice): void
    {
        $data = (string) ($slice['data'] ?? '');
        $reset = (bool) ($slice['reset'] ?? false);
        if ($reset) {
            $this->logHasMore = strlen($data) >= $this->logLimit - 16 && $this->logLimit < 262144;
        }
        $combined = ($reset || $this->logText === '') ? $data : $this->logText.$data;
        $this->logText = ServiceBoard::boundedTail($combined, 262144);
        if (isset($slice['nextCursor']) && is_numeric($slice['nextCursor'])) {
            $this->logCursor = (int) $slice['nextCursor'];
        }
        if (isset($slice['generation']) && is_numeric($slice['generation'])) {
            $this->logGeneration = (int) $slice['generation'];
        }
    }

    private function maybeReload(string $root): void
    {
        $next = ServiceBoard::catalogStamp($root);
        if (ServiceBoard::shouldReloadCatalog($this->catalogMtime, $next)) {
            $result = app(RunsHearth::class)->manager($root, 'reload');
            if (! $result->ok) {
                $this->notice = $result->visibleMessage() !== ''
                    ? $result->visibleMessage()
                    : 'Catalog reload failed. The running catalog stays.';
            }
        }
        if ($next !== null) {
            $this->catalogMtime = $next;
        }
    }

    private function finishOne(string $id, string $action, bool $killUnowned): void
    {
        $error = $this->runAction($id, $action, $killUnowned);
        $this->afterActions($error === null ? $this->doneWord($action)." {$id}." : $error);
    }

    /**
     * @param  list<string>  $ids
     */
    private function runMany(array $ids, string $action, string $done): void
    {
        if ($ids === []) {
            $this->notice = $action === 'stop' ? 'Nothing to stop.' : 'Nothing to start.';

            return;
        }
        $failed = [];
        foreach ($ids as $id) {
            $error = $this->runAction($id, $action, false);
            if ($error === 'session') {
                $this->afterActions('session');

                return;
            }
            if ($error !== null) {
                $failed[] = $id;
            }
        }
        $this->afterActions($failed === [] ? $done : 'Failed: '.implode(', ', $failed).'.');
    }

    private function afterActions(?string $notice): void
    {
        if ($notice === 'session') {
            $this->resetBoard();
            $this->notice = 'Daemon session ended. Start attaches this window.';

            return;
        }
        $row = $this->selectedRow();
        if ($row !== null && DaemonMemory::hasToken($row['path']) && ! DaemonMemory::isStopped($row['id'])) {
            $this->loadBoard($row['path']);
        }
        $this->notice = $notice;
    }

    private function runAction(string $serviceId, string $action, bool $killUnowned): ?string
    {
        $row = $this->selectedRow();
        if ($row === null) {
            return 'No workspace selected.';
        }
        $api = ManagerHttp::open($row['path']);
        if ($api === null) {
            return 'session';
        }
        $posted = $api->submit($serviceId, $action, $killUnowned);
        if ($posted->unauthorized) {
            return 'session';
        }
        if (! $posted->ok) {
            return $posted->message !== '' ? $posted->message : "{$action} failed";
        }
        $operation = is_array($posted->json['operation'] ?? null) ? $posted->json['operation'] : null;
        $id = is_array($operation) ? ($operation['id'] ?? null) : null;
        $status = is_array($operation) ? ($operation['status'] ?? null) : null;
        if (! is_string($id) || $id === '') {
            return "{$action} failed";
        }
        if ($status === 'failed') {
            return $this->operationMessage($operation) ?? "{$action} failed";
        }
        if ($status === 'succeeded') {
            return null;
        }
        $waited = $api->wait($id);
        if ($waited->unauthorized) {
            return 'session';
        }
        if (! $waited->ok) {
            return $waited->message !== '' ? $waited->message : "{$action} failed";
        }
        $settled = is_array($waited->json['operation'] ?? null) ? $waited->json['operation'] : [];
        if (($settled['status'] ?? null) === 'succeeded') {
            return null;
        }

        return $this->operationMessage($settled) ?? "{$action} failed";
    }

    /**
     * @param  array<string, mixed>  $operation
     */
    private function operationMessage(array $operation): ?string
    {
        $error = $operation['error'] ?? null;
        if (is_array($error) && is_string($error['message'] ?? null) && $error['message'] !== '') {
            return $error['message'];
        }

        return null;
    }

    /**
     * @param  list<string>  $serviceIds
     */
    private function allowProjectShared(string $kind, string $verb, string $key, array $serviceIds): bool
    {
        $instances = [];
        foreach ($serviceIds as $id) {
            $line = $this->line($id);
            $instance = $line['sharedInstance'] ?? null;
            if (is_string($instance) && $instance !== '') {
                $instances[$instance] = true;
            }
        }
        if ($instances === []) {
            return true;
        }
        if ($this->armed($kind, $key)) {
            $this->clearPending();

            return true;
        }
        $row = $this->selectedRow();
        $currentName = $row !== null ? WorkspaceStore::folderName($row['path']) : 'this workspace';
        $currentRoot = $row['path'] ?? null;
        $payload = $this->installedPayload();
        if ($payload === null) {
            $this->pendingKind = $kind;
            $this->pendingId = $key;
            $this->notice = ServiceBoard::uncheckedSharedNotice($verb, ServiceBoard::joinNames(array_keys($instances)));

            return false;
        }
        $byId = $this->instancesById($payload);
        $touches = [];
        $unknown = [];
        foreach (array_keys($instances) as $instanceId) {
            if (! isset($byId[$instanceId])) {
                $unknown[] = $instanceId;

                continue;
            }
            $report = ServiceBoard::classifyAttachments(
                ServiceBoard::attachmentRoots($byId[$instanceId]),
                $currentRoot,
                $this->knownRoots(),
            );
            if ($report['others'] !== []) {
                $touches[] = ['instance' => $instanceId, 'others' => $report['others']];
            }
        }
        if ($touches === [] && $unknown === []) {
            return true;
        }
        usort($touches, fn (array $left, array $right) => $left['instance'] <=> $right['instance']);
        sort($unknown);
        $this->pendingKind = $kind;
        $this->pendingId = $key;
        $this->notice = $touches === []
            ? ServiceBoard::uncheckedSharedNotice($verb, ServiceBoard::joinNames($unknown))
            : ServiceBoard::projectSharedNotice($verb, $currentName, $touches, $unknown);

        return false;
    }

    private function allowInstance(string $id, string $verb): bool
    {
        if ($this->armed('instance-'.$verb, $id)) {
            $this->clearPending();

            return true;
        }
        $payload = $this->installedPayload();
        if ($payload === null) {
            $this->armInstance($verb, $id, ServiceBoard::uncheckedSharedNotice($verb, $id));

            return false;
        }
        $instance = $this->instancesById($payload)[$id] ?? null;
        if (! is_array($instance)) {
            $this->armInstance($verb, $id, ServiceBoard::uncheckedSharedNotice($verb, $id));

            return false;
        }
        $current = $this->selectedRow()['path'] ?? null;
        $report = ServiceBoard::classifyAttachments(ServiceBoard::attachmentRoots($instance), $current, $this->knownRoots());
        if ($report['others'] === []) {
            return true;
        }
        $this->armInstance($verb, $id, ServiceBoard::instanceSharedNotice($verb, $id, $report['all']));

        return false;
    }

    private function armInstance(string $verb, string $id, string $notice): void
    {
        $this->pendingKind = 'instance-'.$verb;
        $this->pendingId = $id;
        $this->notice = $notice;
    }

    /**
     * @return array{0: bool, 1: list<string>, 2: bool}
     */
    private function removeImpact(string $id): array
    {
        $payload = $this->installedPayload();
        if ($payload === null) {
            return [true, [], true];
        }
        $instance = $this->instancesById($payload)[$id] ?? null;
        if (! is_array($instance)) {
            return [true, [], true];
        }
        $report = ServiceBoard::classifyAttachments(ServiceBoard::attachmentRoots($instance), null, $this->knownRoots());

        return [false, $report['all'], $report['all'] !== []];
    }

    /**
     * @return ?array<string, mixed>
     */
    private function installedPayload(): ?array
    {
        $result = app(HearthCommands::class)->run(base_path(), ['shared', 'installed', '--json'], 20);
        if (! $result->ok || ! is_array($result->json)) {
            return null;
        }

        return $result->json;
    }

    private function loadShared(): void
    {
        $commands = app(HearthCommands::class);
        $cwd = base_path();
        $list = $commands->run($cwd, ['shared', 'list', '--json'], 20);
        $status = $commands->run($cwd, ['shared', 'status', '--json'], 20);
        if ($status->ok && is_array($status->json)) {
            $this->smpLive = true;
            $this->instances = ServiceBoard::instancesFrom($status->json);
        } else {
            $this->smpLive = false;
            $installed = $commands->run($cwd, ['shared', 'installed', '--json'], 20);
            $this->instances = $installed->ok && is_array($installed->json)
                ? ServiceBoard::instancesFrom($installed->json)
                : [];
            if ($this->notice === null || $this->notice === '') {
                $this->notice = $status->visibleMessage() !== ''
                    ? $status->visibleMessage()
                    : 'smp is not running. Showing the local registry.';
            }
        }
        $this->recipes = is_array($list->json) ? ServiceBoard::recipesFrom($list->json) : [];
    }

    /**
     * @param  array<string, mixed>  $payload
     * @return array<string, array<string, mixed>>
     */
    private function instancesById(array $payload): array
    {
        $byId = [];
        foreach ($payload['instances'] ?? [] as $instance) {
            if (is_array($instance)) {
                $byId[ServiceBoard::instanceId($instance)] = $instance;
            }
        }

        return $byId;
    }

    /**
     * @return list<array{root: string, name: string, path: string}>
     */
    private function knownRoots(): array
    {
        $known = [];
        foreach (WorkspaceStore::instance()->rows() as $row) {
            $known[] = [
                'root' => $row['path'],
                'name' => WorkspaceStore::folderName($row['path']),
                'path' => WorkspaceStore::displayPath($row['path']),
            ];
        }

        return $known;
    }

    /**
     * @return ?array<string, mixed>
     */
    private function line(string $id): ?array
    {
        foreach ($this->sections as $section) {
            foreach ($section['services'] as $service) {
                if ($service['id'] === $id) {
                    return $service;
                }
            }
        }

        return null;
    }

    private function doneWord(string $action): string
    {
        return match ($action) {
            'stop' => 'Stopped',
            'restart' => 'Restarted',
            default => 'Started',
        };
    }

    private function healthz(int $port): bool
    {
        try {
            $response = Http::timeout(5)->get("http://127.0.0.1:{$port}/healthz");
        } catch (\Throwable) {
            return false;
        }

        return $response->successful();
    }

    /**
     * @param  array{id: string, path: string, trusted: bool, addedAt: string}  $row
     */
    private function missingNotice(array $row): string
    {
        $name = WorkspaceStore::folderName($row['path']);
        $path = WorkspaceStore::displayPath($row['path']);

        return $path === '' ? "{$name} is missing." : "{$name} ({$path}) is missing.";
    }

    private function armed(string $kind, string $id): bool
    {
        return $this->pendingKind === $kind && $this->pendingId === $id;
    }

    private function keepArmed(string $kind, string $id): void
    {
        if (! $this->armed($kind, $id)) {
            $this->clearPending();
        }
    }

    private function clearPending(): void
    {
        $this->pendingKind = null;
        $this->pendingId = null;
    }

    private function resetBoard(): void
    {
        $this->sections = [];
        $this->summary = '';
        $this->urls = [];
        $this->catalogGroups = [];
        $this->logText = '';
        $this->logCursor = null;
        $this->logGeneration = null;
        $this->serviceGeneration = null;
        $this->logLimit = 16384;
        $this->logHasMore = false;
        $this->catalogMtime = null;
        $this->selectedService = '$daemon';
    }
}
