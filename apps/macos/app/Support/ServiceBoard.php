<?php

namespace App\Support;

/**
 * Pure desk rules ported from the TUI. Wire strings stay wire strings.
 * `running-unready` is the only state whose label changes (`degraded`).
 */
final class ServiceBoard
{
    public static function displayState(string $wire): string
    {
        return $wire === 'running-unready' ? 'degraded' : $wire;
    }

    public static function isUp(string $state): bool
    {
        return in_array($state, ['ready', 'running', 'running-unready'], true);
    }

    /**
     * Stop is the primary action while a start is still in flight. Succeeded is not up.
     */
    public static function showsStop(string $state): bool
    {
        return self::isUp($state) || in_array($state, ['queued-start', 'starting', 'preparing', 'stopping'], true);
    }

    public static function boundedTail(string $text, int $limit): string
    {
        if ($limit < 1) {
            return '';
        }
        if (mb_strlen($text) <= $limit) {
            return $text;
        }

        return mb_substr($text, -$limit);
    }

    /**
     * @param  array<string, mixed>  $service
     */
    public static function sharedInstanceOf(array $service): ?string
    {
        $profile = $service['profiles']['run'] ?? null;
        if (! is_array($profile) || ($profile['commandStatus'] ?? null) !== 'verified') {
            return null;
        }
        $argv = $profile['command']['command']['argv'] ?? null;
        if (! is_array($argv)) {
            return null;
        }
        $count = count($argv);
        for ($index = 0; $index + 1 < $count; $index++) {
            if ($argv[$index] !== 'shared' || $argv[$index + 1] !== 'attach') {
                continue;
            }
            $id = $argv[$index + 2] ?? null;
            if (is_string($id) && str_contains($id, '@') && ! str_starts_with($id, '-')) {
                return $id;
            }

            return null;
        }

        return null;
    }

    /**
     * @param  array<string, mixed>  $service
     */
    public static function isFinite(array $service): bool
    {
        return ($service['profiles']['run']['readiness']['kind'] ?? null) === 'exit';
    }

    /**
     * @param  array<string, mixed>  $catalog
     * @param  list<array<string, mixed>>  $live
     * @return list<array{name: ?string, services: list<array<string, mixed>>}>
     */
    public static function sections(array $catalog, array $live): array
    {
        $byId = [];
        foreach ($live as $row) {
            if (! is_array($row)) {
                continue;
            }
            $id = $row['serviceId'] ?? null;
            if (is_string($id) && $id !== '') {
                $byId[$id] = $row;
            }
        }

        $metas = [];
        $seen = [];
        foreach ($catalog['services'] ?? [] as $service) {
            if (! is_array($service) || ! is_string($service['id'] ?? null) || $service['id'] === '') {
                continue;
            }
            $metas[] = self::meta($service);
            $seen[$service['id']] = true;
        }
        foreach ($byId as $id => $row) {
            if (isset($seen[$id])) {
                continue;
            }
            $metas[] = [
                'id' => $id,
                'label' => $id,
                'ports' => '',
                'disabled' => false,
                'finite' => false,
                'infra' => false,
                'shared' => false,
                'sharedInstance' => null,
            ];
        }

        $line = function (array $meta) use ($byId): array {
            $found = $byId[$meta['id']] ?? null;
            $state = is_array($found) && is_string($found['actualState'] ?? null) ? $found['actualState'] : 'stopped';
            $error = is_array($found) && is_string($found['error'] ?? null) && $found['error'] !== '' ? $found['error'] : null;

            return [
                'id' => $meta['id'],
                'label' => $meta['label'] !== '' ? $meta['label'] : $meta['id'],
                'ports' => $meta['ports'],
                'state' => $state,
                'display' => self::displayState($state),
                'disabled' => $meta['disabled'],
                'finite' => $meta['finite'],
                'infra' => $meta['infra'],
                'shared' => $meta['shared'],
                'sharedInstance' => $meta['sharedInstance'],
                'error' => $error,
                'up' => self::isUp($state),
            ];
        };

        $tree = $catalog['groupTree'] ?? [];
        if (! is_array($tree) || $tree === []) {
            return [[
                'name' => null,
                'services' => array_map($line, $metas),
            ]];
        }

        $first = [];
        $built = [];
        foreach ($tree as $group) {
            if (! is_array($group) || ! is_string($group['name'] ?? null)) {
                continue;
            }
            $built[] = ['name' => $group['name'], 'services' => []];
            foreach ($group['members'] ?? [] as $member) {
                if (is_string($member) && ! isset($first[$member])) {
                    $first[$member] = $group['name'];
                }
            }
        }

        $rest = [];
        foreach ($metas as $meta) {
            $service = $line($meta);
            $name = $first[$meta['id']] ?? null;
            if ($name === null) {
                $rest[] = $service;

                continue;
            }
            foreach ($built as $index => $section) {
                if ($section['name'] === $name) {
                    $built[$index]['services'][] = $service;

                    continue 2;
                }
            }
            $rest[] = $service;
        }

        $built = array_values(array_filter($built, fn (array $section) => $section['services'] !== []));
        if ($rest !== []) {
            $built[] = ['name' => null, 'services' => $rest];
        }

        return $built !== [] ? $built : [['name' => null, 'services' => []]];
    }

    /**
     * @param  list<array{name: ?string, services: list<array<string, mixed>>}>  $sections
     */
    public static function summary(array $sections): string
    {
        $ready = 0;
        $failed = 0;
        $total = 0;
        foreach (self::lines($sections) as $service) {
            if ($service['finite'] && $service['state'] !== 'failed') {
                continue;
            }
            $total++;
            if (self::isUp($service['state'])) {
                $ready++;
            } elseif ($service['state'] === 'failed') {
                $failed++;
            }
        }
        if ($total === 0) {
            return '';
        }
        if ($failed > 0) {
            return "{$ready}/{$total} ready  {$failed} failed";
        }

        return "{$ready}/{$total} ready";
    }

    /**
     * @param  list<array{name: ?string, services: list<array<string, mixed>>}>  $sections
     * @return list<string>
     */
    public static function stopAllTargets(array $sections): array
    {
        $ids = [];
        foreach (self::lines($sections) as $service) {
            if ($service['disabled']) {
                continue;
            }
            if (in_array($service['state'], ['stopped', 'succeeded'], true)) {
                continue;
            }
            $ids[] = $service['id'];
        }

        return $ids;
    }

    /**
     * @param  list<array{name: ?string, services: list<array<string, mixed>>}>  $sections
     * @return list<string>
     */
    public static function groupTargets(array $sections, string $name): array
    {
        foreach ($sections as $section) {
            if (($section['name'] ?? null) !== $name) {
                continue;
            }
            $ids = [];
            foreach ($section['services'] as $service) {
                if (! $service['disabled']) {
                    $ids[] = $service['id'];
                }
            }

            return $ids;
        }

        return [];
    }

    /**
     * @param  list<array{name: ?string, services: list<array<string, mixed>>}>  $sections
     */
    public static function groupIsUp(array $sections, string $name): bool
    {
        $longLived = [];
        foreach ($sections as $section) {
            if (($section['name'] ?? null) !== $name) {
                continue;
            }
            foreach ($section['services'] as $service) {
                if (! $service['disabled'] && ! $service['finite']) {
                    $longLived[] = $service;
                }
            }
        }

        if ($longLived === []) {
            return false;
        }
        foreach ($longLived as $service) {
            if (! self::isUp($service['state'])) {
                return false;
            }
        }

        return true;
    }

    /**
     * @param  array<string, mixed>  $catalog
     * @param  list<array{name: ?string, services: list<array<string, mixed>>}>  $sections
     * @return list<string>
     */
    public static function startAllTargets(array $groups, array $sections): array
    {
        $all = $groups['all'] ?? null;
        if (is_array($all) && $all !== []) {
            return array_values(array_filter($all, 'is_string'));
        }
        $ids = [];
        foreach (self::lines($sections) as $service) {
            if (! $service['disabled']) {
                $ids[] = $service['id'];
            }
        }

        return $ids;
    }

    public static function urlVisible(bool $requiresRunning, string $state): bool
    {
        return ! $requiresRunning || self::isUp($state) || $state === 'succeeded';
    }

    /**
     * @param  list<array<string, mixed>>  $urls
     * @param  list<array{name: ?string, services: list<array<string, mixed>>}>  $sections
     * @return list<array{serviceId: string, label: string, url: string}>
     */
    public static function visibleUrls(array $urls, array $sections): array
    {
        $states = [];
        foreach (self::lines($sections) as $service) {
            $states[$service['id']] = $service['state'];
        }
        $visible = [];
        foreach ($urls as $url) {
            if (! is_array($url) || ! is_string($url['serviceId'] ?? null) || ! is_string($url['url'] ?? null)) {
                continue;
            }
            $requires = array_key_exists('requiresRunning', $url) ? (bool) $url['requiresRunning'] : true;
            $state = $states[$url['serviceId']] ?? 'stopped';
            if (! self::urlVisible($requires, $state)) {
                continue;
            }
            $visible[] = [
                'serviceId' => $url['serviceId'],
                'label' => is_string($url['label'] ?? null) && $url['label'] !== '' ? $url['label'] : $url['serviceId'],
                'url' => $url['url'],
            ];
        }

        return $visible;
    }

    public static function shouldReloadCatalog(?int $previous, ?int $next): bool
    {
        return $previous !== null && $next !== null && $previous !== $next;
    }

    public static function catalogStamp(string $root): ?int
    {
        $latest = null;
        foreach (['hearth.yaml', 'hearth.yml', 'hearth.json'] as $name) {
            $path = $root.'/'.$name;
            if (! is_file($path)) {
                continue;
            }
            $mtime = filemtime($path);
            if ($mtime !== false && ($latest === null || $mtime > $latest)) {
                $latest = $mtime;
            }
        }

        return $latest;
    }

    /**
     * @param  array<string, mixed>  $instance
     * @return list<string>
     */
    public static function attachmentRoots(array $instance): array
    {
        $attachments = $instance['attachments'] ?? [];
        if (! is_array($attachments)) {
            return [];
        }
        $roots = [];
        foreach ($attachments as $row) {
            if (is_array($row) && is_string($row['projectRoot'] ?? null) && $row['projectRoot'] !== '') {
                $roots[] = $row['projectRoot'];
            }
        }

        return $roots;
    }

    /**
     * @param  array<string, mixed>  $instance
     */
    public static function instanceId(array $instance): string
    {
        if (is_string($instance['id'] ?? null) && $instance['id'] !== '') {
            return $instance['id'];
        }

        return (string) ($instance['name'] ?? '').'@'.(string) ($instance['version'] ?? '');
    }

    /**
     * @param  array<string, mixed>  $document
     * @return list<array{id: string, name: string, version: string}>
     */
    public static function recipesFrom(array $document): array
    {
        $services = $document['services'] ?? [];
        if (! is_array($services)) {
            return [];
        }
        $rows = [];
        foreach ($services as $name => $family) {
            if (! is_string($name) || ! is_array($family)) {
                continue;
            }
            $versions = array_keys($family['versions'] ?? []);
            sort($versions, SORT_STRING);
            foreach ($versions as $version) {
                if (! is_string($version)) {
                    continue;
                }
                $rows[] = ['id' => $name.'@'.$version, 'name' => $name, 'version' => $version];
            }
        }

        return $rows;
    }

    /**
     * @param  array<string, mixed>  $payload
     * @return list<array{id: string, port: mixed, installState: string, state: string, display: string, attachments: int}>
     */
    public static function instancesFrom(array $payload): array
    {
        $rows = [];
        foreach ($payload['instances'] ?? [] as $instance) {
            if (! is_array($instance)) {
                continue;
            }
            $state = $instance['state']['actualState'] ?? '';
            $state = is_string($state) ? $state : '';
            $rows[] = [
                'id' => self::instanceId($instance),
                'port' => $instance['port'] ?? null,
                'installState' => is_string($instance['installState'] ?? null) ? $instance['installState'] : '',
                'state' => $state,
                'display' => $state === '' ? '' : self::compactWire($state),
                'attachments' => count(self::attachmentRoots($instance)),
            ];
        }

        return $rows;
    }

    /**
     * @param  list<string>  $roots
     * @param  list<array{root: string, name: string, path: string}>  $known
     * @return array{all: list<string>, others: list<string>}
     */
    public static function classifyAttachments(array $roots, ?string $current, array $known): array
    {
        $others = [];
        foreach ($roots as $root) {
            if ($current !== null && self::sameRoot($root, $current)) {
                continue;
            }
            $others[] = $root;
        }

        return [
            'all' => self::labelsFor($roots, $known),
            'others' => self::labelsFor($others, $known),
        ];
    }

    /**
     * @param  list<string>  $affected
     */
    public static function instanceSharedNotice(string $verb, string $instance, array $affected): string
    {
        $who = self::joinNames($affected);
        [$doing, $consequence] = match ($verb) {
            'restart' => ['Restarting', "takes the shared service down for {$who}"],
            'remove' => ['Removing', "deletes its data and takes it down for {$who}"],
            default => ['Stopping', "takes the shared service down for {$who}"],
        };

        return "{$doing} {$instance} {$consequence}. Press again to {$verb}.";
    }

    /**
     * Periods, not the TUI em dash. Stopping here only detaches this workspace.
     *
     * @param  list<array{instance: string, others: list<string>}>  $touches
     * @param  list<string>  $unknown
     */
    public static function projectSharedNotice(string $verb, string $current, array $touches, array $unknown): string
    {
        $sentences = [];
        if ($touches !== []) {
            $parts = [];
            foreach ($touches as $touch) {
                $parts[] = $touch['instance'].' ('.self::joinNames($touch['others']).')';
            }
            $listed = self::joinNames($parts);
            $be = count($touches) === 1 ? 'is' : 'are';
            $them = count($touches) === 1 ? 'it' : 'them';
            $doing = $verb === 'restart' ? 'Restarting' : 'Stopping';
            $effect = $verb === 'restart'
                ? "only detaches and reattaches {$current}"
                : "only detaches {$current}";
            $sentences[] = "{$listed} {$be} also used by other workspaces. {$doing} {$them} here {$effect}. Those workspaces keep {$them}";
        }
        if ($unknown !== []) {
            $sentences[] = 'Could not check which workspaces use '.self::joinNames($unknown);
        }

        return implode('. ', $sentences).". Press again to {$verb}.";
    }

    public static function uncheckedSharedNotice(string $verb, string $instance): string
    {
        return "Could not check which workspaces use {$instance}. Press again to {$verb} anyway.";
    }

    public static function removeNotice(string $id, array $affected, bool $unchecked): string
    {
        if ($unchecked) {
            return self::uncheckedSharedNotice('remove', $id);
        }
        if ($affected === []) {
            return "Press again to remove {$id}.";
        }

        return self::instanceSharedNotice('remove', $id, $affected);
    }

    /**
     * @param  list<string>  $names
     */
    public static function joinNames(array $names): string
    {
        $count = count($names);
        if ($count === 0) {
            return '';
        }
        if ($count === 1) {
            return $names[0];
        }
        if ($count === 2) {
            return $names[0].' and '.$names[1];
        }
        $last = $names[$count - 1];

        return implode(', ', array_slice($names, 0, -1)).', and '.$last;
    }

    public static function sameRoot(string $left, string $right): bool
    {
        $a = realpath($left);
        $b = realpath($right);

        return ($a !== false ? $a : $left) === ($b !== false ? $b : $right);
    }

    public static function compactWire(string $state): string
    {
        return match ($state) {
            'running-unready' => 'running',
            'preparing' => 'starting',
            'queued-start' => 'queued',
            'externally-owned' => 'external',
            default => $state,
        };
    }

    /**
     * @param  array<string, mixed>  $service
     * @return array{id: string, label: string, ports: string, disabled: bool, finite: bool, infra: bool, shared: bool, sharedInstance: ?string}
     */
    private static function meta(array $service): array
    {
        $ports = [];
        foreach ($service['ports'] ?? [] as $port) {
            if (is_array($port) && isset($port['port'])) {
                $ports[] = (string) $port['port'];
            } elseif (is_int($port) || (is_string($port) && $port !== '')) {
                $ports[] = (string) $port;
            }
        }
        $shared = self::sharedInstanceOf($service);
        $label = $service['label'] ?? null;

        return [
            'id' => $service['id'],
            'label' => is_string($label) && $label !== '' ? $label : $service['id'],
            'ports' => implode(', ', $ports),
            'disabled' => (bool) ($service['disabled'] ?? false),
            'finite' => self::isFinite($service),
            'infra' => ($service['kind'] ?? null) === 'infrastructure',
            'shared' => $shared !== null,
            'sharedInstance' => $shared,
        ];
    }

    /**
     * @param  list<array{name: ?string, services: list<array<string, mixed>>}>  $sections
     * @return list<array<string, mixed>>
     */
    private static function lines(array $sections): array
    {
        $lines = [];
        foreach ($sections as $section) {
            foreach ($section['services'] as $service) {
                $lines[] = $service;
            }
        }

        return $lines;
    }

    /**
     * @param  list<string>  $roots
     * @param  list<array{root: string, name: string, path: string}>  $known
     * @return list<string>
     */
    private static function labelsFor(array $roots, array $known): array
    {
        $rows = [];
        foreach ($roots as $root) {
            $found = null;
            foreach ($known as $item) {
                if (self::sameRoot($item['root'], $root)) {
                    $found = $item;
                    break;
                }
            }
            $name = $found['name'] ?? basename($root);
            $path = $found['path'] ?? $root;
            $rows[] = [$name, $path];
        }
        $counts = [];
        foreach ($rows as [$name]) {
            $counts[$name] = ($counts[$name] ?? 0) + 1;
        }
        $labels = [];
        foreach ($rows as [$name, $path]) {
            $labels[] = ($counts[$name] ?? 0) > 1 ? "{$name} ({$path})" : $name;
        }
        sort($labels);

        return array_values(array_unique($labels));
    }
}
