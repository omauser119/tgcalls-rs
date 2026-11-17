package org.tgcalls.rust;

/** Ordinary in-process mode. Use a separate HardenedClient for isolated calls. */
public final class NativeClient implements AutoCloseable {
    private long handle = NativeBridge.create();
    public NativeClient() { if (handle == 0) throw new IllegalStateException("native worker unavailable"); }
    public synchronized byte[] request(byte[] request) {
        if (handle == 0) throw new IllegalStateException("client closed");
        if (request.length == 0 || request.length > 8 * 1024 * 1024) throw new IllegalArgumentException("request size");
        return NativeBridge.request(handle, request);
    }
    @Override public synchronized void close() { if (handle != 0) NativeBridge.destroy(handle); handle = 0; }
}
