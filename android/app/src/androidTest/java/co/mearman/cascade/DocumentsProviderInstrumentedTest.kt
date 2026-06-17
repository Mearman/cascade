package co.mearman.cascade

import android.provider.DocumentsContract
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

/**
 * On-device e2e test: drives the REAL DocumentsProvider through the Android
 * ContentResolver, so it exercises the full provider -> CascadeNode -> FFI ->
 * engine -> local-backend stack on an actual Android runtime (the emulator in
 * CI), with the native .so loaded by JNA. Seeds a file under the node's files
 * dir and asserts it surfaces through queryRoots/queryChildDocuments, then drives
 * a create -> list -> rename -> delete round-trip through the Storage Access
 * Framework's DocumentsContract to exercise the write verbs end to end.
 */
@RunWith(AndroidJUnit4::class)
class DocumentsProviderInstrumentedTest {

    private val authority = "co.mearman.cascade.documents"

    private fun rootsUri() = DocumentsContract.buildRootsUri(authority)

    private fun childNames(parentDocumentId: String): List<String> {
        val ctx = InstrumentationRegistry.getInstrumentation().targetContext
        val children = DocumentsContract.buildChildDocumentsUri(authority, parentDocumentId)
        val names = mutableListOf<String>()
        ctx.contentResolver.query(
            children,
            arrayOf(DocumentsContract.Document.COLUMN_DISPLAY_NAME),
            null, null, null,
        )?.use { c -> while (c.moveToNext()) names += c.getString(0) }
        return names
    }

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

        val names = childNames("/local")

        assertNotNull("listing returned a cursor", names)
        assertTrue("seeded file surfaced via the provider: $names", names.any { it == "e2e-probe.txt" })
    }

    @Test
    fun createDocument_renameDocument_deleteDocument_roundTrip() {
        val ctx = InstrumentationRegistry.getInstrumentation().targetContext
        val resolver = ctx.contentResolver
        val parentDocId = "/local"

        // createDocument (a directory) under /local.
        val createdDocId = DocumentsContract.createDocument(
            resolver,
            DocumentsContract.buildDocumentUri(authority, parentDocId),
            DocumentsContract.Document.MIME_TYPE_DIR,
            "saf-roundtrip-dir",
        )?.lastPathSegment
            ?: error("createDocument returned null URI")

        try {
            assertTrue(
                "createDocument produced a child doc id: $createdDocId",
                createdDocId == "/local/saf-roundtrip-dir",
            )

            // The new directory must surface in a fresh listing.
            assertTrue(
                "created dir lists: ${childNames(parentDocId)}",
                childNames(parentDocId).any { it == "saf-roundtrip-dir" },
            )

            // renameDocument: rename the directory and confirm the new id.
            val renamedDocId = DocumentsContract.renameDocument(
                resolver,
                DocumentsContract.buildDocumentUri(authority, createdDocId),
                "saf-roundtrip-renamed",
            )?.lastPathSegment
                ?: error("renameDocument returned null URI")

            assertTrue(
                "rename produced the renamed doc id: $renamedDocId",
                renamedDocId == "/local/saf-roundtrip-renamed",
            )
            assertFalse(
                "old name gone after rename: ${childNames(parentDocId)}",
                childNames(parentDocId).any { it == "saf-roundtrip-dir" },
            )
            assertTrue(
                "renamed dir lists: ${childNames(parentDocId)}",
                childNames(parentDocId).any { it == "saf-roundtrip-renamed" },
            )

            // deleteDocument: remove and confirm it no longer lists.
            DocumentsContract.deleteDocument(
                resolver,
                DocumentsContract.buildDocumentUri(authority, renamedDocId),
            )
            assertFalse(
                "deleted dir is gone: ${childNames(parentDocId)}",
                childNames(parentDocId).any { it == "saf-roundtrip-renamed" },
            )
        } finally {
            // Clean up anything left behind if an assertion threw.
            runCatching {
                DocumentsContract.deleteDocument(
                    resolver,
                    DocumentsContract.buildDocumentUri(authority, createdDocId),
                )
            }
            runCatching {
                DocumentsContract.deleteDocument(
                    resolver,
                    DocumentsContract.buildDocumentUri(authority, "/local/saf-roundtrip-renamed"),
                )
            }
        }
    }
}
