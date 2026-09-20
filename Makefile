# Builds the usbfwd Android app: the Rust JNI library, then the APK.
#
# Toolchain layout expected (see android/README.md for how it was bootstrapped):
#   JAVA_HOME        JDK 21 — AGP 8.5.2 / Gradle 8.9 don't yet support newer JDKs
#   ANDROID_HOME     Android SDK (platform 34, build-tools 34.0.0)
#   ANDROID_NDK_HOME NDK 26.3.11579264, used by cargo-ndk
#
# Override any of these on the command line if your toolchain lives elsewhere, e.g.:
#   make apk JAVA_HOME=/usr/lib/jvm/java-21-openjdk

ANDROID_TOOLCHAIN ?= $(HOME)/android-toolchain
JAVA_HOME          ?= $(ANDROID_TOOLCHAIN)/jdk-21.0.12.1+1
ANDROID_HOME       ?= $(ANDROID_TOOLCHAIN)/android-sdk
ANDROID_NDK_HOME   ?= $(ANDROID_HOME)/ndk/26.3.11579264

export JAVA_HOME
export ANDROID_HOME
export ANDROID_SDK_ROOT := $(ANDROID_HOME)
export ANDROID_NDK_HOME
export PATH := $(JAVA_HOME)/bin:$(HOME)/.cargo/bin:$(PATH)

ANDROID_DIR  := android
JNI_LIBS_DIR := $(ANDROID_DIR)/app/src/main/jniLibs
SO_FILES     := $(JNI_LIBS_DIR)/arm64-v8a/libusbfwd.so $(JNI_LIBS_DIR)/armeabi-v7a/libusbfwd.so
DEBUG_APK    := $(ANDROID_DIR)/app/build/outputs/apk/debug/app-debug.apk
RELEASE_APK  := $(ANDROID_DIR)/app/build/outputs/apk/release/app-release-unsigned.apk

RUST_SOURCES := $(shell find crates -name '*.rs' -o -name 'Cargo.toml')

.PHONY: all native apk debug release install clean doctor

all: debug

# Rebuild the native library whenever Rust sources change; cargo-ndk itself
# is fast to no-op when nothing changed.
native: $(SO_FILES)

$(SO_FILES) &: $(RUST_SOURCES)
	cargo ndk -t arm64-v8a -t armeabi-v7a -o $(JNI_LIBS_DIR) build --release -p usbfwd-jni

apk debug: native
	cd $(ANDROID_DIR) && ./gradlew assembleDebug
	@echo "APK: $(DEBUG_APK)"

release: native
	cd $(ANDROID_DIR) && ./gradlew assembleRelease
	@echo "APK: $(RELEASE_APK)"

install: debug
	adb install -r $(DEBUG_APK)

clean:
	cd $(ANDROID_DIR) && ./gradlew clean
	rm -rf $(JNI_LIBS_DIR)
	cargo clean

# Sanity-check that the toolchain is where the variables above expect it.
doctor:
	@test -x "$(JAVA_HOME)/bin/java" && echo "OK  JAVA_HOME=$(JAVA_HOME)" || echo "MISSING JAVA_HOME=$(JAVA_HOME)"
	@test -d "$(ANDROID_HOME)/platforms" && echo "OK  ANDROID_HOME=$(ANDROID_HOME)" || echo "MISSING ANDROID_HOME=$(ANDROID_HOME)"
	@test -d "$(ANDROID_NDK_HOME)" && echo "OK  ANDROID_NDK_HOME=$(ANDROID_NDK_HOME)" || echo "MISSING ANDROID_NDK_HOME=$(ANDROID_NDK_HOME)"
	@command -v cargo-ndk >/dev/null && echo "OK  cargo-ndk" || echo "MISSING cargo-ndk (cargo install cargo-ndk)"
	@rustup target list --installed | grep -q aarch64-linux-android && echo "OK  aarch64-linux-android target" || echo "MISSING aarch64-linux-android target"
	@rustup target list --installed | grep -q armv7-linux-androideabi && echo "OK  armv7-linux-androideabi target" || echo "MISSING armv7-linux-androideabi target"
