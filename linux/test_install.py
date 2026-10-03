import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('installer', Path(__file__).with_name('install.py'))
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)


class InstallationTests(unittest.TestCase):
    def test_install_update_uninstall_preserves_unrelated_files(self):
        with tempfile.TemporaryDirectory(prefix='codenotch install ') as tmp:
            root = Path(tmp)
            binaries, prefix, data = root / 'build', root / "O'Brien $test % local", root / 'share'
            binaries.mkdir()
            for name in ('codenotch', 'codenotch-hook'):
                p = binaries / name
                p.write_text('#!/bin/sh\nexit 0\n')
                p.chmod(0o755)
            installer.install(binaries, prefix, data)
            entry = data / 'applications/com.immidi.codenotch.desktop'
            self.assertIn('StartupWMClass=Codenotch\n', entry.read_text())
            self.assertIn(f'Icon={data}/icons/hicolor/512x512/apps/codenotch.png\n', entry.read_text())
            for size in installer.ICONS:
                image = (data / f'icons/hicolor/{size}x{size}/apps/codenotch.png').read_bytes()
                self.assertEqual(image[:8], b'\x89PNG\r\n\x1a\n')
                self.assertEqual(int.from_bytes(image[16:20], 'big'), size)
            if shutil.which('desktop-file-validate'):
                subprocess.run(['desktop-file-validate', str(entry)], check=True)
            for name in ('codenotch', 'codenotch-hook'):
                subprocess.run([str(prefix / 'bin' / name)], check=True)
            (prefix / 'lib/codenotch/keep').write_text('user file')
            installer.install(binaries, prefix, data)
            installer.uninstall(prefix, data)
            self.assertFalse(entry.exists())
            self.assertFalse((prefix / 'bin/codenotch').is_symlink())
            for size in installer.ICONS:
                self.assertFalse((data / f'icons/hicolor/{size}x{size}/apps/codenotch.png').exists())
            self.assertEqual((prefix / 'lib/codenotch/keep').read_text(), 'user file')

    def test_refuses_to_overwrite_an_unrelated_launcher(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'bin').mkdir()
            (root / 'bin/codenotch').write_text('keep me')
            binaries = root / 'build'
            binaries.mkdir()
            for name in ('codenotch', 'codenotch-hook'):
                p = binaries / name
                p.write_text('#!/bin/sh\nexit 0\n')
                p.chmod(0o755)
            with self.assertRaises(SystemExit):
                installer.install(binaries, root, root / 'share')
            self.assertEqual((root / 'bin/codenotch').read_text(), 'keep me')

    def test_launcher_respects_explicit_binary_and_arguments(self):
        launcher = Path(__file__).resolve().parents[1] / 'windows/scripts/run-linux.sh'
        with tempfile.TemporaryDirectory() as tmp:
            fake = Path(tmp) / 'fake app'
            fake.write_text('#!/bin/sh\nprintf "%s\\n" "$GDK_BACKEND" "$@"\n')
            fake.chmod(0o755)
            env = dict(os.environ, CODENOTCH_BIN=str(fake), GDK_BACKEND='wayland')
            out = subprocess.run([str(launcher), 'doctor', 'two words'], env=env, text=True, capture_output=True, check=True)
            self.assertEqual(out.stdout, 'x11\ndoctor\ntwo words\n')
            env['CODENOTCH_BIN'] = str(fake) + '-missing'
            result = subprocess.run([str(launcher)], env=env, capture_output=True)
            self.assertNotEqual(result.returncode, 0)

    def test_launcher_defaults_to_shared_memory_and_respects_opt_in(self):
        launcher = Path(__file__).resolve().parents[1] / 'windows/scripts/run-linux.sh'
        with tempfile.TemporaryDirectory() as tmp:
            fake = Path(tmp) / 'fake app'
            fake.write_text('#!/bin/sh\nprintf "%s" "$WEBKIT_DMABUF_RENDERER_FORCE_SHM"\n')
            fake.chmod(0o755)
            env = dict(os.environ, CODENOTCH_BIN=str(fake))
            env.pop('WEBKIT_DMABUF_RENDERER_FORCE_SHM', None)
            for value, expected in [(None, '1'), ('', '1'), ('0', '0'), ('1', '1')]:
                if value is not None:
                    env['WEBKIT_DMABUF_RENDERER_FORCE_SHM'] = value
                out = subprocess.run([str(launcher)], env=env, text=True, capture_output=True, check=True)
                self.assertEqual(out.stdout, expected)


if __name__ == '__main__':
    unittest.main()
