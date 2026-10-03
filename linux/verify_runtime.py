#!/usr/bin/env python3
"""Exercise compiled Linux hooks and autostart with temporary XDG data only."""
import argparse
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import threading


def verify(bin_dir):
    with tempfile.TemporaryDirectory(prefix='codenotch-runtime-') as tmp:
        root = Path(tmp)
        env = dict(os.environ, XDG_CONFIG_HOME=str(root / 'config'))
        config = root / 'config/codenotch'
        config.mkdir(parents=True)
        with socket.socket() as server:
            server.bind(('127.0.0.1', 0))
            server.listen()
            server.settimeout(5)
            port = server.getsockname()[1]
            (config / 'config.json').write_text(json.dumps({'port': port}))
            received = []

            def receive():
                connection, _ = server.accept()
                with connection:
                    connection.settimeout(5)
                    data = b''
                    while b'\r\n\r\n' not in data:
                        data += connection.recv(4096)
                    headers, body = data.split(b'\r\n\r\n', 1)
                    length = int(next(line.split(b':', 1)[1] for line in headers.split(b'\r\n') if line.lower().startswith(b'content-length:')))
                    while len(body) < length:
                        body += connection.recv(4096)
                    received.append((headers, json.loads(body)))
                    connection.sendall(b'HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK')

            worker = threading.Thread(target=receive, daemon=True)
            worker.start()
            subprocess.run([str(bin_dir / 'codenotch-hook'), 'done'], env=env,
                           input='{"session_id":"fedora-fixture"}', text=True, check=True, timeout=5)
            worker.join(6)
            assert received, 'hook did not use the custom XDG port'
            headers, body = received[0]
            assert headers.startswith(b'POST /event?e=done&ppid=')
            assert b'ppid=0 ' not in headers
            assert body == {'session_id': 'fedora-fixture'}

        # A failing connection must launch the Linux sibling, then exit quietly.
        fake_dir = root / 'bin with spaces'
        fake_dir.mkdir()
        shutil.copy2(bin_dir / 'codenotch-hook', fake_dir / 'codenotch-hook')
        marker = root / 'launched'
        fake_app = fake_dir / 'codenotch'
        fake_app.write_text('#!/bin/sh\nprintf started > "$CODENOTCH_TEST_MARKER"\n')
        fake_app.chmod(0o755)
        subprocess.run([str(fake_dir / 'codenotch-hook'), 'ping'],
                       env=dict(env, CODENOTCH_TEST_MARKER=str(marker)), input='', text=True, check=True, timeout=5)
        assert marker.read_text() == 'started'

        # No GTK initialization or display is needed by autostart subcommands.
        env.pop('DISPLAY', None)
        app = str(bin_dir / 'codenotch')
        enabled = subprocess.run([app, 'autostart', 'on'], env=env, capture_output=True, text=True, check=True)
        assert enabled.stdout.startswith('OK:'), enabled.stdout
        entry = root / 'config/autostart/codenotch.desktop'
        assert entry.is_file() and '--silent' in entry.read_text()
        if shutil.which('desktop-file-validate'):
            subprocess.run(['desktop-file-validate', str(entry)], check=True)
        disabled = subprocess.run([app, 'autostart', 'off'], env=env, capture_output=True, text=True, check=True)
        assert disabled.stdout.startswith('OK:') and not entry.exists()
        result = subprocess.run([app], env=env, capture_output=True, text=True, timeout=5)
        assert result.returncode != 0 and 'XWayland' in result.stderr
    print('PASS: XDG hook delivery, parent PID, helper auto-launch, autostart and headless diagnostic')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=Path, default=Path(__file__).resolve().parents[1] / 'windows/target/release')
    verify(parser.parse_args().bin_dir.resolve())
