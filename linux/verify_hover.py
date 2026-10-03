#!/usr/bin/env python3
"""Exercise native painting, mouse enter/leave and X11 input in a private Xvfb.

Uses --smoke-test (no providers or account access), temporary XDG directories,
and its own display and session bus. Never moves the desktop's real pointer.
"""
import argparse
import ctypes as C
import os
from pathlib import Path
import select
import subprocess
import tempfile
import time


class Rectangle(C.Structure):
    _fields_ = [('x', C.c_short), ('y', C.c_short), ('width', C.c_ushort), ('height', C.c_ushort)]


class XImage(C.Structure):
    # Public Xlib structure prefix; trailing function pointers are not used.
    _fields_ = [('width', C.c_int), ('height', C.c_int), ('xoffset', C.c_int),
                ('format', C.c_int), ('data', C.c_void_p), ('byte_order', C.c_int),
                ('bitmap_unit', C.c_int), ('bitmap_bit_order', C.c_int),
                ('bitmap_pad', C.c_int), ('depth', C.c_int),
                ('bytes_per_line', C.c_int), ('bits_per_pixel', C.c_int),
                ('red_mask', C.c_ulong), ('green_mask', C.c_ulong), ('blue_mask', C.c_ulong)]


class XErrorEvent(C.Structure):
    _fields_ = [('type', C.c_int), ('display', C.c_void_p), ('resourceid', C.c_ulong),
                ('serial', C.c_ulong), ('error_code', C.c_ubyte),
                ('request_code', C.c_ubyte), ('minor_code', C.c_ubyte)]


class Display:
    def __init__(self, name):
        x11, ext = C.CDLL('libX11.so.6'), C.CDLL('libXext.so.6')
        ptr, ul, ui, si = C.c_void_p, C.c_ulong, C.c_uint, C.c_int

        def bind(lib, name, result, *args):
            function = getattr(lib, name)
            function.restype, function.argtypes = result, args
            return function

        self.close = bind(x11, 'XCloseDisplay', si, ptr)
        self.free = bind(x11, 'XFree', si, ptr)
        self.tree = bind(x11, 'XQueryTree', si, ptr, ul, C.POINTER(ul), C.POINTER(ul), C.POINTER(C.POINTER(ul)), C.POINTER(ui))
        self.geometry = bind(x11, 'XGetGeometry', si, ptr, ul, C.POINTER(ul), C.POINTER(si), C.POINTER(si), C.POINTER(ui), C.POINTER(ui), C.POINTER(ui), C.POINTER(ui))
        self.shapes = bind(ext, 'XShapeGetRectangles', C.POINTER(Rectangle), ptr, ul, si, C.POINTER(si), C.POINTER(si))
        self.warp = bind(x11, 'XWarpPointer', si, ptr, ul, ul, si, si, ui, ui, si, si)
        self.sync = bind(x11, 'XSync', si, ptr, si)
        self.set_error_handler = bind(x11, 'XSetErrorHandler', ptr, ptr)
        self.get_image = bind(x11, 'XGetImage', C.POINTER(XImage), ptr, ul, si, si, ui, ui, ul, si)
        self.destroy_image = bind(x11, 'XDestroyImage', si, C.POINTER(XImage))
        self.pointer = bind(x11, 'XQueryPointer', si, ptr, ul, C.POINTER(ul), C.POINTER(ul), C.POINTER(si), C.POINTER(si), C.POINTER(si), C.POINTER(si), C.POINTER(ui))
        self.handle = bind(x11, 'XOpenDisplay', ptr, C.c_char_p)(name.encode())
        assert self.handle, f'Cannot open test display {name}'
        self.root = bind(x11, 'XDefaultRootWindow', ul, ptr)(self.handle)

    def windows(self):
        root, parent, children, count = C.c_ulong(), C.c_ulong(), C.POINTER(C.c_ulong)(), C.c_uint()
        self.tree(self.handle, self.root, C.byref(root), C.byref(parent), C.byref(children), C.byref(count))
        result = list(children[:count.value])
        self.free(children)
        return result

    def bounds(self, window):
        # GTK creates short-lived startup windows. A child returned by XQueryTree
        # can disappear before XGetGeometry; Xlib's default handler would exit
        # the entire test before Python can check the failed request's status.
        errors = []

        @C.CFUNCTYPE(C.c_int, C.c_void_p, C.POINTER(XErrorEvent))
        def handle_error(display, event):
            error = event.contents
            errors.append((display, error.resourceid, error.error_code, error.request_code))
            return 0

        root, x, y = C.c_ulong(), C.c_int(), C.c_int()
        width, height, border, depth = [C.c_uint() for _ in range(4)]
        # This harness uses Xlib on one thread. Drain earlier requests and scope
        # the temporary handler to this query, preserving all other X errors.
        self.sync(self.handle, 0)
        previous = self.set_error_handler(C.cast(handle_error, C.c_void_p))
        try:
            status = self.geometry(self.handle, window, C.byref(root), C.byref(x), C.byref(y), C.byref(width), C.byref(height), C.byref(border), C.byref(depth))
            self.sync(self.handle, 0)
        finally:
            self.set_error_handler(previous)
        for error in errors:
            assert error == (self.handle, window, 9, 14), f'Unexpected X11 geometry error: {error}'
        if not status or errors:  # BadDrawable (9) from X_GetGeometry (14)
            return None
        return x.value, y.value, width.value, height.value

    def regions(self, window):
        count, order = C.c_int(), C.c_int()
        result = self.shapes(self.handle, window, 2, C.byref(count), C.byref(order))  # ShapeInput
        rows = [(r.x, r.y, r.width, r.height) for r in result[:count.value]]
        self.free(result)
        return rows

    def move(self, x, y):
        self.warp(self.handle, 0, self.root, 0, 0, 0, 0, x, y)
        self.sync(self.handle, 0)

    def window_under_pointer(self):
        root, child = C.c_ulong(), C.c_ulong()
        rx, ry, wx, wy = [C.c_int() for _ in range(4)]
        mask = C.c_uint()
        self.pointer(self.handle, self.root, C.byref(root), C.byref(child), C.byref(rx), C.byref(ry), C.byref(wx), C.byref(wy), C.byref(mask))
        return child.value

    def capture(self, window, width, height, path=None):
        native = self.get_image(self.handle, window, 0, 0, width, height, C.c_ulong(-1).value, 2)
        assert native, 'Missing native window capture'
        try:
            frame = native.contents
            assert frame.depth == 32 and frame.bits_per_pixel == 32, 'Expected an ARGB notch window'
            assert (frame.red_mask, frame.green_mask, frame.blue_mask) == (0xff0000, 0xff00, 0xff)
            data = C.string_at(frame.data, frame.bytes_per_line * height)
            stride, alpha = frame.bytes_per_line, 3 if frame.byte_order == 0 else 0
            if path:
                import gi
                gi.require_version('GdkPixbuf', '2.0')
                from gi.repository import GdkPixbuf, GLib
                # Encode without opening another GDK display or attaching its
                # fatal X11 error handler to the short-lived smoke-test window.
                offsets = (2, 1, 0, 3) if frame.byte_order == 0 else (1, 2, 3, 0)
                rgba = bytes(data[y * stride + x * 4 + channel]
                             for y in range(height) for x in range(width) for channel in offsets)
                pixbuf = GdkPixbuf.Pixbuf.new_from_bytes(GLib.Bytes.new(rgba), GdkPixbuf.Colorspace.RGB,
                                                       True, 8, width, height, width * 4)
                pixbuf.savev(str(path), 'png', [], [])
            return data, stride, alpha
        finally:
            self.destroy_image(native)


def until(check, message, seconds=2):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        result = check()
        if result:
            return result
        time.sleep(.03)
    raise AssertionError(message)


def verify(binary, output_dir=None):
    with tempfile.TemporaryDirectory(prefix='codenotch-hover-') as tmp:
        read, write = os.pipe()
        xvfb = subprocess.Popen(['Xvfb', '-displayfd', str(write), '-screen', '0', '1280x900x24', '-nolisten', 'tcp'],
                                pass_fds=(write,), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        os.close(write)
        display, app = None, None
        try:
            with os.fdopen(read) as fd:
                assert select.select([fd], [], [], 10)[0], 'Xvfb did not start'
                number = fd.readline().strip()
                assert number.isdigit(), 'Xvfb did not provide a display number'
            name = ':' + number
            display = Display(name)
            display.move(100, 100)
            env = dict(os.environ, DISPLAY=name, GDK_SCALE='1', GDK_DPI_SCALE='1',
                       XDG_CONFIG_HOME=tmp+'/config', XDG_CACHE_HOME=tmp+'/cache',
                       XDG_DATA_HOME=tmp+'/data', NO_AT_BRIDGE='1')
            Path(tmp+'/config/codenotch').mkdir(parents=True)
            # Files instead of PIPE keep portal services from retaining our pipe
            # after the app and its private session bus exit.
            with open(Path(tmp) / 'startup.log', 'w+') as log:
                app = subprocess.Popen(['dbus-run-session', '--', str(binary), '--smoke-test'],
                                       env=env, stdout=log, stderr=subprocess.STDOUT)
                window, (x, y, width, height) = until(
                    lambda: next(((w, bounds) for w in display.windows()
                                  if (bounds := display.bounds(w)) and bounds[2:] == (360, 650)), None),
                    'The notch window did not appear')

                def card_pixels(label):
                    # Read the *native window*, not WebKit.get_snapshot: the
                    # latter bypasses GTK presentation and hid the bug.
                    path = output_dir / (label + '.png') if output_dir else None
                    pixels, stride, alpha = display.capture(window, width, height, path)
                    # The middle of the card excludes the animated ring at
                    # x > 290 and the Settings window behind the left edge.
                    return sum(pixels[y * stride + x * 4 + alpha] > 200
                               for y in range(height) for x in range(100, 220))

                def folded():
                    rects = display.regions(window)
                    return rects if len(rects) == 1 and 0 < rects[0][2] <= 80 else None

                def check_folded_pixels(label):
                    path = output_dir / (label + '.png') if output_dir else None
                    pixels, stride, alpha = display.capture(window, width, height, path)
                    # The resting pill is 10 x 79 CSS px at the right edge;
                    # allow one antialiasing pixel, but no provider-ring ghost.
                    occupied = [(x, y) for y in range(height) for x in range(80, width)
                                if pixels[y * stride + x * 4 + alpha] > 32]
                    assert len(occupied) > 100, 'Resting pill disappeared'
                    assert all(x >= width - 11 and abs(y - height / 2) <= 41 for x, y in occupied), \
                        'Pixels remained outside the resting pill after folding'

                initial = until(folded, 'The notch did not fold on startup')
                # Inside the transparent window, outside the wake strip: input
                # must reach the window underneath, not be eaten by Codenotch.
                display.move(x + 100, y + height // 2)
                assert display.window_under_pointer() != window, 'Transparent area intercepted the pointer'
                for attempt in range(2):
                    display.move(x + width - 5, y + height // 2)
                    assert display.window_under_pointer() == window, 'The wake strip did not receive input'
                    until(lambda: any(r[2] > 150 for r in display.regions(window)),
                          f'Hover {attempt + 1} did not open the notch and card')
                    # Hold across multiple animation frames. A correct input
                    # region alone does not prove that the card stays visible.
                    counts = []
                    for frame in range(3):
                        time.sleep(.35)
                        assert display.window_under_pointer() == window, 'Pointer left the notch'
                        assert any(r[2] > 150 for r in display.regions(window)), 'Notch folded during hover'
                        counts.append(card_pixels(f'hover-{attempt + 1}-{frame + 1}'))
                    assert min(counts) > 1500, f'Card disappeared while hovering: {counts}'
                    assert min(counts) >= max(counts) * .95, f'Card flickered while hovering: {counts}'
                    display.move(100, 100)
                    until(folded, f'Leave {attempt + 1} did not fold the notch')
                    assert display.regions(window) == initial, 'The wake region was not restored'
                    time.sleep(.7)  # wait for the 360 ms fold and outline fade
                    check_folded_pixels(f'folded-{attempt + 1}')
                assert app.wait(timeout=10) == 0, 'Smoke test failed'
                log.seek(0)
                output = log.read()
                assert 'Failed to create GBM buffer' not in output, output
                app_log = Path(tmp+'/config/codenotch/run.log').read_text()
                assert 'Could not update' not in app_log, app_log
                print('PASS: two held hover/leave cycles, stable native card pixels, clean folding, transparent input, no GBM errors')
        except Exception:
            startup = Path(tmp) / 'startup.log'
            if startup.exists():
                print(startup.read_text())
            app_log = Path(tmp) / 'config/codenotch/run.log'
            if app_log.exists():
                print(app_log.read_text())
            raise
        finally:
            if app is not None and app.poll() is None:
                app.terminate()
                app.wait(timeout=5)
            if display is not None:
                display.close(display.handle)
            xvfb.terminate()
            xvfb.wait(timeout=5)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path(__file__).resolve().parents[1] / 'windows/target/release/codenotch')
    parser.add_argument('--output-dir', type=Path, help='Optionally keep native window PNGs')
    args = parser.parse_args()
    if args.output_dir:
        args.output_dir.mkdir(parents=True, exist_ok=True)
    verify(args.binary.resolve(), args.output_dir)
