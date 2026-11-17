package org.tgcalls.rust;

import android.app.Service;
import android.content.Intent;
import android.os.Binder;
import android.os.IBinder;
import android.os.Parcel;
import android.os.SharedMemory;
import android.os.Process;
import java.nio.ByteBuffer;

/** Runs under an Android isolated UID. The app brokers UDP, permissions and
 * capture. Do not apply a native seccomp filter to ART or Binder threads. */
public final class IsolatedWorkerService extends Service {
    static final String DESCRIPTOR = "org.tgcalls.rust.IsolatedWorker";
    static final int REQUEST = IBinder.FIRST_CALL_TRANSACTION;
    static final int MAX_INPUT = 8 * 1024 * 1024;
    static final int MAX_OUTPUT = 32 * 1024 * 1024;
    private long worker;
    private final Binder binder = new Binder() {
        @Override protected synchronized boolean onTransact(int code, Parcel input, Parcel output, int flags) {
            if (code == INTERFACE_TRANSACTION) { output.writeString(DESCRIPTOR); return true; }
            if (code != REQUEST) return false;
            input.enforceInterface(DESCRIPTOR);
            int length = input.readInt();
            if (length <= 0 || length > MAX_INPUT) throw new IllegalArgumentException("request size");
            try (SharedMemory memory = SharedMemory.CREATOR.createFromParcel(input)) {
                if (memory.getSize() != MAX_INPUT + MAX_OUTPUT) throw new IllegalArgumentException("shared memory size");
                ByteBuffer mapping = memory.mapReadWrite();
                try {
                    byte[] request = new byte[length]; mapping.get(request);
                    if (worker == 0) worker = NativeBridge.create();
                    byte[] response = NativeBridge.request(worker, request);
                    if (response.length > MAX_OUTPUT) throw new IllegalStateException("response budget");
                    mapping.position(MAX_INPUT); mapping.put(response);
                    output.writeNoException(); output.writeInt(response.length); return true;
                } finally { SharedMemory.unmap(mapping); }
            } catch (Exception error) { output.writeException(error); return true; }
        }
    };
    @Override public IBinder onBind(Intent intent) {
        // Fail closed if a manifest merger removed android:isolatedProcess.
        if (!Process.isIsolated()) throw new SecurityException("Hardened Mode requires an isolated UID");
        return binder;
    }
    @Override public void onDestroy() { if (worker != 0) NativeBridge.destroy(worker); worker = 0; super.onDestroy(); }
}
