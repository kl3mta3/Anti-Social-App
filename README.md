# Anti Social companion

The desktop app players run next to the game. It holds the encrypted connection to the server and is where queueing, ready checks, the party view and ratings happen. Built with Tauri: the core is Rust, the windows are HTML and CSS shown through WebView2. The design is in [../DESIGN.md](../DESIGN.md).

## What it does

- **Overlay icon.** A small gold-rimmed orb that floats over the game. Click to open or hide the main window, drag to move it. It breathes amber while queued and flashes green when a group is ready. The first time, it starts just inside the top-left corner of the game window. It carries no text; locking and hiding are shown as check marks in the tray menu.
- **Main window.** Pick a character, pick roles and instances, queue, answer the ready check, see the party with thumbs and run counts, copy the leader's code, leave or mark done. It can be resized by its edges and remembers its size and place. The first time, it opens beside the orb and no taller than the game window.
- **Tray icon.** Open; "Show overlay icon" and "Lock overlay icon in place" (each with a check mark when on); "Move overlay icon to the game window"; quit. Closing the main window only hides it.
- **Queue numbers.** Shows how many players are online, how many are queued in your world, who is queued for each instance by role, and the usual wait per role. These are totals from the server; no names are involved.
- **Small window.** Below about 640 pixels tall the role picker becomes a compact row of toggles and the layout tightens.
- **Update check.** Looks at the latest release of `kl3mta3/Anti-Social-App` shortly after starting and every six hours. When a newer version exists a notice appears with a Download button that opens the release page. It does not replace itself.
- **Addon installer.** Settings has an "Install or update the addon" button. It downloads the latest release of the addon from GitHub and unpacks it into the game's AddOns folder.
- **Identity.** On first connect the server issues this install a secret token. It is kept in `%APPDATA%\com.antisocial.companion\settings.json` and never shown in the windows.

## Run it from source

Needs Node, the Rust toolchain, Visual Studio's C++ build tools and a Windows 10/11 SDK. Use PowerShell or cmd, not Git Bash (Git Bash puts its own unrelated `link.exe` ahead of Microsoft's linker and the build fails).

```powershell
npm install
```

```powershell
npm run dev
```

Build the installer:

```powershell
npm run build
```

The installer lands in `src-tauri\target\release\bundle\nsis\`.

## Settings

Open them with the cog in the title bar.

| Setting | Meaning |
|---|---|
| World of Warcraft Forever folder | The client folder that contains `WTF`, for example `D:\World of Warcraft\_retail_`. Found automatically in the usual places. |
| Overlay icon | Show it, lock it in place, or move it back to the game window. These apply as soon as you click them. |
| This app | Shows the version and checks for updates on request. |
| In-game addon | Shows the installed version; the button installs or updates it. |

The server address is not a setting. Every copy uses the one built into the app (`SERVER` in `src-tauri/src/main.rs`), so all players share one queue; to move the server, change it there and release an update. When running from source with `npm run dev`, set the `ANTISOCIAL_SERVER` environment variable to point at a test server, for example `ws://localhost:8080/ws`. Released builds ignore it.

## How it reads your character

The addon writes one line per character into its saved variables, which the game flushes to disk on `/reload` or logout:

```lua
AntiSocialExport = {
	["Brakka Ironhide"] = "v1|Brakka Ironhide|horde|Warrior|42|Fury|31.5|1794559247",
}
```

Fields: version, name, faction, class, level, spec, item level, snapshot time (Unix seconds). The companion reads `WTF\Account\*\SavedVariables\AntiSocial.lua`. Without the addon, type a character in by hand; other players see those stats marked "typed in".

Region and ruleset are chosen in the companion because the addon cannot reliably tell them apart.

## Finding the game window

To place the orb, the companion asks Windows where the game's window is (its position and size). That is the same question any window manager asks. It reads nothing from the game and sends nothing to it.

## Installing the addon

The button downloads `AntiSocialAddon.zip` from the latest release of `kl3mta3/Anti-Social-Addon`, falling back to the main branch. The address is fixed in the code. Before anything is written, the download must contain `AntiSocial.toc` and `AntiSocial.lua`; only addon file types are unpacked; entries that try to write outside the addon folder are ignored; and the old copy is only replaced once the new one is fully unpacked. After updating, type `/reload` in game.

To publish an update: raise `## Version` in `AntiSocial.toc`, then create a new GitHub release with a zip named `AntiSocialAddon.zip` that contains the `AntiSocial` folder.

## Not built yet

- Reporting who has joined the in-game party (waits on the probe results), so the server's invite timeout stays off and the leader replaces no-shows by hand.
- Installing an update by itself (today it only tells you and opens the download page), and start with Windows.

## Publishing a new version

Raise `version` in `src-tauri/tauri.conf.json` and `src-tauri/Cargo.toml`, build, then create a GitHub release in `kl3mta3/Anti-Social-App` whose tag is that version (for example `0.2.0`) and attach the exe. Copies already installed compare the tag with their own version.

## Layout

| Path | What it holds |
|---|---|
| `src-tauri/src/main.rs` | Connection, sign-in, reconnecting, settings, tray, overlay behaviour, addon file reader. |
| `src-tauri/tauri.conf.json` | The two windows and the installer settings. |
| `ui/index.html`, `ui/app.js`, `ui/style.css` | The main window. |
| `ui/overlay.html`, `ui/overlay.js`, `ui/overlay.css` | The floating icon. |
| `scripts/make-icon.mjs` | Draws the app icon. |
