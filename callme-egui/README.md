# callme-egui

A very WIP user interface for callme.

## Run on desktop

```
cargo run
```

## Run on android

Prerequisites: Android SDK and NDK must be installed, through Android studio.
See e.g. the [Dioxus guide](https://dioxuslabs.com/learn/0.6/guides/mobile/#android) on what you need to do.

Also install [`xbuild`](https://github.com/rust-mobile/xbuild), a Rust build tool for mobile.

```
cargo binstall xbuild
```

Now you need to set some environment variables for things to work. It is a bit of a pain to get this working.
For me, I'm using these here, needs to be adapted to your system likely.

```sh
export ANDROID_HOME=$HOME/Android/Sdk
export ANDROID_SDK=$ANDROID_HOME
export ANDROID_SDK_ROOT=$ANDROID_HOME

export ANDROID_NDK=$ANDROID_HOME/ndk/28.0.12674087
export ANDROID_NDK_HOME=$ANDROID_NDK
export NDK_HOME=$ANDROID_NDK

export JAVA_HOME=/opt/android-studio/jbr
export PATH=$ANDROID_HOME/platform-tools:$ANDROID_HOME/cmdline-tools/latest/bin:$ANDROID_HOME/emulator:$NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/:$PATH
```
You can put those into a file and the `source android-vars.sh` in your terminal before following the rest of the guide.
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
x run --device adb:IP:Port
```

This should now run the GUI directly on your phone!

Click the *Accept* button, copy the node id to your computer through a third channel, and call from `callme-cli` with `cargo run -- connect NODE_ID`!
