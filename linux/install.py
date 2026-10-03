#!/usr/bin/env python3
"""Install both Linux executables and a desktop entry for the current user."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess

ICONS = {32: '32x32.png', 128: '128x128.png', 256: '128x128@2x.png', 512: 'icon.png'}


def desktop_value(value):
    return str(value).replace('\\', '\\\\').replace('\n', '\\n').replace('\r', '\\r').replace('\t', '\\t')


def refresh_desktop(data_dir):
    # Icon themes are cached; touching the root signals a changed theme even
    # when its index.theme is inherited from /usr/share/icons/hicolor.
    theme = data_dir / 'icons/hicolor'
    if theme.is_dir():
        os.utime(theme, None)
    if shutil.which('update-desktop-database'):
        subprocess.run(['update-desktop-database', str(data_dir / 'applications')], check=True)


def desktop_quote(value):
    quoted = ''.join('\\' + c if c in '\\"`$' else '%%' if c == '%' else c for c in str(value))
    quoted = quoted.replace('\\', '\\\\').replace('\n', '\\n').replace('\r', '\\r').replace('\t', '\\t')
    return '"' + quoted + '"'


def install(bin_dir, prefix, data_dir):
    app_dir = prefix / 'lib/codenotch'
    for name in ('codenotch', 'codenotch-hook'):
        source = bin_dir / name
        if not source.is_file() or not os.access(source, os.X_OK):
            raise SystemExit(f'Missing executable: {source}. Run make build first.')
        link = prefix / 'bin' / name
        if (link.exists() or link.is_symlink()) and not (link.is_symlink() and link.resolve() == app_dir / name):
            raise SystemExit(f'Refusing to replace an unrelated file: {link}')
    app_dir.mkdir(parents=True, exist_ok=True)
    (prefix / 'bin').mkdir(parents=True, exist_ok=True)
    for name in ('codenotch', 'codenotch-hook'):
        # Replace atomically so an already running executable can be updated.
        temporary = app_dir / (name + '.new')
        shutil.copyfile(bin_dir / name, temporary)
        temporary.chmod(0o755)
        temporary.replace(app_dir / name)
        link = prefix / 'bin' / name
        if not link.is_symlink():
            link.symlink_to(app_dir / name)
    applications = data_dir / 'applications'
    applications.mkdir(parents=True, exist_ok=True)
    source_icons = Path(__file__).resolve().parents[1] / 'windows/codenotch/icons'
    for size, filename in ICONS.items():
        icon = data_dir / f'icons/hicolor/{size}x{size}/apps/codenotch.png'
        icon.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source_icons / filename, icon)
    # An absolute icon also works in launchers holding a stale theme cache.
    icon = data_dir / 'icons/hicolor/512x512/apps/codenotch.png'
    (applications / 'com.immidi.codenotch.desktop').write_text(
        '[Desktop Entry]\nType=Application\nName=Codenotch\n'
        'Comment=AI usage and session monitor\n'
        f'Exec={desktop_quote(app_dir / "codenotch")}\n'
        f'Icon={desktop_value(icon)}\nTerminal=false\nCategories=Utility;\n'
        'StartupNotify=false\nStartupWMClass=Codenotch\n',
        encoding='utf-8')
    refresh_desktop(data_dir)
    print(f'Installed Codenotch. Launch it from the application menu or {prefix / "bin/codenotch"}')


def uninstall(prefix, data_dir):
    app_dir = prefix / 'lib/codenotch'
    for name in ('codenotch', 'codenotch-hook'):
        link = prefix / 'bin' / name
        if link.is_symlink() and link.resolve() == app_dir / name:
            link.unlink()
        (app_dir / name).unlink(missing_ok=True)
    for relative in ['applications/com.immidi.codenotch.desktop'] + [
            f'icons/hicolor/{size}x{size}/apps/codenotch.png' for size in ICONS]:
        (data_dir / relative).unlink(missing_ok=True)
    if (data_dir / 'applications').is_dir():
        refresh_desktop(data_dir)
    print('Removed executables and desktop entry. Settings and Claude hooks are preserved.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=Path, default=Path(__file__).resolve().parents[1] / 'windows/target/release')
    parser.add_argument('--prefix', type=Path, default=Path.home() / '.local')
    parser.add_argument('--data-dir', type=Path, default=Path(os.environ.get('XDG_DATA_HOME') or Path.home() / '.local/share'))
    parser.add_argument('--uninstall', action='store_true')
    args = parser.parse_args()
    prefix, data_dir = args.prefix.resolve(), args.data_dir.resolve()
    if args.uninstall:
        uninstall(prefix, data_dir)
    else:
        install(args.bin_dir.resolve(), prefix, data_dir)


if __name__ == '__main__':
    main()
