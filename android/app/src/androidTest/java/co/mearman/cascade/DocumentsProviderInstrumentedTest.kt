package co.mearman.cascade

import android.provider.DocumentsContract
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

/**
 * On-device e2e test: drives the REAL DocumentsProvider through the Android
 * ContentResolver, so it exercises the full provider -> CascadeNode -> FFI ->
 * engine -> local-backend stack on an actual Android runtime (the emulator in
 * CI), with the native .so loaded by JNA. Seeds a file under the node's files
 * dir and asserts it surfaces through queryRoots/queryChildDocuments.
 */
@RunWith(AndroidJUnit4::class)
class DocumentsProviderInstrumentedTest {

    private val authority = "co.mearman.cascade.documents"

    private fun rootsUri() = DocumentsContract.buildRootsUri(authority)

    @Test
    fun queryRoots_advertisesTheCascadeRoot() {
        val resolver = InstrumentationRegistry.getInstrumentation().targetContext.contentResolver
        val titles = mutableListOf<String>()
        resolver.query(rootsUri(), arrayOf(DocumentsContract.Root.COLUMN_TITLE), null, null, null)?.use { c ->
            while (c.moveToNext()) titles += c.getString(0)
        }
        assertTrue("Cascade root advertised: $titles", titles.any { it == "Cascade" })
    }

    @Test
    fun queryChildDocuments_listsSeededFile() {
        val ctx = InstrumentationRegistry.getInstrumentation().targetContext
        // The node mounts the local backend over <configDir>/files (its files
        // root); configDir is the app's filesDir. Seed there, then list the
        // `local` document's children through the provider and assert it appears.
        val filesRoot = java.io.File(ctx.filesDir, "files").apply { mkdirs() }
        java.io.File(filesRoot, "e2e-probe.txt").writeText("hello e2e")

        // The local backend is mounted at "/local"; list its children. (The root
        // document "/" lists the mount points, so its child is "local".)
        val children = DocumentsContract.buildChildDocumentsUri(authority, "/local")
        val names = mutableListOf<String>()
        ctx.contentResolver.query(
            children,
            arrayOf(DocumentsContract.Document.COLUMN_DISPLAY_NAME),
            null, null, null,
        )?.use { c -> while (c.moveToNext()) names += c.getString(0) }

        assertNotNull("listing returned a cursor", names)
        assertTrue("seeded file surfaced via the provider: $names", names.any { it == "e2e-probe.txt" })
    }
}
