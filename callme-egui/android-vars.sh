# setup
# you never know which tools needs which of those, so we define them all...
export ANDROID_HOME=$HOME/Android/Sdk
export ANDROID_NDK=$ANDROID_HOME/ndk/28.0.12674087
export ANDROID_NDK_ROOT=$ANDROID_NDK
export JAVA_HOME=/opt/android-studio/jbr
export TOOLCHAIN="${ANDROID_NDK}/toolchains/llvm/prebuilt/linux-x86_64"
export PATH=$ANDROID_HOME/platform-tools:$TOOLCHAIN/bin:$PATH

export CARGO_APK_RELEASE_KEYSTORE_PASSWORD="android"
export CARGO_APK_RELEASE_KEYSTORE=$HOME/.android/debug.keystore
