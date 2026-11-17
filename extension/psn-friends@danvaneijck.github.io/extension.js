// PSN Friends: a top-bar indicator listing what your PSN friends are doing.
//
// All PSN talking (auth, polling, image caching) lives in the `psn-friends`
// CLI; this extension just runs `psn-friends status --json` on a timer and
// draws the result, so network trouble can never wedge the shell.

import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import Pango from 'gi://Pango';
import St from 'gi://St';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

Gio._promisify(Gio.Subprocess.prototype, 'communicate_utf8_async');

const CLI_NAME = 'psn-friends';
const POLL_SECONDS = 60;
const REFRESH_ON_OPEN_AFTER_SECONDS = 30;
const CLI_TIMEOUT_SECONDS = 90;

// Display-only menu rows. Not `reactive: false`: themes grey those out.
const INFO_ROW = {activate: false, hover: false, can_focus: false};

function findCli() {
    const home = GLib.get_home_dir();
    const candidates = [
        GLib.build_filenamev([home, '.local', 'bin', CLI_NAME]),
        GLib.build_filenamev([home, '.cargo', 'bin', CLI_NAME]),
        GLib.find_program_in_path(CLI_NAME),
    ];
    return candidates.find(p => p && GLib.file_test(p, GLib.FileTest.IS_EXECUTABLE)) ?? null;
}

function secondsSince(iso) {
    const then = iso ? GLib.DateTime.new_from_iso8601(iso, null) : null;
    if (!then)
        return null;
    return Math.max(0, GLib.DateTime.new_now_utc().difference(then) / 1_000_000);
}

function timeAgo(iso) {
    const secs = secondsSince(iso);
    if (secs === null)
        return null;
    if (secs < 60)
        return 'just now';
    const mins = Math.floor(secs / 60);
    if (mins < 60)
        return `${mins}m ago`;
    const hours = Math.floor(mins / 60);
    if (hours < 24)
        return `${hours}h ago`;
    const days = Math.floor(hours / 24);
    if (days < 30)
        return `${days}d ago`;
    return GLib.DateTime.new_from_iso8601(iso, null).to_local().format('%-d %b %Y');
}

// GNOME 48 replaced St.BoxLayout's `vertical` with `orientation`.
function column(params) {
    const box = new St.BoxLayout(params);
    if ('orientation' in box)
        box.orientation = Clutter.Orientation.VERTICAL;
    else
        box.vertical = true;
    return box;
}

function label(text, styleClass) {
    const l = new St.Label({text, style_class: styleClass, y_align: Clutter.ActorAlign.CENTER});
    l.clutter_text.ellipsize = Pango.EllipsizeMode.END;
    return l;
}

// A square image drawn as a CSS background, so the theme can round its corners.
function picture(path, styleClass, fallbackIcon) {
    const bin = new St.Bin({style_class: styleClass, y_align: Clutter.ActorAlign.CENTER});
    if (path && GLib.file_test(path, GLib.FileTest.EXISTS)) {
        bin.style = `background-image: url("${path.replace(/"/g, '\\"')}");`;
    } else {
        bin.add_style_class_name('psn-picture-empty');
        if (fallbackIcon)
            bin.set_child(new St.Icon({icon_name: fallbackIcon, style_class: 'psn-picture-fallback'}));
    }
    return bin;
}

const FriendItem = GObject.registerClass(
class PsnFriendItem extends PopupMenu.PopupBaseMenuItem {
    _init(friend) {
        super._init({...INFO_ROW, style_class: 'psn-friend'});

        this.add_child(picture(friend.avatar, 'psn-avatar', 'avatar-default-symbolic'));

        const text = column({
            x_expand: true,
            y_align: Clutter.ActorAlign.CENTER,
            style_class: 'psn-friend-text',
        });
        text.add_child(label(friend.online_id, 'psn-name'));

        const platform = friend.platform ?? '';
        if (friend.state === 'playing') {
            text.add_child(label(friend.game.title, 'psn-detail psn-playing'));
            const extra = [platform, friend.game.status].filter(Boolean).join(' · ');
            if (extra)
                text.add_child(label(extra, 'psn-subtle'));
        } else if (friend.state === 'online') {
            text.add_child(label(platform ? `Online on ${platform}` : 'Online', 'psn-detail psn-online'));
        } else {
            const seen = timeAgo(friend.last_online);
            text.add_child(label(seen ? `Last seen ${seen}` : 'Offline', 'psn-subtle'));
        }
        this.add_child(text);

        if (friend.state === 'playing')
            this.add_child(picture(friend.game.icon, 'psn-game-art', 'input-gaming-symbolic'));
    }
});

const Indicator = GObject.registerClass(
class PsnFriendsIndicator extends PanelMenu.Button {
    _init() {
        super._init(0.5, 'PSN Friends');

        const box = new St.BoxLayout({style_class: 'panel-status-menu-box'});
        this._icon = new St.Icon({icon_name: 'input-gaming-symbolic', style_class: 'system-status-icon'});
        this._count = new St.Label({text: '', y_align: Clutter.ActorAlign.CENTER, style_class: 'psn-count'});
        box.add_child(this._icon);
        box.add_child(this._count);
        this.add_child(box);

        this._snapshot = null;
        this._error = null;
        this._needsLogin = false;
        this._busy = false;
        this._offlineOpen = false;
        this._destroyed = false;
        this._cancellable = new Gio.Cancellable();

        this.menu.connect('open-state-changed', (_menu, open) => {
            if (!open)
                return;
            this._render();
            const age = secondsSince(this._snapshot?.fetched_at);
            if (age === null || age > REFRESH_ON_OPEN_AFTER_SECONDS)
                this.refresh();
        });
        this.connect('destroy', () => this._onDestroy());

        this._render();
        this.refresh();
        this._timer = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, POLL_SECONDS, () => {
            this.refresh();
            return GLib.SOURCE_CONTINUE;
        });
    }

    async refresh() {
        if (this._busy || this._destroyed)
            return;
        const cli = findCli();
        if (!cli) {
            this._setError(`Can't find the ${CLI_NAME} command. See https://github.com/danvaneijck/psn-friends to install it.`, false);
            this._render();
            return;
        }

        this._busy = true;
        let killTimer = 0;
        try {
            const proc = Gio.Subprocess.new(
                [cli, 'status', '--json'],
                Gio.SubprocessFlags.STDOUT_PIPE | Gio.SubprocessFlags.STDERR_PIPE);
            killTimer = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, CLI_TIMEOUT_SECONDS, () => {
                killTimer = 0;
                proc.force_exit();
                return GLib.SOURCE_REMOVE;
            });
            const [stdout, stderr] = await proc.communicate_utf8_async(null, this._cancellable);
            if (this._destroyed)
                return;

            let data = null;
            try {
                data = JSON.parse(stdout);
            } catch {
                const why = stderr?.trim() || `${CLI_NAME} exited with status ${proc.get_exit_status()}`;
                throw new Error(why);
            }
            if (data.error) {
                this._setError(data.error, Boolean(data.needs_login));
            } else {
                this._snapshot = data;
                this._error = null;
                this._needsLogin = false;
            }
        } catch (e) {
            if (this._destroyed || e.matches?.(Gio.IOErrorEnum, Gio.IOErrorEnum.CANCELLED))
                return;
            this._setError(e.message, false);
        } finally {
            if (killTimer)
                GLib.Source.remove(killTimer);
            this._busy = false;
        }
        this._render();
    }

    _setError(message, needsLogin) {
        this._error = message.length > 240 ? `${message.slice(0, 240)}…` : message;
        this._needsLogin = needsLogin;
    }

    _render() {
        const friends = this._snapshot?.friends ?? [];
        const playing = friends.filter(f => f.state === 'playing');
        const online = friends.filter(f => f.state === 'online');
        const offline = friends.filter(f => f.state === 'offline');
        const active = playing.length + online.length;

        // Panel: count of friends online; dim the icon when nobody is.
        if (this._error && !this._snapshot)
            this._count.text = '!';
        else
            this._count.text = active > 0 ? String(active) : '';
        this._icon.opacity = active > 0 || this._error ? 255 : 150;

        // Destroying the old Offline submenu emits a close, so read this first.
        const reopenOffline = this._offlineOpen && this.menu.isOpen;
        this.menu.removeAll();
        this._offlineOpen = reopenOffline;

        if (this._error)
            this._addError();

        if (this._snapshot) {
            if (playing.length)
                this._addSection(`Playing · ${playing.length}`, playing);
            if (online.length)
                this._addSection(`Online · ${online.length}`, online);
            if (!active) {
                const empty = new PopupMenu.PopupMenuItem('Nobody is online right now', {reactive: false});
                empty.add_style_class_name('psn-empty');
                this.menu.addMenuItem(empty);
            }
            if (offline.length)
                this._addOffline(offline);
        } else if (!this._error) {
            this.menu.addMenuItem(new PopupMenu.PopupMenuItem('Loading friends…', {reactive: false}));
        }

        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        this._addFooter();
    }

    _addSection(title, friends) {
        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem(title));
        for (const f of friends)
            this.menu.addMenuItem(new FriendItem(f));
    }

    _addOffline(friends) {
        const sub = new PopupMenu.PopupSubMenuMenuItem(`Offline · ${friends.length}`);
        for (const f of friends)
            sub.menu.addMenuItem(new FriendItem(f));
        // Rebuilding the menu on every poll shouldn't snap this shut.
        sub.menu.connect('open-state-changed', (_m, open) => {
            this._offlineOpen = open;
        });
        this.menu.addMenuItem(sub);
        if (this._offlineOpen)
            sub.menu.open(false);
    }

    _addError() {
        const title = this._needsLogin ? 'Sign in to PSN' : "Couldn't reach PSN";
        const head = new PopupMenu.PopupMenuItem(title, INFO_ROW);
        head.add_style_class_name('psn-error-title');
        this.menu.addMenuItem(head);

        const msg = this._needsLogin
            ? 'Run “psn-friends login” in a terminal.'
            : this._error;
        const body = new PopupMenu.PopupMenuItem(msg, INFO_ROW);
        body.label.add_style_class_name('psn-error-body');
        body.label.clutter_text.line_wrap = true;
        body.label.clutter_text.ellipsize = Pango.EllipsizeMode.NONE;
        this.menu.addMenuItem(body);
        if (this._snapshot)
            this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem('Last known'));
    }

    _addFooter() {
        const item = new PopupMenu.PopupBaseMenuItem({style_class: 'psn-footer'});
        item.add_child(new St.Label({text: 'Refresh', y_align: Clutter.ActorAlign.CENTER, x_expand: true}));
        const updated = timeAgo(this._snapshot?.fetched_at);
        const status = this._busy ? 'updating…' : updated ? `updated ${updated}` : '';
        item.add_child(new St.Label({text: status, style_class: 'psn-subtle', y_align: Clutter.ActorAlign.CENTER}));
        item.connect('activate', () => this.refresh());
        this.menu.addMenuItem(item);
    }

    _onDestroy() {
        this._destroyed = true;
        this._cancellable.cancel();
        if (this._timer) {
            GLib.Source.remove(this._timer);
            this._timer = 0;
        }
    }
});

export default class PsnFriendsExtension extends Extension {
    enable() {
        this._indicator = new Indicator();
        Main.panel.addToStatusArea(this.uuid, this._indicator);
    }

    disable() {
        this._indicator?.destroy();
        this._indicator = null;
    }
}
