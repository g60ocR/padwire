# usbfwd for Android

Turns a tablet into a USB/IP exporter so a controller plugged into it shows up
on the host running Sunshine. No root: `UsbManager` hands the app a file
descriptor for the device, and everything below that is the same usbfs code the
Steam Deck build uses.

## Building the native library

The Rust side is not part of the Gradle build, so `cargo test` works on a
desktop with no Android SDK installed. Build it separately:

```sh
cargo install cargo-ndk
rustup target add aarch64-linux-android armv7-linux-androideabi
# from the workspace root:
cargo ndk -t arm64-v8a -t armeabi-v7a \
    -o android/app/src/main/jniLibs \
    build --release -p usbfwd-jni
```

That produces `android/app/src/main/jniLibs/<abi>/libusbfwd.so`. Then:

```sh
cd android && ./gradlew assembleDebug
```

`./gradlew cargoNdkBuild` runs the `cargo ndk` line above if you would rather
not remember it.

## How it fits together

* `MainActivity` — requests `UsbManager` permission and starts the service. The
  `USB_DEVICE_ATTACHED` intent filter (see `res/xml/device_filter.xml`, which
  matches Valve's vendor id `0x28de`) launches it automatically when the puck
  is plugged in, and attaching that way grants permission implicitly.
* `ForwardService` — a foreground service holding a partial wake lock. Without
  it Android freezes the process when the screen goes off, which a gamepad
  notices immediately. It claims every interface with `force = true`, the
  no-root equivalent of `USBDEVFS_DISCONNECT`.
* `NativeBridge` — `int`-only JNI. The native side takes the descriptor, dups
  it, and reads `/proc/self/fd/<fd>` to learn the `/dev/bus/usb/BBB/DDD` path.

Because `dup(2)` shares the file description, the interfaces claimed in Kotlin
are already claimed as far as the Rust exporter is concerned.

## Logs

Everything the exporter logs goes to logcat under the tag `usbfwd`:

```sh
adb logcat -s usbfwd
```

Set the level with `USBFWD_LOG` if you launch it from a shell; otherwise it is
`info`.
