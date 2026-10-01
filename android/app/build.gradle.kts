import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "app.audiobridge"
    compileSdk = 35

    defaultConfig {
        applicationId = "app.audiobridge"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "1.0"
        ndk {
            abiFilters += "arm64-v8a"
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildFeatures {
        compose = true
    }

    testOptions {
        unitTests.isReturnDefaultValues = false
    }
}

kotlin {
    compilerOptions {
        jvmTarget.set(JvmTarget.JVM_17)
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.16.0")
    implementation("androidx.activity:activity-compose:1.10.1")
    // play-services pulls fragment 1.1.0; the Activity Result API needs ≥ 1.3.0.
    implementation("androidx.fragment:fragment:1.3.6")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.9.1")
    implementation("androidx.compose.ui:ui:1.8.3")
    implementation("androidx.compose.foundation:foundation:1.8.3")
    implementation("androidx.compose.material3:material3:1.3.2")
    implementation("com.google.android.gms:play-services-code-scanner:16.1.0")

    testImplementation("junit:junit:4.13.2")
    testImplementation("org.json:json:20240303")
}

// Builds libaudiobridge.so (crates/android-native) into src/main/jniLibs before AGP merges native libs.
// Skip with -PskipRust (e.g. when the Rust toolchain is unavailable and a prebuilt .so is already present).
val repoRoot: File = rootProject.projectDir.parentFile
val jniLibsDir: File = project.file("src/main/jniLibs")
val skipRust: Boolean = providers.gradleProperty("skipRust").isPresent

val buildRustLib by tasks.registering(Exec::class) {
    group = "build"
    description = "Builds libaudiobridge.so for arm64-v8a with cargo-ndk."
    workingDir = repoRoot
    commandLine(
        "cargo", "ndk",
        "-t", "arm64-v8a",
        "-o", jniLibsDir.absolutePath,
        "build", "--release",
        "-p", "audiobridge-android",
    )
    if (System.getenv("ANDROID_HOME").isNullOrEmpty() && System.getenv("ANDROID_NDK_HOME").isNullOrEmpty()) {
        environment("ANDROID_HOME", android.sdkDirectory.absolutePath)
    }
    // cargo tracks its own incremental state; always let it decide what to rebuild.
    outputs.upToDateWhen { false }
    onlyIf { !skipRust }
}

tasks.configureEach {
    if (name.startsWith("merge") && (name.endsWith("JniLibFolders") || name.endsWith("NativeLibs"))) {
        dependsOn(buildRustLib)
    }
}
