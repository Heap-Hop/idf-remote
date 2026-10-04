#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
NDK="${ANDROID_NDK_HOME:-$SDK/ndk/28.2.13676358}"
case "$(uname -s)" in
  Darwin) NDK_HOST=darwin-x86_64 ;;
  Linux) NDK_HOST=linux-x86_64 ;;
  *) echo 'Use macOS or Linux to build this PoC' >&2; exit 1 ;;
esac
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$NDK/toolchains/llvm/prebuilt/$NDK_HOST/bin/aarch64-linux-android26-clang"
export ANDROID_HOME="$SDK"
cargo build --manifest-path native/Cargo.toml --target aarch64-linux-android --release --locked
mkdir -p app/src/main/jniLibs/arm64-v8a
cp native/target/aarch64-linux-android/release/libidf_remote_android.so app/src/main/jniLibs/arm64-v8a/
./gradlew :app:assembleDebug
