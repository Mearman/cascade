package co.mearman.cascade

import android.provider.DocumentsContract.Document
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import uniffi.cascade_ffi.DirEntry

/**
 * Robolectric integration tests for the SAF cursor-building logic. Runs on the
 * JVM (no emulator) against the real Android `MatrixCursor` and `MimeTypeMap`,
 * driving the provider's cursor construction from a fake `DirEntry` list — the
 * Android-framework half that the pure DocIdLogic tests don't cover.
 */
@RunWith(RobolectricTestRunner::class)
class CursorBuilderTest {

    private fun stringAt(cursor: android.database.MatrixCursor, row: Int, column: String): String? {
        cursor.moveToPosition(row)
        return cursor.getString(cursor.getColumnIndexOrThrow(column))
    }

    @Test
    fun childrenCursor_emits_one_row_per_entry_with_child_doc_ids() {
        val entries = listOf(
            DirEntry("report.txt", false),
            DirEntry("Photos", true),
        )
        val cursor = CursorBuilder.childrenCursor("/local", entries, null)

        assertEquals(2, cursor.count)
        // The file row: child doc id joined under the parent, a non-empty mime.
        assertEquals("/local/report.txt", stringAt(cursor, 0, Document.COLUMN_DOCUMENT_ID))
        assertEquals("report.txt", stringAt(cursor, 0, Document.COLUMN_DISPLAY_NAME))
        val fileMime = stringAt(cursor, 0, Document.COLUMN_MIME_TYPE)
        assertNotNull("file has a mime type", fileMime)
        assertTrue("file is not a directory mime", fileMime != Document.MIME_TYPE_DIR)
        // The directory row: child doc id joined, dir mime.
        assertEquals("/local/Photos", stringAt(cursor, 1, Document.COLUMN_DOCUMENT_ID))
        assertEquals("Photos", stringAt(cursor, 1, Document.COLUMN_DISPLAY_NAME))
        assertEquals(Document.MIME_TYPE_DIR, stringAt(cursor, 1, Document.COLUMN_MIME_TYPE))
    }

    @Test
    fun documentCursor_describes_a_single_file() {
        val entry = DirEntry("notes.md", false)
        val cursor = CursorBuilder.documentCursor("/local/notes.md", entry, null)

        assertEquals(1, cursor.count)
        assertEquals("/local/notes.md", stringAt(cursor, 0, Document.COLUMN_DOCUMENT_ID))
        assertEquals("notes.md", stringAt(cursor, 0, Document.COLUMN_DISPLAY_NAME))
    }

    @Test
    fun rootDocumentCursor_is_a_directory() {
        val cursor = CursorBuilder.rootDocumentCursor("/", "Cascade", null)
        assertEquals(1, cursor.count)
        assertEquals(Document.MIME_TYPE_DIR, stringAt(cursor, 0, Document.COLUMN_MIME_TYPE))
    }

    @Test
    fun mimeOf_always_returns_a_non_empty_type() {
        // Whatever the platform map knows, mimeOf never returns null or empty.
        for (name in listOf("readme.txt", "archive.zzzunknown", "Makefile", "photo.jpg")) {
            val mime = CursorBuilder.mimeOf(name)
            assertTrue("non-empty mime for $name: $mime", mime.isNotEmpty())
        }
    }

    @Test
    fun mimeOf_unknown_extension_falls_back_to_octet_stream() {
        // An extension the platform map does not know resolves to the fallback.
        assertEquals("application/octet-stream", CursorBuilder.mimeOf("archive.zzzunknown"))
    }

    @Test
    fun mimeOf_no_extension_is_octet_stream() {
        assertEquals("application/octet-stream", CursorBuilder.mimeOf("Makefile"))
    }
}
