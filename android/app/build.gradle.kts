plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "dev.usbfwd"
    compileSdk = 34

    defaultConfig {
        applicationId = "dev.usbfwd"
        minSdk = 26
        targetSdk = 34
        versionCode = 1
        versionName = "0.1.0"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }

    // libusbfwd.so is produced by cargo-ndk; see android/README.md. Keeping it
    // out of the Gradle build means the Rust side is testable on a desktop
    // without an Android SDK anywhere near it.
    sourceSets["main"].jniLibs.srcDirs("src/main/jniLibs")

    ndkVersion = "26.3.11579264"
}

dependencies {
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.appcompat:appcompat:1.7.0")
}

/**
 * Convenience wrapper around cargo-ndk. Not wired into the build graph on
 * purpose: CI and desktop developers should be able to run `cargo test`
 * without an NDK, and the .so is checked for freshness by hand.
 */
tasks.register<Exec>("cargoNdkBuild") {
    workingDir = rootProject.projectDir.parentFile
    commandLine(
        "cargo", "ndk",
        "-t", "arm64-v8a",
        "-t", "armeabi-v7a",
        "-o", "android/app/src/main/jniLibs",
        "build", "--release", "-p", "usbfwd-jni",
    )
}
