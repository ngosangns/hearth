<?php

$home = getenv('HOME') ?: '';

return [
    /*
    | The Swift-era list shared with `hearth tui`. Tests override this. The app
    | must not point a test at the real file, or it would rewrite the user's list.
    */
    'workspace_file' => env(
        'HEARTH_WORKSPACE_FILE',
        $home.'/Library/Application Support/HearthApp/workspaces.json',
    ),

    /*
    | Where `task install` points a shell at hearth. The app retargets this
    | symlink only. Tests override both paths and never touch the real link.
    */
    'bin_link' => $home.'/.local/bin/hearth',

    'versioned_dir' => $home.'/.local/share/hearth/bin',
];
