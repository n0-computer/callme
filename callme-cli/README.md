# callme-cli

A command-line interface to make calls with `callme`.

## Usage

```
cargo run --release
```

On Linux, you need ALSA and DBUS development headers:
```
apt-get install libasound2-dev libdbus-1-dev
```

The crate includes a C dependency for echo cancellation (`webrtc-audio-processing`) that needs C build tools to be installed.
On macOS these can be installed with homebrew:
```
brew install automake libtool
```

On Windows, or if the build fails, you can disable the audio processing entirely. You should only use callme with headphones then.
```
cargo run --release --no-default-features
```
