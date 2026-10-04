package dev.idfremote.android

import org.junit.Assert.*
import org.junit.Test
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.RejectedExecutionException
import java.util.concurrent.TimeUnit

class GatewaySessionTest {
    @Test fun consoleProceedsWhileApplicationWaitsAndApplicationBacklogIsRejected() {
        val session = GatewaySession(7)
        val started = CountDownLatch(1)
        val finish = CountDownLatch(1)
        val console = Executors.newSingleThreadExecutor()
        try {
            session.application { started.countDown(); finish.await() }
            assertTrue(started.await(2, TimeUnit.SECONDS))
            assertEquals(7L, console.submit<Long> { session.use { it } }.get(1, TimeUnit.SECONDS))
            try { session.application { fail("Busy call must not execute") }; fail("Expected busy rejection") }
            catch (_: RejectedExecutionException) { }
        } finally {
            finish.countDown()
            session.close { }
            console.shutdownNow()
        }
    }

    @Test fun closeWaitsForApplicationAndConsoleLeasesAndRejectsNewCalls() {
        val session = GatewaySession(9)
        val appStarted = CountDownLatch(1)
        val consoleStarted = CountDownLatch(1)
        val appFinish = CountDownLatch(1)
        val consoleFinish = CountDownLatch(1)
        val closing = CountDownLatch(1)
        val closed = CountDownLatch(1)
        val threads = Executors.newFixedThreadPool(2)
        try {
            session.application { appStarted.countDown(); appFinish.await() }
            val console = threads.submit { session.use { consoleStarted.countDown(); consoleFinish.await() } }
            assertTrue(appStarted.await(2, TimeUnit.SECONDS))
            assertTrue(consoleStarted.await(2, TimeUnit.SECONDS))
            val close = threads.submit { closing.countDown(); session.close { assertEquals(9L, it); closed.countDown() } }
            assertTrue(closing.await(2, TimeUnit.SECONDS))
            val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(2)
            while (session.isOpen && System.nanoTime() < deadline) Thread.yield()
            assertFalse(session.isOpen)
            try { session.use { fail("Closed session must not send") }; fail("Expected closing rejection") }
            catch (_: IllegalStateException) { }
            appFinish.countDown()
            assertFalse(closed.await(100, TimeUnit.MILLISECONDS))
            consoleFinish.countDown()
            console.get(2, TimeUnit.SECONDS)
            close.get(2, TimeUnit.SECONDS)
            assertEquals(0L, closed.count)
        } finally {
            appFinish.countDown(); consoleFinish.countDown()
            threads.shutdownNow()
        }
    }
}
