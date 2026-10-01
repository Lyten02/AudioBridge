# JNI entry points are resolved by name from libaudiobridge.so.
-keep class app.audiobridge.NativeBridge { *; }
# Native code calls StatusListener.onStatus(String) on the registered listener.
-keep interface app.audiobridge.StatusListener { *; }
-keep class * implements app.audiobridge.StatusListener { public void onStatus(java.lang.String); }
