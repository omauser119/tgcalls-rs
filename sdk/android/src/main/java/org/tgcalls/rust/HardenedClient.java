package org.tgcalls.rust;

import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.content.ServiceConnection;
import android.os.IBinder;
import android.os.Parcel;
import android.os.RemoteException;
import android.os.SharedMemory;
import android.system.ErrnoException;
import java.nio.ByteBuffer;
import java.util.concurrent.CompletableFuture;

/** API 28+. Call request on a worker thread. Binder death means call state was
 * lost: reconnect and repeat authenticated negotiation, never replay old keys. */
public final class HardenedClient implements AutoCloseable {
    private final Context context;
    private final SharedMemory memory;
    private final ByteBuffer mapping;
    private volatile IBinder remote;
    private final CompletableFuture<Void> ready = new CompletableFuture<>();
    private final ServiceConnection connection = new ServiceConnection() {
        @Override public void onServiceConnected(ComponentName name, IBinder binder) { remote = binder; ready.complete(null); }
        @Override public void onServiceDisconnected(ComponentName name) { remote = null; }
        @Override public void onBindingDied(ComponentName name) { remote = null; }
        @Override public void onNullBinding(ComponentName name) { ready.completeExceptionally(new IllegalStateException("isolated service unavailable")); }
    };
    public HardenedClient(Context context) throws ErrnoException {
        this.context = context.getApplicationContext();
        memory = SharedMemory.create("tgcalls-ipc", IsolatedWorkerService.MAX_INPUT + IsolatedWorkerService.MAX_OUTPUT);
        mapping = memory.mapReadWrite();
        if (!this.context.bindService(new Intent(this.context, IsolatedWorkerService.class), connection, Context.BIND_AUTO_CREATE)) {
            SharedMemory.unmap(mapping); memory.close(); throw new IllegalStateException("isolated service unavailable");
        }
    }
    public CompletableFuture<Void> ready() { return ready; }
    public synchronized byte[] request(byte[] bytes) throws RemoteException {
        IBinder binder = remote;
        if (binder == null) throw new IllegalStateException("worker disconnected; reconnect and re-establish call");
        if (bytes.length == 0 || bytes.length > IsolatedWorkerService.MAX_INPUT) throw new IllegalArgumentException("request size");
        mapping.position(0); mapping.put(bytes);
        Parcel input = Parcel.obtain(), output = Parcel.obtain();
        try {
            input.writeInterfaceToken(IsolatedWorkerService.DESCRIPTOR); input.writeInt(bytes.length); memory.writeToParcel(input, 0);
            if (!binder.transact(IsolatedWorkerService.REQUEST, input, output, 0)) throw new RemoteException("worker transaction refused");
            output.readException(); int length = output.readInt();
            if (length <= 0 || length > IsolatedWorkerService.MAX_OUTPUT) throw new IllegalStateException("response size");
            byte[] response = new byte[length]; mapping.position(IsolatedWorkerService.MAX_INPUT); mapping.get(response); return response;
        } finally { input.recycle(); output.recycle(); }
    }
    @Override public synchronized void close() { remote = null; context.unbindService(connection); SharedMemory.unmap(mapping); memory.close(); }
}
