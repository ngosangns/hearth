<!DOCTYPE html>
<html lang="{{ str_replace('_', '-', app()->getLocale()) }}">
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>{{ $title ?? 'Hearth' }}</title>
    <style>
        /* Design tokens — three layers: primitive → semantic → component. */
        :root {
            color-scheme: light dark;

            /* Primitives (macOS system palette) */
            --gray-0: #ffffff; --gray-50: #f6f6f7; --gray-100: #ececee;
            --gray-200: #e0e0e3; --gray-300: #d2d2d7; --gray-800: #3a3a3c;
            --ink-900: #1d1d1f; --ink-500: #6e6e73;
            --blue-500: #007aff; --blue-600: #0066d6;
            --red-500: #ff3b30;  --red-600: #d70015;
            --green-500: #34c759; --green-700: #248a3d;
            --orange-500: #ff9500; --orange-700: #b25000;
            --teal-600: #0e7c86;

            /* Semantic */
            --bg: var(--gray-100);
            --panel: var(--gray-50);
            --sidebar: var(--gray-200);
            --ink: var(--ink-900);
            --muted: var(--ink-500);
            --line: var(--gray-300);
            --accent: var(--blue-600);
            --accent-ink: var(--gray-0);
            --danger: var(--red-600);
            --danger-ink: var(--gray-0);
            --ok: var(--green-700);
            --run: var(--teal-600);
            --warn: var(--orange-700);
            --shared: var(--blue-600);
            --focus: var(--blue-500);

            /* Component */
            --sel: rgb(0 0 0 / 0.08);
            --btn-bg: var(--gray-0);
            --btn-line: rgb(0 0 0 / 0.12);
            --btn-shadow: 0 0.5px 1px rgb(0 0 0 / 0.08);
            --seg-bg: rgb(0 0 0 / 0.06);
            --seg-on: var(--gray-0);
            --log-bg: #fbfbfc;
            --log-line: var(--line);
            --radius-sm: 6px;
            --radius: 8px;
            --radius-lg: 10px;
            --ease-out: cubic-bezier(0.16, 1, 0.3, 1);
            --dur-fast: 120ms;
            --dur-med: 200ms;
        }
        @media (prefers-color-scheme: dark) {
            :root {
                --bg: #1e1e20;
                --panel: #2a2a2d;
                --sidebar: #262628;
                --ink: #f5f5f7;
                --muted: #98989d;
                --line: #3d3d40;
                --accent: #0a84ff;
                --accent-ink: #ffffff;
                --danger: #ff453a;
                --danger-ink: #1c0b09;
                --ok: #30d158;
                --run: #5ac8f5;
                --warn: #ffd60a;
                --shared: #64b5f6;
                --focus: #0a84ff;

                --sel: rgb(255 255 255 / 0.10);
                --btn-bg: rgb(255 255 255 / 0.08);
                --btn-line: rgb(255 255 255 / 0.14);
                --btn-shadow: 0 0.5px 1px rgb(0 0 0 / 0.3);
                --seg-bg: rgb(255 255 255 / 0.08);
                --seg-on: rgb(255 255 255 / 0.16);
                --log-bg: #141416;
                --log-line: var(--line);
            }
        }
        * { box-sizing: border-box; }
        html, body { height: 100%; margin: 0; }
        body {
            background: var(--bg);
            color: var(--ink);
            font: 13px/1.45 -apple-system, BlinkMacSystemFont, "SF Pro Text", "Segoe UI", sans-serif;
            -webkit-font-smoothing: antialiased;
        }
        button, input { font: inherit; color: inherit; }
        button { cursor: pointer; }
        .icon { display: inline-block; vertical-align: -0.15em; flex: none; }
        :focus-visible { outline: 2px solid var(--focus); outline-offset: 2px; }
        @media (prefers-reduced-motion: reduce) {
            * { transition: none !important; animation-duration: 0.01ms !important; animation-iteration-count: 1 !important; }
        }
    </style>
</head>
<body>
    {{ $slot }}
</body>
</html>
