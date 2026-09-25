# MicBridge is the Kotlin AudioRecord capture layer. Its methods are invoked
# entirely from native (JNI) code — `startRecording`/`stopRecording` are up-called
# from the Rust engine and `nativePush`/`nativeCacheClass` are the native bridge —
# so the R8 shrinker sees no Kotlin callers and would strip or rename them, breaking
# the JNI `GetStaticMethodID` lookups ("Method not found: startRecording"). Keep the
# whole class and its members.
-keep class com.ambientdisplay.ambient_display.MicBridge { *; }
-keepclassmembers class com.ambientdisplay.ambient_display.MicBridge { *; }

# CameraBridge is the camera-proximity capture layer (Plan.MD §5), the exact twin of
# MicBridge: `startCamera`/`stopCamera` are up-called from the Rust engine and
# `nativePushLuma`/`nativeCacheClass` are the native bridge. Same JNI-only reachability,
# so keep it whole or R8 strips the up-call targets ("Method not found: startCamera").
-keep class com.ambientdisplay.ambient_display.CameraBridge { *; }
-keepclassmembers class com.ambientdisplay.ambient_display.CameraBridge { *; }
