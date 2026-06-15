package co.mearman.cascade

import android.content.Context
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import uniffi.cascade_ffi.CascadeNode
import uniffi.cascade_ffi.cascadeNodeNew

/**
 * Process-wide owner of the running [CascadeNode].
 *
 * The engine runs in-process; there is exactly one node per app process, rooted
 * at the app's private files directory. The node is built and started on first
 * use and then reused for the lifetime of the process. Construction and start are
 * async on the FFI, so callers on the binder threads the DocumentsProvider runs on
 * obtain the node through [blockingGet], which drives the suspend path to
 * completion.
 */
object CascadeNodeHolder {
    private val initMutex = Mutex()

    @Volatile
    private var node: CascadeNode? = null

    /**
     * Return the started node, building and starting it on first call.
     *
     * Safe to call concurrently: the first caller wins the init lock and the rest
     * observe the started node.
     */
    suspend fun get(context: Context): CascadeNode {
        node?.let { return it }
        return initMutex.withLock {
            node?.let { return it }
            val configDir = context.filesDir.absolutePath
            val built = cascadeNodeNew(configDir)
            built.start()
            node = built
            built
        }
    }

    /**
     * Blocking accessor for synchronous provider callbacks.
     */
    fun blockingGet(context: Context): CascadeNode = runBlocking { get(context.applicationContext) }
}
