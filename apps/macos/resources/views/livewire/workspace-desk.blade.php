@php $board = $pane === 'workspaces' && $selected && $selected['hasToken'] && ! $selected['stopped']; @endphp
<div
    class="desk {{ $board ? 'with-log' : '' }} {{ $board && ! $logOpen ? 'log-closed' : '' }}"
    @if ($board) wire:poll.2s="tick" @endif
>
    <style>
        .desk {
            display: grid;
            grid-template-columns: 260px minmax(0, 1fr);
            height: 100dvh;
            min-height: 0;
            transition: grid-template-columns var(--dur-med) var(--ease-out);
        }
        .desk.with-log { grid-template-columns: 260px minmax(0, 1fr) minmax(340px, 42%); }
        .desk.with-log.log-closed { grid-template-columns: 260px minmax(0, 1fr) 56px; }
        .sidebar, .detail { min-height: 0; overflow: auto; }
        .sidebar {
            background: color-mix(in srgb, var(--sidebar) 82%, transparent);
            -webkit-backdrop-filter: blur(24px) saturate(1.6);
            backdrop-filter: blur(24px) saturate(1.6);
            border-right: 1px solid var(--line);
            padding: 12px 10px 16px;
        }
        .detail { padding: 16px 20px 24px; }

        /* Window chrome */
        .brand {
            display: flex; align-items: center; gap: 6px;
            font-size: 13px; font-weight: 700; letter-spacing: 0.01em;
            color: var(--ink);
            padding: 4px 8px 10px;
            -webkit-app-region: drag;
        }
        .brand .icon { color: var(--accent); }

        /* Segmented control */
        .seg {
            display: grid; grid-template-columns: 1fr 1fr; gap: 2px;
            background: var(--seg-bg);
            border-radius: var(--radius);
            padding: 2px;
            margin: 0 4px 12px;
        }
        .seg-item {
            border: 0; border-radius: var(--radius-sm);
            background: transparent; color: var(--muted);
            font-size: 12px; font-weight: 600;
            padding: 4px 8px;
            transition: background-color var(--dur-fast) var(--ease-out), color var(--dur-fast), box-shadow var(--dur-fast);
        }
        .seg-item.on {
            background: var(--seg-on); color: var(--ink);
            box-shadow: var(--btn-shadow), 0 0 0 0.5px rgb(0 0 0 / 0.04);
        }
        .seg-item:active { transform: scale(0.98); }

        h1 { font-size: 17px; font-weight: 700; letter-spacing: -0.01em; margin: 0 0 2px; }
        h2 {
            font-size: 11px; font-weight: 700; letter-spacing: 0.04em; text-transform: uppercase;
            margin: 18px 0 6px; color: var(--muted);
        }
        .lede, .hint, .meta, .summary { color: var(--muted); margin: 0; }
        .lede { font-size: 12px; margin-bottom: 10px; }
        .summary { margin: 2px 0 10px; font-size: 12px; font-variant-numeric: tabular-nums; }
        .side-title {
            display: flex; align-items: center; justify-content: space-between;
            font-size: 11px; font-weight: 700; letter-spacing: 0.04em; text-transform: uppercase;
            color: var(--muted);
            padding: 0 8px; margin: 14px 0 4px;
        }

        form { display: grid; gap: 6px; margin: 8px 4px 4px; }
        .addrow { display: flex; gap: 6px; align-items: center; }
        .addrow input { flex: 1; min-width: 0; }
        label { font-size: 12px; font-weight: 600; }
        .sr {
            position: absolute; width: 1px; height: 1px; overflow: hidden;
            clip: rect(0 0 0 0); clip-path: inset(50%); white-space: nowrap;
        }
        input[type="text"] {
            width: 100%;
            border: 1px solid var(--btn-line);
            background: var(--btn-bg);
            border-radius: var(--radius-sm);
            padding: 5px 8px;
            font-size: 13px;
            transition: border-color var(--dur-fast), box-shadow var(--dur-fast);
        }
        input[type="text"]:focus-visible {
            outline: none;
            border-color: var(--focus);
            box-shadow: 0 0 0 3px color-mix(in srgb, var(--focus) 25%, transparent);
        }
        input::placeholder { color: var(--muted); }

        .rows, .services, .urls { list-style: none; margin: 0; padding: 0; display: grid; gap: 2px; }
        .services { gap: 4px; }

        /* Sidebar rows (Finder-style selection) */
        .row {
            display: flex; align-items: center; gap: 8px;
            width: 100%; text-align: left;
            border: 0; border-radius: var(--radius-sm);
            background: transparent;
            padding: 6px 8px;
            transition: background-color var(--dur-fast) var(--ease-out);
        }
        .row:hover { background: color-mix(in srgb, var(--sel) 55%, transparent); }
        .row[aria-current="true"] { background: var(--sel); }
        .row .icon { color: var(--muted); }
        .row[aria-current="true"] .icon { color: var(--accent); }
        .row-body { min-width: 0; flex: 1; }
        .row:active, .svc:active, .btn:active, .iconbtn:active { transform: scale(0.98); }

        .name { font-weight: 600; font-size: 13px; }
        .sub, .ports { display: block; color: var(--muted); font-size: 11px; overflow-wrap: anywhere; }
        .ports { font-family: ui-monospace, "SF Mono", Menlo, monospace; }
        .flags { display: flex; gap: 4px; margin-top: 2px; }
        .chip {
            font-size: 10px; font-weight: 600; letter-spacing: 0.02em;
            padding: 1px 6px; border-radius: 999px;
            background: color-mix(in srgb, currentColor 12%, transparent);
        }
        .chip-warn { color: var(--warn); }
        .chip-danger { color: var(--danger); }
        .chip-muted { color: var(--muted); }

        /* Service rows */
        .svc {
            display: grid;
            grid-template-columns: minmax(0, 1fr) auto;
            gap: 8px;
            align-items: center;
            width: 100%; text-align: left;
            border: 1px solid transparent;
            border-radius: var(--radius);
            background: transparent;
            padding: 7px 10px;
            transition: background-color var(--dur-fast) var(--ease-out), border-color var(--dur-fast);
        }
        .svc:hover { background: color-mix(in srgb, var(--sel) 45%, transparent); }
        .svc[aria-current="true"] {
            background: color-mix(in srgb, var(--accent) 12%, transparent);
            border-color: color-mix(in srgb, var(--accent) 30%, transparent);
        }

        .empty {
            display: flex; align-items: center; gap: 6px;
            color: var(--muted); font-size: 12px; margin: 8px 4px 0;
        }

        /* Notices */
        .banner, .notice {
            display: flex; align-items: flex-start; gap: 7px;
            border-radius: var(--radius);
            padding: 8px 10px;
            margin: 0 0 12px;
            font-size: 12.5px;
            animation: rise var(--dur-med) var(--ease-out);
        }
        .banner {
            background: color-mix(in srgb, var(--danger) 10%, var(--panel));
            color: var(--danger);
        }
        .banner .icon { flex: none; margin-top: 1px; }
        .notice { background: var(--panel); border: 1px solid var(--line); }
        .notice .icon { flex: none; margin-top: 1px; color: var(--muted); }
        @keyframes rise {
            from { opacity: 0; transform: translateY(-4px); }
            to { opacity: 1; transform: none; }
        }

        /* Global busy progress bar */
        .progress {
            position: fixed; top: 0; left: 0; right: 0; height: 2px;
            z-index: 50; pointer-events: none;
            overflow: hidden;
        }
        .progress i {
            display: block; height: 100%; width: 40%;
            background: var(--accent);
            border-radius: 999px;
            animation: slide 1s var(--ease-out) infinite;
        }
        @keyframes slide {
            from { transform: translateX(-110%); }
            to { transform: translateX(280%); }
        }
        .spin { animation: rot 0.8s linear infinite; }
        @keyframes rot { to { transform: rotate(360deg); } }
        .refreshing { opacity: 0.55; transition: opacity var(--dur-med); }

        /* Info card */
        dl.meta {
            display: grid; grid-template-columns: auto 1fr; gap: 4px 14px;
            margin: 10px 0 4px; padding: 10px 12px;
            background: var(--panel);
            border: 1px solid var(--line);
            border-radius: var(--radius-lg);
            font-size: 12.5px;
        }
        dl.meta dt { color: var(--muted); font-weight: 500; }
        dl.meta dd { margin: 0; overflow-wrap: anywhere; font-variant-numeric: tabular-nums; }

        /* Buttons */
        .actions, .group-actions { display: flex; flex-wrap: wrap; gap: 6px; margin-top: 10px; }
        .group-actions { margin: 0 0 4px; }
        .btn, .iconbtn {
            display: inline-flex; align-items: center; justify-content: center; gap: 5px;
            border-radius: var(--radius-sm);
            border: 1px solid var(--btn-line);
            background: var(--btn-bg);
            box-shadow: var(--btn-shadow);
            padding: 4px 10px;
            font-size: 12.5px; font-weight: 500;
            white-space: nowrap;
            transition:
                background-color var(--dur-fast) var(--ease-out),
                border-color var(--dur-fast),
                transform 60ms var(--ease-out),
                box-shadow var(--dur-med) var(--ease-out);
        }
        .btn:hover, .iconbtn:hover { background: color-mix(in srgb, var(--btn-bg) 70%, var(--sel)); }
        .iconbtn { padding: 4px; border-radius: var(--radius-sm); color: var(--muted); }
        .btn.primary { background: var(--accent); border-color: var(--accent); color: var(--accent-ink); }
        .btn.primary:hover { background: color-mix(in srgb, var(--accent) 88%, var(--ink)); }
        .btn.danger, .btn.armed-danger { background: var(--danger); border-color: var(--danger); color: var(--danger-ink); }
        .btn.armed {
            border-color: var(--accent);
            color: var(--accent);
            animation: armed 1.4s var(--ease-out) infinite;
        }
        .btn.armed-danger { animation: armed-danger 1.4s var(--ease-out) infinite; }
        @keyframes armed {
            0%, 100% { box-shadow: var(--btn-shadow), 0 0 0 0 color-mix(in srgb, var(--accent) 35%, transparent); }
            50% { box-shadow: var(--btn-shadow), 0 0 0 3px color-mix(in srgb, var(--accent) 22%, transparent); }
        }
        @keyframes armed-danger {
            0%, 100% { box-shadow: var(--btn-shadow), 0 0 0 0 color-mix(in srgb, var(--danger) 40%, transparent); }
            50% { box-shadow: var(--btn-shadow), 0 0 0 3px color-mix(in srgb, var(--danger) 28%, transparent); }
        }
        .btn:disabled, .iconbtn:disabled { opacity: 0.45; cursor: default; transform: none; }

        /* State pills */
        .state {
            display: inline-flex; align-items: center; gap: 5px;
            font-variant-numeric: tabular-nums; font-size: 11px; font-weight: 600;
            padding: 2px 8px; border-radius: 999px;
            background: color-mix(in srgb, currentColor 12%, transparent);
            transition: background-color var(--dur-med), color var(--dur-med);
        }
        .state::before {
            content: ""; width: 6px; height: 6px; border-radius: 50%;
            background: currentColor; flex: none;
        }
        .state-ready, .state-succeeded { color: var(--ok); }
        .state-running, .state-running-unready { color: var(--run); }
        .state-starting, .state-preparing, .state-queued-start, .state-stopping { color: var(--warn); }
        .state-failed, .state-orphaned, .state-externally-owned { color: var(--danger); }
        .state-stopped { color: var(--muted); }
        .state-log { color: var(--muted); }
        .error-line { color: var(--danger); font-size: 11.5px; }
        .shared-word {
            display: inline-flex; align-items: center; gap: 3px;
            color: var(--shared); font-weight: 600; font-size: 11px;
        }

        /* URLs */
        .urls a {
            display: inline-flex; align-items: center; gap: 5px;
            color: var(--accent); text-decoration: none;
            border-radius: var(--radius-sm);
        }
        .urls a:hover { text-decoration: underline; }
        .urls li { padding: 3px 0; }

        /* Log column */
        .logcol {
            display: flex; flex-direction: column;
            min-height: 0; min-width: 0;
            border-left: 1px solid var(--line);
            background: var(--log-bg);
            overflow: hidden;
        }
        .loghead {
            display: flex; align-items: center; gap: 6px;
            padding: 6px 8px 6px 6px;
            border-bottom: 1px solid var(--line);
            flex: none;
        }
        .loghead-btn {
            display: flex; align-items: center; gap: 6px;
            flex: 1; min-width: 0;
            border: 0; background: transparent; border-radius: var(--radius-sm);
            padding: 4px 6px;
            font-size: 12px; font-weight: 700; color: var(--ink);
            text-align: left;
            transition: background-color var(--dur-fast);
        }
        .loghead-btn:hover { background: var(--sel); }
        .loghead-btn .caret {
            color: var(--muted);
            transition: transform var(--dur-med) var(--ease-out);
            transform: rotate(-90deg);
        }
        .loghead-btn[aria-expanded="true"] .caret { transform: none; }
        .logsub { color: var(--muted); font-weight: 500; }
        .logcol.collapsed .loghead { padding: 6px 4px; }
        .logcol.collapsed .loghead-btn { justify-content: center; padding: 4px; }
        .logcol.collapsed .loghead-btn .caret { transform: rotate(90deg); }
        .logwrap {
            position: relative;
            flex: 1; min-height: 0;
            display: flex;
        }
        .log {
            margin: 0;
            padding: 10px 12px;
            flex: 1; min-height: 0;
            overflow: auto;
            font-family: ui-monospace, "SF Mono", Menlo, monospace;
            font-size: 11.5px;
            line-height: 1.55;
            white-space: pre-wrap;
            overflow-wrap: anywhere;
            scroll-behavior: auto;
        }
        .logempty { margin: 0; padding: 14px 12px; color: var(--muted); font-size: 12px; }
        .logjump {
            position: absolute; right: 10px; bottom: 10px;
            display: inline-flex; align-items: center; gap: 4px;
            border: 1px solid var(--btn-line); border-radius: 999px;
            background: var(--panel); color: var(--ink);
            box-shadow: 0 2px 8px rgb(0 0 0 / 0.18);
            font-size: 11px; font-weight: 600;
            padding: 4px 10px;
            animation: rise var(--dur-med) var(--ease-out);
        }
        .logbusy { color: var(--muted); display: inline-flex; align-items: center; }

        .binary {
            margin-top: 24px; padding-top: 10px;
            border-top: 1px solid var(--line);
            font-family: ui-monospace, "SF Mono", Menlo, monospace;
            font-size: 11px; color: var(--muted);
            overflow-wrap: anywhere;
        }
        .bad { color: var(--danger); }
        .ok { color: var(--ok); }

        @media (max-width: 720px) {
            .desk, .desk.with-log, .desk.with-log.log-closed {
                grid-template-columns: 1fr;
                height: auto; min-height: 100dvh;
            }
            .sidebar { border-right: 0; border-bottom: 1px solid var(--line); }
            .logcol { border-left: 0; border-top: 1px solid var(--line); }
            .logcol.collapsed .loghead-btn { justify-content: flex-start; }
            .logwrap { max-height: 42dvh; }
            .svc { grid-template-columns: 1fr; }
        }
    </style>

    <div class="progress" role="progressbar" aria-label="Working" wire:loading.delay wire:target="addFolder, trust, startDaemon, stopDaemon, restartDaemon, forget, refreshList, select, startService, stopService, restartService, reclaimPort, startAll, stopAll, startGroup, stopGroup, restartGroup, showPane, refreshShared, installRecipe, startInstance, stopInstance, restartInstance, removeInstance, selectService, expandLog"><i></i></div>

    <aside class="sidebar" aria-label="Sidebar">
        <div class="brand"><x-icon name="flame" :size="15" /> Hearth</div>
        <div class="seg" role="tablist" aria-label="Panes">
            <button class="seg-item {{ $pane === 'workspaces' ? 'on' : '' }}" type="button" wire:click="showPane('workspaces')" role="tab" aria-selected="{{ $pane === 'workspaces' ? 'true' : 'false' }}">Workspaces</button>
            <button class="seg-item {{ $pane === 'shared' ? 'on' : '' }}" type="button" wire:click="showPane('shared')" role="tab" aria-selected="{{ $pane === 'shared' ? 'true' : 'false' }}">Shared</button>
        </div>

        <form wire:submit="addFolder" class="addrow">
            <label class="sr" for="folder">Folder</label>
            <input id="folder" type="text" wire:model="folder" autocomplete="off" spellcheck="false" placeholder="/Users/me/project">
            <button class="iconbtn" type="submit" aria-label="Add folder" title="Add folder" wire:loading.attr="disabled" wire:target="addFolder"><x-icon name="folder-plus" /></button>
        </form>

        <div class="side-title">
            <span>Workspaces</span>
            <button class="iconbtn" type="button" wire:click="refreshList" aria-label="Refresh workspaces" title="Refresh" wire:loading.attr="disabled" wire:target="refreshList">
                <span wire:loading.remove wire:target="refreshList"><x-icon name="arrow-clockwise" :size="13" /></span>
                <span wire:loading.delay wire:target="refreshList" class="logbusy"><x-icon name="spinner-gap" :size="13" class="spin" /></span>
            </button>
        </div>

        @if ($loadError)
            <p class="banner" role="alert"><x-icon name="warning-circle" /> {{ $loadError }}</p>
        @endif

        @if ($rows === [])
            <p class="empty"><x-icon name="folder" :size="14" /> No workspaces yet.</p>
        @else
            <ul class="rows">
                @foreach ($rows as $row)
                    <li wire:key="ws-{{ $row['id'] }}">
                        <button
                            class="row"
                            type="button"
                            wire:click="select('{{ $row['id'] }}')"
                            @if ($selected && $selected['id'] === $row['id']) aria-current="true" @endif
                        >
                            <x-icon name="{{ $row['missing'] ? 'warning-circle' : 'folder' }}" :size="15" />
                            <span class="row-body">
                                <span class="name">{{ $row['name'] }}</span>
                                <span class="sub">{{ $row['path'] }}</span>
                                <span class="flags">
                                    @if ($row['missing'])
                                        <span class="chip chip-danger">missing</span>
                                    @elseif ($row['trusted'])
                                        <span class="chip chip-muted">trusted</span>
                                    @else
                                        <span class="chip chip-warn">untrusted</span>
                                    @endif
                                    @if ($row['stopped'])
                                        <span class="chip chip-muted">stopped</span>
                                    @endif
                                </span>
                            </span>
                        </button>
                    </li>
                @endforeach
            </ul>
        @endif
    </aside>

    <section class="detail" aria-label="{{ $pane === 'shared' ? 'Shared' : 'Workspace' }}">
        @if ($notice)
            <p class="notice" role="status" wire:loading.remove wire:target="addFolder, trust, startDaemon, stopDaemon, restartDaemon, forget, refreshList, startService, stopService, restartService, reclaimPort, startAll, stopAll, startGroup, stopGroup, restartGroup, showPane, refreshShared, installRecipe, startInstance, stopInstance, restartInstance, removeInstance"><x-icon name="info" :size="14" /> {{ $notice }}</p>
        @endif
        <p class="notice" role="status" wire:loading.delay wire:target="addFolder, trust, startDaemon, stopDaemon, restartDaemon, forget, refreshList, startService, stopService, restartService, reclaimPort, startAll, stopAll, startGroup, stopGroup, restartGroup, showPane, refreshShared, installRecipe, startInstance, stopInstance, restartInstance, removeInstance"><x-icon name="spinner-gap" :size="14" class="spin" /> Working…</p>

        @if ($pane === 'shared')
            <h1>Shared</h1>
            <p class="hint">{{ $smpLive ? 'Live smp.' : 'Local registry. Drawing does not start smp.' }}</p>
            <div class="actions">
                <button class="btn" type="button" wire:click="refreshShared" wire:loading.attr="disabled" wire:target="refreshShared"><x-icon name="arrow-clockwise" :size="13" /> Refresh</button>
            </div>

            <h2>Recipes</h2>
            @if ($recipes === [])
                <p class="empty"><x-icon name="cube" :size="14" /> No recipes.</p>
            @else
                <ul class="services">
                    @foreach ($recipes as $recipe)
                        <li wire:key="recipe-{{ $recipe['id'] }}" class="svc">
                            <span><x-icon name="cube" :size="14" /> <span class="name">{{ $recipe['name'] }}</span> <span class="sub">{{ $recipe['version'] }}</span></span>
                            <button class="btn" type="button" wire:click="installRecipe('{{ $recipe['id'] }}')" wire:loading.attr="disabled" wire:target="installRecipe('{{ $recipe['id'] }}')"><x-icon name="download-simple" :size="13" /> Install</button>
                        </li>
                    @endforeach
                </ul>
            @endif

            <h2>Instances</h2>
            @if ($instances === [])
                <p class="empty"><x-icon name="hard-drives" :size="14" /> No instances.</p>
            @else
                <ul class="services">
                    @foreach ($instances as $instance)
                        <li wire:key="inst-{{ $instance['id'] }}" class="svc">
                            <span>
                                <x-icon name="hard-drives" :size="14" />
                                <span class="name">{{ $instance['id'] }}</span>
                                <span class="sub">
                                    {{ $instance['installState'] }}
                                    @if ($instance['display'] !== '')
                                        · {{ $instance['display'] }}
                                    @endif
                                    @if ($instance['port'])
                                        · port {{ $instance['port'] }}
                                    @endif
                                    · {{ $instance['attachments'] }} attached
                                </span>
                            </span>
                            <span class="actions">
                                @php $up = in_array($instance['state'], ['ready', 'running', 'running-unready'], true); @endphp
                                @if ($up)
                                    <button class="btn {{ $pendingKind === 'instance-stop' && $pendingId === $instance['id'] ? 'armed-danger' : '' }}" type="button" wire:click="stopInstance('{{ $instance['id'] }}')" wire:loading.attr="disabled" wire:target="stopInstance('{{ $instance['id'] }}')">
                                        <x-icon name="stop" :size="13" /> {{ $pendingKind === 'instance-stop' && $pendingId === $instance['id'] ? 'Confirm stop' : 'Stop' }}
                                    </button>
                                @else
                                    <button class="btn primary" type="button" wire:click="startInstance('{{ $instance['id'] }}')" wire:loading.attr="disabled" wire:target="startInstance('{{ $instance['id'] }}')"><x-icon name="play" :size="13" /> Start</button>
                                @endif
                                <button class="btn {{ $pendingKind === 'instance-restart' && $pendingId === $instance['id'] ? 'armed' : '' }}" type="button" wire:click="restartInstance('{{ $instance['id'] }}')" wire:loading.attr="disabled" wire:target="restartInstance('{{ $instance['id'] }}')">
                                    <x-icon name="arrow-clockwise" :size="13" /> {{ $pendingKind === 'instance-restart' && $pendingId === $instance['id'] ? 'Confirm restart' : 'Restart' }}
                                </button>
                                <button class="btn {{ $pendingKind === 'shared-remove' && $pendingId === $instance['id'] ? 'armed-danger' : '' }}" type="button" wire:click="removeInstance('{{ $instance['id'] }}')" wire:loading.attr="disabled" wire:target="removeInstance('{{ $instance['id'] }}')">
                                    <x-icon name="trash" :size="13" /> {{ $pendingKind === 'shared-remove' && $pendingId === $instance['id'] ? 'Confirm remove' : 'Remove' }}
                                </button>
                            </span>
                        </li>
                    @endforeach
                </ul>
            @endif
        @elseif ($selected)
            <h1>{{ $selected['name'] }}</h1>
            @if ($summary !== '')
                <p class="summary">{{ $summary }}</p>
            @endif
            <dl class="meta">
                <dt>Path</dt>
                <dd>{{ $selected['fullPath'] }}</dd>
                <dt>Trust</dt>
                <dd>{{ $selected['trusted'] ? 'trusted' : 'untrusted' }}</dd>
                <dt>Daemon</dt>
                <dd>
                    @if ($selected['missing'])
                        missing folder
                    @elseif ($selected['stopped'])
                        stopped
                    @elseif ($selected['hasToken'])
                        attached
                    @else
                        not attached
                    @endif
                </dd>
            </dl>
            <div class="actions">
                @if (! $selected['trusted'] && ! $selected['missing'])
                    <button class="btn primary {{ $pendingKind === 'trust' && $pendingId === $selected['id'] ? 'armed' : '' }}" type="button" wire:click="trust" wire:loading.attr="disabled" wire:target="trust">
                        <x-icon name="shield-check" :size="13" /> {{ $pendingKind === 'trust' && $pendingId === $selected['id'] ? 'Confirm trust' : 'Trust' }}
                    </button>
                @endif
                @if ($selected['trusted'] && ! $selected['missing'] && ($selected['stopped'] || ! $selected['hasToken']))
                    <button class="btn primary" type="button" wire:click="startDaemon" wire:loading.attr="disabled" wire:target="startDaemon"><x-icon name="play" :size="13" /> Start</button>
                @endif
                @if ($selected['trusted'] && ! $selected['missing'] && ! $selected['stopped'])
                    <button class="btn {{ $pendingKind === 'restart-daemon' && $pendingId === $selected['id'] ? 'armed' : '' }}" type="button" wire:click="restartDaemon" wire:loading.attr="disabled" wire:target="restartDaemon">
                        <x-icon name="arrow-clockwise" :size="13" /> {{ $pendingKind === 'restart-daemon' && $pendingId === $selected['id'] ? 'Confirm restart' : 'Restart daemon' }}
                    </button>
                    <button class="btn {{ $pendingKind === 'stop' && $pendingId === $selected['id'] ? 'armed-danger' : '' }}" type="button" wire:click="stopDaemon" wire:loading.attr="disabled" wire:target="stopDaemon">
                        <x-icon name="stop" :size="13" /> {{ $pendingKind === 'stop' && $pendingId === $selected['id'] ? 'Confirm stop' : 'Stop daemon' }}
                    </button>
                @endif
                <button class="btn {{ $pendingKind === 'forget' && $pendingId === $selected['id'] ? 'armed-danger' : '' }}" type="button" wire:click="forget" wire:loading.attr="disabled" wire:target="forget">
                    <x-icon name="trash" :size="13" /> {{ $pendingKind === 'forget' && $pendingId === $selected['id'] ? 'Confirm forget' : 'Forget' }}
                </button>
            </div>

            @if ($selected['hasToken'] && ! $selected['stopped'])
                <div class="actions">
                    <button class="btn" type="button" wire:click="startAll" wire:loading.attr="disabled" wire:target="startAll"><x-icon name="play" :size="13" /> Start all</button>
                    <button class="btn {{ $pendingKind === 'stop-all' ? 'armed-danger' : '' }}" type="button" wire:click="stopAll" wire:loading.attr="disabled" wire:target="stopAll">
                        <x-icon name="stop" :size="13" /> {{ $pendingKind === 'stop-all' ? 'Confirm stop all' : 'Stop all' }}
                    </button>
                </div>

                <ul class="services" wire:loading.class="refreshing" wire:target="select">
                    <li>
                        <button class="svc" type="button" wire:click="selectService('$daemon')" @if ($selectedService === '$daemon') aria-current="true" @endif>
                            <span><x-icon name="terminal" :size="14" /> <span class="name">daemon log</span></span>
                            <span class="state state-log" wire:loading.remove.delay wire:target="selectService('$daemon')">log</span>
                            <span class="state state-log" wire:loading.delay wire:target="selectService('$daemon')"><x-icon name="spinner-gap" :size="12" class="spin" /></span>
                        </button>
                    </li>
                </ul>

                @foreach ($sections as $section)
                    <div wire:key="group-{{ $section['name'] ?? 'other' }}">
                        @if ($section['name'])
                            <h2>{{ $section['name'] }}</h2>
                            <div class="group-actions">
                                @if (\App\Support\ServiceBoard::groupIsUp($sections, $section['name']))
                                    <button class="btn {{ $pendingKind === 'restart-group' && $pendingId === $section['name'] ? 'armed' : '' }}" type="button" wire:click="restartGroup('{{ $section['name'] }}')" wire:loading.attr="disabled" wire:target="restartGroup('{{ $section['name'] }}')">
                                        <x-icon name="arrow-clockwise" :size="13" /> {{ $pendingKind === 'restart-group' && $pendingId === $section['name'] ? 'Confirm restart' : 'Restart group' }}
                                    </button>
                                @else
                                    <button class="btn" type="button" wire:click="startGroup('{{ $section['name'] }}')" wire:loading.attr="disabled" wire:target="startGroup('{{ $section['name'] }}')"><x-icon name="play" :size="13" /> Start group</button>
                                @endif
                                <button class="btn {{ $pendingKind === 'stop-group' && $pendingId === $section['name'] ? 'armed-danger' : '' }}" type="button" wire:click="stopGroup('{{ $section['name'] }}')" wire:loading.attr="disabled" wire:target="stopGroup('{{ $section['name'] }}')">
                                    <x-icon name="stop" :size="13" /> {{ $pendingKind === 'stop-group' && $pendingId === $section['name'] ? 'Confirm stop' : 'Stop group' }}
                                </button>
                            </div>
                        @elseif (collect($sections)->contains(fn ($item) => $item['name'] !== null))
                            <h2>Other</h2>
                        @endif
                        <ul class="services" wire:loading.class="refreshing" wire:target="select">
                            @foreach ($section['services'] as $service)
                                <li wire:key="svc-{{ $service['id'] }}">
                                    <button class="svc" type="button" wire:click="selectService('{{ $service['id'] }}')" @if ($selectedService === $service['id']) aria-current="true" @endif>
                                        <span>
                                            <span class="name">{{ $service['label'] }}</span>
                                            @if ($service['shared'])
                                                <span class="shared-word"><x-icon name="share-network" :size="11" /> shared</span>
                                            @endif
                                            @if (! empty($service['infra']))
                                                <span class="chip chip-muted">infra</span>
                                            @endif
                                            @if ($service['disabled'])
                                                <span class="chip chip-muted">disabled</span>
                                            @endif
                                            @if ($service['finite'])
                                                <span class="chip chip-muted">job</span>
                                            @endif
                                            @if ($service['ports'] !== '')
                                                <span class="ports">{{ $service['ports'] }}</span>
                                            @endif
                                            @if ($service['error'])
                                                <span class="error-line">{{ $service['error'] }}</span>
                                            @endif
                                        </span>
                                        <span class="state state-{{ $service['state'] }}" wire:loading.remove.delay wire:target="selectService('{{ $service['id'] }}')">{{ $service['display'] }}</span>
                                        <span class="state" wire:loading.delay wire:target="selectService('{{ $service['id'] }}')"><x-icon name="spinner-gap" :size="12" class="spin" /></span>
                                    </button>
                                </li>
                            @endforeach
                        </ul>
                    </div>
                @endforeach

                @if ($selectedLine && ! $selectedLine['disabled'])
                    <div class="actions">
                        @if (\App\Support\ServiceBoard::showsStop($selectedLine['state']))
                            <button class="btn {{ $pendingKind === 'project-stop' && $pendingId === $selectedLine['id'] ? 'armed-danger' : '' }}" type="button" wire:click="stopService('{{ $selectedLine['id'] }}')" wire:loading.attr="disabled" wire:target="stopService('{{ $selectedLine['id'] }}')">
                                <x-icon name="stop" :size="13" /> {{ $pendingKind === 'project-stop' && $pendingId === $selectedLine['id'] ? 'Confirm stop' : 'Stop' }}
                            </button>
                        @else
                            <button class="btn primary" type="button" wire:click="startService('{{ $selectedLine['id'] }}')" wire:loading.attr="disabled" wire:target="startService('{{ $selectedLine['id'] }}')"><x-icon name="play" :size="13" /> Start</button>
                        @endif
                        <button class="btn {{ $pendingKind === 'project-restart' && $pendingId === $selectedLine['id'] ? 'armed' : '' }}" type="button" wire:click="restartService('{{ $selectedLine['id'] }}')" wire:loading.attr="disabled" wire:target="restartService('{{ $selectedLine['id'] }}')">
                            <x-icon name="arrow-clockwise" :size="13" /> {{ $pendingKind === 'project-restart' && $pendingId === $selectedLine['id'] ? 'Confirm restart' : 'Restart' }}
                        </button>
                        @if ($selectedLine['state'] === 'externally-owned')
                            <button class="btn {{ $pendingKind === 'kill' && $pendingId === $selectedLine['id'] ? 'armed-danger' : '' }}" type="button" wire:click="reclaimPort('{{ $selectedLine['id'] }}')" wire:loading.attr="disabled" wire:target="reclaimPort('{{ $selectedLine['id'] }}')">
                                <x-icon name="warning-circle" :size="13" /> {{ $pendingKind === 'kill' && $pendingId === $selectedLine['id'] ? 'Confirm reclaim' : 'Reclaim port' }}
                            </button>
                        @endif
                    </div>
                @elseif ($selectedLine && $selectedLine['disabled'])
                    <p class="hint">This service is disabled.</p>
                @endif

                @if ($urls !== [])
                    <h2>URLs</h2>
                    <ul class="urls">
                        @foreach ($urls as $url)
                            <li wire:key="url-{{ $url['serviceId'] }}-{{ $url['url'] }}">
                                <a href="{{ $url['url'] }}" target="_blank" rel="noopener noreferrer"><x-icon name="arrow-square-out" :size="12" /> {{ $url['label'] }}</a>
                                <span class="sub">{{ $url['url'] }}</span>
                            </li>
                        @endforeach
                    </ul>
                @endif

            @endif
        @else
            <h1>Hearth</h1>
            <p class="empty"><x-icon name="folder" :size="14" /> No workspace selected.</p>
        @endif

        <p class="binary">
            {{ $binaryLine }}
            <br>
            executable: <span @class(['bad' => ! $binary['executable'], 'ok' => $binary['executable']])>{{ $binary['executable'] ? 'yes' : 'no' }}</span>
            <br>
            inside asar: <span @class(['bad' => $binary['inside_asar']])>{{ $binary['inside_asar'] ? 'yes' : 'no' }}</span>
        </p>
    </section>

    @if ($board)
        <aside class="logcol {{ $logOpen ? '' : 'collapsed' }}" aria-label="Log">
            <div class="loghead">
                <button class="loghead-btn" type="button" wire:click="toggleLog" aria-expanded="{{ $logOpen ? 'true' : 'false' }}" title="{{ $logOpen ? 'Hide log' : 'Show log' }}">
                    <x-icon name="caret-down" :size="12" class="caret" />
                    <x-icon name="terminal" :size="13" />
                    @if ($logOpen)
                        Log <span class="logsub">{{ $selectedService === '$daemon' ? 'daemon' : $selectedService }}</span>
                    @endif
                </button>
                @if ($logOpen && $logHasMore)
                    <button class="btn" type="button" wire:click="expandLog" wire:loading.attr="disabled" wire:target="expandLog"><x-icon name="clock-counter-clockwise" :size="12" /> Earlier</button>
                @endif
                @if ($logOpen)
                    <span class="logbusy" wire:loading.delay wire:target="toggleLog, expandLog"><x-icon name="spinner-gap" :size="13" class="spin" /></span>
                @endif
            </div>
            @if ($logOpen)
                <div class="logwrap">
                    @if ($logText === '')
                        <p class="logempty">No output yet.</p>
                    @else
                        <pre class="log" data-log>{{ $logText }}</pre>
                        <button class="logjump" type="button" data-log-jump hidden><x-icon name="arrow-line-down" :size="12" /> Latest</button>
                    @endif
                </div>
            @endif
        </aside>
    @endif

    <script>
        (function () {
            if (window.__hearthLogInit) return;
            window.__hearthLogInit = true;
            var FLAG = '__hearthLog';
            function attach(pre) {
                if (pre[FLAG]) return;
                var state = { follow: true };
                pre[FLAG] = state;
                var jump = pre.parentElement && pre.parentElement.querySelector('[data-log-jump]');
                function toBottom() { pre.scrollTop = pre.scrollHeight; }
                pre.addEventListener('scroll', function () {
                    var near = pre.scrollHeight - pre.scrollTop - pre.clientHeight < 48;
                    state.follow = near;
                    if (jump) jump.hidden = near;
                }, { passive: true });
                if (jump) {
                    jump.addEventListener('click', function () { state.follow = true; toBottom(); });
                }
                new MutationObserver(function () { if (state.follow) toBottom(); })
                    .observe(pre, { childList: true, characterData: true, subtree: true });
                toBottom();
            }
            function attachAll() {
                document.querySelectorAll('[data-log]').forEach(attach);
            }
            new MutationObserver(attachAll).observe(document.documentElement, { childList: true, subtree: true });
            attachAll();
        })();
    </script>
</div>
