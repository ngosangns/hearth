<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <title>Hearth macOS spike</title>
    <style>
        body { font: 15px/1.45 ui-sans-serif, system-ui, sans-serif; margin: 2rem; color: #1c1917; background: #fafaf9; }
        h1 { font-size: 1.4rem; margin: 0 0 0.4rem; }
        p { margin: 0 0 1rem; max-width: 42rem; }
        dl { display: grid; grid-template-columns: 9rem 1fr; gap: 0.25rem 1rem; margin: 0 0 1.5rem; }
        dt { color: #57534e; }
        dd { margin: 0; font-family: ui-monospace, monospace; }
        pre { white-space: pre-wrap; background: #fff; border: 1px solid #e7e5e4; padding: 0.75rem 1rem; max-width: 48rem; }
        button { font: inherit; padding: 0.4rem 0.8rem; }
        .bad { color: #b91c1c; }
    </style>
</head>
<body>
    <h1>Hearth macOS spike</h1>
    <p>This window only checks that the bundled <code>hearth</code> binary is a real executable outside <code>app.asar</code>, then starts and stops a fixture daemon.</p>

    <h2>Binary</h2>
    <dl>
        <dt>path</dt>
        <dd>{{ $report['path'] }}</dd>
        <dt>real</dt>
        <dd>{{ $report['real'] ?? '—' }}</dd>
        <dt>exists</dt>
        <dd>{{ $report['exists'] ? 'yes' : 'no' }}</dd>
        <dt>executable</dt>
        <dd @class(['bad' => ! $report['executable']])>{{ $report['executable'] ? 'yes' : 'no' }}</dd>
        <dt>inside asar</dt>
        <dd @class(['bad' => $report['inside_asar']])>{{ $report['inside_asar'] ? 'yes' : 'no' }}</dd>
        <dt>version exit</dt>
        <dd>{{ $version['exit'] === null ? '—' : $version['exit'] }}</dd>
    </dl>
    <pre>{{ $version['output'] }}</pre>

    <form method="post" action="{{ route('spike.ensure') }}">
        @csrf
        <button type="submit">Run manager ensure on the fixture, then stop</button>
    </form>

    @if ($ensure)
        <h2>Ensure</h2>
        <dl>
            <dt>ok</dt>
            <dd @class(['bad' => empty($ensure['ok'])])>{{ ! empty($ensure['ok']) ? 'yes' : 'no' }}</dd>
            @isset($ensure['root'])
                <dt>fixture</dt>
                <dd>{{ $ensure['root'] }}</dd>
            @endisset
            @isset($ensure['exit'])
                <dt>ensure exit</dt>
                <dd>{{ $ensure['exit'] }}</dd>
            @endisset
            @isset($ensure['health'])
                <dt>healthz</dt>
                <dd>{{ $ensure['health']['status'] ?? '—' }} {{ $ensure['health']['ok'] ? 'ok' : 'failed' }}</dd>
            @endisset
            @isset($ensure['stop_exit'])
                <dt>stop exit</dt>
                <dd>{{ $ensure['stop_exit'] }}</dd>
            @endisset
        </dl>
        @isset($ensure['detail'])
            <pre>{{ $ensure['detail'] }}</pre>
        @endisset
        @isset($ensure['output'])
            <pre>{{ $ensure['output'] }}</pre>
        @endisset
        @if (! empty($ensure['error']))
            <pre>{{ $ensure['error'] }}</pre>
        @endif
        @isset($ensure['health'])
            <pre>{{ $ensure['health']['body'] }}</pre>
        @endisset
        @if (! empty($ensure['stop_output']))
            <pre>{{ $ensure['stop_output'] }}</pre>
        @endif
    @endif
</body>
</html>
