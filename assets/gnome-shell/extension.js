// SPDX-License-Identifier: BSD-3-Clause
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Shell from 'gi://Shell';
import Meta from 'gi://Meta';
import Clutter from 'gi://Clutter';
import * as Dialog from 'resource:///org/gnome/shell/ui/dialog.js';
import * as ModalDialog from 'resource:///org/gnome/shell/ui/modalDialog.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const APP_ID = 'org.overink.OverInk';
const CANVAS = 'OverInk - canvas';
const TOOLBAR = 'OverInk';
const BUS_NAME = 'org.overink.Overlay';
const BUS_PATH = '/org/overink/Overlay';
const CAPTURE_INTERFACE = `<node>
    <interface name="org.overink.Overlay">
        <method name="CaptureArea">
            <arg type="s" direction="in" name="requestId"/>
            <arg type="d" direction="in" name="left"/>
            <arg type="d" direction="in" name="top"/>
            <arg type="d" direction="in" name="right"/>
            <arg type="d" direction="in" name="bottom"/>
            <arg type="d" direction="in" name="canvasWidth"/>
            <arg type="d" direction="in" name="canvasHeight"/>
            <arg type="b" direction="in" name="hasPosition"/>
            <arg type="d" direction="in" name="canvasX"/>
            <arg type="d" direction="in" name="canvasY"/>
            <arg type="s" direction="out" name="pngPath"/>
        </method>
        <method name="CancelCapture">
            <arg type="s" direction="in" name="requestId"/>
        </method>
    </interface>
</node>`;

// The bridge accepts requests only from the PID of an existing OverInk canvas.
// GNOME's compositor knows its global rectangle, even on native Wayland.
class CaptureBridge {
    constructor(extension) {
        this._extension = extension;
        this._busy = false;
        this._active = true;
        // Approval is ephemeral: one explicit Shell confirmation per live PID
        // and /proc start time, never a persistent "trusted app name".
        this._approved = new Set();
        this._deniedUntil = new Map();
        this._pending = null;
    }

    stop() {
        this._active = false;
        this._approved.clear();
        this._deniedUntil.clear();
        this._cancel(this._pending);
    }

    _cancel(state) {
        if (!state || state.cancelled)
            return;
        state.cancelled = true;
        state.dismissDialog?.();
        state.cancelAfterPaint?.();
    }

    _checkActive(state) {
        if (!this._active || state.cancelled)
            throw new Error('Screenshot capture was cancelled');
    }

    CancelCaptureAsync([requestId], invocation) {
        const state = this._pending;
        if (state?.sender === invocation.get_sender() && state.requestId === requestId)
            this._cancel(state);
        // Only the exact calling D-Bus connection can cancel its request.
        invocation.return_value(new GLib.Variant('()', []));
    }

    _processIdentity(pid, executable) {
        // PID reuse must not inherit an earlier process's approval.
        // /proc/[pid]/stat field 22 is starttime, after the parenthesized comm.
        const [, bytes] = GLib.file_get_contents(`/proc/${pid}/stat`);
        const stat = new TextDecoder().decode(bytes);
        const close = stat.lastIndexOf(')');
        if (close < 0)
            throw new Error('Cannot identify screenshot process');
        const fields = stat.slice(close + 1).trim().split(/\s+/);
        if (!/^\d+$/.test(fields[19] ?? ''))
            throw new Error('Cannot determine screenshot process start time');
        return `${pid}:${fields[19]}:${executable}`;
    }

    _requestPermission(executable, state) {
        return new Promise(resolve => {
            // The user approves via a GNOME Shell modal; the caller cannot
            // forge approval by setting a window title or invoking D-Bus.
            const dialog = new ModalDialog.ModalDialog({destroyOnClose: false});
            dialog.contentLayout.add_child(new Dialog.MessageDialogContent({
                title: 'Allow OverInk screen capture?',
                description: `A process at ${executable} requests access to screen pixels. Allow screenshots for this running process only? Your OverInk rectangle selection stays unchanged.`,
            }));
            let settled = false;
            const finish = allowed => {
                if (settled)
                    return;
                settled = true;
                state.dismissDialog = null;
                resolve(allowed && this._active && !state.cancelled);
                if (dialog.state === ModalDialog.State.OPENED ||
                    dialog.state === ModalDialog.State.OPENING)
                    dialog.close(global.get_current_time());
                else
                    dialog.destroy();
            };
            state.dismissDialog = () => finish(false);
            dialog.connect('closed', () => {
                if (!settled) {
                    settled = true;
                    state.dismissDialog = null;
                    resolve(false);
                }
                dialog.destroy();
            });
            dialog.setButtons([
                {label: 'Deny', key: Clutter.KEY_Escape, isDefault: true,
                    action: () => finish(false)},
                {label: 'Allow for this run', action: () => finish(true)},
            ]);
            if (!dialog.open(global.get_current_time()))
                finish(false);
        });
    }

    CaptureAreaAsync(parameters, invocation) {
        this._capture(parameters, invocation).catch(error => {
            logError(error, 'OverInk capture');
            invocation.return_dbus_error('org.overink.Overlay.CaptureFailed', String(error));
        });
    }

    async _capture([requestId, left, top, right, bottom, width, height,
        hasPosition, canvasX, canvasY], invocation) {
        if (this._busy)
            throw new Error('Another screenshot is still running');
        this._busy = true;
        const state = {
            sender: invocation.get_sender(),
            requestId,
            cancelled: false,
            dismissDialog: null,
            cancelAfterPaint: null,
        };
        this._pending = state;
        let file = null;
        let io = null;
        let completed = false;
        const hiddenActors = [];
        try {
            this._checkActive(state);
            if (!this._extension._windows)
                throw new Error('OverInk Overlay is disabled');
            if (typeof requestId !== 'string' || !/^[a-zA-Z0-9_-]{1,80}$/.test(requestId))
                throw new Error('Invalid screenshot request ID');
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
            this._checkActive(state);
            // Window title/class alone can be forged by another application.
            // The D-Bus caller must also be a local process running OverInk.
            const executable = GLib.file_read_link(`/proc/${pid}/exe`);
            if (GLib.path_get_basename(executable) !== 'overink')
                throw new Error('The screenshot caller is not OverInk');
            const processIdentity = this._processIdentity(pid, executable);
            const actors = global.get_window_actors();
            const mine = actors.filter(actor => {
                const window = actor.meta_window;
                const name = window?.get_wm_class()?.toLowerCase();
                return window?.get_pid() === pid &&
                    (name === APP_ID.toLowerCase() || name === 'overink');
            });
            const canvases = mine.filter(actor =>
                actor.meta_window.get_title() === CANVAS);
            const canvas = canvases.length === 1 ? canvases[0] :
                (hasPosition ? canvases.find(actor => {
                    const bounds = actor.meta_window.get_frame_rect();
                    return Math.abs(bounds.x - canvasX) <= 4 &&
                        Math.abs(bounds.y - canvasY) <= 4;
                }) : null);
            if (!canvas)
                throw new Error('Cannot identify the selected OverInk canvas');
            // The canvas may legitimately lose keyboard focus while OverInk
            // hides its toolbar and the Shell consent dialog is displayed.
            // Do not treat compositor keyboard focus as screenshot authority.
            const now = GLib.get_monotonic_time();
            if ((this._deniedUntil.get(processIdentity) ?? 0) > now)
                throw new Error('Screen capture denied recently; retry in 30 seconds');
            if (!this._approved.has(processIdentity)) {
                if (!await this._requestPermission(executable, state)) {
                    this._checkActive(state);
                    // Rate-limit repeated permission prompts from an impostor.
                    if (this._deniedUntil.size > 32) {
                        for (const [key, deadline] of this._deniedUntil) {
                            if (deadline <= now)
                                this._deniedUntil.delete(key);
                        }
                        if (this._deniedUntil.size > 32)
                            this._deniedUntil.clear();
                    }
                    this._deniedUntil.set(processIdentity, GLib.get_monotonic_time() + 30_000_000);
                    throw new Error('Screen capture was not authorized');
                }
                this._checkActive(state);
                // Recheck lifetime after the modal may have been open for a while.
                if (this._processIdentity(pid, executable) !== processIdentity ||
                    !global.get_window_actors().includes(canvas))
                    throw new Error('OverInk changed while authorizing screen capture');
                this._approved.add(processIdentity);
            }
            this._checkActive(state);
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
                let done = false;
                const signal = global.stage.connect('after-paint', () => {
                    if (done)
                        return;
                    done = true;
                    global.stage.disconnect(signal);
                    state.cancelAfterPaint = null;
                    resolve();
                });
                state.cancelAfterPaint = () => {
                    if (done)
                        return;
                    done = true;
                    global.stage.disconnect(signal);
                    state.cancelAfterPaint = null;
                    resolve();
                };
                global.stage.queue_redraw();
            });
            this._checkActive(state);
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
            this._checkActive(state);
            const size = file.query_info('standard::size',
                Gio.FileQueryInfoFlags.NONE, null).get_size();
            if (size === 0 || size > 128 * 1024 * 1024)
                throw new Error('Screenshot PNG size is unsupported');
            this._checkActive(state);
            invocation.return_value(new GLib.Variant('(s)', [file.get_path()]));
            completed = true;
        } finally {
            state.cancelAfterPaint?.();
            state.dismissDialog?.();
            for (const [actor, opacity] of hiddenActors)
                actor.opacity = opacity;
            if (io)
                io.close(null);
            if (!completed && file)
                file.delete(null);
            if (this._pending === state)
                this._pending = null;
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
        this._bridge?.stop();
        this._dbus?.unexport();
        this._dbus = null;
        Gio.bus_unown_name(this._busOwner);
        this._busOwner = 0;
        this._bridge = null;
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
