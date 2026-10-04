package dev.idfremote.android

import java.util.concurrent.SynchronousQueue
import java.util.concurrent.ThreadPoolExecutor
import java.util.concurrent.TimeUnit
import java.util.concurrent.locks.ReentrantLock
import kotlin.concurrent.withLock

/** Attachment-scoped leases keep JNI calls alive until orderly native/Java close. */
internal class GatewaySession(private val handle: Long) {
    private val lock = ReentrantLock()
    private val idle = lock.newCondition()
    private var closing = false
    private var calls = 0
    // One application wait, with no backlog. Console submission uses its own lane.
    private val applications = ThreadPoolExecutor(1, 1, 0, TimeUnit.MILLISECONDS, SynchronousQueue())

    val isOpen: Boolean get() = lock.withLock { !closing }

    private fun acquire() = lock.withLock {
        check(!closing) { "Connection is closing; command not sent" }
        calls++
    }

    private fun release() = lock.withLock {
        calls--
        if (calls == 0) idle.signalAll()
    }

    fun <T> use(action: (Long) -> T): T {
        acquire()
        try { return action(handle) } finally { release() }
    }

    fun application(action: (Long) -> Unit) {
        acquire()
        try {
            applications.execute { try { action(handle) } finally { release() } }
        } catch (e: Exception) { release(); throw e }
    }

    /** Called only by the lifecycle owner, off the UI thread. */
    fun close(closeNative: (Long) -> Unit) {
        lock.withLock {
            closing = true
            applications.shutdown()
            while (calls != 0) idle.awaitUninterruptibly()
        }
        closeNative(handle)
    }
}
