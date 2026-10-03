"""Regressions for startup-window races in the private X11 test harness."""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import unittest


@unittest.skipUnless(shutil.which('Xvfb') and shutil.which('xvfb-run') and shutil.which('xauth'),
                     'Xvfb and xauth are required for the X11 harness tests')
class WindowDiscoveryTests(unittest.TestCase):
    def run_x11(self, assertion):
        # Reproduce the precise race deterministically: enumerate a real child,
        # destroy it, then ask for its geometry using the stale window ID.
        script = '''
import ctypes as C
import os
from verify_hover import Display

display = Display(os.environ['DISPLAY'])
x11 = C.CDLL('libX11.so.6')
x11.XCreateSimpleWindow.restype = C.c_ulong
x11.XCreateSimpleWindow.argtypes = [C.c_void_p, C.c_ulong, C.c_int, C.c_int,
                                    C.c_uint, C.c_uint, C.c_uint, C.c_ulong, C.c_ulong]
x11.XDestroyWindow.argtypes = [C.c_void_p, C.c_ulong]
window = x11.XCreateSimpleWindow(display.handle, display.root, 10, 20, 30, 40, 0, 0, 0)
display.sync(display.handle, 0)
assert window in display.windows()
assert display.bounds(window) == (10, 20, 30, 40)
x11.XDestroyWindow(display.handle, window)
display.sync(display.handle, 0)
assert display.bounds(window) is None
'''
        # Do not inherit desktop display scaling or an existing session's X auth.
        env = {key: value for key, value in os.environ.items()
               if key not in ('DISPLAY', 'XAUTHORITY')}
        return subprocess.run(['xvfb-run', '-a', '-s', '-screen 0 1280x900x24 -nolisten tcp',
                               sys.executable, '-c', script + assertion],
                              cwd=Path(__file__).resolve().parent, env=env,
                              capture_output=True, text=True, timeout=15)

    def test_destroyed_child_is_skipped_and_next_query_succeeds(self):
        result = self.run_x11('''
assert display.bounds(display.root)[2:] == (1280, 900)
display.close(display.handle)
''')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_other_x_errors_still_fail_after_geometry_probe(self):
        result = self.run_x11('''
# The normal error handler must be restored: a stale drawable during capture
# is a real test failure, not a transient child to skip during discovery.
display.capture(window, 30, 40)
''')
        self.assertNotEqual(result.returncode, 0)
        output = result.stdout + result.stderr
        self.assertIn('BadDrawable', output)
        self.assertIn('X_GetImage', output)


if __name__ == '__main__':
    unittest.main()
