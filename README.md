```
             O     o
            o   .
 _     _      .
| |   | |
| |__ | | ___   ___  _ __
| '_ \| |/ _ \ / _ \| '_ \
| |_) | | (_) | (_) | |_) |
|_.__/|_|\___/ \___/| .__/
                    | |
                    |_|
```

**bloop** is a tiny web radio player for the terminal. It uses Rust, [ratatui](https://ratatui.rs) and [mpv](https://mpv.io).

![bloop plays SomaFM Groove Salad, with the spectrum analyzer, the level meter and a recording](assets/player.png)

## Features

- Play internet radio streams (MP3, AAC, Ogg, HLS and more).
- Connect again automatically when the connection drops.
- Save stations with a name and an image, and sort them in sections.
- Browse and search more than 50 000 stations on [radio-browser.info](https://www.radio-browser.info): popular stations, top voted stations, genres and countries.
- Show the song title and the stream metadata.
- Show the album cover of the current song.
- Record the stream to a file. bloop does not re-encode the audio.
- Keep a history of the songs that played. Like a song to find it again, and copy its title.
- Stop the radio after 15, 30, 60 or 90 minutes with the sleep timer.
- Import and export your stations as an `.m3u` playlist.
- Optional: send the songs that you listen to, to your [ListenBrainz](https://listenbrainz.org) account.
- Show a spectrum analyzer and a stereo level meter. The meters use the real audio levels.
- Show high-resolution images in terminals with sixel, kitty or iTerm2 graphics. Other terminals show half-block images.

| My stations | Browse |
| --- | --- |
| ![The start screen with the bloop logo and the station sections](assets/start.png) | ![Search results for "jazz" in the Browse tab](assets/browse.png) |

## Requirements

- Linux or macOS. On Windows, use WSL2.
- Rust 1.88 or later.
- mpv. bloop starts mpv to play the audio.

bloop is tested on Ubuntu 24.04 (WSL2) with Windows Terminal.

## Installation

### From a release

1. Install mpv:

   ```sh
   sudo apt install mpv    # Debian, Ubuntu, WSL
   brew install mpv        # macOS
   ```

2. Download the archive for your system from the [releases page](../../releases):

   - `x86_64-unknown-linux-musl`: Linux and WSL on x86_64.
   - `aarch64-apple-darwin`: macOS on Apple silicon.

3. Extract the archive and put `bloop` in a directory of your `PATH`:

   ```sh
   tar xzf bloop-v0.1.0-x86_64-unknown-linux-musl.tar.gz
   install bloop-v0.1.0-x86_64-unknown-linux-musl/bloop ~/.local/bin/
   ```

4. Start bloop:

   ```sh
   bloop
   ```

### From the source code

1. Install mpv and Rust 1.88 or later.

2. Build and install bloop:

   ```sh
   cargo install --path . --locked
   ```

### Make a release

1. Change the version in `Cargo.toml`.
2. Commit the change.
3. Push a tag with the same version:

   ```sh
   git tag v0.2.0
   git push origin v0.2.0
   ```

GitHub Actions builds the archives and publishes the release.

## Keys

The sidebar has three tabs. Push `tab` to go to the next tab.

- **My stations** shows your stations in sections.
- **Browse** shows stations from radio-browser.info.
- **Songs** shows the songs that played, newest first.

### All views

| Key | Action |
| --- | --- |
| `tab` | Go to the next tab |
| `↑` `↓` | Select the previous or next row |
| `/` | Type in the search bar |
| `space` | Pause or resume |
| `s` | Stop |
| `-` `+` | Decrease or increase the volume (`=` also increases it) |
| `m` | Mute |
| `r` | Start or stop the recording |
| `l` | Like the current song, or remove the like |
| `y` | Copy the title of the current song |
| `t` | Set the sleep timer: 15, 30, 60 or 90 minutes, or off |
| `o` | Play a stream URL |
| `c` | Open the settings |
| `q` or `ctrl+c` | Quit |

### My stations

| Key | Action |
| --- | --- |
| `enter` | Play the station, or open or close the section |
| `←` `→` | Close or open the section |
| `a` | Add a station |
| `e` | Edit the station, or rename the section |
| `g` | Move the station to a different section |
| `J` `K` | Move the station down or up in its section |
| `D` | Delete the station |
| `i` | Import stations from an `.m3u` file |
| `x` | Export all stations to an `.m3u` file |

The import adds only the stations that are not in your list. It keeps the name, the logo (`tvg-logo`) and the section (`group-title`) of each station. A station without a section goes to the "Imported" section.

### Browse

| Key | Action |
| --- | --- |
| `1` `2` `3` `4` | Show popular stations, top voted stations, genres or countries |
| `←` `→` | Show the previous or next category |
| `enter` | Play the station, or open the genre or country |
| `a` | Save the station in a section |
| `esc` or `←` | Go back to the list of genres or countries |

### Songs

| Key | Action |
| --- | --- |
| `1` `2` | Show all songs, or the liked songs only |
| `←` `→` | Show the other list |
| `enter` | Play the station of the song |
| `l` | Like the song, or remove the like |
| `y` | Copy the title of the song |
| `D` | Delete the song from the history |

The history keeps the last 1000 songs. It always keeps the liked songs. Copy uses the OSC 52 escape code: Windows Terminal, kitty, iTerm2 and most other terminals support it.

### Search bar

| Key | Action |
| --- | --- |
| `enter` | Stop typing |
| `esc` | Clear the text and stop typing |
| `ctrl+u` | Clear the text |

In **My stations**, the search bar filters your stations. In **Browse**, it searches all stations while you type. In **Songs**, it filters the song titles and the station names.

### Mouse

| Action | Result |
| --- | --- |
| Click a tab, a category or the search bar | Selects it |
| Click a station | Selects the station |
| Double-click a station, a genre or a country | Plays the station, or opens the genre or country |
| Click a section header | Opens or closes the section |
| Click a key in the bottom bar | Does the same as the key |
| Click the image or the ON AIR sign | Pauses or resumes |
| Click the volume bar | Sets the volume |
| Mouse wheel on a list | Selects the previous or next row |
| Mouse wheel on the player | Decreases or increases the volume |

bloop captures the mouse. To select text in the terminal, hold `shift` while you drag.

## Settings

Push `c` to open the settings. bloop saves each change immediately.

| Setting | Default | Description |
| --- | --- | --- |
| Spectrum analyzer | off | Shows the levels of 32 frequency bands. |
| Audio level meter | off | Shows the left and right levels in dBFS. |
| Album covers | on | Finds the cover of the current song. |
| High-res images | on | Uses sixel, kitty or iTerm2 graphics if the terminal supports them. |
| Meter delay | 250 ms | Moves the meters in time with the speakers. |
| Debug log | off | Writes the mpv messages to `mpv.log`. Attach this file to a bug report. |
| ListenBrainz scrobbling | off | Push `enter` and type your user token from [listenbrainz.org/settings](https://listenbrainz.org/settings/). An empty token stops the scrobbling. |

When ListenBrainz scrobbling is on, bloop sends a song when it ends, if you listened to it for 30 seconds or more. bloop sends only the titles in the format "Artist - Title".

The meters need CPU time. When both meters are off, bloop does not analyze the audio.

## Files

| File | Content |
| --- | --- |
| `~/.config/bloop/stations.json` | Your stations and their sections |
| `~/.config/bloop/settings.json` | Your settings, the volume, the last station, the closed sections and your ListenBrainz token |
| `~/.config/bloop/history.json` | The song history and the liked songs |
| `~/.config/bloop/mpv.log` | The mpv messages, when the debug log is on |
| `~/Music/bloop/` | Your recordings |

## Privacy

bloop sends data to these services:

- **iTunes Search API**: the title of the current song, when the "Album covers" setting is on.
- **radio-browser.info**: the text of your station search, and the genre or country that you open.
- **Station logo servers**: a request for the logo of the station that you play.
- **ListenBrainz**: the artist and the title of each song that you listen to, when you set a token.
