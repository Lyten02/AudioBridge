# JNI entry points are resolved by name from libaudiobridge.so.
-keep class app.audiobridge.NativeBridge { *; }
# Native code calls onStatus(String), onRemoteMic(boolean) and onRemoteVolume(int) on the registered listener by name.
-keep interface app.audiobridge.StatusListener { *; }
-keep class * implements app.audiobridge.StatusListener {
    public void onStatus(java.lang.String);
    public void onRemoteMic(boolean);
    public void onRemoteVolume(int);
}
