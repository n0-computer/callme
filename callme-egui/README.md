# callme-egui

A very WIP user interface for callme.

## Run on desktop (Linux / macOS / Windows)

```
cargo run --release
```

On Linux, you need ALSA and DBUS development headers:
```
apt-get install libasound2-dev libdbus-1-dev libtool automake
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

## Run on Android

Prerequisites: Android SDK and NDK must be installed, through Android studio.
See e.g. the [Dioxus guide](https://dioxuslabs.com/learn/0.6/guides/mobile/#android) on what you need to do.

Also install [`cargo apk`](https://github.com/rust-mobile/xbuild), a Rust build tool for mobile.

```
cargo binstall cargo-apk
```

Now you need to set some environment variables for things to work. It is a bit of a pain to get this working.
For me, I'm using these here, the paths might needsto be adapted to your system.

```sh
export ANDROID_HOME=$HOME/Android/Sdk
export ANDROID_NDK=$ANDROID_HOME/ndk/28.0.12674087
export ANDROID_NDK_ROOT=$ANDROID_NDK
export JAVA_HOME=/opt/android-studio/jbr
export TOOLCHAIN="${ANDROID_NDK}/toolchains/llvm/prebuilt/linux-x86_64"
export PATH=$ANDROID_HOME/platform-tools:$TOOLCHAIN/bin:$PATH

export CARGO_APK_RELEASE_KEYSTORE_PASSWORD="android"
export CARGO_APK_RELEASE_KEYSTORE=$HOME/.android/debug.keystore
```
You can put those into a file and then do `source android-vars.sh` in your terminal before following the rest of the guide.
Note: You should only do android builds in this terminal then - e.g. Wasm builds might fail with the changed `PATH`.

Now, on your phone, go to *Settings* -> *System* -> *Developer settings* and enable *Wireless debugging* and click on *Pair device with pairing code*
Make sure your computer and phone are in the same WIFI.

Now run
```
adb pair IP:Port
```
with IP and port as printed on the pairing screen on the phone.
and afterwards
```
adb connect IP:Port
```
with the IP and port as printed on the Wireless debugging screen.

And now, finally:

```
cargo apk run --device IP:Port --target aarch64-linux-android --lib --release
```

This should now run the GUI directly on your phone!

## Run on iOS

*Note: This is untested. Instructions taken from [here](https://github.com/emilk/eframe_template/pull/152)*

#### Prerequesites

* Install xcode
* Accept license `sudo xcodebuild -license`
* Install cargo-bundle `cargo install cargo-bundle`
* Install the required target:
   * `rustup target add aarch64-apple-ios` for modern iOS devices
   * `rustup target add aarch64-apple-ios-sim` for simulator on modern machines
   * `rustup target add x86_64-apple-ios` for old iOS devices or simulator on old machines
* Install python 3.11 or newer
* Run the build scripts `./build-ios.py` - it will print a help text with supported options

#### Run in simulator

`./build-ios.py run --sim`

#### Run on device

`./build-ios-py run`
