// SPDX-License-Identifier: BSD-3-Clause
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Shell from 'gi://Shell';
import Meta from 'gi://Meta';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const APP_ID = 'org.overink.OverInk';
const CANVAS = 'OverInk - canvas';
const TOOLBAR = 'OverInk';
const BUS_NAME = 'org.overink.Overlay';
const BUS_PATH = '/org/overink/Overlay';
const CAPTURE_INTERFACE = `<node>
    <interface name="org.overink.Overlay">
        <method name="CaptureArea">
            <arg type="d" direction="in" name="left"/>
            <arg type="d" direction="in" name="top"/>
            <arg type="d" direction="in" name="right"/>
            <arg type="d" direction="in" name="bottom"/>
            <arg type="d" direction="in" name="canvasWidth"/>
            <arg type="d" direction="in" name="canvasHeight"/>
            <arg type="s" direction="out" name="pngPath"/>
        </method>
    </interface>
</node>`;

// The bridge accepts requests only from the PID of an existing OverInk canvas.
// GNOME's compositor knows its global rectangle, even on native Wayland.
class CaptureBridge {
    constructor(extension) {
        this._extension = extension;
        this._busy = false;
    }

    CaptureAreaAsync(parameters, invocation) {
        this._capture(parameters, invocation).catch(error => {
            logError(error, 'OverInk capture');
            invocation.return_dbus_error('org.overink.Overlay.CaptureFailed', String(error));
        });
    }

    async _capture([left, top, right, bottom, width, height], invocation) {
        if (this._busy)
            throw new Error('Another screenshot is still running');
        this._busy = true;
        let file = null;
        let io = null;
        let completed = false;
        const hiddenActors = [];
        try {
            if (!this._extension._windows)
                throw new Error('OverInk Overlay is disabled');
            const values = [left, top, right, bottom, width, height];
            if (!values.every(Number.isFinite) || width <= 0 || height <= 0 ||
                left < 0 || top < 0 || right <= left || bottom <= top ||
                right > width || bottom > height)
                throw new Error('Invalid screenshot selection');
            const reply = await Gio.DBus.session.call(
                'org.freedesktop.DBus', '/org/freedesktop/DBus',
                'org.freedesktop.DBus', 'GetConnectionUnixProcessID',
                new GLib.Variant('(s)', [invocation.get_sender()]),
                new GLib.VariantType('(u)'), Gio.DBusCallFlags.NONE, 2000, null);
            const [pid] = reply.deepUnpack();
            const actors = global.get_window_actors();
            const mine = actors.filter(actor => {
                const window = actor.meta_window;
                const name = window?.get_wm_class()?.toLowerCase();
                return window?.get_pid() === pid &&
                    (name === APP_ID.toLowerCase() || name === 'overink');
            });
            const canvas = mine.find(actor => actor.meta_window.get_title() === CANVAS);
            if (!canvas)
                throw new Error('No OverInk canvas belongs to the requesting process');
            const frame = canvas.meta_window.get_frame_rect();
            if (frame.width <= 0 || frame.height <= 0)
                throw new Error('The screenshot canvas has no visible frame');

            const x1 = Math.floor(frame.x + left * frame.width / width);
            const y1 = Math.floor(frame.y + top * frame.height / height);
            const x2 = Math.ceil(frame.x + right * frame.width / width);
            const y2 = Math.ceil(frame.y + bottom * frame.height / height);
            if (x2 <= x1 || y2 <= y1 || (x2 - x1) * (y2 - y1) > 100_000_000)
                throw new Error('Invalid compositor screenshot bounds');

            // Suppress OverInk's annotation and toolbar actors for one frame,
            // without changing the user's own selection interaction.
            for (const actor of mine) {
                hiddenActors.push([actor, actor.opacity]);
                actor.opacity = 0;
            }
            await new Promise(resolve => {
                const signal = global.stage.connect('after-paint', () => {
                    global.stage.disconnect(signal);
                    resolve();
                });
                global.stage.queue_redraw();
            });
            // g_file_new_tmp creates a private temporary file (mode 0600).
            [file, io] = Gio.File.new_tmp('overink-capture-XXXXXX');
            const screenshot = new Shell.Screenshot();
            await new Promise((resolve, reject) => {
                screenshot.screenshot_area(
                    x1, y1, x2 - x1, y2 - y1, io.get_output_stream(),
                    (source, result) => {
                        try {
                            const finished = source.screenshot_area_finish(result);
                            const ok = Array.isArray(finished) ? finished[0] : finished;
                            if (!ok)
                                throw new Error('GNOME Shell failed to capture the selected region');
                            resolve();
                        } catch (error) {
                            reject(error);
                        }
                    });
            });
            io.close(null);
            io = null;
            const size = file.query_info('standard::size',
                Gio.FileQueryInfoFlags.NONE, null).get_size();
            if (size === 0 || size > 128 * 1024 * 1024)
                throw new Error('Screenshot PNG size is unsupported');
            invocation.return_value(new GLib.Variant('(s)', [file.get_path()]));
            completed = true;
        } finally {
            for (const [actor, opacity] of hiddenActors)
                actor.opacity = opacity;
            if (io)
                io.close(null);
            if (!completed && file)
                file.delete(null);
            this._busy = false;
        }
    }
}


export default class OverInkOverlay extends Extension {
    enable() {
        this._windows = new Map();
        this._bridge = new CaptureBridge(this);
        this._busOwner = Gio.bus_own_name(
            Gio.BusType.SESSION,
            BUS_NAME,
            Gio.BusNameOwnerFlags.NONE,
            connection => {
                this._dbus = Gio.DBusExportedObject.wrapJSObject(
                    CAPTURE_INTERFACE, this._bridge);
                this._dbus.export(connection, BUS_PATH);
            },
            () => {},
            () => log('OverInk capture bridge could not own its D-Bus name'));

        this._layout = 0;
        this._map = global.window_manager.connect('map', (_wm, actor) => {
            this._watch(actor.meta_window);
        });
        this._workareas = global.display.connect('workareas-changed', () => this._queueLayout());
        this._monitor = global.display.connect('window-entered-monitor', (_display, _monitor, window) => {
            if (this._windows.has(window))
                this._queueLayout();
        });
        for (const actor of global.get_window_actors())
            this._watch(actor.meta_window);
    }

    disable() {
        this._dbus?.unexport();
        this._dbus = null;
        Gio.bus_unown_name(this._busOwner);
        this._busOwner = 0;
        global.window_manager.disconnect(this._map);
        global.display.disconnect(this._workareas);
        global.display.disconnect(this._monitor);
        if (this._layout) {
            GLib.Source.remove(this._layout);
            this._layout = 0;
        }
        for (const [window, state] of this._windows) {
            this._unwatch(window);
            if (state.aboveAdded)
                window.unmake_above();
        }
    }

    _watch(window) {
        if (!window || window.get_client_type() !== Meta.WindowClientType.WAYLAND ||
            window.get_wm_class() !== APP_ID)
            return;

        const title = window.get_title();
        if (title !== CANVAS && title !== TOOLBAR)
            return;
        if (this._windows.has(window)) {
            this._queueLayout();
            return;
        }

        const state = {canvas: title === CANVAS, placement: null, aboveAdded: false, lastResize: null, signals: []};
        this._windows.set(window, state);
        state.signals.push(window.connect('unmanaged', () => this._unwatch(window)));
        if (state.canvas) {
            for (const axis of ['horizontally', 'vertically']) {
                state.signals.push(window.connect(`notify::maximized-${axis}`,
                    () => this._queueLayout()));
            }
            state.signals.push(window.connect('position-changed', () => this._queueLayout()));
        }
        state.signals.push(window.connect('size-changed', () => this._queueLayout()));
        this._queueLayout();
    }

    _unwatch(window) {
        const state = this._windows.get(window);
        for (const signal of state.signals)
            window.disconnect(signal);
        this._windows.delete(window);
    }

    _queueLayout() {
        if (this._layout)
            return;
        this._layout = GLib.idle_add(GLib.PRIORITY_DEFAULT_IDLE, () => {
            this._layout = 0;
            for (const [window, state] of this._windows) {
                if (state.canvas)
                    this._place(window, state);
            }
            for (const [window, state] of this._windows) {
                if (!state.canvas)
                    this._place(window, state);
            }
            return GLib.SOURCE_REMOVE;
        });
    }

    _place(window, state) {
        const parent = state.canvas ? null : window.get_transient_for();
        const monitor = (parent ?? window).get_monitor();
        if (monitor < 0)
            return;
        const area = window.get_work_area_for_monitor(monitor);
        if (area.width <= 0 || area.height <= 0)
            return;

        if (state.canvas) {
            // Mutter excludes maximized windows from its above layer.
            // Fit a normal toplevel to the work area to keep the desktop visible.
            if (window.maximized_horizontally || window.maximized_vertically) {
                if (typeof window.set_unmaximize_flags === 'function')
                    window.set_unmaximize_flags(Meta.MaximizeFlags.BOTH);
                else
                    window.unmaximize(Meta.MaximizeFlags.BOTH);
            }
            const frame = window.get_frame_rect();
            if (frame.x === area.x && frame.y === area.y &&
                frame.width === area.width && frame.height === area.height) {
                state.lastResize = null;
            } else {
                // The unmaximize configure can be acknowledged later. Refit
                // on that resize, without resending an unchanged constraint.
                const key = [frame.x, frame.y, frame.width, frame.height,
                    area.x, area.y, area.width, area.height].join(':');
                if (key !== state.lastResize) {
                    state.lastResize = key;
                    window.move_resize_frame(false, area.x, area.y, area.width, area.height);
                }
            }
        } else {
            const frame = window.get_frame_rect();
            if (frame.height > 0) {
                const y = area.y + Math.max(0, Math.floor((area.height - frame.height) / 2));
                const placement = [monitor, area.x, y].join(':');
                if (placement !== state.placement) {
                    state.placement = placement;
                    if (frame.x !== area.x || frame.y !== y)
                        window.move_frame(false, area.x, y);
                }
            }
        }

        if (!window.is_above()) {
            window.make_above();
            state.aboveAdded = true;
        }
    }
}
