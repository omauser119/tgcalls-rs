package org.tgcalls.rust;
final class NativeBridge {
    static { System.loadLibrary("tgcalls_ffi"); }
    static native long create();
    static native byte[] request(long handle, byte[] request);
    static native void destroy(long handle);
    private NativeBridge() {}
}
