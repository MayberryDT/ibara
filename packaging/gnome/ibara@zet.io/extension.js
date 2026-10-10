import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import St from 'gi://St';
import Shell from 'gi://Shell';
import Clutter from 'gi://Clutter';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import { Extension } from 'resource:///org/gnome/shell/extensions/extension.js';

const XML = `<node><interface name="org.ibara.Gnome">
<method name="CursorBegin"><arg type="s" direction="in"/><arg type="s" direction="out"/></method>
<method name="CursorMove"><arg type="s" direction="in"/><arg type="d" direction="in"/><arg type="d" direction="in"/><arg type="b" direction="out"/></method>
<method name="CursorEnd"><arg type="s" direction="in"/><arg type="b" direction="out"/></method>
<method name="Snapshot"><arg type="s" direction="in"/><arg type="s" direction="out"/></method>
<method name="GetState"><arg type="s" direction="out"/></method>
<method name="Focus"><arg type="s" direction="in"/><arg type="b" direction="out"/></method>
<method name="Close"><arg type="s" direction="in"/><arg type="b" direction="out"/></method>
<method name="InputBegin"><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="s" direction="out"/></method>
<method name="InputKey"><arg type="s" direction="in"/><arg type="u" direction="in"/><arg type="b" direction="in"/><arg type="b" direction="out"/></method>
<method name="InputText"><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="b" direction="out"/></method>
<method name="InputMotion"><arg type="s" direction="in"/><arg type="d" direction="in"/><arg type="d" direction="in"/><arg type="b" direction="out"/></method>
<method name="InputButton"><arg type="s" direction="in"/><arg type="u" direction="in"/><arg type="b" direction="in"/><arg type="b" direction="out"/></method>
<method name="InputScroll"><arg type="s" direction="in"/><arg type="u" direction="in"/><arg type="b" direction="out"/></method>
<method name="InputEnd"><arg type="s" direction="in"/><arg type="s" direction="out"/></method>
<method name="InputState"><arg type="s" direction="in"/><arg type="s" direction="out"/></method>
<signal name="Changed"><arg type="s"/></signal>
</interface></node>`;

export default class IbaraIdentity extends Extension {
    enable() {
        this._epoch = GLib.uuid_string_random();
        this._sessionGeneration = 0;
        this._signals = [];
        this._windows = new Map();
        this._input = null;
        this._cursor = null;
        this._personAt = GLib.get_monotonic_time();
        this._personSignal = 0;
        if (typeof global.display.ibara_input_state === 'function' &&
            JSON.parse(global.display.ibara_input_state()).person_signal) {
            this._personSignal = global.display.connect('ibara-person-input', () => {
                this._personAt = GLib.get_monotonic_time();
                this._stopCursor();
            });
        }
        this._inputTimer = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 100, () => {
            if (this._locked()) this._stopCursor();
            if (this._input && (this._locked() || global.display.focus_window !== this._input.window))
                this._stopInput();
            return GLib.SOURCE_CONTINUE;
        });
        this._ownerWatch = Gio.DBus.session.signal_subscribe('org.freedesktop.DBus',
            'org.freedesktop.DBus', 'NameOwnerChanged', '/org/freedesktop/DBus', null,
            Gio.DBusSignalFlags.NONE, (_bus, _sender, _path, _iface, _signal, params) => {
                const [name, , owner] = params.deep_unpack();
                if (this._input?.owner === name && !owner) this._stopInput();
                if (this._cursor?.owner === name && !owner) this._stopCursor();
            });
        this._impl = Gio.DBusExportedObject.wrapJSObject(XML, this);
        this._impl.export(Gio.DBus.session, '/org/ibara/Gnome');
        this._name = Gio.bus_own_name_on_connection(Gio.DBus.session, 'org.ibara.Gnome', Gio.BusNameOwnerFlags.NONE, null, null);
        this._listen(global.display, 'window-created', (_, window) => { this._watchWindow(window); this._changed('windows'); });
        this._listen(global.display, 'notify::focus-window', () => this._changed('focus'));
        this._listen(Main.layoutManager, 'monitors-changed', () => this._changed('displays'));
        this._listen(Main.sessionMode, 'updated', () => this._changed('session'));
        for (const actor of global.get_window_actors()) this._watchWindow(actor.meta_window);
    }
    _listen(object, signal, callback) { this._signals.push([object, object.connect(signal, callback)]); }
    _watchWindow(window) {
        if (!window || this._windows.has(window)) return;
        const ids = ['notify::title', 'notify::minimized', 'size-changed', 'position-changed', 'workspace-changed']
            .map(signal => window.connect(signal, () => this._changed('windows')));
        ids.push(
            window.connect('unmanaged', () => { this._unwatchWindow(window); this._changed('windows'); }));
        this._windows.set(window, ids);
    }
    _unwatchWindow(window) {
        const ids = this._windows.get(window);
        if (ids) for (const id of ids) window.disconnect(id);
        this._windows.delete(window);
    }
    _changed(reason) {
        if (reason === 'session' || reason === 'displays') this._stopCursor();
        if (this._input && (reason === 'session' || reason === 'displays' ||
            global.display.focus_window !== this._input.window || !this._windows.has(this._input.window)))
            this._stopInput();
        if (reason === 'session' || reason === 'displays') this._sessionGeneration += 1;
        this._impl?.emit_signal('Changed', new GLib.Variant('(s)', [reason]));
    }
    _locked() { return Main.sessionMode.isLocked || Main.sessionMode.isGreeter; }
    _shellInputBlocked() {
        return typeof global.stage.get_grab_actor !== 'function' ||
            !!global.stage.get_grab_actor() || !!global.stage.get_key_focus();
    }
    GetState() {
        const shellInputBlocked = this._shellInputBlocked();
        const workspace = global.workspace_manager.get_active_workspace_index() + 1;
        const monitors = Main.layoutManager.monitors.map((monitor, index) => {
            const scale = global.display.get_monitor_scale(index);
            return { id: index, name: `gnome-${index}`, x: monitor.x, y: monitor.y,
                width: monitor.width * scale, height: monitor.height * scale, scale,
                activeWorkspace: { id: workspace, name: String(workspace) },
                solitaryBlockedBy: this._locked() ? ['LOCK'] : [],
                focused: Main.layoutManager.currentMonitor?.index === index };
        });
        const windows = global.display.sort_windows_by_stacking(global.get_window_actors().map(actor => actor.meta_window).filter(Boolean)).map(window => {
            const frame = window.get_frame_rect();
            const client = window.get_client_content_rect();
            return { address: `gnome:${window.get_stable_sequence()}`, pid: window.get_pid(),
                class: window.get_wm_class() || window.get_gtk_application_id() || '', title: window.get_title() || '',
                at: [frame.x, frame.y], size: [frame.width, frame.height], monitor: window.get_monitor(),
                clientRect: [client.x, client.y, client.width, client.height],
                workspace: { id: (window.get_workspace()?.index() ?? -1) + 1 },
                mapped: true, hidden: window.minimized || !window.showing_on_its_workspace(),
                visible: window.showing_on_its_workspace() && !window.minimized,
                acceptsInput: !this._locked() && !shellInputBlocked, focusHistoryID: window === global.display.focus_window ? 0 : 1 };
        });
        return JSON.stringify({ epoch: this._epoch, session_generation: this._sessionGeneration, locked: this._locked(),
            capture_api: 1, input_readiness_api: 1, shell_input_blocked: shellInputBlocked,
            cursor_api: this._personSignal ? 1 : 0,
            cursor_visible: !!this._cursor?.inhibited, last_person_us: this._personAt,
            guarded_input_api: ['begin', 'key', 'motion', 'button', 'scroll', 'text', 'end', 'state', 'idle']
                .every(method => typeof global.display[`ibara_input_${method}`] === 'function') ? 1 : 0,
            input_settled: typeof global.display.ibara_input_state === 'function' ?
                JSON.parse(global.display.ibara_input_state()).settled : null, monitors, windows,
            focused: global.display.focus_window ? `gnome:${global.display.focus_window.get_stable_sequence()}` : null });
    }
    _target(encoded) {
        if (this._locked()) throw new Error('Desktop is locked or at login');
        const identity = JSON.parse(encoded);
        if (identity.epoch !== this._epoch) throw new Error('Session identity changed');
        const window = global.get_window_actors().map(actor => actor.meta_window).find(window =>
            window && `gnome:${window.get_stable_sequence()}` === identity.address && window.get_pid() === identity.pid &&
            (window.get_wm_class() || window.get_gtk_application_id() || '') === identity.class);
        if (!window) throw new Error('Window identity changed');
        return window;
    }
    async SnapshotAsync([encoded], invocation) {
        let stream = null, clone = null;
        try {
            const request = JSON.parse(encoded);
            if (this._locked() || request.epoch !== this._epoch ||
                request.generation !== this._sessionGeneration ||
                Main.layoutManager.monitors.length !== 1 || global.display.get_monitor_scale(0) !== 1)
                throw new Error('Snapshot session/display identity changed');
            stream = Gio.MemoryOutputStream.new_resizable();
            const window = request.address ? this._target(encoded) : null;
            const rect = window ? window.get_frame_rect() : Main.layoutManager.monitors[0];
            if (rect.width < 1 || rect.height < 1 || rect.width > 4096 || rect.height > 4096)
                throw new Error('Snapshot geometry exceeds supported bounds');
            if (window) {
                const actor = window.get_compositor_private();
                if (!actor || window.minimized || !window.showing_on_its_workspace())
                    throw new Error('Snapshot window is not mapped');
                clone = new Clutter.Clone({source:actor, opacity:0, width:1, height:1, reactive:false});
                global.stage.add_child(clone);
                const buffer = window.get_buffer_rect();
                const texture = actor.paint_to_content(null)?.get_texture();
                const x = rect.x-buffer.x, y = rect.y-buffer.y;
                if (!texture || actor.get_resource_scale() !== 1 || x < 0 || y < 0 ||
                    x+rect.width > texture.get_width() || y+rect.height > texture.get_height())
                    throw new Error('Snapshot actor does not match its frame');
                await Shell.Screenshot.composite_to_stream(texture, x, y, rect.width, rect.height,
                    1, null, 0, 0, 1, stream);
            } else {
                // Explicitly excludes cursor sprite copying on software/headless seats.
                await new Shell.Screenshot().screenshot(false, stream);
            }
            stream.close(null);
            const current = window ? this._target(encoded).get_frame_rect() : Main.layoutManager.monitors[0];
            if (this._locked() || !this._impl || request.epoch !== this._epoch ||
                request.generation !== this._sessionGeneration || !current ||
                ['x','y','width','height'].some(key => rect[key] !== current[key]) ||
                stream.get_data_size() > 32*1024*1024)
                throw new Error('Snapshot changed or exceeds its byte limit');
            invocation.return_value(new GLib.Variant('(s)', [GLib.base64_encode(stream.steal_as_bytes().get_data())]));
        } catch (error) {
            invocation.return_dbus_error('org.ibara.Gnome.SnapshotRefused', String(error));
        } finally {
            clone?.destroy();
            if (stream && !stream.is_closed()) stream.close(null);
        }
    }
    Focus(encoded) {
        const window = this._target(encoded);
        Main.activateWindow(window);
        return global.display.focus_window === window;
    }
    Close(encoded) {
        this._target(encoded).delete(global.get_current_time());
        return true;
    }
    _stopCursor(reason = 'Cursor lease ended before painting') {
        const cursor = this._cursor;
        if (!cursor) return;
        this._cursor = null;
        if (this._input?.owner === cursor.owner) this._stopInput();
        if (cursor.paint) global.stage.disconnect(cursor.paint);
        if (cursor.timeout) GLib.source_remove(cursor.timeout);
        if (cursor.inhibited) {
            cursor.tracker.uninhibit_cursor_visibility();
        }
        if (cursor.unfocus) cursor.seat.uninhibit_unfocus();
        cursor.actor.destroy();
        cursor.pending?.return_dbus_error('org.ibara.Gnome.CursorRefused', reason);
    }
    _ownedCursor(token, invocation) {
        const cursor = this._cursor;
        if (!cursor || cursor.token !== token || cursor.owner !== invocation.get_sender())
            throw new Error('Cursor lease is stale or belongs to another connection');
        return cursor;
    }
    _cursorPaint(cursor, invocation, signature, value, painted = () => {}) {
        if (cursor.pending) throw new Error('Previous cursor paint is pending');
        cursor.pending = invocation;
        cursor.paint = global.stage.connect('after-paint', () => {
            global.stage.disconnect(cursor.paint);
            cursor.paint = 0;
            GLib.source_remove(cursor.timeout);
            cursor.timeout = 0;
            if (this._cursor !== cursor) return;
            if (this._locked()) { this._stopCursor(); return; }
            try { painted(); }
            catch (error) { this._stopCursor(String(error)); return; }
            cursor.pending = null;
            invocation.return_value(new GLib.Variant(signature, [value]));
        });
        cursor.timeout = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 1500, () => {
            cursor.timeout = 0;
            this._stopCursor('Cursor paint was not witnessed; input must not start');
            return GLib.SOURCE_REMOVE;
        });
        cursor.actor.queue_redraw();
    }
    _positionCursor(cursor, x, y) {
        const monitor = Main.layoutManager.monitors.find(m =>
            x >= m.x && y >= m.y && x < m.x+m.width && y < m.y+m.height);
        if (!monitor) throw new Error('Cursor point is outside the current displays');
        const [,width] = cursor.label.get_preferred_width(-1);
        const [,height] = cursor.label.get_preferred_height(width);
        if (width > monitor.width || height > monitor.height)
            throw new Error('Agent label cannot fit on this display');
        cursor.actor.set_position(x,y);
        const left = Math.max(monitor.x, Math.min(x+24, monitor.x+monitor.width-width));
        const top = Math.max(monitor.y, Math.min(y, monitor.y+monitor.height-height));
        cursor.label.set_translation(left-x-24, top-y, 0);
    }
    CursorBeginAsync([label], invocation) {
        let created = null;
        try {
            if (!this._personSignal || this._locked() || this._cursor ||
                !global.display.ibara_input_idle() ||
                GLib.get_monotonic_time() - this._personAt < 1000000)
                throw new Error('Cursor takeover unavailable while person/session input is active');
            if (typeof label !== 'string' || !label.trim() || label.length > 64 || /[\x00-\x1f\x7f]/.test(label))
                throw new Error('Cursor needs a plain agent label of 1–64 characters');
            const tracker = global.backend.get_cursor_tracker();
            const seat = Clutter.get_default_backend().get_default_seat();
            const actor = new St.BoxLayout({name:'ibara-agent-cursor', reactive:false, visible:false});
            const arrow = new St.DrawingArea({width:24, height:24, reactive:false});
            arrow.connect('repaint', area => {
                const cr = area.get_context();
                cr.moveTo(1, 1); cr.lineTo(7, 22); cr.lineTo(12, 15); cr.lineTo(21, 15); cr.closePath();
                cr.setSourceRGBA(0.16, 0.45, 0.95, 1); cr.fillPreserve();
                cr.setSourceRGBA(1, 1, 1, 1); cr.setLineWidth(1.5); cr.stroke(); cr.$dispose();
            });
            actor.add_child(arrow);
            const name = new St.Label({text:label, reactive:false,
                style:'color:white; background-color:#2973f2; border-radius:6px; padding:3px 7px;'});
            actor.add_child(name);
            const cursor = {actor, label:name, owner:invocation.get_sender(), token:GLib.uuid_string_random(),
                tracker, seat, inhibited:false, unfocus:false};
            this._cursor = cursor;
            created = cursor;
            Main.layoutManager.addTopChrome(actor);
            const [x,y] = global.get_pointer();
            this._positionCursor(cursor,x,y);
            actor.show();
            this._cursorPaint(cursor, invocation, '(s)', cursor.token, () => {
                cursor.seat.inhibit_unfocus();
                cursor.unfocus = true;
                cursor.tracker.inhibit_cursor_visibility();
                cursor.inhibited = true;
            });
        } catch (error) {
            if (created && this._cursor === created) {
                const replied = !!created.pending;
                this._stopCursor(String(error));
                if (replied) return;
            }
            invocation.return_dbus_error('org.ibara.Gnome.CursorRefused', String(error));
        }
    }
    CursorMoveAsync([token,x,y], invocation) {
        try {
            const cursor = this._ownedCursor(token, invocation);
            if (!Number.isFinite(x) || !Number.isFinite(y) || !Main.layoutManager.monitors.some(m =>
                x >= m.x && y >= m.y && x < m.x+m.width && y < m.y+m.height))
                throw new Error('Cursor point is outside the current displays');
            if (cursor.pending) throw new Error('Previous cursor paint is pending');
            this._positionCursor(cursor,x,y);
            this._cursorPaint(cursor, invocation, '(b)', true);
        } catch (error) {
            invocation.return_dbus_error('org.ibara.Gnome.CursorRefused', String(error));
        }
    }
    CursorEndAsync([token], invocation) {
        this._inputCall(invocation, '(b)', () => {
            this._ownedCursor(token, invocation);
            this._stopCursor();
            return true;
        });
    }
    _stopInput() {
        if (this._input && typeof global.display.ibara_input_end === 'function')
            global.display.ibara_input_end(this._input.token);
    }
    _inputCall(invocation, signature, action) {
        try { invocation.return_value(new GLib.Variant(signature, [action()])); }
        catch (error) { invocation.return_dbus_error('org.ibara.Gnome.InputRefused', String(error)); }
    }
    _ownedInput(token, invocation, requireCursor = false) {
        if (!this._input || this._input.token !== token || this._input.owner !== invocation.get_sender())
            throw new Error('Input transaction is stale or belongs to another connection');
        if (requireCursor && this._input.cursorLease && (!this._cursor?.inhibited || this._cursor.pending ||
            this._cursor.token !== this._input.cursorLease || this._cursor.owner !== invocation.get_sender()))
            throw new Error('The input cursor lease was interrupted');
        return this._input;
    }
    InputBeginAsync([encoded, generation], invocation) {
        this._inputCall(invocation, '(s)', () => {
            if (Main.layoutManager.monitors.length !== 1 || global.display.get_monitor_scale(0) !== 1)
                throw new Error('GNOME input supports one output at scale1 only');
            if (typeof global.display.ibara_input_begin !== 'function')
                throw new Error('The qualified Mutter input API is unavailable; no global fallback');
            if (typeof generation !== 'string' || !/^[a-zA-Z0-9_-]{1,64}$/.test(generation))
                throw new Error('Invalid controller generation');
            const cursorLease = JSON.parse(encoded).cursor_lease;
            if (cursorLease && (!this._cursor?.inhibited || this._cursor.pending ||
                this._cursor.token !== cursorLease || this._cursor.owner !== invocation.get_sender()))
                throw new Error('The required cursor lease is absent or belongs to another connection');
            if (this._cursor && this._cursor.owner !== invocation.get_sender())
                throw new Error('Another connection holds cursor control');
            const window = this._target(encoded);
            if (this._input && !JSON.parse(global.display.ibara_input_state()).settled)
                throw new Error('Previous input has not settled');
            if (!global.display.ibara_input_idle()) throw new Error('Existing input is held; takeover refused');
            // Activation is preparation; the native filter checks again at delivery.
            Main.activateWindow(window);
            if (global.display.focus_window !== window) throw new Error('Target did not focus');
            const token = GLib.uuid_string_random();
            if (!global.display.ibara_input_begin(window, token)) throw new Error('Mutter refused input begin');
            this._input = { token, generation, window, cursorLease, owner: invocation.get_sender() };
            return token;
        });
    }
    InputKeyAsync([token, key, pressed], invocation) {
        this._inputCall(invocation, '(b)', () => {
            this._ownedInput(token, invocation, true);
            if (this._locked()) { this._stopInput(); return false; }
            return global.display.ibara_input_key(token, key, pressed);
        });
    }
    InputScrollAsync([token, direction], invocation) {
        this._inputCall(invocation, '(b)', () => {
            this._ownedInput(token, invocation, true);
            if (this._locked()) { this._stopInput(); return false; }
            if (typeof global.display.ibara_input_scroll !== 'function')
                throw new Error('Guarded wheel input is unavailable; no fallback');
            return global.display.ibara_input_scroll(token, direction);
        });
    }
    InputTextAsync([token, text], invocation) {
        this._inputCall(invocation, '(b)', () => {
            this._ownedInput(token, invocation, true);
            if (this._locked()) { this._stopInput(); return false; }
            if (typeof global.display.ibara_input_text !== 'function')
                throw new Error('Guarded text input is unavailable; no clipboard fallback');
            return global.display.ibara_input_text(token, text);
        });
    }
    InputMotionAsync([token, x, y], invocation) {
        this._inputCall(invocation, '(b)', () => {
            this._ownedInput(token, invocation, true);
            if (this._locked()) { this._stopInput(); return false; }
            return global.display.ibara_input_motion(token, x, y);
        });
    }
    InputButtonAsync([token, button, pressed], invocation) {
        this._inputCall(invocation, '(b)', () => {
            this._ownedInput(token, invocation, true);
            if (this._locked()) { this._stopInput(); return false; }
            return global.display.ibara_input_button(token, button, pressed);
        });
    }
    InputEndAsync([token], invocation) {
        this._inputCall(invocation, '(s)', () => {
            this._ownedInput(token, invocation);
            this._stopInput();
            return global.display.ibara_input_state();
        });
    }
    InputStateAsync([token], invocation) {
        this._inputCall(invocation, '(s)', () => {
            this._ownedInput(token, invocation);
            if (this._locked()) this._stopInput();
            return global.display.ibara_input_state();
        });
    }
    disable() {
        this._stopCursor();
        if (this._personSignal) global.display.disconnect(this._personSignal);
        this._personSignal = 0;
        this._stopInput();
        this._input = null;
        if (this._inputTimer) GLib.source_remove(this._inputTimer);
        this._inputTimer = 0;
        if (this._ownerWatch) Gio.DBus.session.signal_unsubscribe(this._ownerWatch);
        this._ownerWatch = 0;
        for (const window of [...this._windows.keys()]) this._unwatchWindow(window);
        for (const [object, signal] of this._signals) object.disconnect(signal);
        this._signals = [];
        this._impl?.unexport();
        this._impl = null;
        if (this._name) Gio.bus_unown_name(this._name);
        this._name = 0;
    }
}
