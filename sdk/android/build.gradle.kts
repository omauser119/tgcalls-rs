plugins { id("com.android.library") version "8.9.1" }
android {
    namespace = "org.tgcalls.rust"
    compileSdk = 35
    defaultConfig { minSdk = 28 }
    sourceSets["main"].jniLibs.srcDir("src/main/jniLibs")
}
