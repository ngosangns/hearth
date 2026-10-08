<?php

namespace Tests\Unit;

use App\Support\ServiceBoard;
use Tests\TestCase;

class ServiceBoardTest extends TestCase
{
    public function test_sections_follow_direct_membership_and_degrade_unready(): void
    {
        $catalog = [
            'services' => [
                $this->service('web', 'http'),
                $this->service('job', 'exit', 'Job'),
                $this->service('off', 'http', disabled: true),
                [
                    'id' => 'db',
                    'label' => 'Database',
                    'profiles' => [
                        'run' => [
                            'commandStatus' => 'verified',
                            'command' => ['command' => ['argv' => ['hearth', 'shared', 'attach', 'postgres@16.4']]],
                            'readiness' => ['kind' => 'command'],
                        ],
                    ],
                ],
            ],
            'groups' => ['all' => ['web', 'job', 'db']],
            'groupTree' => [
                ['name' => 'app', 'members' => ['web', 'job']],
                ['name' => 'data', 'members' => ['db', 'off']],
            ],
        ];
        $live = [
            ['serviceId' => 'web', 'actualState' => 'running-unready', 'generation' => 2],
            ['serviceId' => 'job', 'actualState' => 'succeeded', 'generation' => 1],
            ['serviceId' => 'db', 'actualState' => 'ready', 'generation' => 4],
            ['serviceId' => 'off', 'actualState' => 'stopped', 'generation' => 1],
        ];

        $sections = ServiceBoard::sections($catalog, $live);

        $this->assertSame(['app', 'data'], array_column($sections, 'name'));
        $this->assertSame('degraded', $sections[0]['services'][0]['display']);
        $this->assertTrue($sections[0]['services'][0]['up']);
        $this->assertFalse(ServiceBoard::showsStop('succeeded'));
        $this->assertTrue($sections[0]['services'][1]['finite']);
        // Inside a group, rows follow catalog order, the same rule as the TUI.
        $this->assertTrue($sections[1]['services'][0]['disabled']);
        $this->assertSame('postgres@16.4', $sections[1]['services'][1]['sharedInstance']);
        $this->assertSame('2/3 ready', ServiceBoard::summary($sections));
        $this->assertSame(['web', 'db'], ServiceBoard::stopAllTargets($sections));
        $this->assertSame(['web', 'job', 'db'], ServiceBoard::startAllTargets($catalog['groups'], $sections));
        $this->assertTrue(ServiceBoard::groupIsUp($sections, 'app'));
        $this->assertSame(['db'], ServiceBoard::groupTargets($sections, 'data'));
    }

    public function test_summary_counts_a_failed_finite_service_and_hides_a_stopped_url(): void
    {
        $sections = ServiceBoard::sections([
            'services' => [$this->service('job', 'exit'), $this->service('export', 'exit'), $this->service('web', 'http')],
        ], [
            ['serviceId' => 'job', 'actualState' => 'failed'],
            ['serviceId' => 'export', 'actualState' => 'succeeded'],
            ['serviceId' => 'web', 'actualState' => 'stopped'],
        ]);

        $this->assertSame('0/2 ready  1 failed', ServiceBoard::summary($sections));
        $this->assertFalse(ServiceBoard::showsStop('succeeded'));
        $this->assertTrue(ServiceBoard::urlVisible(true, 'succeeded'));
        $urls = ServiceBoard::visibleUrls([
            ['serviceId' => 'export', 'url' => 'http://127.0.0.1/export', 'label' => 'Export'],
            ['serviceId' => 'web', 'url' => 'http://127.0.0.1/web'],
            ['serviceId' => 'web', 'url' => 'http://127.0.0.1/always', 'requiresRunning' => false],
        ], $sections);
        $this->assertSame(['http://127.0.0.1/export', 'http://127.0.0.1/always'], array_column($urls, 'url'));
    }

    public function test_catalog_reload_waits_for_a_second_observation(): void
    {
        $this->assertFalse(ServiceBoard::shouldReloadCatalog(null, 10));
        $this->assertFalse(ServiceBoard::shouldReloadCatalog(10, 10));
        $this->assertFalse(ServiceBoard::shouldReloadCatalog(10, null));
        $this->assertTrue(ServiceBoard::shouldReloadCatalog(10, 11));
    }

    public function test_shared_notices_name_workspaces_and_use_periods(): void
    {
        $notice = ServiceBoard::projectSharedNotice('stop', 'desk', [
            ['instance' => 'postgres@16.4', 'others' => ['viclass']],
        ], []);
        $this->assertStringContainsString('postgres@16.4 (viclass)', $notice);
        $this->assertStringContainsString('only detaches desk', $notice);
        $this->assertStringContainsString('Those workspaces keep it.', $notice);
        $this->assertStringNotContainsString('—', $notice);

        $instance = ServiceBoard::instanceSharedNotice('remove', 'redis@8', ['infra', 'viclass']);
        $this->assertStringContainsString('deletes its data', $instance);
        $this->assertSame(
            'Could not check which workspaces use redis@8. Press again to stop anyway.',
            ServiceBoard::uncheckedSharedNotice('stop', 'redis@8'),
        );
    }

    public function test_attachment_roots_accept_a_map_or_a_list(): void
    {
        $roots = ServiceBoard::attachmentRoots([
            'attachments' => [
                'abc' => ['projectRoot' => '/work/a'],
            ],
        ]);
        $this->assertSame(['/work/a'], $roots);
        $this->assertSame('redis@8.2.10', ServiceBoard::instanceId(['name' => 'redis', 'version' => '8.2.10']));
        $this->assertSame('redis@8.2.10', ServiceBoard::recipesFrom([
            'services' => ['redis' => ['versions' => ['8.2.10' => []]]],
        ])[0]['id']);
    }

    public function test_log_tail_counts_characters(): void
    {
        $this->assertSame('éé', ServiceBoard::boundedTail('ééé', 2));
    }

    /**
     * @return array<string, mixed>
     */
    private function service(string $id, string $kind, ?string $label = null, bool $disabled = false): array
    {
        return [
            'id' => $id,
            'label' => $label,
            'disabled' => $disabled,
            'profiles' => ['run' => ['commandStatus' => 'verified', 'readiness' => ['kind' => $kind], 'command' => ['command' => ['argv' => ['/bin/echo']]]]],
        ];
    }
}
