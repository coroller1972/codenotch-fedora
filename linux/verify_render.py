#!/usr/bin/env python3
"""Render the actual notch HTML in WebKitGTK with synthetic Codex activity.

Run with xvfb-run -a dbus-run-session -- python3 linux/verify_render.py.
Requires python3-gobject and python3-cairo. No accounts or network are used.
"""
import argparse
import json
import math
import os
from pathlib import Path
import sys
import tempfile
import time

# Match the application's renderer without inheriting the user's WebKit data.
os.environ['GDK_BACKEND'] = 'x11'
os.environ.setdefault('WEBKIT_DMABUF_RENDERER_FORCE_SHM', '1')
import gi
gi.require_version('Gtk', '3.0')
gi.require_version('WebKit2', '4.1')
from gi.repository import Gtk, Gdk, GLib, WebKit2

BRIDGE = '''<script>
window.events={};
window.emitFixture=(name,payload)=>(events[name]||[]).forEach(cb=>cb({payload}));
const fixtureSnap={status:'ok',windows:[{id:'primary',label:'5 hours',used:.25,reset_at:0}],fetched_at:Date.now(),note:''};
window.__TAURI__={event:{listen:async(n,cb)=>{(events[n]??=[]).push(cb);return ()=>{};}},core:{invoke:async(n,a)=>{
const values={get_theme_resolved:'light',get_notch_insets:[0,0,0,0],get_ui_flags:{notch_on_hover:true},
get_notch_slots:[{provider:'codex'}],get_notch_edge:'right',get_codex:fixtureSnap,
get_usage:{status:'needsAuth',windows:[],fetched_at:0,note:''},get_state:{sessions:[],lang_resolved:'en',clock_24h:true},
get_activity:[{provider:'codex',state:'busy',name:'Synthetic task',detail:'Working',since:0}],get_glyphs:{},get_weekly_ring:'off'};
return values[n]??null;}}};
</script>'''


def pump(seconds):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        while GLib.MainContext.default().pending():
            GLib.MainContext.default().iteration(False)
        time.sleep(.01)


def completed(results):
    end = time.monotonic() + 8
    while not results and time.monotonic() < end:
        pump(.01)
    assert results, 'WebKit callback timed out'
    if isinstance(results[0], Exception):
        raise results[0]
    return results[0]


def verify(output_dir):
    window = Gtk.Window()
    window.set_default_size(360, 650)
    window.set_decorated(False)
    window.set_app_paintable(True)
    window.set_visual(window.get_screen().get_rgba_visual())
    view = WebKit2.WebView()
    view.set_background_color(Gdk.RGBA(0, 0, 0, 0))
    window.add(view)
    window.show_all()

    def evaluate(script):
        results = []
        def done(view, result, _):
            try:
                results.append(json.loads(view.evaluate_javascript_finish(result).to_json(0)))
            except Exception as error:
                results.append(error)
        view.evaluate_javascript(script, -1, None, None, None, done, None)
        return completed(results)

    def snapshot(name):
        results = []
        def done(view, result, _):
            try:
                results.append(view.get_snapshot_finish(result))
            except Exception as error:
                results.append(error)
        view.get_snapshot(WebKit2.SnapshotRegion.VISIBLE, WebKit2.SnapshotOptions.TRANSPARENT_BACKGROUND, None, done, None)
        surface = completed(results)
        if output_dir:
            surface.write_to_png(str(output_dir / (name + '.png')))
        return surface

    def check_folded(name):
        state = evaluate("({folded,nodes:pill.querySelectorAll('svg.activity>*').length,running:document.getAnimations().filter(a=>a.playState==='running').length,rest:document.getElementById('rest').getBoundingClientRect().toJSON()})")
        assert state['folded'], state
        assert state['nodes'] == 0 and state['running'] == 0, state
        # Actual painted pixels must stay inside the small resting pill. This
        # catches ghost provider rings/card/handles that a hit-test cannot see.
        surface = snapshot(name)
        data, stride = bytes(surface.get_data()), surface.get_stride()
        alpha_offset = 3 if sys.byteorder == 'little' else 0
        rest, pixels = state['rest'], 0
        for y in range(surface.get_height()):
            for x in range(surface.get_width()):
                if data[y * stride + 4 * x + alpha_offset] > 0:
                    pixels += 1
                    assert math.floor(rest['left']) - 1 <= x <= math.ceil(rest['right']), (name, x, y)
                    assert math.floor(rest['top']) - 1 <= y <= math.ceil(rest['bottom']), (name, x, y)
        assert pixels > 100, 'The resting pill was not painted'

    try:
        html = (Path(__file__).resolve().parents[1] / 'windows/codenotch/ui/notch.html').read_text()
        ready = []
        view.connect('load-changed', lambda _, event: ready.append(True) if event == WebKit2.LoadEvent.FINISHED else None)
        view.load_html(html.replace('<script>', BRIDGE + '<script>', 1), 'file:///tmp/')
        completed(ready)
        pump(1.3)
        check_folded('folded-busy')
        for phase, selector in [('busy', '.arc-spin'), ('waiting', '.arc-pulse')]:
            evaluate(f"emitFixture('activity',[{{provider:'codex',state:'{phase}',name:'Synthetic task',detail:'Test',since:0}}]);true")
            pump(.15)
            check_folded('folded-update-' + phase)
            evaluate("emitFixture('notch_pointer',true);document.dispatchEvent(new MouseEvent('mousemove',{clientX:330,clientY:325,bubbles:true}));true")
            pump(.8)
            assert evaluate(f"!folded && !!pill.querySelector('{selector}')"), phase
            snapshot('open-' + phase)
            # Refreshing quotas must not replace/restart the active animation.
            assert evaluate("window.previousArc=pill.querySelector('svg.activity').firstChild;renderRing();previousArc===pill.querySelector('svg.activity').firstChild")
            # Deliberately no DOM mouseout: this is what failed with animated
            # WebKit layers. The real app gets this event from native X11.
            evaluate("emitFixture('notch_pointer',false);true")
            pump(1.3)
            check_folded('folded-after-' + phase)
        print('PASS: Codex busy/waiting resume, no hidden animation, folded pixels confined to resting pill')
    finally:
        window.destroy()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output-dir', type=Path, help='Optionally keep the rendered PNGs')
    args = parser.parse_args()
    if args.output_dir:
        args.output_dir.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='codenotch-render-') as tmp:
        os.environ.update(XDG_CONFIG_HOME=tmp+'/config', XDG_CACHE_HOME=tmp+'/cache', XDG_DATA_HOME=tmp+'/data')
        verify(args.output_dir)
