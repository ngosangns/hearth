<?php

namespace Tests;

use App\Support\DaemonMemory;
use App\Support\WorkspaceStore;
use Illuminate\Foundation\Testing\TestCase as BaseTestCase;

abstract class TestCase extends BaseTestCase
{
    protected string $workspaceFile = '';

    protected function setUp(): void
    {
        parent::setUp();

        $this->workspaceFile = sys_get_temp_dir().'/hearth-macos-ws-'.bin2hex(random_bytes(8)).'.json';
        config(['hearth.workspace_file' => $this->workspaceFile]);
        WorkspaceStore::reset();
        DaemonMemory::reset();
    }

    protected function tearDown(): void
    {
        if ($this->workspaceFile !== '') {
            @unlink($this->workspaceFile);
            @unlink($this->workspaceFile.'.tmp');
            foreach (glob($this->workspaceFile.'.corrupt-*') ?: [] as $file) {
                @unlink($file);
            }
        }
        WorkspaceStore::reset();
        DaemonMemory::reset();

        parent::tearDown();
    }
}
