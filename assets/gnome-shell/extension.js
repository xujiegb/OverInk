// SPDX-License-Identifier: BSD-3-Clause
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Shell from 'gi://Shell';
import {getInputSourceManager} from 'resource:///org/gnome/shell/ui/status/keyboard.js';
import Meta from 'gi://Meta';
import Clutter from 'gi://Clutter';
import * as Dialog from 'resource:///org/gnome/shell/ui/dialog.js';
import * as ModalDialog from 'resource:///org/gnome/shell/ui/modalDialog.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const APP_ID = 'org.overink.OverInk';
const MAX_TRUST_KEY_LENGTH = 2048;
const MAX_EXECUTABLE_BYTES = 256 * 1024 * 1024;
const CANVAS = 'OverInk - canvas';
const TOOLBAR = 'OverInk';
const BUS_NAME = 'org.overink.Overlay';
const BUS_PATH = '/org/overink/Overlay';
const TRUST_PATH = GLib.build_filenamev([
    GLib.get_user_config_dir(), 'overink', 'capture-trust.json',
]);
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
        <method name="NextInputSource">
            <arg type="b" direction="out" name="switched"/>
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
        // Per-run grants are bound to the PID/start time, not a forgeable name.
        this._approved = new Set();
        // Persistent grants are opt-in and pinned to a root-owned, non-writable
        // executable and its metadata; replacing/upgrading it requires consent.
        this._trusted = this._loadTrusted();
        this._deniedUntil = new Map();
        this._pending = null;
    }

    stop() {
        this._active = false;
        this._approved.clear();
        this._trusted.clear();
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

    _loadTrusted() {
        try {
            const [, data] = Gio.File.new_for_path(TRUST_PATH).load_contents(null);
            const entries = JSON.parse(new TextDecoder().decode(data));
            if (!Array.isArray(entries))
                return new Set();
            return new Set(entries.filter(entry =>
                typeof entry === 'string' && entry.length <= MAX_TRUST_KEY_LENGTH).slice(-32));
        } catch (error) {
            // Missing or corrupt permission state must fail closed: re-prompt.
            return new Set();
        }
    }

    _rememberTrusted(key) {
        // The writer and reader share the same JavaScript string-length limit.
        // The normal caller already checks this before offering persistence.
        if (typeof key !== 'string' || key.length > MAX_TRUST_KEY_LENGTH)
            throw new Error('Screenshot permission identity is too long');
        const folder = GLib.path_get_dirname(TRUST_PATH);
        if (GLib.mkdir_with_parents(folder, 0o700) !== 0)
            throw new Error('Cannot create OverInk permission directory');
        const next = new Set(this._trusted);
        next.delete(key);
        next.add(key);
        while (next.size > 32)
            next.delete(next.values().next().value);
        const data = new TextEncoder().encode(JSON.stringify([...next]));
        // Atomic replace; PRIVATE creates files with mode 0600.
        Gio.File.new_for_path(TRUST_PATH).replace_contents(
            data, null, false, Gio.FileCreateFlags.PRIVATE, null);
        this._trusted = next;
    }

    _binaryDigest(executable, installedSize) {
        // Hash the verified root-owned executable in bounded chunks. Unlike
        // st_dev/inode, its SHA-256 identity survives Btrfs and OSTree remounts.
        const checksum = GLib.Checksum.new(GLib.ChecksumType.SHA256);
        if (!checksum)
            return null;
        const stream = Gio.File.new_for_path(executable).read(null);
        let readSize = 0;
        try {
            while (true) {
                const chunk = stream.read_bytes(64 * 1024, null);
                const size = chunk.get_size();
                if (size === 0)
                    break;
                readSize += size;
                // A root update can grow the file after its initial stat.
                // Bound actual I/O rather than trusting only that first size.
                if (readSize > MAX_EXECUTABLE_BYTES || readSize > installedSize)
                    return null;
                checksum.update(chunk.get_data());
            }
            return readSize === installedSize ? checksum.get_string() : null;
        } finally {
            stream.close(null);
        }
    }

    _persistentIdentity(pid, executable) {
        // Validate /proc/<pid>/exe against the root-owned path in this mount
        // namespace using device AND inode. These live-process checks must
        // remain even though the persistent key is content-based: trusting a
        // path or a hash alone would let another mount namespace impersonate
        // the installed executable. An upgrade requires new consent.
        if (!GLib.path_is_absolute(executable) || executable.endsWith(' (deleted)'))
            return null;
        try {
            const running = Gio.File.new_for_path(`/proc/${pid}/exe`).query_info(
                'unix::device,unix::inode', Gio.FileQueryInfoFlags.NONE, null);
            const runningDevice = String(running.get_attribute_uint32('unix::device'));
            const runningInode = String(running.get_attribute_uint64('unix::inode'));
            let path = executable;
            let installedSize = null;
            while (true) {
                const info = Gio.File.new_for_path(path).query_info(
                    'unix::uid,unix::mode,unix::device,unix::inode,standard::size,time::modified',
                    Gio.FileQueryInfoFlags.NOFOLLOW_SYMLINKS, null);
                const mode = info.get_attribute_uint32('unix::mode');
                if (info.get_attribute_uint32('unix::uid') !== 0 ||
                    (mode & 0o022) !== 0)
                    return null;
                if (path === executable) {
                    if ((mode & 0o170000) !== 0o100000 ||
                        String(info.get_attribute_uint32('unix::device')) !== runningDevice ||
                        String(info.get_attribute_uint64('unix::inode')) !== runningInode)
                        return null;
                    installedSize = Number(info.get_size());
                    // GJS may expose 64-bit sizes as Number or BigInt. Keep
                    // the digest's byte counter and initial size comparable.
                    // Reject oversized values before reading any file bytes.
                    if (!Number.isSafeInteger(installedSize) ||
                        installedSize <= 0 || installedSize > MAX_EXECUTABLE_BYTES)
                        return null;
                } else if ((mode & 0o170000) !== 0o040000) {
                    return null;
                }
                if (path === '/')
                    break;
                path = GLib.path_get_dirname(path);
            }
            const digest = this._binaryDigest(executable, installedSize);
            if (!digest)
                return null;
            // Root-managed updates can replace a file between the initial
            // stat and read. Accept only the same running executable inode.
            const after = Gio.File.new_for_path(executable).query_info(
                'unix::device,unix::inode,standard::size',
                Gio.FileQueryInfoFlags.NOFOLLOW_SYMLINKS, null);
            if (String(after.get_attribute_uint32('unix::device')) !== runningDevice ||
                String(after.get_attribute_uint64('unix::inode')) !== runningInode ||
                Number(after.get_size()) !== installedSize)
                return null;
            // Version the key deliberately: old st_dev/inode grants require
            // one fresh opt-in, and do not silently migrate to content trust.
            const identity = `v2:${executable}:sha256:${digest}`;
            return identity.length <= MAX_TRUST_KEY_LENGTH ? identity : null;
        } catch (error) {
            return null;
        }
    }

    _requestPermission(executable, state, canRemember) {
        return new Promise(resolve => {
            // Only GNOME Shell can present this modal. A successful capture
            // must wait for the modal to finish closing, or the first PNG
            // can contain a fading permission prompt.
            const dialog = new ModalDialog.ModalDialog({destroyOnClose: false});
            dialog.contentLayout.add_child(new Dialog.MessageDialogContent({
                title: 'Allow OverInk screen capture?',
                description: canRemember
                    ? `A process at ${executable} requests screen pixels. Allow this run, or remember this verified installation? Your selected rectangle stays unchanged.`
                    : `A process at ${executable} requests screen pixels. Allow this running process only? Your selected rectangle stays unchanged.`,
            }));
            let settled = false;
            let closing = false;
            let decision = null;
            const settle = () => {
                if (settled)
                    return;
                settled = true;
                state.dismissDialog = null;
                resolve(this._active && !state.cancelled ? decision : null);
            };
            const close = choice => {
                if (closing) {
                    if (state.cancelled) {
                        decision = null;
                        settle();
                    }
                    return;
                }
                closing = true;
                decision = !state.cancelled ? choice : null;
                if (state.cancelled)
                    settle();
                if (dialog.state === ModalDialog.State.OPENED ||
                    dialog.state === ModalDialog.State.OPENING) {
                    dialog.close(global.get_current_time());
                } else {
                    settle();
                    dialog.destroy();
                }
            };
            state.dismissDialog = () => close(null);
            dialog.connect('closed', () => {
                settle();
                dialog.destroy();
            });
            dialog.connect('destroy', () => settle());
            const buttons = [
                {label: 'Deny', key: Clutter.KEY_Escape, isDefault: true,
                    action: () => close(null)},
                {label: 'Allow for this run', action: () => close('run')},
            ];
            if (canRemember) {
                buttons.push({label: 'Always allow this installation',
                    action: () => close('always')});
            }
            dialog.setButtons(buttons);
            if (!dialog.open(global.get_current_time()))
                close(null);
        });
    }

    // GNOME Shell owns BOTH XKB layouts and IBus sources. Do not rewrite
    // GSettings "current" (ignored by modern Shell) or call setxkbmap on Wayland.
    // Restrict this D-Bus action to the process owning a live OverInk canvas.
    async NextInputSourceAsync(_parameters, invocation) {
        try {
            if (!this._active || !this._extension._windows)
                throw new Error('OverInk Overlay is not active');
            const reply = await Gio.DBus.session.call(
                'org.freedesktop.DBus', '/org/freedesktop/DBus',
                'org.freedesktop.DBus', 'GetConnectionUnixProcessID',
                new GLib.Variant('(s)', [invocation.get_sender()]),
                new GLib.VariantType('(u)'), Gio.DBusCallFlags.NONE, 2000, null);
            const [pid] = reply.deepUnpack();
            const exe = GLib.file_read_link(`/proc/${pid}/exe`);
            if (GLib.path_get_basename(exe) !== 'overink')
                throw new Error('Only OverInk can change its input source');
            const ownsCanvas = global.get_window_actors().some(actor => {
                const window = actor.meta_window;
                const name = window?.get_wm_class()?.toLowerCase();
                return window?.get_pid() === pid &&
                    window?.get_title() === CANVAS &&
                    (name === APP_ID.toLowerCase() || name === 'overink');
            });
            if (!ownsCanvas)
                throw new Error('The request has no matching OverInk canvas');
            const manager = getInputSourceManager();
            const sources = Object.values(manager.inputSources).filter(Boolean);
            if (sources.length < 2)
                throw new Error('Configure at least two GNOME input sources');
            const active = sources.indexOf(manager.currentSource);
            if (active < 0)
                throw new Error('Active source is not in GNOME input sources');
            sources[(active + 1) % sources.length].activate(true);
            invocation.return_value(new GLib.Variant('(b)', [true]));
        } catch (error) {
            invocation.return_dbus_error('org.overink.Overlay.InputSourceFailed', String(error));
        }
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
            const approved = this._approved.has(processIdentity);
            // Already authorized processes need not rehash their executable
            // on each frame; PID + start time prevents inheriting this grant.
            const trusted = approved ? null : this._persistentIdentity(pid, executable);
            // Re-read user permissions for each unapproved process. Deleting
            // capture-trust.json now revokes future approvals immediately.
            if (!approved)
                this._trusted = this._loadTrusted();
            if (!approved && !(trusted && this._trusted.has(trusted))) {
                const choice = await this._requestPermission(executable, state, !!trusted);
                if (!choice) {
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
                if (choice === 'always' && trusted) {
                    // A changed binary during the modal cannot inherit consent.
                    if (this._persistentIdentity(pid, executable) !== trusted)
                        throw new Error('OverInk executable changed during approval');
                    try {
                        this._rememberTrusted(trusted);
                    } catch (error) {
                        // 'Always' also grants this verified process access
                        // for the current run. A read-only configuration or
                        // full disk must not turn that consent into a failed
                        // screenshot; only the persistent grant is lost.
                        logError(error, 'OverInk could not remember screenshot permission; allowed for this run only');
                    }
                }
            }
            this._approved.add(processIdentity);
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
            // A destroyed actor or a failed stream close must never leave the
            // screenshot service permanently busy or a PNG file orphaned.
            try {
                state.cancelAfterPaint?.();
                state.dismissDialog?.();
                for (const [actor, opacity] of hiddenActors) {
                    try {
                        actor.opacity = opacity;
                    } catch (error) {
                        logError(error, 'OverInk restore screenshot actor');
                    }
                }
                if (io) {
                    try {
                        io.close(null);
                    } catch (error) {
                        logError(error, 'OverInk close screenshot stream');
                    }
                }
                if (!completed && file) {
                    try {
                        file.delete(null);
                    } catch (error) {
                        logError(error, 'OverInk delete cancelled PNG');
                    }
                }
            } finally {
                if (this._pending === state)
                    this._pending = null;
                this._busy = false;
            }
        }
    }
}


export default class OverInkOverlay extends Extension {
    enable() {
        this._windows = new Map();
        this._bridge = new CaptureBridge(this);
        const bridge = this._bridge;
        this._busOwner = Gio.bus_own_name(
            Gio.BusType.SESSION,
            BUS_NAME,
            Gio.BusNameOwnerFlags.NONE,
            connection => {
                // A previous bus-owner callback can arrive after disable and
                // even after a new enable has installed a different bridge.
                if (this._bridge !== bridge || !this._windows)
                    return;
                this._dbus = Gio.DBusExportedObject.wrapJSObject(
                    CAPTURE_INTERFACE, bridge);
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
        if (this._busOwner)
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
        this._windows = null;
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
        const state = this._windows?.get(window);
        if (!state)
            return;
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
