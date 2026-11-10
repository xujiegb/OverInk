// SPDX-License-Identifier: BSD-3-Clause
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const APP_ID = 'org.overink.OverInk';
const CANVAS = 'OverInk - canvas';
const TOOLBAR = 'OverInk';

export default class OverInkOverlay extends Extension {
    enable() {
        this._windows = new Map();
        this._layout = 0;
        this._map = global.window_manager.connect('map', (_wm, actor) => {
            this._watch(actor.meta_window);
        });
        this._workareas = global.display.connect('workareas-changed', () => this._queueLayout());
        for (const actor of global.get_window_actors())
            this._watch(actor.meta_window);
    }

    disable() {
        global.window_manager.disconnect(this._map);
        global.display.disconnect(this._workareas);
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

        const state = {canvas: title === CANVAS, placed: false, aboveAdded: false, lastResize: null, signals: []};
        this._windows.set(window, state);
        state.signals.push(window.connect('unmanaged', () => this._unwatch(window)));
        if (state.canvas) {
            for (const axis of ['horizontally', 'vertically']) {
                state.signals.push(window.connect(`notify::maximized-${axis}`,
                    () => this._queueLayout()));
            }
            for (const signal of ['position-changed', 'size-changed'])
                state.signals.push(window.connect(signal, () => this._queueLayout()));
        }
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
        } else if (!state.placed) {
            const frame = window.get_frame_rect();
            if (frame.height > 0) {
                const y = area.y + Math.max(0, Math.floor((area.height - frame.height) / 2));
                window.move_frame(false, area.x, y);
                state.placed = true;
            }
        }

        if (!window.is_above()) {
            window.make_above();
            state.aboveAdded = true;
        }
    }
}
