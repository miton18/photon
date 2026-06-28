// Photon Companion — GNOME Shell top-bar indicator.
//
// Adds a panel button (icon + dropdown menu) into the right status area, next to
// the clock/battery, with quick actions: open the desktop companion, open the web
// library in a browser, and a live reachability check of the Photon server.
//
// GNOME 45+ ESM extension. No GSettings: edit WEB_URL below if your server isn't
// on the default port.

import GObject from 'gi://GObject';
import GLib from 'gi://GLib';
import St from 'gi://St';
import Gio from 'gi://Gio';
import Shell from 'gi://Shell';
import Soup from 'gi://Soup';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

const WEB_URL = 'http://localhost:3000';
const APP_DESKTOP_ID = 'app.photon.Companion.desktop';
const APP_COMMAND = 'photon-companion';

const PhotonIndicator = GObject.registerClass(
class PhotonIndicator extends PanelMenu.Button {
    _init() {
        super._init(0.0, 'Photon Companion');

        this.add_child(new St.Icon({
            icon_name: 'camera-photo-symbolic',
            style_class: 'system-status-icon',
        }));

        // Status line (updated by the reachability check).
        this._status = new PopupMenu.PopupMenuItem('Photon: checking…', {
            reactive: false,
        });
        this.menu.addMenuItem(this._status);
        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());

        const openApp = new PopupMenu.PopupMenuItem('Open Photon Companion');
        openApp.connect('activate', () => this._openApp());
        this.menu.addMenuItem(openApp);

        const openWeb = new PopupMenu.PopupMenuItem('Open web library');
        openWeb.connect('activate', () => {
            Gio.AppInfo.launch_default_for_uri(WEB_URL, null);
        });
        this.menu.addMenuItem(openWeb);

        // Re-check reachability whenever the menu opens.
        this.menu.connect('open-state-changed', (_m, open) => {
            if (open) this._checkServer();
        });

        this._httpSession = new Soup.Session();
        this._checkServer();
    }

    _openApp() {
        // Prefer the installed .desktop (so it shows as a proper app); fall back
        // to spawning the binary directly.
        const app = Shell.AppSystem.get_default().lookup_app(APP_DESKTOP_ID);
        if (app) {
            app.activate();
            return;
        }
        try {
            Gio.Subprocess.new([APP_COMMAND], Gio.SubprocessFlags.NONE);
        } catch (e) {
            Main.notify('Photon Companion', `Could not launch: ${e}`);
        }
    }

    _checkServer() {
        const msg = Soup.Message.new('GET', `${WEB_URL}/api/health`);
        if (!msg) {
            this._status.label.text = 'Photon: bad URL';
            return;
        }
        this._httpSession.send_and_read_async(
            msg,
            GLib.PRIORITY_DEFAULT,
            null,
            (session, res) => {
                try {
                    session.send_and_read_finish(res);
                    const ok = msg.get_status() === Soup.Status.OK;
                    this._status.label.text = ok
                        ? 'Photon: online'
                        : `Photon: HTTP ${msg.get_status()}`;
                } catch (_e) {
                    this._status.label.text = 'Photon: offline';
                }
            }
        );
    }
});

export default class PhotonCompanionExtension extends Extension {
    enable() {
        this._indicator = new PhotonIndicator();
        // position 0, 'right' box → leftmost of the system indicators (clock/battery).
        Main.panel.addToStatusArea(this.uuid, this._indicator, 0, 'right');
    }

    disable() {
        this._indicator?.destroy();
        this._indicator = null;
    }
}
